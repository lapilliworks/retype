/* Handle-owned cleanup on qualified local NTFS paths; failures retain checked recovery. */

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows_sys::Win32::Storage::FileSystem::{
	DELETE, FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
	FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
	FILE_DISPOSITION_INFO_EX, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
	FileDispositionInfoEx, SetFileInformationByHandle,
};

use super::Fingerprint;
use crate::error::{Error, Result};
use crate::fsx::windows::Directory;

pub(super) fn remove_owned(
	path: &Path,
	expected: &Fingerprint,
	cancellation: &crate::execute::Cancellation,
) -> Result<()> {
	let absolute = std::path::absolute(path)?;
	let parent_path = absolute
		.parent()
		.ok_or_else(|| io::Error::other("cleanup path has no parent"))?;
	let parent = Directory::open(parent_path)?;
	let path = crate::fsx::entry_path(path)?;
	let mut file = open_owned(&path)?;
	let actual = Fingerprint::of_open_file_cancellable(&path, &mut file, cancellation)?;
	if !expected.same_file_and_content(&actual) || !actual.matches_cancellable(&path, cancellation)? {
		return Err(Error::Exists(path));
	}

	let directory = tempfile::Builder::new()
		.prefix(".retype-recovery.")
		.tempdir_in(parent_path)?
		.keep();
	let held_directory = Directory::open(&directory)?;
	let claimed = directory.join(path.file_name().expect("resolved file name"));
	cancellation.commit(|| {
		Ok(crate::fsx::rename_open(
			&file,
			&claimed,
			crate::convert::Publication::Create,
		)?)
	})?;
	#[cfg(test)]
	crate::durability_tests::checkpoint("cleanup.claimed")?;
	file.sync_all()?;
	held_directory.sync()?;
	parent.sync()?;
	#[cfg(test)]
	crate::durability_tests::checkpoint("cleanup.claim-durable")?;

	if cancellation.is_cancelled() {
		if crate::fsx::rename_open(&file, &path, crate::convert::Publication::Create).is_ok() {
			held_directory.sync()?;
			parent.sync()?;
		}
		return Err(crate::error::Diagnostic::global(
			crate::error::Code::JobCancelled,
			"cleanup cancelled; claimed data remains at its original or recovery path",
		)
		.into());
	}
	/* Delete the object we checked, never a pathname that could select another file. */
	cancellation.commit(|| Ok(remove_open(file)?))?;
	if claimed.try_exists()? {
		return Err(io::Error::other("claimed file is still visible after handle-owned deletion").into());
	}
	#[cfg(test)]
	crate::durability_tests::checkpoint("cleanup.unlinked")?;
	held_directory.sync()?;
	drop(held_directory);
	fs::remove_dir(&directory)?;
	parent.sync()?;
	#[cfg(test)]
	crate::durability_tests::checkpoint("cleanup.complete")?;
	Ok(())
}

pub(super) fn restore_owned_name(
	from: &Path,
	to: &Path,
	expected: &Fingerprint,
	mut note_rename: impl FnMut(&Path, &Path),
) -> Result<Fingerprint> {
	let absolute = std::path::absolute(from)?;
	let parent_path = absolute
		.parent()
		.ok_or_else(|| io::Error::other("source restoration path has no parent"))?;
	let parent = Directory::open(parent_path)?;
	let resolved_from = crate::fsx::entry_path(from)?;
	let resolved_to = crate::fsx::entry_path(to)?;
	if resolved_from.parent() != resolved_to.parent() {
		return Err(io::Error::other("source restoration must stay within one directory").into());
	}
	let mut file = open_owned(&resolved_from)?;
	let before = Fingerprint::of_open_file(&resolved_from, &mut file)?;
	if !expected.same_file_and_content(&before) || !path_matches_open_file(&resolved_from, &file)? {
		return Err(io::Error::other("the renamed input changed before restoration").into());
	}
	crate::fsx::rename_open(&file, &resolved_to, crate::convert::Publication::Create)?;
	note_rename(from, to);
	let checked = (|| {
		/* The guarded handle denies writers while the filesystem changes the name. */
		let after = Fingerprint {
			identity: crate::job::FileIdentity::of_open_file(&file),
			..before.clone()
		};
		if after.identity.is_none() || !path_matches_open_file(&resolved_to, &file)? {
			return Err(Error::Io(io::Error::other(
				"the renamed input changed during restoration",
			)));
		}
		file.sync_all()?;
		Ok(after)
	})();
	let after = match checked {
		Ok(after) => after,
		Err(error) => {
			if let Err(rollback) = crate::fsx::rename_open(&file, &resolved_from, crate::convert::Publication::Create) {
				return Err(io::Error::other(format!(
					"{error}; rollback failed: {rollback}; check {} and {}",
					from.display(),
					to.display()
				))
				.into());
			}
			note_rename(to, from);
			file.sync_all()?;
			drop(file);
			parent.sync()?;
			return Err(error);
		}
	};
	drop(file);
	parent.sync()?;
	Ok(after)
}

fn path_matches_open_file(path: &Path, file: &File) -> io::Result<bool> {
	let candidate = OpenOptions::new()
		.read(true)
		.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
		.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
		.open(path)?;
	let metadata = candidate.metadata()?;
	if !metadata.file_type().is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
		return Ok(false);
	}
	Ok(crate::job::FileIdentity::of_open_file(file)
		.zip(crate::job::FileIdentity::of_open_file(&candidate))
		.is_some_and(|(held, selected)| held.matches(&selected)))
}

fn open_owned(path: &Path) -> io::Result<File> {
	let metadata = fs::symlink_metadata(path)?;
	let placeholders = FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;
	if metadata.file_attributes() & placeholders != 0 {
		return Err(io::Error::new(
			io::ErrorKind::Unsupported,
			"file operation refuses cloud placeholders",
		));
	}
	let mut options = OpenOptions::new();
	options
		.read(true)
		.write(true)
		.access_mode(GENERIC_READ | GENERIC_WRITE | DELETE);
	let file = super::open_guarded_file(path, options)?;
	if file.metadata()?.file_attributes() & placeholders != 0 {
		return Err(io::Error::new(
			io::ErrorKind::Unsupported,
			"file operation refuses cloud placeholders",
		));
	}
	Ok(file)
}

fn remove_open(file: File) -> io::Result<()> {
	let info = FILE_DISPOSITION_INFO_EX {
		Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
	};
	let result = unsafe {
		SetFileInformationByHandle(
			file.as_raw_handle() as _,
			FileDispositionInfoEx,
			(&raw const info).cast(),
			std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
		)
	};
	if result == 0 {
		return Err(io::Error::last_os_error());
	}
	drop(file);
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn restoration_holds_the_file_and_its_parent_against_replacement() {
		let directory = tempfile::tempdir().unwrap();
		let from = directory.path().join("requested.zip");
		let to = directory.path().join("original.tar");
		fs::write(&from, b"original bytes").unwrap();
		let expected = Fingerprint::of(&from).unwrap();
		let mut observed = false;
		let restored = restore_owned_name(&from, &to, &expected, |old, new| {
			assert_eq!(old, from);
			assert_eq!(new, to);
			assert!(fs::write(new, b"replacement").is_err());
			assert!(fs::rename(new, directory.path().join("moved")).is_err());
			assert!(fs::rename(directory.path(), directory.path().with_extension("moved")).is_err());
			observed = true;
		})
		.unwrap();
		assert!(observed);
		assert!(!from.exists());
		assert!(restored.same_content(&expected));
		assert!(restored.matches(&to).unwrap());
		assert_eq!(fs::read(to).unwrap(), b"original bytes");
	}

	#[test]
	fn restoration_refuses_a_same_content_replacement() {
		let directory = tempfile::tempdir().unwrap();
		let from = directory.path().join("requested.zip");
		let to = directory.path().join("original.tar");
		fs::write(&from, b"original bytes").unwrap();
		let expected = Fingerprint::of(&from).unwrap();
		fs::rename(&from, directory.path().join("previous")).unwrap();
		fs::write(&from, b"original bytes").unwrap();
		assert!(restore_owned_name(&from, &to, &expected, |_, _| panic!("unexpected rename")).is_err());
		assert_eq!(fs::read(from).unwrap(), b"original bytes");
		assert!(!to.exists());
	}

	#[test]
	fn restoration_never_overwrites_an_existing_destination() {
		let directory = tempfile::tempdir().unwrap();
		let from = directory.path().join("requested.zip");
		let to = directory.path().join("original.tar");
		fs::write(&from, b"original bytes").unwrap();
		fs::write(&to, b"unrelated bytes").unwrap();
		let expected = Fingerprint::of(&from).unwrap();
		assert!(restore_owned_name(&from, &to, &expected, |_, _| panic!("unexpected rename")).is_err());
		assert!(expected.matches(&from).unwrap());
		assert_eq!(fs::read(to).unwrap(), b"unrelated bytes");
	}
}
