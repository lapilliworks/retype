//! The request loop, and what the daemon answers.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use retype_core::error::Code;
use retype_core::job::{Job, JobError, JobState, Output, OutputState, Source, Tier};
use retype_core::journal::Journal;
use retype_core::policy::Policy;
use retype_core::watches;
use retype_ipc::{Hello, HelloResult, IPC_VERSION, Request, Response, WireError, read_frame, same_major};
use serde_json::json;

use crate::daemon::machine;
use crate::daemon::watcher::Shared;
use crate::error::Result;

pub struct Server {
	endpoint: Option<String>,
	shared: Arc<Shared>,
	pool: Arc<crate::daemon::queue::Pool>,
	machine: String,
	started: Instant,
	stopped: bool,
}

impl Server {
	/// Open the state the daemon owns, replaying the journal so `jobs.list`
	/// answers from what survives rather than from what this process did.
	pub fn open(state_dir: &Path) -> Result<Self> {
		let registry = crate::config::load()?;
		let (mut journal, replay) = Journal::open(&state_dir.join("jobs.jsonl"))?;

		if let Some(quarantined) = &replay.quarantined {
			eprintln!(
				"retype: the journal was damaged; the previous file is at {}",
				quarantined.display()
			);
		}

		// Startup step 6, `docs/dev/architecture/state.md` §6: retention before compaction, so the
		// records a rewrite would have dropped are already gone. Nothing here is
		// a conversion, so it never blocks one.
		let retention = registry.undo().retention;
		let compact = journal.compact(retention)?;
		let jobs = compact.kept.unwrap_or(replay.jobs);
		let swept = retype_core::undo::Store::open(state_dir).sweep(retention)?;

		if compact.dropped > 0 {
			eprintln!("retype: dropped {} journal record(s) past the horizon", compact.dropped);
		}

		if !swept.is_empty() {
			eprintln!("retype: dropped {} undo record(s) past retention", swept.len());
		}

 //: asking needs notifications, which land in. Until
		// then the answer is the one a user would give when a conversion is waiting.
		if registry.undo().on_overflow == retype_core::undo::Overflow::Ask {
			eprintln!("retype: [undo] on_overflow = 'ask' behaves as 'evict' until the overflow actions exist");
		}

		// The log directory exists before any job runs, so a tool's output has
		// somewhere to go without the runner having to create directories mid-flight.
		std::fs::create_dir_all(state_dir.join("logs"))?;

		let machine = machine::load_or_create(state_dir)?;

		// Decided once, at startup: the session is either there or it is not, and a
		// daemon that re-asked on every batch would spawn a process to find out.
		let policy = registry.notify();
		let notifier = match policy.enabled {
			true => Some(crate::notify::Channel::detect(policy.actions)),
			false => None,
		};

		let shared = Arc::new(Shared {
			registry: std::sync::RwLock::new(std::sync::Arc::new(registry)),
			machine: machine.clone(),
			state_dir: state_dir.to_path_buf(),
			journal: Mutex::new(journal),
			jobs: Mutex::new(jobs),
			recent: Default::default(),
			dropped: AtomicU64::new(0),
			skipped: AtomicU64::new(0),
			placeholders: AtomicU64::new(0),
			primed: AtomicU64::new(0),
			events: Mutex::new(crate::daemon::events::Events::default()),
			policies: Default::default(),
			batches: Default::default(),
			notifier: Mutex::new(notifier),
		});

		let pool = crate::daemon::queue::Pool::start(Arc::clone(&shared), crate::daemon::queue::runners());

		// Before anything watches a directory or answers a client: a job that was in
		// flight when the last daemon died is finished or declared, not left hanging,
		// `docs/dev/architecture/state.md` §6.
		crate::daemon::reconcile::run(&shared, &pool);

		Ok(Self {
			endpoint: None,
			shared,
			pool,
			machine,
			started: Instant::now(),
			stopped: false,
		})
	}

	/// What the watcher threads need, so one registry and one journal serve both.
	pub fn shared(&self) -> Arc<Shared> {
		Arc::clone(&self.shared)
	}

	/// The runners, which the daemon stops with everything else.
	pub fn pool(&self) -> Arc<crate::daemon::queue::Pool> {
		Arc::clone(&self.pool)
	}

	/// Whether the running jobs have finished, `docs/dev/architecture/ipc.md` §2.
	pub fn idle(&self) -> bool {
		self.pool.idle()
	}

	pub fn journal_path(&self) -> PathBuf {
		match self.shared.journal.lock() {
			Ok(journal) => journal.path().to_path_buf(),
			Err(_) => PathBuf::new(),
		}
	}

	pub fn stopped(&self) -> bool {
		self.stopped
	}

	/// The endpoint this daemon is reachable at, which changes per platform and
	/// per machine, so it is reported rather than assumed.
	pub fn set_endpoint(&mut self, endpoint: impl Into<String>) {
		self.endpoint = Some(endpoint.into());
	}

	pub fn machine(&self) -> &str {
		&self.machine
	}

	/// Serve one client until it disconnects or asks the daemon to stop.
	///
	/// Takes the two halves rather than a stream so that the same loop serves a
	/// Unix socket and a Windows pipe, and so that it can be tested without a
	/// socket at all.
	pub fn serve_connection(&mut self, reader: &mut impl BufRead, writer: &mut impl Write) -> Result<()> {
		self.serve_connection_from(reader, writer, None)
	}

	/// The same, with a frame that was already read. A Windows pipe can only be
	/// impersonated after its client has written, so the first frame is read
	/// before authentication and handed over rather than read twice,
	/// `docs/dev/architecture/ipc.md` §4.
	pub fn serve_connection_from(
		&mut self,
		reader: &mut impl BufRead,
		writer: &mut impl Write,
		first: Option<serde_json::Value>,
	) -> Result<()> {
		let mut greeted = false;
		let mut pending = first;

		loop {
			let frame = match pending.take() {
				Some(frame) => frame,
				None => match read_frame(reader) {
					Ok(Some(frame)) => frame,
					Ok(None) => return Ok(()),
					Err(error) => {
						let response = Response::error(0, WireError::new(Code::IpcFrame, error.to_string()));
						let _ = retype_ipc::write_frame(writer, &response);
						return Ok(());
					}
				},
			};

			let request: Request = match serde_json::from_value(frame) {
				Ok(request) => request,
				Err(error) => {
					let response = Response::error(0, WireError::new(Code::IpcFrame, error.to_string()));
					retype_ipc::write_frame(writer, &response)?;
					return Ok(());
				}
			};

			if !greeted {
				if request.method != "hello" {
					let response = Response::error(
						request.id,
						WireError::new(Code::IpcHandshake, "send hello before any other method"),
					);
					retype_ipc::write_frame(writer, &response)?;
					return Ok(());
				}

				let hello: Hello = match request.parse() {
					Ok(hello) => hello,
					Err(error) => {
						let response =
							Response::error(request.id, WireError::new(Code::IpcHandshake, error.to_string()));
						retype_ipc::write_frame(writer, &response)?;
						return Ok(());
					}
				};

				if !same_major(&hello.ipc, IPC_VERSION) {
					let response = Response::error(
						request.id,
						WireError::new(
							Code::IpcVersion,
							format!("this daemon speaks {IPC_VERSION}, the client speaks {}", hello.ipc),
						),
					);
					retype_ipc::write_frame(writer, &response)?;
					return Ok(());
				}

				greeted = true;
				let response = Response::ok(request.id, self.hello())?;
				retype_ipc::write_frame(writer, &response)?;
				continue;
			}

			let response = self.answer(&request)?;
			let stopping = request.method == "shutdown";
			retype_ipc::write_frame(writer, &response)?;

			if stopping {
				return Ok(());
			}
		}
	}

	/// Stop a job that is waiting or running, `docs/dev/architecture/ipc.md` §6. A job that has
	/// finished is not cancellable, which is a state error rather than a silent
	/// success.
	/// One job in full, which is what a client polling a submitted conversion
	/// needs: the terminal record carries the outputs, their bytes and their
	/// errors.
	fn get(&self, request: &Request) -> Result<Response> {
		let Some(written) = request.params.get("id").and_then(|id| id.as_str()) else {
			return Ok(Response::error(
				request.id,
				WireError::new(Code::IpcMethod, "jobs.get needs an id"),
			));
		};

		match self.find(written) {
			Some(job) => match job.to_record() {
				Ok(value) => Ok(Response::ok(request.id, value)?),
				Err(error) => Ok(Response::error(
					request.id,
					WireError::new(Code::IpcFrame, error.to_string()),
				)),
			},
			None => Ok(Response::error(
				request.id,
				WireError::new(Code::JobState, format!("no job matches '{written}'")),
			)),
		}
	}

	/// What the journal says about a path, `docs/provenance.md` §8.
	fn why(&self, request: &Request) -> Result<Response> {
		use retype_core::provenance;

		let Some(written) = request.params.get("path").and_then(|path| path.as_str()) else {
			return Ok(Response::error(
				request.id,
				WireError::new(Code::IpcMethod, "why needs a path"),
			));
		};

		let Ok(jobs) = self.shared.jobs.lock() else {
			return Ok(Response::error(
				request.id,
				WireError::new(Code::IpcInternal, "the job list is poisoned"),
			));
		};

		let answer = provenance::answer(&jobs, Path::new(written));
		let mut fields = json!({
			"path": written,
			"store": answer.store.as_str(),
			"confidence": answer.confidence.as_str(),
		});

		if let Some(intent) = &answer.intent {
			fields["intent"] = json!(intent);
		}

		if let Some(job) = answer.job {
			fields["job"] = json!(job.id);
			fields["intent"] = json!(job.intent);
			fields["source"] = json!(job.source.format);
			fields["backend"] = json!(job.backend);
			fields["engine"] = json!(job.engine);
			fields["created"] = json!(job.created);
			fields["machine"] = json!(job.machine);
			fields["outputs"] = match serde_json::to_value(&job.outputs) {
				Ok(value) => value,
				Err(error) => {
					return Ok(Response::error(
						request.id,
						WireError::new(Code::IpcFrame, error.to_string()),
					));
				}
			};
		}

		if let Some(output) = answer.output {
			fields["target"] = json!(output.format);
			fields["tier"] = json!(output.tier);
			fields["bytes"] = json!(output.bytes);
		}

		Ok(Response::ok(request.id, fields)?)
	}

	/// `retype convert` is a job like any other, `docs/dev/architecture/ipc.md` §6: submitted,
	/// journalled and run by a runner, so it shares cancellation and the queue's
	/// accounting.
	fn submit(&self, request: &Request) -> Result<Response> {
		use crate::daemon::immediate::{self, Request as Convert};

		let Some(input) = request.params.get("input").and_then(|input| input.as_str()) else {
			return Ok(Response::error(
				request.id,
				WireError::new(Code::IpcMethod, "jobs.submit needs an input"),
			));
		};

		let Some(intent) = request.params.get("intent").and_then(|intent| intent.as_str()) else {
			return Ok(Response::error(
				request.id,
				WireError::new(Code::IpcMethod, "jobs.submit needs an intent"),
			));
		};

		// The only mode that exists so far. A rename is queued by the watcher
		// rather than by a client, so there is nothing else to name yet.
		if let Some(mode) = request.params.get("mode").and_then(|mode| mode.as_str()) {
			if mode != "immediate" {
				return Ok(Response::error(
					request.id,
					WireError::new(Code::IpcMethod, format!("'{mode}' is not a mode this daemon runs")),
				));
			}
		}

		let asked = Convert {
			input: PathBuf::from(input),
			intent: intent.to_string(),
			output: request
				.params
				.get("output")
				.and_then(|output| output.as_str())
				.map(str::to_string),
			force: request
				.params
				.get("force")
				.and_then(|force| force.as_bool())
				.unwrap_or(false),
		};

		// Everything that can fail before a byte moves fails here, so the caller
		// gets a diagnosis rather than a job that will only fail later.
		let prepared = match immediate::prepare(&self.shared.registry(), &asked) {
			Ok(prepared) => prepared,
			Err(problems) => {
				let code = problems.codes().first().cloned().unwrap_or(Code::ToolFailed);

				return Ok(Response::error(request.id, WireError::new(code, problems.to_string())));
			}
		};

		let id = retype_core::job::JobId::now()?;
		crate::daemon::watcher::publish(&self.shared, &immediate::queued(&self.shared, &prepared, &id));

		let work = crate::daemon::queue::Work {
			id: id.clone(),
			kind: crate::daemon::queue::Kind::Immediate(asked),
		};

		if !self.pool.submit(work) {
			return Ok(Response::error(
				request.id,
				WireError::new(Code::JobState, "the queue is full, so nothing was submitted"),
			));
		}

		Ok(Response::ok(request.id, json!({"id": id, "state": "queued"}))?)
	}

	fn cancel(&self, request: &Request) -> Result<Response> {
		let Some(written) = request.params.get("id").and_then(|id| id.as_str()) else {
			return Ok(Response::error(
				request.id,
				WireError::new(Code::IpcMethod, "jobs.cancel needs an id"),
			));
		};

		let Ok(id) = retype_core::job::JobId::parse(written) else {
			return Ok(Response::error(
				request.id,
				WireError::new(Code::JobState, format!("'{written}' is not a job id")),
			));
		};

		// A parked job never reached the queue, so it is cancelled in the record rather
		// than by stopping anything, `docs/deferral.md` §8.
		if let Some(parked) = self
			.find(id.as_str())
			.filter(|job| matches!(job.state, JobState::Pending | JobState::NeedsAttention))
		{
			let mut cancelled = parked;
			cancelled.set_state(JobState::Cancelled);
			crate::daemon::watcher::publish(&self.shared, &cancelled);

			return Ok(Response::ok(
				request.id,
				json!({"id": id, "cancelled": true, "state": "cancelled"}),
			)?);
		}

		let (found, queued) = self.pool.cancel(&id);

		let response = match found {
			crate::daemon::queue::Cancelled::Queued => {
				if let Some(work) = queued {
					self.record_cancelled(&id, &work);
				}

				Response::ok(request.id, json!({"id": id, "cancelled": true, "state": "cancelled"}))?
			}
			crate::daemon::queue::Cancelled::Running => {
				Response::ok(request.id, json!({"id": id, "cancelled": true, "state": "cancelling"}))?
			}
			crate::daemon::queue::Cancelled::Unknown => Response::error(
				request.id,
				WireError::new(Code::JobState, format!("{id} is not waiting or running")),
			),
		};

		Ok(response)
	}

	/// Continue a `partial` job, `docs/dev/architecture/ipc.md` §6: outputs that still carry the
	/// recorded identity are skipped, the rest run again.
	fn resume(&self, request: &Request) -> Result<Response> {
		let Some(written) = request.params.get("id").and_then(|id| id.as_str()) else {
			return Ok(Response::error(
				request.id,
				WireError::new(Code::IpcMethod, "jobs.resume needs an id"),
			));
		};

		let force = request
			.params
			.get("force")
			.and_then(serde_json::Value::as_bool)
			.unwrap_or(false);

		let Some(job) = self.find(written) else {
			return Ok(Response::error(
				request.id,
				WireError::new(Code::JobState, format!("no job matches '{written}'")),
			));
		};

		if job.state != JobState::Partial {
			return Ok(Response::error(
				request.id,
				WireError::new(
					Code::JobState,
					format!("{} is {:?}, and only a partial job resumes", job.id, job.state),
				),
			));
		}

		match crate::daemon::resume::run(&self.shared, &self.machine, &job, force) {
			Ok(state) => Ok(Response::ok(request.id, json!({"id": job.id, "state": state}))?),
			Err(error) => Ok(Response::error(
				request.id,
				WireError::new(error.code(), error.to_string()),
			)),
		}
	}

	/// Run a parked job now, or run what a repair produced, `docs/deferral.md` §8.
	fn resolve(&self, request: &Request) -> Result<Response> {
		let Some(written) = request.params.get("id").and_then(|id| id.as_str()) else {
			return Ok(Response::error(
				request.id,
				WireError::new(Code::IpcMethod, "jobs.resolve needs an id"),
			));
		};

		let mode = request
			.params
			.get("mode")
			.and_then(|mode| mode.as_str())
			.unwrap_or("now");

		// Only `--now` can be asked to overwrite: `--convert` runs a reduced request,
		// and a person who has not seen the output that appeared cannot have meant to
		// replace it.
		let force = request
			.params
			.get("force")
			.and_then(serde_json::Value::as_bool)
			.unwrap_or(false);

		let Some(job) = self.find(written) else {
			return Ok(Response::error(
				request.id,
				WireError::new(Code::JobState, format!("no job matches '{written}'")),
			));
		};

		let parked = match job.state {
			JobState::Pending | JobState::NeedsAttention => job.parked.clone(),
			other => {
				return Ok(Response::error(
					request.id,
					WireError::new(Code::JobState, format!("{} is {other:?}, and not parked", job.id)),
				));
			}
		};

		let intent = match mode {
			// The request as written cannot run, which is why it is parked: asking again
			// is a state error rather than a second failure.
			"now" if job.state == JobState::NeedsAttention => {
				return Ok(Response::error(
					request.id,
					WireError::new(
						Code::JobState,
						format!(
							"{} cannot run as asked; `jobs resolve --convert` runs what can be carried",
							job.id
						),
					),
				));
			}
			"now" => None,
			"convert" => match parked.as_ref().and_then(|parked| parked.corrected.clone()) {
				Some(corrected) => Some(corrected),
				None => {
					return Ok(Response::error(
						request.id,
						WireError::new(
							Code::JobState,
							format!("{} has nothing that can be carried: fix the name instead", job.id),
						),
					));
				}
			},
			other => {
				return Ok(Response::error(
					request.id,
					WireError::new(Code::IpcMethod, format!("'{other}' is not a way to resolve a job")),
				));
			}
		};

		if !crate::daemon::timer::start_job(&self.pool, &job, intent.as_deref(), force) {
			return Ok(Response::error(
				request.id,
				WireError::new(Code::JobState, "the queue is full, so nothing was started"),
			));
		}

		Ok(Response::ok(request.id, json!({"id": job.id, "state": "queued"}))?)
	}

	/// Stop, unless a supervisor would only start us again.
	///
	/// `now` cancels everything instead of letting it drain: a stop that waits for a
	/// twenty minute transcode is not a stop.
	fn shutdown(&mut self, request: &Request) -> Result<Response> {
		let force = request
			.params
			.get("force")
			.and_then(serde_json::Value::as_bool)
			.unwrap_or(false);
		let now = request
			.params
			.get("now")
			.and_then(serde_json::Value::as_bool)
			.unwrap_or(false);

		if !force {
			if let Some(supervisor) = crate::daemon::supervision::supervisor() {
				return Ok(Response::error(
					request.id,
					WireError::new(
						Code::IpcSupervised,
						format!(
							"this daemon was started by {supervisor}, which would start it again; \
							 stop the {supervisor} unit, or pass --force"
						),
					),
				));
			}
		}

		let cancelled = self.stop(now);

		Ok(Response::ok(
			request.id,
			json!({"stopping": true, "cancelled": cancelled}),
		)?)
	}

	/// Stop the way `daemon stop` does, and answer with what was cancelled.
	///
	/// Shared with the signal path, which cannot send itself a request: a `SIGTERM` is a
	/// stop request whether or not a client made it, `docs/dev/architecture/ipc.md` §2.
	pub fn stop(&mut self, now: bool) -> usize {
		let cancelled = match now {
			true => {
				let cancelled = self.cancel_everything();

				// The runners write the cancellation record after the tool is killed, and
				// a process that exits first would leave the job saying `running` for ever.
				if cancelled > 0 {
					self.pool.wait_until_idle(std::time::Duration::from_secs(5));
				}

				cancelled
			}
			false => 0,
		};

		self.stopped = true;
		cancelled
	}

	/// Stop every job and record the ones that never started, which is what a client
	/// would otherwise have to do one at a time, `jobs.cancel` per id.
	fn cancel_everything(&self) -> usize {
		let (running, waiting) = self.pool.stop_all();

		for work in &waiting {
			self.record_cancelled(&work.id, work);
		}

		running.len() + waiting.len()
	}

	/// What followed `since`, or `E_IPC_RESYNC` when the window no longer holds it,
	/// `docs/dev/architecture/ipc.md` §7.
	fn events(&self, request: &Request) -> Result<Response> {
		let since = request
			.params
			.get("since")
			.and_then(serde_json::Value::as_u64)
			.unwrap_or(0);

		let replayed = self.shared.events.lock().ok().and_then(|events| events.since(since));

		match replayed {
			Some((events, newest)) => Ok(Response::ok(request.id, json!({"events": events, "seq": newest}))?),
			None => Ok(Response::error(
				request.id,
				WireError::new(
					Code::IpcResync,
					"this client is older than the events still retained; reload with jobs.list",
				),
			)),
		}
	}

	/// A job by full id or unique prefix.
	fn find(&self, written: &str) -> Option<Job> {
		let wanted = written.to_ascii_lowercase();

		self.shared
			.jobs
			.lock()
			.ok()?
			.iter()
			.rev()
			.find(|job| job.id.as_str().to_ascii_lowercase().starts_with(&wanted))
			.cloned()
	}

	/// A queued job that never started still gets a record, so its id is answerable
	/// afterwards.
	fn record_cancelled(&self, id: &retype_core::job::JobId, work: &crate::daemon::queue::Work) {
		use crate::daemon::queue::Kind;

		let (source, target, consume_source) = match &work.kind {
			Kind::Rename { source, target, .. } => (source.clone(), target.clone(), true),
			Kind::Immediate(request) => (request.input.clone(), request.input.clone(), false),
			Kind::Deferred { source, .. } => (source.clone(), source.clone(), true),
		};

		let job = Job {
			id: id.clone(),
			created: retype_core::job::now_ms(),
			updated: retype_core::job::now_ms(),
			source: Source {
				path: source,
				format: String::new(),
				size: 0,
				mtime_ns: 0,
			},
			intent: None,
			outputs: vec![Output {
				path: target,
				format: String::new(),
				state: OutputState::Failed,
				bytes: None,
				tier: Tier::Converted,
				identity: None,
				stderr: None,
				error: Some(JobError {
					code: Code::JobCancelled.to_string(),
					message: "cancelled before it started".to_string(),
				}),
			}],
			state: JobState::Cancelled,
			backend: None,
			engine: None,
			machine: self.machine.clone(),
			error: None,
			parked: None,
			consume_source,
		};

		crate::daemon::watcher::publish(&self.shared, &job);
	}

	/// The registrations as `daemon status` and `watch.list` report them: what is
	/// registered, whether its path is there, and whether state or policy paused it.
	fn watches(&self) -> Vec<serde_json::Value> {
		let Ok(registrations) = watches::load(&self.shared.state_dir) else {
			return Vec::new();
		};

		registrations
			.into_iter()
			.map(|registration| {
				let available = registration.path.exists();
				let policy = Policy::open(&registration.path).ok();
				let paused = registration.paused || policy.as_ref().is_some_and(|policy| policy.paused);

				json!({
					"path": registration.path.display().to_string(),
					"recursive": registration.recursive,
					"added": registration.added,
					"paused": paused,
					"paused_by": match (registration.paused, policy.as_ref().is_some_and(|policy| policy.paused)) {
						(true, _) => "state",
						(_, true) => "policy",
						_ => "no",
					},
					"available": available,
					"patterns": policy.map(|policy| policy.patterns).unwrap_or_default(),
				})
			})
			.collect()
	}

	fn watch_add(&self, request: &Request) -> Result<Response> {
		let Some(path) = request.params.get("path").and_then(|path| path.as_str()) else {
			return Ok(Response::error(
				request.id,
				WireError::new(Code::IpcMethod, "watch.add needs a path"),
			));
		};

		let recursive = request
			.params
			.get("recursive")
			.and_then(|value| value.as_bool())
			.unwrap_or(true);

		let paused = request.params.get("paused").and_then(|value| value.as_bool());
		let directory = Path::new(path);

		if !directory.is_dir() {
			return Ok(Response::error(
				request.id,
				WireError::new(Code::UsageArgs, format!("{path} is not a directory")),
			));
		}

		let policy = Policy::open(directory)?;
		if policy.paused && paused != Some(true) {
			return Ok(Response::error(
				request.id,
				WireError::new(
					Code::WatchPausedByPolicy,
					format!(
						"{} sets paused = true, so {path} stays unwatched",
						policy.path.display()
					),
				),
			));
		}

		if paused.is_none() && !self.shared.registry().watch().allow_synced {
			if let Some(provider) = retype_core::synced::provider(directory) {
				return Ok(Response::error(
					request.id,
					WireError::new(
						Code::UsageArgs,
						format!(
							"{path} is inside {provider}, which can hold placeholders instead of files; \
							 [watch] allow_synced = true overrides this"
						),
					),
				));
			}
		}

		let registration = match paused {
			Some(paused) => watches::set_paused(&self.shared.state_dir, directory, recursive, paused),
			None => watches::register(&self.shared.state_dir, directory, recursive),
		}?;

		Ok(Response::ok(
			request.id,
			json!({
				"path": registration.path.display().to_string(),
				"recursive": registration.recursive,
				"added": registration.added,
				"paused": registration.paused,
			}),
		)?)
	}

	fn watch_remove(&self, request: &Request) -> Result<Response> {
		let Some(path) = request.params.get("path").and_then(|path| path.as_str()) else {
			return Ok(Response::error(
				request.id,
				WireError::new(Code::IpcMethod, "watch.remove needs a path"),
			));
		};

		let removed = watches::unregister(&self.shared.state_dir, Path::new(path))?;
		Ok(Response::ok(request.id, json!({"path": path, "removed": removed}))?)
	}

	fn config_reload(&self, request: &Request) -> Result<Response> {
		match crate::config::load() {
			Ok(registry) => {
				crate::config::report_warnings(&registry);
				self.shared.reload_registry(registry);
				Ok(Response::ok(request.id, json!({"reloaded": true}))?)
			}
			Err(error) => Ok(Response::error(
				request.id,
				WireError::new(error.code(), error.to_string()),
			)),
		}
	}

	/// Reverse one job from the store, in the daemon rather than in a client, so a
	/// conversion still running cannot have its record pulled out from under it.
	fn undo(&self, request: &Request) -> Result<Response> {
		use retype_core::undo::Store;

		let store = Store::open(&self.shared.state_dir);
		let records = store.list()?;
		let active: std::collections::BTreeSet<String> = self
			.shared
			.jobs
			.lock()
			.map(|jobs| {
				jobs.iter()
					.filter(|job| {
						matches!(
							job.state,
							JobState::Queued | JobState::Running | JobState::Pending | JobState::NeedsAttention
						)
					})
					.map(|job| job.id.as_str().to_string())
					.collect()
			})
			.unwrap_or_default();

		let record = match request
			.params
			.get("last")
			.and_then(|value| value.as_bool())
			.unwrap_or(false)
		{
			true => match records.iter().find(|record| !active.contains(record.id.as_str())) {
				Some(record) => record.clone(),
				None => {
					return Ok(Response::error(
						request.id,
						WireError::new(Code::JobState, "there is nothing to undo"),
					));
				}
			},
			false => {
				let Some(written) = request.params.get("id").and_then(|id| id.as_str()) else {
					return Ok(Response::error(
						request.id,
						WireError::new(Code::UsageArgs, "undo needs an id or --last"),
					));
				};
				let wanted = written.to_ascii_lowercase();
				let matches: Vec<_> = records
					.iter()
					.filter(|record| record.id.as_str().to_ascii_lowercase().starts_with(&wanted))
					.collect();

				match matches.as_slice() {
					[record] => (*record).clone(),
					[] => {
						return Ok(Response::error(
							request.id,
							WireError::new(Code::JobState, format!("no undo record matches '{written}'")),
						));
					}
					_ => {
						let candidates: Vec<_> = matches.iter().map(|record| record.id.as_str()).collect();
						return Ok(Response::error(
							request.id,
							WireError::new(
								Code::IpcAmbiguous,
								format!(
									"'{written}' matches {} records: {}",
									candidates.len(),
									candidates.join(", ")
								),
							),
						));
					}
				}
			}
		};

		if active.contains(record.id.as_str()) {
			return Ok(Response::error(
				request.id,
				WireError::new(
					Code::JobState,
					format!("{} is still waiting or running; stop it before undoing it", record.id),
				),
			));
		}

		let restored = store.restore(&record.id)?;

		Ok(Response::ok(
			request.id,
			json!({
				"id": restored.id,
				"source": restored.source_path.display().to_string(),
				"consume_source": restored.consume_source,
				"removed": restored.outputs.iter().map(|output| output.path.display().to_string()).collect::<Vec<_>>(),
			}),
		)?)
	}

	fn hello(&self) -> HelloResult {
		HelloResult {
			ipc: IPC_VERSION.to_string(),
			pid: std::process::id(),
			machine: self.machine.clone(),
			uptime_ms: self.started.elapsed().as_millis() as u64,
			features: vec!["events".to_string()],
			capabilities: json!({
				"backends": self.shared.registry().backends(),
			}),
		}
	}

	fn answer(&mut self, request: &Request) -> Result<Response> {
		let response = match request.method.as_str() {
			"status" => Response::ok(
				request.id,
				json!({
					"pid": std::process::id(),
					"machine": self.machine,
					"uptime_ms": self.started.elapsed().as_millis() as u64,
					"jobs": self.shared.jobs.lock().map(|jobs| jobs.len()).unwrap_or_default(),
					"parked": self.shared.jobs.lock().map(|jobs| jobs.iter().filter(|job| {
						matches!(job.state, JobState::Pending | JobState::NeedsAttention)
					}).count()).unwrap_or_default(),
					"journal": self.journal_path().display().to_string(),
					"dropped": self.shared.dropped.load(Ordering::Relaxed),
					"skipped": self.shared.skipped.load(Ordering::Relaxed),
					"placeholders": self.shared.placeholders.load(Ordering::Relaxed),
					"primed": self.shared.primed.load(Ordering::Relaxed),
					"watches": self.watches(),
					"endpoint": self.endpoint,
					"supervised": crate::daemon::supervision::supervisor(),
				}),
			)?,
			"jobs.list" => Response::ok(
				request.id,
				json!({
					"jobs": self.shared.jobs.lock().map(|jobs| jobs.iter().map(|job| {
						json!({"id": job.id, "state": job.state, "source": job.source.path, "created": job.created})
					}).collect::<Vec<_>>()).unwrap_or_default(),
				}),
			)?,
			"events" | "events.subscribe" => self.events(request)?,
			"watch.add" => self.watch_add(request)?,
			"watch.remove" => self.watch_remove(request)?,
			"watch.list" => Response::ok(request.id, json!({"watches": self.watches()}))?,
			"undo" => self.undo(request)?,
			"config.reload" => self.config_reload(request)?,
			"why" => self.why(request)?,
			"jobs.get" => self.get(request)?,
			"jobs.submit" => self.submit(request)?,
			"jobs.cancel" => self.cancel(request)?,
			"jobs.resume" => self.resume(request)?,
			"jobs.resolve" => self.resolve(request)?,
			"shutdown" => self.shutdown(request)?,
			other => Response::error(
				request.id,
				WireError::new(Code::IpcMethod, format!("no method '{other}'"))
					.with_detail(json!({"method": other}))?,
			),
		};

		Ok(response)
	}
}

/// Accept connections until the daemon is asked to stop.
#[cfg(unix)]
pub fn serve(listener: std::os::unix::net::UnixListener, server: &mut Server) -> Result<()> {
	use std::io::BufReader;

	// Polled rather than blocked, so a signal can be noticed while nothing is
	// connecting. Fifty milliseconds of latency on a stop is not a stop that waits.
	listener.set_nonblocking(true)?;

	let mut signals = crate::daemon::signals::Seen::now();

	loop {
		let mut stream = match listener.accept() {
			Ok((stream, _)) => stream,
			Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
				let (asked, seen) = signals.asked();
				signals = seen;

				match asked {
					crate::daemon::signals::Asked::Nothing => {}
					crate::daemon::signals::Asked::Drain => {
						server.stop(false);
						eprintln!("retype: stopping after the running jobs finish; a second signal cancels them");
					}
					crate::daemon::signals::Asked::Now => {
						// The waiting is done in `signals::drain`, which is where the
						// escalation is decided; here the queue is emptied and the loop
						// ends.
						server.stop(true);
					}
				}

				if server.stopped() {
					break;
				}

				std::thread::sleep(crate::daemon::signals::POLL);
				continue;
			}
			Err(error) => return Err(error.into()),
		};

		stream.set_nonblocking(false)?;

		// The state directory's modes are the boundary; this is the second lock, for
		// the day a backup or a stray `chmod` moved them, `docs/dev/architecture/ipc.md` §4.
		let peer = crate::daemon::peer::peer_uid(&stream);

		if !crate::daemon::peer::accepts(peer, crate::daemon::peer::our_uid()) {
			eprintln!("retype: refused a connection from uid {}", peer.unwrap_or_default());
			continue;
		}

		let mut reader = BufReader::new(stream.try_clone()?);

		server.serve_connection(&mut reader, &mut stream)?;

		if server.stopped() {
			break;
		}
	}

	Ok(())
}
