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
	/// Exit codes are contract, `docs/cli.md` §6. The category decides, and this
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
