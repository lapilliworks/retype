//! The request loop, and what the daemon answers.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use retype_core::error::Code;
use retype_core::job::{Job, JobError, JobState, Output, OutputState, Source, Tier};
use retype_core::journal::Journal;
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
		let (journal, replay) = Journal::open(&state_dir.join("jobs.jsonl"))?;

		if let Some(quarantined) = &replay.quarantined {
			eprintln!(
				"retype: the journal was damaged; the previous file is at {}",
				quarantined.display()
			);
		}

		let machine = machine::load_or_create(state_dir)?;
		let shared = Arc::new(Shared {
			registry: crate::config::load()?,
			machine: machine.clone(),
			state_dir: state_dir.to_path_buf(),
			journal: Mutex::new(journal),
			jobs: Mutex::new(replay.jobs),
			recent: Default::default(),
			dropped: AtomicU64::new(0),
		});

		let pool = crate::daemon::queue::Pool::start(Arc::clone(&shared), crate::daemon::queue::runners());

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
	/// `docs/ipc.md` §4.
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

	/// Stop a job that is waiting or running, `docs/ipc.md` §6. A job that has
	/// finished is not cancellable, which is a state error rather than a silent
	/// success.
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

	/// Continue a `partial` job, `docs/ipc.md` §6: outputs that still carry the
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
		let job = Job {
			id: id.clone(),
			created: retype_core::job::now_ms(),
			updated: retype_core::job::now_ms(),
			source: Source {
				path: work.source.clone(),
				format: String::new(),
				size: 0,
				mtime_ns: 0,
			},
			intent: None,
			outputs: vec![Output {
				path: work.target.clone(),
				format: String::new(),
				state: OutputState::Failed,
				bytes: None,
				tier: Tier::Converted,
				identity: None,
				error: Some(JobError {
					code: Code::JobCancelled.to_string(),
					message: "cancelled before it started".to_string(),
				}),
			}],
			state: JobState::Cancelled,
			backend: None,
			machine: self.machine.clone(),
			error: None,
		};

		if let Ok(mut journal) = self.shared.journal.lock() {
			let _ = journal.append(&job);
		}

		if let Ok(mut jobs) = self.shared.jobs.lock() {
			jobs.push(job);
		}
	}

	fn hello(&self) -> HelloResult {
		HelloResult {
			ipc: IPC_VERSION.to_string(),
			pid: std::process::id(),
			machine: self.machine.clone(),
			uptime_ms: self.started.elapsed().as_millis() as u64,
			features: Vec::new(),
			capabilities: json!({}),
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
					"journal": self.journal_path().display().to_string(),
					"dropped": self.shared.dropped.load(Ordering::Relaxed),
					"endpoint": self.endpoint,
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
			"jobs.cancel" => self.cancel(request)?,
			"jobs.resume" => self.resume(request)?,
			"shutdown" => {
				self.stopped = true;
				Response::ok(request.id, json!({"stopping": true}))?
			}
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
pub fn serve(listener: std::os::unix::net::UnixListener, mut server: Server) -> Result<()> {
	use std::io::BufReader;

	for stream in listener.incoming() {
		let mut stream = stream?;
		let mut reader = BufReader::new(stream.try_clone()?);

		server.serve_connection(&mut reader, &mut stream)?;

		if server.stopped() {
			break;
		}
	}

	Ok(())
}
