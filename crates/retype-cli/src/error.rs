//! Errors the command line and the daemon raise before the domain taxonomy
//! takes over.

use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error(transparent)]
	Core(#[from] retype_core::error::Error),

	/// A configuration that could not be located, read, validated or written.
	#[error(transparent)]
	Config(#[from] retype_config::Error),

	#[error(transparent)]
	Io(#[from] std::io::Error),

	#[error(transparent)]
	Frame(#[from] retype_ipc::FrameError),

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
	DaemonStartFailed(std::time::Duration, PathBuf),

	#[error(transparent)]
	Client(#[from] retype_ipc::ClientError),
}

impl From<serde_json::Error> for Error {
	fn from(error: serde_json::Error) -> Self {
		Self::Core(retype_core::error::Error::Json(error.to_string()))
	}
}

impl From<retype_client::Error> for Error {
	fn from(error: retype_client::Error) -> Self {
		match error {
			retype_client::Error::Core(error) => Self::Core(error),
			retype_client::Error::Io(error) => Self::Io(error),
			retype_client::Error::Frame(error) => Self::Frame(error),
			retype_client::Error::Client(error) => Self::Client(error),
			retype_client::Error::AlreadyRunning(path) => Self::AlreadyRunning(path),
			retype_client::Error::NoStateDirectory => Self::NoStateDirectory,
			retype_client::Error::SocketPathTooLong { length, limit } => Self::SocketPathTooLong { length, limit },
			retype_client::Error::UnsupportedTransport => Self::UnsupportedTransport,
			retype_client::Error::BadIdentity(path) => Self::BadIdentity(path),
			retype_client::Error::DaemonUnavailable => Self::DaemonUnavailable,
			retype_client::Error::DaemonStartFailed(timeout, log) => Self::DaemonStartFailed(timeout, log),
		}
	}
}

impl Error {
	/// The code a script sees, `docs/reference/errors.md` §3.
	pub fn code(&self) -> retype_core::error::Code {
		use retype_core::error::Code;

		match self {
			Self::Core(error) => error.code(),
			Self::Config(error) => error.code(),
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
			Self::SocketPathTooLong { .. } => Code::IoFailed,
			Self::UnsupportedTransport => Code::IpcUnavailable,
			Self::BadIdentity(_) => Code::IoFailed,
			Self::DaemonUnavailable | Self::DaemonStartFailed(..) => Code::IpcUnavailable,
		}
	}

	/// Exit codes are contract, `docs/cli.md` §6. The category decides, and this
	/// layer narrows it where it knows more: a transport failure is a daemon
	/// problem rather than a filesystem one, and a held lock is a state conflict.
	pub fn exit_code(&self) -> u8 {
		match self {
			Self::Core(error) => error.exit_code(),
			Self::Config(error) => error.exit_code(),
			Self::Io(error) => retype_core::error::category_for_io(error).exit_code(),
			Self::Client(retype_ipc::ClientError::Rpc(error)) => exit_code(&error.code),
			Self::Frame(_)
			| Self::Client(_)
			| Self::UnsupportedTransport
			| Self::DaemonUnavailable
			| Self::DaemonStartFailed(..) => 6,
			Self::AlreadyRunning(_) => 7,
			Self::SocketPathTooLong { .. } | Self::BadIdentity(_) => 5,
			Self::NoStateDirectory => 1,
		}
	}
}

/// "Cannot be done here" and "cannot be done" are different answers,
/// `docs/reference/cli.md` §6. The category comes from the code and nowhere else.
pub fn exit_code(code: &retype_core::error::Code) -> u8 {
	code.category().exit_code()
}

/// The same mapping, for a code that arrived as the string a job records.
pub fn exit_code_of(raw: &str) -> u8 {
	match retype_core::error::Code::parse(raw) {
		Some(code) => exit_code(&code),
		None => 4,
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use retype_core::error::Code;

	#[test]
	fn job_lifecycle_failures_are_state_conflicts() {
		for code in [
			Code::JobCancelled,
			Code::JobInterrupted,
			Code::JobResumeUnavailable,
			Code::JobState,
		] {
			assert_eq!(exit_code(&code), 7, "{code}");
		}
	}

	#[test]
	fn a_missing_tool_is_an_environment_failure() {
		assert_eq!(exit_code(&Code::ToolMissing), 4);
	}
}
