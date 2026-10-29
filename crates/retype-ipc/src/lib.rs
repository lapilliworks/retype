//! The wire between the daemon and its clients.
//!
//! Contract: `docs/ipc.md`. This crate holds only what both sides have to agree
//! on, the frames, the handshake and the error codes. Transport, locking and
//! process lifecycle belong to the daemon, and the client's copy of those rules
//! belongs to the CLI.
//!
//! Newline-delimited JSON, UTF-8, one object per line, one megabyte per frame.

use std::io::{BufRead, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Protocol identifier, `docs/ipc.md`.
pub const IPC_VERSION: &str = "retype-ipc-1";

/// Largest frame accepted. A bigger one closes the connection rather than being
/// truncated, because a truncated frame is a request the client did not make.
pub const MAX_FRAME: usize = 1024 * 1024;

/// Error codes carried over the wire, `docs/ipc.md` §8 and `docs/errors.md`.
pub mod code {
	pub const VERSION: &str = "E_IPC_VERSION";
	pub const HANDSHAKE: &str = "E_IPC_HANDSHAKE";
	pub const FRAME: &str = "E_IPC_FRAME";
	pub const METHOD: &str = "E_IPC_METHOD";
	pub const UNAVAILABLE: &str = "E_IPC_UNAVAILABLE";
	pub const AMBIGUOUS: &str = "E_IPC_AMBIGUOUS";
	pub const RESYNC: &str = "E_IPC_RESYNC";
	pub const SUPERVISED: &str = "E_IPC_SUPERVISED";
	pub const AUTH: &str = "E_IPC_AUTH";
	pub const INTERNAL: &str = "E_IPC_INTERNAL";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
	pub id: u64,
	pub method: String,
	#[serde(default, skip_serializing_if = "Value::is_null")]
	pub params: Value,
}

impl Request {
	pub fn new(id: u64, method: impl Into<String>) -> Self {
		Self {
			id,
			method: method.into(),
			params: Value::Null,
		}
	}

	pub fn with_params(self, params: impl Serialize) -> Result<Self, FrameError> {
		Ok(Self {
			params: serde_json::to_value(params).map_err(|error| FrameError::Encode(error.to_string()))?,
			..self
		})
	}

	/// Typed view of the parameters, for the daemon side.
	pub fn parse<T: for<'de> Deserialize<'de>>(&self) -> Result<T, FrameError> {
		serde_json::from_value(self.params.clone()).map_err(|error| FrameError::Invalid(error.to_string()))
	}
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
	pub id: u64,
	pub ok: bool,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub result: Option<Value>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub error: Option<WireError>,
}

impl Response {
	pub fn ok(id: u64, result: impl Serialize) -> Result<Self, FrameError> {
		Ok(Self {
			id,
			ok: true,
			result: Some(serde_json::to_value(result).map_err(|error| FrameError::Encode(error.to_string()))?),
			error: None,
		})
	}

	pub fn error(id: u64, error: WireError) -> Self {
		Self {
			id,
			ok: false,
			result: None,
			error: Some(error),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireError {
	pub code: String,
	pub message: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub detail: Option<Value>,
}

impl WireError {
	pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
		Self {
			code: code.into(),
			message: message.into(),
			detail: None,
		}
	}

	pub fn with_detail(self, detail: impl Serialize) -> Result<Self, FrameError> {
		Ok(Self {
			detail: Some(serde_json::to_value(detail).map_err(|error| FrameError::Encode(error.to_string()))?),
			..self
		})
	}
}

/// What a client says before anything else, and what the daemon answers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
	pub ipc: String,
	pub client: String,
	#[serde(default)]
	pub features: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloResult {
	pub ipc: String,
	pub pid: u32,
	pub machine: String,
	pub uptime_ms: u64,
	pub features: Vec<String>,
	pub capabilities: Value,
}

/// Whether two protocol identifiers can talk. Only the major differs in
/// meaning, and only a different major is refused, `docs/ipc.md` §5.
pub fn same_major(left: &str, right: &str) -> bool {
	major(left) == major(right)
}

fn major(version: &str) -> &str {
	match version.rsplit_once('-') {
		Some((head, _)) => head,
		None => version,
	}
}

#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
	/// The peer went away between frames, which is not an error.
	Closed,
	TooLarge(usize),
	Invalid(String),
	Encode(String),
	Io(String),
}

impl std::fmt::Display for FrameError {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Closed => formatter.write_str("the connection closed"),
			Self::TooLarge(size) => write!(formatter, "a frame of {size} bytes exceeds the limit of {MAX_FRAME}"),
			Self::Invalid(reason) => write!(formatter, "malformed frame: {reason}"),
			Self::Encode(reason) => write!(formatter, "cannot encode a frame: {reason}"),
			Self::Io(reason) => write!(formatter, "transport error: {reason}"),
		}
	}
}

impl std::error::Error for FrameError {}

/// Read one frame. `Ok(None)` at a clean end of stream.
///
/// Reads through `BufRead` rather than a raw reader so that two requests written
/// in one packet both survive: the second stays in the buffer for the next call.
pub fn read_frame<R: BufRead>(reader: &mut R) -> Result<Option<Value>, FrameError> {
	let mut line = Vec::new();

	loop {
		let (consumed, complete) = {
			let available = reader.fill_buf().map_err(|error| FrameError::Io(error.to_string()))?;

			if available.is_empty() {
				return if line.is_empty() {
					Ok(None)
				} else {
					Err(FrameError::Invalid("the stream ended inside a frame".to_string()))
				};
			}

			match available.iter().position(|byte| *byte == b'\n') {
				Some(end) => {
					check_size(line.len() + end)?;
					line.extend_from_slice(&available[..end]);
					(end + 1, true)
				}
				None => {
					check_size(line.len() + available.len())?;
					line.extend_from_slice(available);
					(available.len(), false)
				}
			}
		};

		reader.consume(consumed);

		if complete {
			break;
		}
	}

	serde_json::from_slice(&line)
		.map(Some)
		.map_err(|error| FrameError::Invalid(error.to_string()))
}

fn check_size(size: usize) -> Result<(), FrameError> {
	if size > MAX_FRAME {
		return Err(FrameError::TooLarge(size));
	}
	Ok(())
}

/// Write one frame, including its newline, and make it visible to the peer.
pub fn write_frame<W: Write>(writer: &mut W, frame: &impl Serialize) -> Result<(), FrameError> {
	let mut line = serde_json::to_vec(frame).map_err(|error| FrameError::Encode(error.to_string()))?;
	line.push(b'\n');

	writer
		.write_all(&line)
		.map_err(|error| FrameError::Io(error.to_string()))?;
	writer.flush().map_err(|error| FrameError::Io(error.to_string()))
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::io::Cursor;

	fn encode(frame: &impl Serialize) -> Vec<u8> {
		let mut buffer = Vec::new();
		write_frame(&mut buffer, frame).unwrap();
		buffer
	}

	#[test]
	fn a_request_round_trips() {
		let request = Request::new(7, "jobs.get")
			.with_params(serde_json::json!({"id": "01J8Z"}))
			.unwrap();

		let mut cursor = Cursor::new(encode(&request));
		let frame = read_frame(&mut cursor).unwrap().unwrap();
		let parsed: Request = serde_json::from_value(frame).unwrap();

		assert_eq!(parsed, request);
		assert_eq!(parsed.parse::<Value>().unwrap()["id"], "01J8Z");
	}

	#[test]
	fn a_request_without_params_omits_them() {
		let line = String::from_utf8(encode(&Request::new(1, "status"))).unwrap();

		assert_eq!(line.trim_end(), r#"{"id":1,"method":"status"}"#);
	}

	#[test]
	fn a_success_response_has_no_error_field() {
		let response = Response::ok(2, serde_json::json!({"pid": 12})).unwrap();
		let value = serde_json::to_value(&response).unwrap();

		assert_eq!(value["ok"], true);
		assert!(value.get("error").is_none());
		assert_eq!(value["result"]["pid"], 12);
	}

	#[test]
	fn an_error_response_carries_its_code() {
		let response = Response::error(
			3,
			WireError::new(code::METHOD, "no such method")
				.with_detail(serde_json::json!({"method": "nope"}))
				.unwrap(),
		);
		let value = serde_json::to_value(&response).unwrap();

		assert_eq!(value["ok"], false);
		assert_eq!(value["error"]["code"], "E_IPC_METHOD");
		assert_eq!(value["error"]["detail"]["method"], "nope");
		assert!(value.get("result").is_none());
	}

	#[test]
	fn an_empty_stream_is_a_clean_close() {
		let mut cursor = Cursor::new(Vec::new());

		assert_eq!(read_frame(&mut cursor), Ok(None));
	}

	#[test]
	fn a_stream_ending_inside_a_frame_is_malformed() {
		let mut cursor = Cursor::new(br#"{"id":1,"method":"sta"#.to_vec());

		assert!(matches!(read_frame(&mut cursor), Err(FrameError::Invalid(_))));
	}

	#[test]
	fn an_oversized_frame_is_refused() {
		let mut oversized = vec![b' '; MAX_FRAME + 1];
		oversized.push(b'\n');
		let mut cursor = Cursor::new(oversized);

		assert!(matches!(read_frame(&mut cursor), Err(FrameError::TooLarge(_))));
	}

	#[test]
	fn two_frames_in_one_packet_both_survive() {
		let mut buffer = encode(&Request::new(1, "status"));
		buffer.extend(encode(&Request::new(2, "hello")));
		let mut cursor = Cursor::new(buffer);

		let first: Request = serde_json::from_value(read_frame(&mut cursor).unwrap().unwrap()).unwrap();
		let second: Request = serde_json::from_value(read_frame(&mut cursor).unwrap().unwrap()).unwrap();
		let end = read_frame(&mut cursor).unwrap();

		assert_eq!((first.id, first.method.as_str()), (1, "status"));
		assert_eq!((second.id, second.method.as_str()), (2, "hello"));
		assert_eq!(end, None);
	}

	#[test]
	fn a_frame_that_is_not_json_is_refused() {
		let mut cursor = Cursor::new(b"not json\n".to_vec());

		assert!(matches!(read_frame(&mut cursor), Err(FrameError::Invalid(_))));
	}

	#[test]
	fn only_the_major_version_has_to_match() {
		assert!(same_major(IPC_VERSION, "retype-ipc-1"));
		assert!(same_major(IPC_VERSION, "retype-ipc-2"));
		assert!(!same_major(IPC_VERSION, "retype-ipc2"));
		assert!(!same_major("retype-ipc-1", "other-ipc-1"));
	}

	#[test]
	fn the_handshake_names_the_daemon() {
		let result = HelloResult {
			ipc: IPC_VERSION.to_string(),
			pid: 41233,
			machine: "7f3c1a2b".to_string(),
			uptime_ms: 91_000,
			features: vec!["events".to_string(), "notify".to_string()],
			capabilities: serde_json::json!({"backends": ["native", "magick"]}),
		};
		let value = serde_json::to_value(&result).unwrap();

		assert_eq!(value["ipc"], IPC_VERSION);
		assert_eq!(value["pid"], 41233);
		assert_eq!(value["features"][0], "events");
	}
}
