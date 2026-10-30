//! Extended attributes: provenance that travels with the file,
//! `docs/provenance.md` §3.
//!
//! One string, the canonical intent, because that is the question a file cannot
//! answer for itself: `holiday.jpg` does not say whether it was quality 85 or 40.
//! The name is fixed per platform and nothing else is written, so an attribute
//! can only ever be ours.
//!
//! Writing is best effort by design. A filesystem may not carry attributes, a
//! policy may forbid them, and a synced folder may silently strip them; none of
//! that is a reason to fail a conversion, and none of it is read as a
//! precondition. The journal is the record that always exists.

use std::io;
use std::path::Path;

/// The attribute a converted file carries.
#[cfg(target_os = "linux")]
pub const INTENT: &str = "user.retype.intent";

#[cfg(target_os = "macos")]
pub const INTENT: &str = "com.retype.intent";

/// Attributes are written on Linux and macOS. Elsewhere they are a no-op rather
/// than an error: Windows streams are undecided, `docs/provenance.md` §6.2, and
/// the BSDs use `extattr` rather than xattrs.
pub fn supported() -> bool {
	cfg!(any(target_os = "linux", target_os = "macos"))
}

/// Record the intent on the file.
///
/// `Ok(false)` is the platform or the filesystem declining, which is expected and
/// not reported: the answer to "was this stripped?" is that provenance is best
/// effort everywhere.
pub fn write_intent(path: &Path, intent: &str) -> io::Result<bool> {
	#[cfg(any(target_os = "linux", target_os = "macos"))]
	{
		let Some(path) = cstring(path) else {
			return Ok(false);
		};

		let Some(name) = cstring(Path::new(INTENT)) else {
			return Ok(false);
		};

		let value = intent.as_bytes();
		let written = unsafe {
			#[cfg(target_os = "linux")]
			{
				libc::setxattr(path.as_ptr(), name.as_ptr(), value.as_ptr().cast(), value.len(), 0)
			}

			#[cfg(target_os = "macos")]
			{
				libc::setxattr(path.as_ptr(), name.as_ptr(), value.as_ptr().cast(), value.len(), 0, 0)
			}
		};

		if written == 0 {
			return Ok(true);
		}

		let error = io::Error::last_os_error();
		Ok(match declined(&error) {
			true => false,
			false => return Err(error),
		})
	}

	#[cfg(not(any(target_os = "linux", target_os = "macos")))]
	{
		let _ = (path, intent);
		Ok(false)
	}
}

/// Read the intent a file carries, if it carries one.
pub fn read_intent(path: &Path) -> io::Result<Option<String>> {
	#[cfg(any(target_os = "linux", target_os = "macos"))]
	{
		let Some(path) = cstring(path) else {
			return Ok(None);
		};

		let Some(name) = cstring(Path::new(INTENT)) else {
			return Ok(None);
		};

		// An attribute is short, but its length is the filesystem's business, so a
		// short buffer asks again with the size it was told.
		let mut buffer = vec![0u8; 256];

		loop {
			let read = unsafe {
				#[cfg(target_os = "linux")]
				{
					libc::getxattr(path.as_ptr(), name.as_ptr(), buffer.as_mut_ptr().cast(), buffer.len())
				}

				#[cfg(target_os = "macos")]
				{
					libc::getxattr(
						path.as_ptr(),
						name.as_ptr(),
						buffer.as_mut_ptr().cast(),
						buffer.len(),
						0,
						0,
					)
				}
			};

			if read >= 0 {
				buffer.truncate(read as usize);
				return String::from_utf8(buffer)
					.map(Some)
					.map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "the recorded intent is not UTF-8"));
			}

			let error = io::Error::last_os_error();

			if declined(&error) {
				return Ok(None);
			}

			// ERANGE means the buffer was too small, and the size is not knowable
			// from the failed call, so grow and ask again.
			if error.raw_os_error() == Some(libc::ERANGE) && buffer.len() < 64 * 1024 {
				let grown = buffer.len() * 2;
				buffer.resize(grown, 0);
				continue;
			}

			return Err(error);
		}
	}

	#[cfg(not(any(target_os = "linux", target_os = "macos")))]
	{
		let _ = path;
		Ok(None)
	}
}

/// Whether the failure is the platform saying "not here" rather than something
/// going wrong.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn declined(error: &io::Error) -> bool {
	match error.raw_os_error() {
		// The attribute is not there: `ENODATA` is Linux's name for it and
		// `ENOATTR` is macOS's.
		#[cfg(target_os = "linux")]
		Some(libc::ENODATA) => true,
		#[cfg(target_os = "macos")]
		Some(libc::ENOATTR) => true,
		// The filesystem has no attributes, or the user may not write them.
		Some(libc::ENOTSUP) | Some(libc::EPERM) | Some(libc::EACCES) => true,
		// No such file is a race the caller cannot act on either.
		Some(libc::ENOENT) => true,
		_ => false,
	}
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn cstring(path: &Path) -> Option<std::ffi::CString> {
	use std::os::unix::ffi::OsStrExt;

	std::ffi::CString::new(path.as_os_str().as_bytes()).ok()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn an_intent_survives_a_write_and_a_read() {
		let dir = tempfile::tempdir().unwrap();
		let file = dir.path().join("photo.webp");
		std::fs::write(&file, b"pixels").unwrap();

		match write_intent(&file, "webp+quality85") {
			Ok(true) => {}
			// A filesystem without attributes is a supported outcome, not a failure.
			_ => return,
		}

		assert_eq!(read_intent(&file).unwrap().as_deref(), Some("webp+quality85"));
	}

	#[test]
	fn a_file_without_an_intent_says_nothing() {
		let dir = tempfile::tempdir().unwrap();
		let file = dir.path().join("photo.webp");
		std::fs::write(&file, b"pixels").unwrap();

		assert_eq!(read_intent(&file).unwrap(), None);
	}

	#[test]
	fn writing_to_a_file_that_is_not_there_is_declined_rather_than_fatal() {
		let dir = tempfile::tempdir().unwrap();

		assert!(!write_intent(&dir.path().join("absent.webp"), "webp").unwrap());
		assert_eq!(read_intent(&dir.path().join("absent.webp")).unwrap(), None);
	}
}
