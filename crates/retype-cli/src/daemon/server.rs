//! The request loop, and what the daemon answers.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use retype_core::deferral::Reason;
use retype_core::error::Code;
use retype_core::job::{Job, JobError, JobState, OutputState};
use retype_core::journal::Journal;
use retype_core::policy::Policy;
use retype_core::watches;
use retype_ipc::{Hello, HelloResult, IPC_VERSION, Request, Response, WireError, read_frame, same_major};
use serde_json::json;

use crate::daemon::machine;
use crate::daemon::watcher::Shared;
use crate::error::Result;

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;

pub struct Server {
	endpoint: Option<String>,
	shared: Arc<Shared>,
	pool: Arc<crate::daemon::queue::Pool>,
	machine: String,
	started: Instant,
	stopped: bool,
	supervisor: Option<&'static str>,
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
			placeholders: AtomicU64::new(0),
			primed: AtomicU64::new(0),
			observed: AtomicU64::new(0),
			own_renames: Default::default(),
			parked_admission: Mutex::new(()),
			events: Mutex::new(crate::daemon::events::Events::default()),
			watch_health: Default::default(),
			policies: Default::default(),
			batches: Default::default(),
			notifier: Mutex::new(notifier),
		});

		let pool = crate::daemon::queue::Pool::start(Arc::clone(&shared), crate::daemon::queue::runners());

		// Before anything watches a directory or answers a client: a job that was in
		// flight when the last daemon died is finished or declared, not left hanging,
		// `docs/dev/architecture/state.md` §6.
		crate::daemon::reconcile::run(&shared);

		Ok(Self {
			endpoint: None,
			shared,
			pool,
			machine,
			started: Instant::now(),
			stopped: false,
			// A server opened here was not started by a supervisor. The daemon's
			// entry point says otherwise when the environment says so.
			supervisor: None,
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

	/// Record which supervisor started this daemon. `open` assumes
	/// none: a server built by a test was started by the test, and a supervisor's
	/// environment variables are inherited by every child of the session, not only
	/// by the process the unit runs.
	pub fn set_supervisor(&mut self, supervisor: Option<&'static str>) {
		self.supervisor = supervisor;
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

		match self.resolve_job(written) {
			Ok(job) => match job.to_record() {
				Ok(value) => Ok(Response::ok(request.id, value)?),
				Err(error) => Ok(Response::error(
					request.id,
					WireError::new(Code::IpcFrame, error.to_string()),
				)),
			},
			Err(error) => Ok(Response::error(request.id, error)),
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
		if !Path::new(input).is_absolute() {
			return Ok(Response::error(
				request.id,
				WireError::new(Code::UsageArgs, "jobs.submit input must be an absolute path"),
			));
		}
		let output = request.params.get("output").and_then(|output| output.as_str());
		if output.is_some_and(|path| !Path::new(path).is_absolute()) {
			return Ok(Response::error(
				request.id,
				WireError::new(Code::UsageArgs, "jobs.submit output must be an absolute path"),
			));
		}

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
			output: output.map(str::to_string),
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
			Err(problems) => return Ok(Response::error(request.id, wire_problems(&problems))),
		};

		let id = retype_core::job::JobId::now()?;
		let job = immediate::queued(&self.shared, &prepared, &id);
		if prepared.resolved.meta.ask {
			return self.await_confirmation(request.id, job, &asked.intent);
		}

		let work = crate::daemon::queue::Work {
			id: id.clone(),
			kind: crate::daemon::queue::Kind::Immediate(asked),
		};

		match self
			.pool
			.submit_after(work, || crate::daemon::watcher::publish_checked(&self.shared, &job))
		{
			Ok(true) => Ok(Response::ok(request.id, json!({"id": id, "state": "queued"}))?),
			Ok(false) => Ok(Response::error(
				request.id,
				WireError::new(Code::JobState, "the queue is full or closed, so nothing was submitted"),
			)),
			Err(error) => Ok(Response::error(request.id, admission_error(&id, error))),
		}
	}

	fn await_confirmation(&self, request_id: u64, mut job: Job, requested: &str) -> Result<Response> {
		let fingerprint = match retype_core::safety::Fingerprint::of(&job.source.path) {
			Ok(fingerprint) => fingerprint,
			Err(error) => {
				return Ok(Response::error(
					request_id,
					WireError::new(
						error.code(),
						format!(
							"cannot verify {} before confirmation: {error}; no work was submitted",
							job.source.path.display()
						),
					),
				));
			}
		};
		job.set_state(JobState::NeedsAttention);
		job.parked = Some(retype_core::job::Parked {
			reason: Reason::Ask.as_str().to_string(),
			deadline: None,
			batch: None,
			estimate: None,
			requested: Some(requested.to_string()),
			corrected: None,
			dropped: Vec::new(),
			consume: false,
			force: false,
			input_fingerprint: Some(fingerprint),
		});

		match crate::daemon::watcher::publish_checked(&self.shared, &job) {
			Ok(()) => Ok(Response::ok(
				request_id,
				json!({"id": job.id, "state": "needs-attention"}),
			)?),
			Err(error) => Ok(Response::error(request_id, admission_error(&job.id, error))),
		}
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
				WireError::new(Code::UsageArgs, format!("'{written}' is not a job id")),
			));
		};

		if let Some(partial) = self.find(id.as_str()).filter(|job| job.state == JobState::Partial) {
			return self.cancel_partial(request.id, partial);
		}
		let _guard = match self.shared.lock_parked_admission() {
			Ok(guard) => guard,
			Err(error) => {
				return Ok(Response::error(
					request.id,
					WireError::new(error.code(), error.to_string()),
				));
			}
		};

		// A parked job never reached the queue, so it is cancelled in the record rather
		// than by stopping anything, `docs/deferral.md` §8.
		if let Some(parked) = self
			.find(id.as_str())
			.filter(|job| matches!(job.state, JobState::Pending | JobState::NeedsAttention))
		{
			let mut cancelled = parked;
			cancel_unfinished_outputs(&mut cancelled);
			if let Err(error) = crate::daemon::watcher::publish_checked(&self.shared, &cancelled) {
				return Ok(Response::error(
					request.id,
					WireError::new(
						error.code(),
						format!(
							"cancellation of {id} could not be durably recorded: {error}; \
							 the job may still run, so check it before retrying"
						),
					),
				));
			}

			return Ok(Response::ok(
				request.id,
				json!({"id": id, "cancelled": true, "state": "cancelled"}),
			)?);
		}

		let (found, queued) = self.pool.cancel(&id);

		let response = match found {
			crate::daemon::queue::Cancelled::Queued => {
				let result = match queued {
					Some(_) => self.record_cancelled(&id),
					None => Err(retype_core::error::Error::Io(std::io::Error::other(
						"the removed queue entry was unavailable",
					))),
				};
				match result {
					Ok(()) => Response::ok(request.id, json!({"id": id, "cancelled": true, "state": "cancelled"}))?,
					Err(error) => Response::error(
						request.id,
						WireError::new(
							error.code(),
							format!(
								"job {id} was removed from the queue, but cancellation could not be durably recorded: \
								 {error}; check the job after restart"
							),
						),
					),
				}
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

	fn cancel_partial(&self, request_id: u64, mut job: Job) -> Result<Response> {
		let store = retype_core::undo::Store::open(&self.shared.state_dir);
		let lease = match store.lease(&job.id) {
			Ok(lease) => lease,
			Err(error) => {
				return Ok(Response::error(
					request_id,
					WireError::new(error.code(), format!("job {} was not cancelled: {error}", job.id)),
				));
			}
		};

		cancel_unfinished_outputs(&mut job);
		if let Err(error) = crate::daemon::watcher::persist(&self.shared, &job) {
			return Ok(Response::error(
				request_id,
				WireError::new(
					error.code(),
					format!(
						"cancellation of {} could not be durably recorded: {error}; check the job before retrying",
						job.id
					),
				),
			));
		}

		let warning = super::cancel::finish_after_persist(&self.shared, &store, &mut job);
		drop(lease);
		crate::daemon::watcher::publish_persisted(&self.shared, &job);
		Ok(Response::ok(
			request_id,
			json!({"id": job.id, "cancelled": true, "state": "cancelled", "warning": warning}),
		)?)
	}

	/// Continue a `partial` job, `docs/dev/architecture/state.md` §6: outputs that still carry the
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

		let job = match self.resolve_job(written) {
			Ok(job) => job,
			Err(error) => return Ok(Response::error(request.id, error)),
		};

		let is_interrupted = job.state == JobState::Failed
			&& job
				.error
				.as_ref()
				.is_some_and(|error| error.code == Code::JobInterrupted.to_string());
		if job.state != JobState::Partial && !is_interrupted {
			return Ok(Response::error(
				request.id,
				WireError::new(
					Code::JobState,
					format!(
						"{} is {:?}, and only partial or interrupted jobs resume",
						job.id, job.state
					),
				),
			));
		}

		match crate::daemon::resume::run(&self.shared, &self.machine, &job, force) {
			Ok(state) => match self.resolve_job(job.id.as_str()) {
				Ok(current) => Ok(Response::ok(
					request.id,
					json!({"id": job.id, "state": state, "source_disposition": current.source_disposition}),
				)?),
				Err(error) => Ok(Response::error(request.id, error)),
			},
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

		let job = match self.resolve_job(written) {
			Ok(job) => job,
			Err(error) => return Ok(Response::error(request.id, error)),
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

		let reason = parked.as_ref().and_then(|parked| Reason::parse(&parked.reason));

		let intent = match mode {
			// An explicit ask or size park can run as asked once a person confirms it.
			// Other needs-attention reasons require correction rather than consent.
			"now" if job.state == JobState::NeedsAttention && !reason.is_some_and(Reason::can_run_as_asked) => {
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

		use crate::daemon::timer::Admission;
		match crate::daemon::timer::start_confirmed(&self.shared, &self.pool, &job, intent.as_deref(), force) {
			Ok(Admission::Queued) => Ok(Response::ok(request.id, json!({"id": job.id, "state": "queued"}))?),
			Ok(Admission::Full) => Ok(Response::error(
				request.id,
				WireError::new(Code::JobState, "the queue is full, so nothing was started"),
			)),
			Ok(Admission::Stale) => Ok(Response::error(
				request.id,
				WireError::new(
					Code::JobState,
					"the job changed while it was being resolved; check its current state",
				),
			)),
			Err(error) if error.code() == Code::JobResumeUnavailable => Ok(Response::error(
				request.id,
				WireError::new(error.code(), format!("job {} did not start: {error}", job.id)),
			)),
			Err(error) => Ok(Response::error(request.id, admission_error(&job.id, error))),
		}
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
			if let Some(supervisor) = self.supervisor {
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
		self.pool.close_admissions();
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
			if let Err(error) = self.record_cancelled(&work.id) {
				eprintln!(
					"retype: cancellation of queued job {} was not recorded: {error}",
					work.id
				);
			}
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
	fn resolve_job(&self, written: &str) -> std::result::Result<Job, WireError> {
		if !retype_core::job::JobId::valid_prefix(written) {
			return Err(WireError::new(
				Code::UsageArgs,
				format!(
					"'{written}' is not a job handle; use at least {} characters",
					retype_core::job::JobId::PREFIX_MIN
				),
			));
		}

		let Ok(jobs) = self.shared.jobs.lock() else {
			return Err(WireError::new(Code::IpcInternal, "the job list is poisoned"));
		};

		let mut matches: Vec<Job> = jobs.iter().filter(|job| job.id.starts_with(written)).cloned().collect();

		match matches.len() {
			1 => Ok(matches.pop().expect("one match")),
			0 => Err(WireError::new(Code::JobNotFound, format!("no job matches '{written}'"))),
			_ => {
				let candidates: Vec<&str> = matches.iter().map(|job| job.id.as_str()).collect();
				Err(WireError::new(
					Code::JobAmbiguous,
					format!(
						"'{written}' matches {} jobs: {}",
						candidates.len(),
						candidates.join(", ")
					),
				))
			}
		}
	}

	/// A job by full id, for the paths that must have one already.
	fn find(&self, written: &str) -> Option<Job> {
		self.shared
			.jobs
			.lock()
			.ok()?
			.iter()
			.rev()
			.find(|job| job.id.as_str() == written)
			.cloned()
	}

	/// Preserve the admitted request when a queued job is cancelled before starting.
	fn record_cancelled(&self, id: &retype_core::job::JobId) -> retype_core::error::Result<()> {
		let mut job = self.find(id.as_str()).ok_or_else(|| {
			retype_core::error::Error::Io(std::io::Error::other("the queued job has no admission record"))
		})?;
		cancel_unfinished_outputs(&mut job);
		crate::daemon::watcher::publish_checked(&self.shared, &job)
	}

	/// Registrations and the separately observed state of each installed watcher.
	fn watches(&self) -> (Vec<serde_json::Value>, Option<String>) {
		let registrations = match watches::load(&self.shared.state_dir) {
			Ok(registrations) => registrations,
			Err(error) => return (Vec::new(), Some(format!("cannot read watch registrations: {error}"))),
		};

		let watches = registrations
			.into_iter()
			.map(|registration| {
				let available = registration.path.is_dir();
				let policy = Policy::open(&registration.path);
				let paused_by_policy = policy.as_ref().is_ok_and(|policy| policy.paused);
				let paused = registration.paused || paused_by_policy;
				let (status, problem) = self.watch_status(&registration, available, paused, policy.as_ref().err());

				json!({
					"path": registration.path.display().to_string(),
					"recursive": registration.recursive,
					"added": registration.added,
					"paused": paused,
					"paused_by": match (registration.paused, paused_by_policy) {
						(true, _) => Some("state"),
						(_, true) => Some("policy"),
						_ => None,
					},
					"available": available,
					"patterns": policy.map(|policy| policy.patterns).unwrap_or_default(),
					"status": status,
					"problem": problem,
				})
			})
			.collect();
		(watches, self.shared.watch_health.registry_error())
	}

	fn watch_status(
		&self,
		registration: &watches::Registration,
		available: bool,
		paused: bool,
		policy_error: Option<&retype_core::error::Error>,
	) -> (&'static str, Option<String>) {
		use super::watch_health::Phase;

		let phase = self
			.shared
			.watch_health
			.phase(&registration.path, registration.recursive);
		if paused {
			return match phase {
				Some(Phase::Starting | Phase::Ready | Phase::Stopping | Phase::Degraded(_)) => ("stopping", None),
				None | Some(Phase::Failed(_)) => ("paused", None),
			};
		}
		if !available {
			return (
				"unavailable",
				Some("directory is missing or is not a directory".to_string()),
			);
		}
		if let Some(error) = policy_error {
			return ("error", Some(format!("cannot read directory policy: {error}")));
		}
		if let Some(error) = self.shared.watch_health.registry_error() {
			return ("error", Some(error));
		}
		match phase {
			None => ("pending", None),
			Some(Phase::Starting) => ("starting", None),
			Some(Phase::Ready) => ("ready", None),
			Some(Phase::Stopping) => ("stopping", None),
			Some(Phase::Degraded(message)) => ("degraded", Some(message)),
			Some(Phase::Failed(message)) => ("error", Some(message)),
		}
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
		if !directory.is_absolute() {
			return Ok(Response::error(
				request.id,
				WireError::new(Code::UsageArgs, "watch.add path must be absolute"),
			));
		}

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

		if !Path::new(path).is_absolute() {
			return Ok(Response::error(
				request.id,
				WireError::new(Code::UsageArgs, "watch.remove path must be absolute"),
			));
		}
		let removed = watches::unregister(&self.shared.state_dir, Path::new(path))?;
		Ok(Response::ok(request.id, json!({"path": path, "removed": removed}))?)
	}

	fn config_reload(&self, request: &Request) -> Result<Response> {
		match crate::config::load() {
			Ok(registry) => {
				let warnings = registry.warnings().to_vec();
				crate::config::report_warnings(&registry);
				self.shared.reload_registry(registry);
				Ok(Response::ok(
					request.id,
					json!({"reloaded": true, "warnings": warnings}),
				)?)
			}
			Err(error) => Ok(Response::error(
				request.id,
				WireError::new(error.code(), error.to_string()),
			)),
		}
	}

	/// What the undo store holds, for a client that needs a list rather than one
	/// record, `docs/dev/architecture/state.md` §5.
	fn undo_list(&self, request: &Request) -> Result<Response> {
		use retype_core::undo::Store;

		let records = Store::open(&self.shared.state_dir).list()?;
		let summaries: Vec<retype_ipc::UndoSummary> = records
			.into_iter()
			.map(|record| retype_ipc::UndoSummary {
				id: record.id,
				intent: record.intent,
				created: record.created,
				source_path: record.source_path,
				source_ext: record.source_ext,
				source_size: record.source_size,
				outputs: record.outputs,
				pinned: record.pinned,
				consume_source: record.consume_source,
			})
			.collect();

		Ok(Response::ok(request.id, json!({"records": summaries}))?)
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
				if !retype_core::job::JobId::valid_prefix(written) {
					return Ok(Response::error(
						request.id,
						WireError::new(
							Code::UsageArgs,
							format!(
								"'{written}' is not a job handle; use at least {} characters",
								retype_core::job::JobId::PREFIX_MIN
							),
						),
					));
				}

				let matches: Vec<_> = records.iter().filter(|record| record.id.starts_with(written)).collect();

				match matches.as_slice() {
					[record] => (*record).clone(),
					[] => {
						return Ok(Response::error(
							request.id,
							WireError::new(Code::JobNotFound, format!("no undo record matches '{written}'")),
						));
					}
					_ => {
						let candidates: Vec<_> = matches.iter().map(|record| record.id.as_str()).collect();
						return Ok(Response::error(
							request.id,
							WireError::new(
								Code::JobAmbiguous,
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
			version: env!("CARGO_PKG_VERSION").to_string(),
			pid: std::process::id(),
			machine: self.machine.clone(),
			uptime_ms: self.started.elapsed().as_millis() as u64,
			features: vec![
				"events".to_string(),
				"jobs.list.summary".to_string(),
				"undo.list".to_string(),
				"config.warnings".to_string(),
			],
			capabilities: json!({
				"backends": self.shared.registry().backends(),
			}),
		}
	}

	fn answer(&mut self, request: &Request) -> Result<Response> {
		let response = match request.method.as_str() {
			"status" => {
				let (watches, watch_error) = self.watches();
				Response::ok(
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
					"placeholders": self.shared.placeholders.load(Ordering::Relaxed),
					"primed": self.shared.primed.load(Ordering::Relaxed),
					"observed": self.shared.observed.load(Ordering::Relaxed),
					"watches": watches,
					"watch_error": watch_error,
					"endpoint": self.endpoint,
					"supervised": self.supervisor,
					}),
				)?
			}
			"jobs.list" => Response::ok(
				request.id,
				json!({
					"jobs": self.shared.jobs.lock().map(|jobs| jobs.iter().map(|job| {
						json!({
							"id": job.id,
							"state": job.state,
							"source": job.source.path,
							"created": job.created,
							"updated": job.updated,
							"intent": job.intent,
							"parked_reason": job.parked.as_ref().map(|parked| parked.reason.clone()),
							"error": job.error,
							"outputs_done": job.outputs.iter().filter(|output| output.state == OutputState::Done).count(),
							"outputs_total": job.outputs.len(),
						})
					}).collect::<Vec<_>>()).unwrap_or_default(),
				}),
			)?,
			"events" | "events.subscribe" => self.events(request)?,
			"watch.add" => self.watch_add(request)?,
			"watch.remove" => self.watch_remove(request)?,
			"watch.list" => {
				let (watches, error) = self.watches();
				Response::ok(request.id, json!({"watches": watches, "error": error}))?
			}
			"undo" | "undo.list" => {
				let result = if request.method == "undo" {
					self.undo(request)
				} else {
					self.undo_list(request)
				};
				match result {
					Ok(response) => response,
					Err(error) => Response::error(request.id, WireError::new(error.code(), error.to_string())),
				}
			}
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

fn cancel_unfinished_outputs(job: &mut Job) {
	job.set_state(JobState::Cancelled);
	for output in &mut job.outputs {
		if matches!(output.state, OutputState::Pending | OutputState::Running) {
			output.state = OutputState::Skipped;
			output.error = Some(JobError::new(
				Code::JobCancelled.to_string(),
				"cancelled before this output started",
			));
		}
	}
}

fn admission_error(id: &retype_core::job::JobId, error: retype_core::error::Error) -> WireError {
	WireError::new(
		error.code(),
		format!(
			"job {id} did not start because admission could not be recorded: {error}; \
			 its outcome after restart is uncertain, so check the job before retrying"
		),
	)
}

/// Turn a set of diagnostics into one wire error that keeps its first span and
/// hint. The message still carries every problem, because a client may print it.
fn wire_problems(problems: &retype_core::error::Diagnoses) -> WireError {
	let first = problems.iter().next();
	let code = first.map(retype_core::error::Error::code).unwrap_or(Code::ToolFailed);
	let mut error = WireError::new(code, problems.to_string());

	if let Some(span) = first.and_then(retype_core::error::Error::span) {
		error = error.with_span(span);
	}

	if let Some(hint) = first.and_then(retype_core::error::Error::hint) {
		error = error.with_hint(hint);
	}

	error
}

/// A client can disappear while a reply is being written; that is local to the
/// connection, not a reason to stop the daemon or its watchers.
pub(crate) fn connection_result(result: Result<()>) -> Result<()> {
	match result {
		Err(crate::error::Error::Frame(retype_ipc::FrameError::Io(message))) => {
			eprintln!("retype: client connection ended while sending a response: {message}");
			Ok(())
		}
		other => other,
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

		connection_result(server.serve_connection(&mut reader, &mut stream))?;

		if server.stopped() {
			break;
		}
	}

	Ok(())
}
