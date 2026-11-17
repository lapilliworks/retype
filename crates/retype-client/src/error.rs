//! Failures the client layer raises before the domain taxonomy takes over.

use std::path::PathBuf;
use std::time::Duration;

use retype_core::error::{Code, Error as CoreError};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error(transparent)]
	Core(#[from] CoreError),

	#[error(transparent)]
	Io(#[from] std::io::Error),

	#[error(transparent)]
	Frame(#[from] retype_ipc::FrameError),

	#[error(transparent)]
	Client(#[from] retype_ipc::ClientError),

	#[error("another daemon owns {}", .0.display())]
	AlreadyRunning(PathBuf),

	#[error("no state directory: set RETYPE_STATE_DIR, or make one of the system directories available")]
	NoStateDirectory,

	#[error(
		"the state directory path is too long for a socket ({length} bytes, limit {limit}): set RETYPE_STATE_DIR to a shorter path"
	)]
	SocketPathTooLong { length: usize, limit: usize },

	#[error("the daemon transport is not implemented on this platform yet")]
	UnsupportedTransport,

	#[error("cannot read an identity from {}", .0.display())]
	BadIdentity(PathBuf),

	#[error("the daemon is not running, and starting it was not requested")]
	DaemonUnavailable,

	#[error("the daemon did not answer within {} ms; its output is in {}", .0.as_millis(), .1.display())]
	DaemonStartFailed(Duration, PathBuf),
}

impl Error {
	pub fn code(&self) -> Code {
		match self {
			Self::Core(error) => error.code(),
			Self::Io(error) => retype_core::error::code_for_io(error),
			Self::Frame(_) => Code::IpcFrame,
			Self::Client(error) => match error {
				retype_ipc::ClientError::Rpc(error) => error.code.clone(),
				retype_ipc::ClientError::Connect(_)
				| retype_ipc::ClientError::Timeout(_)
				| retype_ipc::ClientError::Protocol(_) => Code::IpcUnavailable,
				retype_ipc::ClientError::Frame(_) => Code::IpcFrame,
				retype_ipc::ClientError::LocalPath(error) => retype_core::error::code_for_io(error),
			},
			Self::AlreadyRunning(_) => Code::IoLocked,
			Self::NoStateDirectory => Code::InternalUnexpected,
			Self::SocketPathTooLong { .. } | Self::BadIdentity(_) => Code::IoFailed,
			Self::UnsupportedTransport | Self::DaemonUnavailable | Self::DaemonStartFailed(..) => Code::IpcUnavailable,
		}
	}

	pub fn exit_code(&self) -> u8 {
		self.code().category().exit_code()
	}
}
