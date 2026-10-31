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
	/// Exit codes are contract, `docs/cli.md` §7. The category decides, and this
	/// layer narrows it where it knows more: a transport failure is a daemon
	/// problem rather than a filesystem one, and a held lock is a state conflict.
	pub fn exit_code(&self) -> u8 {
		match self {
			Self::Core(error) => error.exit_code(),
			Self::Config(_) => retype_core::error::Category::Diagnosis.exit_code(),
			Self::Io(error) => retype_core::error::category_for_io(error).exit_code(),
			Self::Frame(_) | Self::Client(_) | Self::UnsupportedTransport => 6,
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
/// `docs/cli.md` §7. The categories are the ones `docs/errors.md` gives each code,
/// restated here because a job carries its code as a string across the wire and the
/// client is what turns it into a status.
pub fn exit_code(code: &retype_core::error::Code) -> u8 {
	use retype_core::error::Code;

	match code {
		// The environment refused, not the request.
		Code::ToolMissing
		| Code::ToolVersion
		| Code::ToolFailed
		| Code::ToolTimeout
		| Code::JobCancelled
		| Code::JobInterrupted
		| Code::IoLocked => 4,
		// Something to retry: the filesystem or the moment.
		Code::IoFailed
		| Code::IoPermission
		| Code::IoSpace
		| Code::IoNotFound
		| Code::IoExists
		| Code::IoCorruptJournal => 5,
		// A daemon that cannot be reached is its own code, handled before this.
		Code::IpcUnavailable | Code::IpcVersion | Code::IpcHandshake | Code::IpcAuth => 6,
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
