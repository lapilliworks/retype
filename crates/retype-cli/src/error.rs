//! Errors the command line and the daemon raise before the domain taxonomy
//! takes over.

use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error(transparent)]
	Core(#[from] retype_core::error::Error),

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
}

impl Error {
	/// Exit codes are contract, `docs/cli.md` §6.
	pub fn exit_code(&self) -> u8 {
		match self {
			Self::AlreadyRunning(_) => 7,
			Self::UnsupportedTransport | Self::Frame(_) => 6,
			Self::Io(_) | Self::Core(_) | Self::SocketPathTooLong { .. } | Self::BadIdentity(_) => 5,
			Self::NoStateDirectory => 1,
		}
	}
}
