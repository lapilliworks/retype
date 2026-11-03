//! The client side of the protocol: connect, greet, call.
//!
//! Transport differences stop here. A Unix socket and a Windows named pipe are
//! both opened by name and both behave as a duplex byte stream, so everything
//! above this module is transport independent.

use std::io::BufReader;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use retype_core::error::Code;

use crate::{FrameError, Hello, HelloResult, IPC_VERSION, Request, Response, WireError, read_frame, write_frame};

/// How long a request waits for its answer, `docs/dev/architecture/ipc.md` §5.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(unix)]
type Stream = std::os::unix::net::UnixStream;

#[cfg(windows)]
type Stream = std::fs::File;

#[derive(Debug)]
pub struct Client {
	writer: Stream,
	/// A clone kept only so a timed-out or dropped client can unblock its reader.
	cancel: Stream,
	frames: Receiver<Result<Option<Value>, FrameError>>,
	next_id: u64,
	daemon: Option<HelloResult>,
	timed_out: bool,
}

/// A line of wire detail for `-vv`, set once by the CLI. A function pointer
/// rather than a closure keeps the client free of a captured logger and its lock.
static TRACE: OnceLock<fn(&str)> = OnceLock::new();

pub fn trace_with(trace: fn(&str)) {
	let _ = TRACE.set(trace);
}

fn trace(line: String) {
	if let Some(trace) = TRACE.get() {
		trace(&line);
	}
}

impl Client {
	/// Connect to a socket path or a pipe name. No greeting is sent yet.
	///
	/// Reading happens on a small thread so a request can time out on every
	/// transport. A Windows named pipe has no read timeout, and a client that
	/// blocks its caller for ever is worse than one that closes the connection.
	pub fn connect(endpoint: &Path) -> Result<Self, ClientError> {
		let stream = open(endpoint).map_err(ClientError::Connect)?;
		let reader = stream.try_clone().map_err(ClientError::Connect)?;
		let cancel = stream.try_clone().map_err(ClientError::Connect)?;
		let (sender, frames) = mpsc::channel();

		std::thread::spawn(move || {
			let mut reader = BufReader::new(reader);
			loop {
				match read_frame(&mut reader) {
					Ok(Some(frame)) => {
						if sender.send(Ok(Some(frame))).is_err() {
							return;
						}
					}
					Ok(None) => {
						let _ = sender.send(Ok(None));
						return;
					}
					Err(error) => {
						let _ = sender.send(Err(error));
						return;
					}
				}
			}
		});

		Ok(Self {
			writer: stream,
			cancel,
			frames,
			next_id: 1,
			daemon: None,
			timed_out: false,
		})
	}

	/// Greet the daemon and keep what it says about itself.
	pub fn hello(&mut self, client: &str) -> Result<&HelloResult, ClientError> {
		let params = Hello {
			ipc: IPC_VERSION.to_string(),
			client: client.to_string(),
			features: Vec::new(),
		};

		let params = serde_json::to_value(&params).map_err(|error| ClientError::Protocol(error.to_string()))?;
		let result = self.request("hello", params)?;
		let hello: HelloResult =
			serde_json::from_value(result).map_err(|error| ClientError::Protocol(error.to_string()))?;

		self.daemon = Some(hello);
		Ok(self.daemon.as_ref().expect("just stored"))
	}

	pub fn daemon(&self) -> Option<&HelloResult> {
		self.daemon.as_ref()
	}

	/// Call a method and return its result.
	pub fn call(&mut self, method: &str, params: impl Serialize) -> Result<Value, ClientError> {
		let params = serde_json::to_value(params).map_err(|error| ClientError::Protocol(error.to_string()))?;
		self.request(method, params)
	}

	pub fn call_without_params(&mut self, method: &str) -> Result<Value, ClientError> {
		self.request(method, Value::Null)
	}

	fn typed<T: DeserializeOwned>(&mut self, method: &str, params: impl Serialize) -> Result<T, ClientError> {
		let value = self.call(method, params)?;
		serde_json::from_value(value).map_err(|error| ClientError::Protocol(error.to_string()))
	}

	fn typed_without_params<T: DeserializeOwned>(&mut self, method: &str) -> Result<T, ClientError> {
		let value = self.call_without_params(method)?;
		serde_json::from_value(value).map_err(|error| ClientError::Protocol(error.to_string()))
	}

	/// The daemon's own account of itself.
	pub fn status(&mut self) -> Result<crate::Status, ClientError> {
		self.typed_without_params("status")
	}

	pub fn watch_add(&mut self, request: &crate::WatchAdd) -> Result<crate::Watch, ClientError> {
		self.typed("watch.add", request)
	}

	pub fn watch_remove(&mut self, request: &crate::WatchRemove) -> Result<crate::WatchRemoved, ClientError> {
		self.typed("watch.remove", request)
	}

	pub fn watch_list(&mut self) -> Result<crate::WatchList, ClientError> {
		self.typed_without_params("watch.list")
	}

	pub fn jobs_list(&mut self) -> Result<crate::JobList, ClientError> {
		self.typed_without_params("jobs.list")
	}

	pub fn jobs_get(&mut self, id: &str) -> Result<retype_core::job::Job, ClientError> {
		let record: crate::JobRecord = self.typed("jobs.get", crate::JobGet { id: id.to_string() })?;
		Ok(record.job)
	}

	pub fn jobs_submit(&mut self, request: &crate::JobSubmit) -> Result<crate::JobSubmitted, ClientError> {
		self.typed("jobs.submit", request)
	}

	pub fn jobs_cancel(&mut self, id: &str) -> Result<crate::JobCancelled, ClientError> {
		self.typed("jobs.cancel", crate::JobCancel { id: id.to_string() })
	}

	pub fn jobs_resume(&mut self, id: &str, force: bool) -> Result<crate::JobResumed, ClientError> {
		self.typed(
			"jobs.resume",
			crate::JobResume {
				id: id.to_string(),
				force,
			},
		)
	}

	pub fn jobs_resolve(
		&mut self,
		id: &str,
		mode: crate::ResolveMode,
		force: bool,
	) -> Result<crate::JobResolved, ClientError> {
		self.typed(
			"jobs.resolve",
			crate::JobResolve {
				id: id.to_string(),
				mode,
				force,
			},
		)
	}

	pub fn undo(&mut self, request: &crate::UndoRequest) -> Result<crate::Undone, ClientError> {
		self.typed("undo", request)
	}

	pub fn undo_list(&mut self) -> Result<crate::UndoRecords, ClientError> {
		self.typed_without_params("undo.list")
	}

	pub fn why(&mut self, path: &std::path::Path) -> Result<crate::WhyAnswer, ClientError> {
		self.typed(
			"why",
			crate::WhyRequest {
				path: path.to_path_buf(),
			},
		)
	}

	pub fn events(&mut self, since: u64) -> Result<crate::EventBatch, ClientError> {
		self.typed("events", crate::EventsRequest { since })
	}

	pub fn config_reload(&mut self) -> Result<crate::ConfigReload, ClientError> {
		self.typed_without_params("config.reload")
	}

	pub fn shutdown(&mut self, request: &crate::ShutdownRequest) -> Result<crate::ShutdownResult, ClientError> {
		self.typed("shutdown", request)
	}

	fn request(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
		if self.timed_out {
			return Err(ClientError::Timeout(REQUEST_TIMEOUT));
		}

		let id = self.next_id;
		self.next_id += 1;

		let request = Request::new(id, method).with_params(params)?;

		trace(format!("request {method} #{id}"));
		write_frame(&mut self.writer, &request)?;

		let frame = match self.frames.recv_timeout(REQUEST_TIMEOUT) {
			Ok(Ok(Some(frame))) => frame,
			Ok(Ok(None)) => {
				return Err(ClientError::Protocol(format!(
					"the daemon closed the connection during '{method}'"
				)));
			}
			Ok(Err(error)) => return Err(ClientError::Frame(error)),
			Err(RecvTimeoutError::Timeout) => {
				self.timed_out = true;
				abort(&self.cancel);
				return Err(ClientError::Timeout(REQUEST_TIMEOUT));
			}
			Err(RecvTimeoutError::Disconnected) => {
				return Err(ClientError::Protocol("the reader stopped".to_string()));
			}
		};

		let response: Response =
			serde_json::from_value(frame).map_err(|error| ClientError::Protocol(error.to_string()))?;

		if response.id != id {
			return Err(ClientError::Protocol(format!(
				"expected a reply to {id}, received one to {}",
				response.id
			)));
		}

		match (response.ok, response.result, response.error) {
			(true, Some(result), _) => {
				trace(format!("response {method} #{id}: ok"));
				Ok(result)
			}
			(false, _, Some(error)) => {
				trace(format!("response {method} #{id}: {}", error.code));
				Err(ClientError::Rpc(Box::new(error)))
			}
			_ => Err(ClientError::Protocol(
				"a reply that is neither a result nor an error".to_string(),
			)),
		}
	}
}

impl Drop for Client {
	fn drop(&mut self) {
		abort(&self.cancel);
	}
}

#[cfg(unix)]
fn abort(stream: &Stream) {
	use std::net::Shutdown;
	let _ = stream.shutdown(Shutdown::Both);
}

#[cfg(windows)]
fn abort(stream: &Stream) {
	use std::os::windows::io::AsRawHandle;
	use windows_sys::Win32::System::IO::CancelIoEx;

	unsafe {
		CancelIoEx(stream.as_raw_handle() as _, std::ptr::null());
	}
}

#[cfg(unix)]
fn open(endpoint: &Path) -> std::io::Result<Stream> {
	Stream::connect(endpoint)
}

#[cfg(windows)]
fn open(endpoint: &Path) -> std::io::Result<Stream> {
	std::fs::OpenOptions::new().read(true).write(true).open(endpoint)
}

#[derive(Debug)]
pub enum ClientError {
	/// The endpoint could not be reached, which is the answer that makes a
	/// caller consider spawning a daemon.
	Connect(std::io::Error),
	Frame(crate::FrameError),
	/// The daemon answered with a code, `docs/dev/architecture/ipc.md` §8.
	Rpc(Box<WireError>),
	/// The daemon did not answer within [`REQUEST_TIMEOUT`].
	Timeout(Duration),
	Protocol(String),
}

impl std::fmt::Display for ClientError {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Connect(error) => write!(formatter, "cannot reach the daemon: {error}"),
			Self::Frame(error) => write!(formatter, "{error}"),
			Self::Rpc(error) => write!(formatter, "{}: {}", error.code, error.message),
			Self::Timeout(timeout) => write!(formatter, "the daemon did not answer within {} ms", timeout.as_millis()),
			Self::Protocol(reason) => write!(formatter, "unexpected reply: {reason}"),
		}
	}
}

impl std::error::Error for ClientError {}

impl From<crate::FrameError> for ClientError {
	fn from(error: crate::FrameError) -> Self {
		Self::Frame(error)
	}
}

impl ClientError {
	/// Whether the daemon is simply not there, as opposed to broken.
	pub fn is_unavailable(&self) -> bool {
		matches!(self, Self::Connect(_) | Self::Timeout(_))
	}

	pub fn code(&self) -> Option<&Code> {
		match self {
			Self::Rpc(error) => Some(&error.code),
			_ => None,
		}
	}
}

/// The version refusal is worth recognising on its own, because a client that
/// guesses past it is worse than one that stops, `docs/dev/architecture/ipc.md` §9.
pub fn is_version_refusal(error: &ClientError) -> bool {
	error.code() == Some(&Code::IpcVersion)
}
