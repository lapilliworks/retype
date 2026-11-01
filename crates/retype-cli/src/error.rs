//! Errors the command line and the daemon raise before the domain taxonomy
//! takes over.

use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error(transparent)]
	Core(#[from] retype_core::error::Error),

	/// Every diagnostic about a configuration file, printed in order.
	#[error("{0}")]
	Config(retype_core::error::Diagnoses),

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

impl Error {
	/// The code a script sees, `docs/errors.md` §3.
	pub fn code(&self) -> retype_core::error::Code {
		use retype_core::error::Code;

		match self {
			Self::Core(error) => error.code(),
			Self::Config(problems) => problems
				.iter()
				.next()
				.map(retype_core::error::Error::code)
				.unwrap_or(Code::CfgParse),
			Self::Io(error) => retype_core::error::code_for_io(error),
			Self::Frame(_) => Code::IpcFrame,
			Self::Client(error) => match error {
				retype_ipc::ClientError::Rpc(error) => error.code.clone(),
				retype_ipc::ClientError::Connect(_) | retype_ipc::ClientError::Protocol(_) => Code::IpcUnavailable,
				retype_ipc::ClientError::Frame(_) => Code::IpcFrame,
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
			Self::Core(error) => exit_code(&error.code()),
			Self::Config(problems) => problems
				.iter()
				.next()
				.map(|error| exit_code(&error.code()))
				.unwrap_or(3),
			Self::Io(error) => retype_core::error::category_for_io(error).exit_code(),
			Self::Frame(_) => 6,
			// A daemon's own refusal is classified by its code, not by the fact that it
			// arrived over the wire.
			Self::Client(retype_ipc::ClientError::Rpc(error)) => exit_code(&error.code),
			Self::Client(_) | Self::UnsupportedTransport => 6,
			Self::DaemonUnavailable | Self::DaemonStartFailed(..) => 6,
			Self::AlreadyRunning(_) => 7,
			Self::SocketPathTooLong { .. } | Self::BadIdentity(_) => {
				retype_core::error::Category::Transient.exit_code()
			}
			Self::NoStateDirectory => retype_core::error::Category::Internal.exit_code(),
		}
	}
}

/// "Cannot be done here" and "cannot be done" are different answers,
/// `docs/cli.md` §6. The categories are the ones `docs/errors.md` gives each code,
/// restated here because a job carries its code as a string across the wire and the
/// client is what turns it into a status.
pub fn exit_code(code: &retype_core::error::Code) -> u8 {
	use retype_core::error::Code;

	match code {
		// The argument parser or the caller asked for something contradictory.
		Code::UsageArgs | Code::UsageConflict | Code::UsageOutputCount => 2,
		// The environment refused, not the request.
		Code::ToolMissing
		| Code::ToolVersion
		| Code::ToolFailed
		| Code::ToolTimeout => 4,
		// A state conflict: the lock, an ambiguous prefix, or a job that is not in
		// the state the request needs, `docs/cli.md` §6. A cancellation or an
		// interruption is a state the user or the daemon put the job in, not a
		// capability this machine lacks.
		Code::IoLocked
		| Code::IpcAmbiguous
		| Code::JobState
		| Code::JobCancelled
		| Code::JobInterrupted
		| Code::JobResumeUnavailable => 7,
		// Something to retry: the filesystem or the moment.
		Code::IoFailed
		| Code::IoPermission
		| Code::IoSpace
		| Code::IoNotFound
		| Code::IoExists
		| Code::IoCorruptJournal => 5,
		// A daemon that cannot be reached is its own code, handled before this.
		Code::IpcUnavailable | Code::IpcVersion | Code::IpcHandshake | Code::IpcAuth => 6,
		Code::InternalInvariant | Code::InternalPanic | Code::InternalUnexpected => 1,
		_ => 3,
	}
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
