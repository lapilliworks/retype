//! One daemon per state directory, enforced by the file system rather than by
//! convention, `docs/dev/architecture/ipc.md` §1.
//!
//! The lock is advisory and released by the kernel when the process dies, so a
//! kill leaves nothing to clean up.

use std::fs;
use std::io::Write;
use std::path::Path;

use crate::error::{Error, Result};

pub struct Lock {
	_file: fs::File,
}

impl Lock {
	pub fn acquire(path: &Path, state_dir: &Path) -> Result<Self> {
		let mut file = fs::OpenOptions::new()
			.create(true)
			.write(true)
			.truncate(false)
			.open(path)?;

		if !try_lock(&file)? {
			return Err(Error::AlreadyRunning(state_dir.to_path_buf()));
		}

		file.set_len(0)?;
		writeln!(file, "{}", std::process::id())?;
		file.flush()?;
		set_private(&file)?;

		Ok(Self { _file: file })
	}
}

#[cfg(unix)]
fn set_private(file: &fs::File) -> Result<()> {
	use std::os::unix::fs::PermissionsExt;
	file.set_permissions(fs::Permissions::from_mode(0o600))?;
	Ok(())
}

#[cfg(windows)]
fn set_private(_file: &fs::File) -> Result<()> {
	Ok(())
}

#[cfg(not(any(unix, windows)))]
fn set_private(_file: &fs::File) -> Result<()> {
	Ok(())
}

#[cfg(unix)]
fn try_lock(file: &fs::File) -> Result<bool> {
	use std::os::unix::io::AsRawFd;

	let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };

	if result == 0 {
		return Ok(true);
	}

	let error = std::io::Error::last_os_error();
	if error.kind() == std::io::ErrorKind::WouldBlock {
		return Ok(false);
	}

	Err(error.into())
}

#[cfg(windows)]
fn try_lock(file: &fs::File) -> Result<bool> {
	use std::os::windows::io::AsRawHandle;

	use windows_sys::Win32::Storage::FileSystem::{LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx};
	use windows_sys::Win32::System::IO::OVERLAPPED;

	// 33 is `ERROR_LOCK_VIOLATION`, which is a held lock rather than a failure.
	const LOCK_VIOLATION: i32 = 33;

	let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
	let result = unsafe {
		LockFileEx(
			file.as_raw_handle() as _,
			LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
			0,
			u32::MAX,
			u32::MAX,
			&mut overlapped,
		)
	};

	if result != 0 {
		return Ok(true);
	}

	let error = std::io::Error::last_os_error();
	if error.raw_os_error() == Some(LOCK_VIOLATION) {
		return Ok(false);
	}

	Err(error.into())
}

#[cfg(not(any(unix, windows)))]
fn try_lock(_file: &fs::File) -> Result<bool> {
	Err(Error::UnsupportedTransport)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_held_lock_refuses_a_second_daemon() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("lock");

		let first = Lock::acquire(&path, dir.path()).unwrap();
		let second = Lock::acquire(&path, dir.path());

		assert!(matches!(second, Err(Error::AlreadyRunning(_))));

		drop(first);
		let third = Lock::acquire(&path, dir.path());
		assert!(third.is_ok(), "the lock died with its owner");
	}

	/// Unix only: an exclusive byte-range lock makes the file unreadable through
	/// any handle but the owner's on Windows, which is exactly how the lock is
	/// supposed to behave there. The recorded pid is a diagnostic for a human
	/// with a debugger, not something a second process can read.
	#[cfg(unix)]
	#[test]
	fn the_lock_records_its_owner() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("lock");

		let lock = Lock::acquire(&path, dir.path()).unwrap();
		let recorded = fs::read_to_string(&path).unwrap();

		assert_eq!(recorded.trim(), std::process::id().to_string());
		drop(lock);
	}
}
