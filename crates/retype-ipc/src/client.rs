//! The client side of the protocol: connect, greet, call.
//!
//! Transport differences stop here. A Unix socket and a Windows named pipe are
//! both opened by name and both behave as a duplex byte stream, so everything
//! above this module is transport independent.

use std::io::BufReader;
use std::path::Path;
use std::sync::OnceLock;

use serde::Serialize;
use serde_json::Value;

use retype_core::error::Code;

use crate::{Hello, HelloResult, IPC_VERSION, Request, Response, WireError, read_frame, write_frame};

#[cfg(unix)]
type Stream = std::os::unix::net::UnixStream;

#[cfg(windows)]
type Stream = std::fs::File;

#[derive(Debug)]
pub struct Client {
	reader: BufReader<Stream>,
	writer: Stream,
	next_id: u64,
	daemon: Option<HelloResult>,
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
		let stream = open(endpoint).map_err(ClientError::Connect)?;
		let reader = BufReader::new(stream.try_clone().map_err(ClientError::Connect)?);

		Ok(Self {
			reader,
			writer: stream,
			next_id: 1,
			daemon: None,
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

	fn request(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
		let id = self.next_id;
		self.next_id += 1;

		let request = Request::new(id, method).with_params(params)?;

		trace(format!("request {method} #{id}"));
		write_frame(&mut self.writer, &request)?;

		let frame = read_frame(&mut self.reader)?
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
				Err(ClientError::Rpc(error))
			}
			_ => Err(ClientError::Protocol(
				"a reply that is neither a result nor an error".to_string(),
			)),
		}
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
	Rpc(WireError),
	Protocol(String),
}

impl std::fmt::Display for ClientError {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Connect(error) => write!(formatter, "cannot reach the daemon: {error}"),
			Self::Frame(error) => write!(formatter, "{error}"),
			Self::Rpc(error) => write!(formatter, "{}: {}", error.code, error.message),
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
		matches!(self, Self::Connect(_))
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
