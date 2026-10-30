//! The request loop, and what the daemon answers.

use std::io::{BufRead, Write};
use std::path::Path;
use std::time::Instant;

use retype_core::error::Code;
use retype_core::job::Job;
use retype_core::journal::Journal;
use retype_ipc::{Hello, HelloResult, IPC_VERSION, Request, Response, WireError, read_frame, same_major};
use serde_json::json;

use crate::daemon::machine;
use crate::error::Result;

pub struct Server {
	endpoint: Option<String>,
	journal: Journal,
	jobs: Vec<Job>,
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

		Ok(Self {
			endpoint: None,
			journal,
			jobs: replay.jobs,
			machine: machine::load_or_create(state_dir)?,
			started: Instant::now(),
			stopped: false,
		})
	}

	pub fn journal_path(&self) -> &Path {
		self.journal.path()
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
		let mut greeted = false;

		loop {
			let frame = match read_frame(reader) {
				Ok(Some(frame)) => frame,
				Ok(None) => return Ok(()),
				Err(error) => {
					let response = Response::error(0, WireError::new(Code::IpcFrame, error.to_string()));
					let _ = retype_ipc::write_frame(writer, &response);
					return Ok(());
				}
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
					"jobs": self.jobs.len(),
					"journal": self.journal_path().display().to_string(),
					"endpoint": self.endpoint,
				}),
			)?,
			"jobs.list" => Response::ok(
				request.id,
				json!({
					"jobs": self.jobs.iter().map(|job| {
						json!({"id": job.id, "state": job.state, "source": job.source.path, "created": job.created})
					}).collect::<Vec<_>>(),
				}),
			)?,
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
