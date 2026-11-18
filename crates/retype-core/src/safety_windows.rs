/* Handle-owned cleanup candidate. The public caller retains the production durability gate. */

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows_sys::Win32::Storage::FileSystem::{
	DELETE, FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
	FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO_EX,
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

fn open_owned(path: &Path) -> io::Result<File> {
	let metadata = fs::symlink_metadata(path)?;
	let placeholders = FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;
	if metadata.file_attributes() & placeholders != 0 {
		return Err(io::Error::new(
			io::ErrorKind::Unsupported,
			"cleanup refuses cloud placeholders",
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
			"cleanup refuses cloud placeholders",
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
