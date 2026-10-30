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
