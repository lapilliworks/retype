//! Where state lives, `docs/state.md` §1.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Paths below this length are safe for a Unix socket on every platform we
/// support; the kernel limit is around 104 bytes including the terminator.
const SOCKET_PATH_LIMIT: usize = 100;

pub fn state_dir() -> Result<PathBuf> {
	if let Some(configured) = std::env::var_os("RETYPE_STATE_DIR") {
		return Ok(PathBuf::from(configured));
	}

	let base = dirs_state().or_else(dirs_data_local).ok_or(Error::NoStateDirectory)?;
	Ok(base.join("retype"))
}

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
