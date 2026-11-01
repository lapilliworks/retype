//! Where state lives, `docs/dev/architecture/state.md` §1.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Paths below this length are safe for a Unix socket on every platform we
/// support; the kernel limit is around 104 bytes including the terminator.
#[cfg(unix)]
const SOCKET_PATH_LIMIT: usize = 100;

/// Where the daemon says it listens, so a client never has to derive it.
pub const ENDPOINT_FILE: &str = "endpoint";

/// An explicit `--state-dir` wins over the environment, which wins over the
/// default, `docs/config.md` §2.
pub fn resolve(explicit: Option<PathBuf>) -> Result<PathBuf> {
	match explicit {
		Some(path) => Ok(path),
		None => state_dir(),
	}
}

pub fn publish_endpoint(state_dir: &Path, endpoint: &str) -> Result<()> {
	let path = state_dir.join(ENDPOINT_FILE);
	fs::write(&path, format!("{endpoint}\n"))?;
	set_private(&path)?;
	Ok(())
}

pub fn read_endpoint(state_dir: &Path) -> Result<String> {
	let path = state_dir.join(ENDPOINT_FILE);

	let endpoint = match fs::read_to_string(&path) {
		Ok(endpoint) => endpoint,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
			return Err(Error::DaemonUnavailable);
		}
		Err(error) => return Err(Error::Io(error)),
	};

	Ok(endpoint.trim().to_string())
}

pub fn clear_endpoint(state_dir: &Path) {
	let _ = fs::remove_file(state_dir.join(ENDPOINT_FILE));
}

pub fn state_dir() -> Result<PathBuf> {
	if let Some(configured) = std::env::var_os("RETYPE_STATE_DIR") {
		return Ok(PathBuf::from(configured));
	}

	let base = dirs_state().or_else(dirs_data_local).ok_or(Error::NoStateDirectory)?;
	Ok(base.join("retype"))
}

/// Where a job's full tool output goes. The journal keeps a tail;
/// this keeps everything, because a converter can print megabytes while a failure
/// needs the last ten lines.
pub fn log_path(state_dir: &Path, id: &retype_core::job::JobId) -> PathBuf {
	state_dir.join("logs").join(format!("{id}.log"))
}

#[cfg(unix)]
pub fn socket_path(state_dir: &Path) -> Result<PathBuf> {
	let path = state_dir.join("daemon.sock");
	let length = path.as_os_str().len();

	if length > SOCKET_PATH_LIMIT {
		return Err(Error::SocketPathTooLong {
			length,
			limit: SOCKET_PATH_LIMIT,
		});
	}

	Ok(path)
}

pub fn ensure_private_directory(path: &Path) -> Result<()> {
	fs::create_dir_all(path)?;
	set_private(path)?;
	Ok(())
}

#[cfg(unix)]
fn set_private(path: &Path) -> Result<()> {
	use std::os::unix::fs::PermissionsExt;
	fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
	Ok(())
}

#[cfg(not(unix))]
fn set_private(_path: &Path) -> Result<()> {
	Ok(())
}

#[cfg(unix)]
fn dirs_state() -> Option<PathBuf> {
	std::env::var_os("XDG_STATE_HOME")
		.map(PathBuf::from)
		.filter(|path| path.is_absolute())
		.or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
}

#[cfg(windows)]
fn dirs_state() -> Option<PathBuf> {
	std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join("retype").join("state"))
}

#[cfg(not(any(unix, windows)))]
fn dirs_state() -> Option<PathBuf> {
	None
}

fn dirs_data_local() -> Option<PathBuf> {
	std::env::var_os("XDG_DATA_HOME")
		.map(PathBuf::from)
		.filter(|path| path.is_absolute())
		.or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_configured_directory_wins() {
		// Serialised by construction: this is the only test that touches the
		// variable, and it restores it before returning.
		unsafe { std::env::set_var("RETYPE_STATE_DIR", "/tmp/retype-test-state") };
		let configured = state_dir().unwrap();
		unsafe { std::env::remove_var("RETYPE_STATE_DIR") };

		assert_eq!(configured, PathBuf::from("/tmp/retype-test-state"));
	}

	#[cfg(unix)]
	#[test]
	fn an_over_long_socket_path_is_refused_by_name() {
		let long = PathBuf::from(format!("/tmp/{}", "a".repeat(SOCKET_PATH_LIMIT)));

		match socket_path(&long) {
			Err(Error::SocketPathTooLong { length, limit }) => {
				assert!(length > limit);
			}
			other => panic!("expected a refusal, got {other:?}"),
		}
	}
}
