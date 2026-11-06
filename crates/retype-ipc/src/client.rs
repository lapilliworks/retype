//! The client side of the protocol: connect, greet, call.
//!
//! Transport differences stop here. A Unix socket and a Windows named pipe are
//! both opened by name and both behave as a duplex byte stream, so everything
//! above this module is transport independent.

use std::io::BufReader;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use retype_core::error::Code;

use crate::{Hello, HelloResult, IPC_VERSION, Request, Response, WireError};

#[cfg(test)]
mod tests;
mod transport;

/// How long a request waits for its answer, `docs/dev/architecture/ipc.md` §5.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct Client {
	stream: Option<BufReader<transport::Stream>>,
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
	pub fn connect(endpoint: &Path) -> Result<Self, ClientError> {
		let stream = transport::open(endpoint).map_err(ClientError::Connect)?;
		Ok(Self::from_stream(stream))
	}

	fn from_stream(stream: transport::Stream) -> Self {
		Self {
			stream: Some(BufReader::new(stream)),
			next_id: 1,
			daemon: None,
			timed_out: false,
		}
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
		self.request_with_timeout(method, params, REQUEST_TIMEOUT)
	}

	fn request_with_timeout(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value, ClientError> {
		if self.timed_out {
			return Err(ClientError::Timeout(timeout));
		}

		let id = self.next_id;
		self.next_id += 1;
		let request = Request::new(id, method).with_params(params)?;
		let stream = self
			.stream
			.take()
			.ok_or_else(|| ClientError::Protocol("the connection is no longer available".to_string()))?;

		trace(format!("request {method} #{id}"));
		let (stream, frame) = match transport::exchange(stream, request, timeout) {
			Ok(result) => result,
			Err(error) => {
				self.timed_out = matches!(error, ClientError::Timeout(_));
				return Err(error);
			}
		};
		self.stream = Some(stream);
		let frame = frame?
			.ok_or_else(|| ClientError::Protocol(format!("the daemon closed the connection during '{method}'")))?;

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
	/// Whether the daemon's connection slot remained busy after the bounded wait.
	pub fn is_connection_busy(&self) -> bool {
		matches!(self, Self::Connect(error) if transport::is_connection_busy(error))
	}

	/// Whether connection failed or timed out, excluding an explicitly busy slot.
	pub fn is_unavailable(&self) -> bool {
		matches!(self, Self::Connect(error) if !transport::is_connection_busy(error))
			|| matches!(self, Self::Timeout(_))
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
