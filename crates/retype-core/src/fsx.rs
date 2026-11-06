//! File operations the platform needs help with.
//!
//! Windows refuses to rename a path that another process holds open without
//! `FILE_SHARE_DELETE`, and indexing or security software commonly holds a file
//! open for a moment right after it appears. Retype's own renames are where that
//! shows up: a conversion commits an output a few milliseconds after writing it,
//! and a rename the user performs lands in the same window.
//!
//! A bounded retry is the practice the platform expects and costs nothing when
//! there is no contention. Every other platform renames directly, because
//! renaming an open file is allowed.

use std::io;
use std::path::Path;

#[cfg(all(test, windows))]
#[path = "fsx_windows_tests.rs"]
mod windows_tests;

/// Rename, retrying while Windows reports a sharing or lock violation.
pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
	#[cfg(windows)]
	{
		let delays = [
			std::time::Duration::from_millis(10),
			std::time::Duration::from_millis(50),
			std::time::Duration::from_millis(150),
			std::time::Duration::from_millis(400),
		];

		let mut attempt = 0;

		loop {
			match std::fs::rename(from, to) {
				Ok(()) => return Ok(()),
				Err(error) if attempt < delays.len() && is_transient(&error) => {
					std::thread::sleep(delays[attempt]);
					attempt += 1;
				}
				Err(error) => return Err(error),
			}
		}
	}

	#[cfg(not(windows))]
	{
		std::fs::rename(from, to)
	}
}

/// Whether the failure is the platform saying "not now" rather than "no".
#[cfg(windows)]
fn is_transient(error: &io::Error) -> bool {
	// 5 ERROR_ACCESS_DENIED, 32 ERROR_SHARING_VIOLATION, 33 ERROR_LOCK_VIOLATION.
	matches!(error.raw_os_error(), Some(5 | 32 | 33))
}

/// Flush file contents and metadata using a handle that permits Windows FlushFileBuffers.
pub fn sync_file(path: &Path) -> io::Result<()> {
	#[cfg(windows)]
	let file = std::fs::OpenOptions::new().write(true).open(path)?;
	#[cfg(not(windows))]
	let file = std::fs::File::open(path)?;
	file.sync_all()
}

/// Whether directory-entry durability is implemented on this platform.
/// Callers must retain originals and refuse destructive undo when this is false.
pub fn supports_durable_cleanup() -> bool {
	cfg!(unix)
}

/// Persist directory-entry changes where supported. This does not grant cleanup authority.
/// Windows can publish outputs, but must retain originals until directory durability is implemented.
pub fn sync_directory(path: &Path) -> io::Result<()> {
	#[cfg(unix)]
	{
		std::fs::File::open(path)?.sync_all()
	}
	#[cfg(not(unix))]
	{
		let _ = path;
		Ok(())
	}
}

/// Atomically move a file without replacing even a dangling destination symlink.
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
	#[cfg(target_os = "linux")]
	{
		use std::os::unix::ffi::OsStrExt;
		let from = std::ffi::CString::new(from.as_os_str().as_bytes())?;
		let to = std::ffi::CString::new(to.as_os_str().as_bytes())?;
		let result = unsafe {
			libc::renameat2(
				libc::AT_FDCWD,
				from.as_ptr(),
				libc::AT_FDCWD,
				to.as_ptr(),
				libc::RENAME_NOREPLACE,
			)
		};
		if result == 0 {
			Ok(())
		} else {
			Err(io::Error::last_os_error())
		}
	}
	#[cfg(target_os = "macos")]
	{
		use std::os::unix::ffi::OsStrExt;
		let from = std::ffi::CString::new(from.as_os_str().as_bytes())?;
		let to = std::ffi::CString::new(to.as_os_str().as_bytes())?;
		let result = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
		if result == 0 {
			Ok(())
		} else {
			Err(io::Error::last_os_error())
		}
	}
	#[cfg(windows)]
	{
		use std::os::windows::ffi::OsStrExt;
		use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
		let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
		let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
		if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_WRITE_THROUGH) } != 0 {
			Ok(())
		} else {
			Err(io::Error::last_os_error())
		}
	}
	#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
	{
		let _ = (from, to);
		Err(io::Error::new(
			io::ErrorKind::Unsupported,
			"atomic no-replace rename is unavailable",
		))
	}
}

/// Resolve the parent too: absolute strings alone miss directory aliases.
pub fn entry_path(path: &Path) -> io::Result<std::path::PathBuf> {
	let absolute = std::path::absolute(path)?;
	let parent = absolute
		.parent()
		.ok_or_else(|| io::Error::other("path has no parent"))?;
	Ok(parent.canonicalize()?.join(
		absolute
			.file_name()
			.ok_or_else(|| io::Error::other("path has no name"))?,
	))
}

/// Reject directory aliases and existing hard-link aliases to an input.
pub fn protects_input(input: &Path, output: &Path) -> io::Result<bool> {
	let source = entry_path(input)?;
	let destination = match entry_path(output) {
		Ok(path) => path,
		Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
		Err(error) => return Err(error),
	};
	if source == destination {
		return Ok(true);
	}
	Ok(crate::job::FileIdentity::of(input)
		.zip(crate::job::FileIdentity::of(output))
		.is_some_and(|(source, target)| source.matches(&target)))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_rename_moves_the_file() {
		let dir = tempfile::tempdir().unwrap();
		let from = dir.path().join("a.txt");
		let to = dir.path().join("b.txt");
		std::fs::write(&from, b"contents").unwrap();

		rename(&from, &to).unwrap();

		assert!(!from.exists());
		assert_eq!(std::fs::read(&to).unwrap(), b"contents");
	}

	#[test]
	fn renaming_something_absent_is_an_error() {
		let dir = tempfile::tempdir().unwrap();

		assert!(rename(&dir.path().join("absent"), &dir.path().join("b")).is_err());
	}
}
