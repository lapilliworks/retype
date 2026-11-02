//! Stable input snapshots and preservation-first filesystem ownership.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::job::FileIdentity;

pub struct Snapshot {
	pub path: std::path::PathBuf,
	pub original: Fingerprint,
	_directory: tempfile::TempDir,
}

impl Snapshot {
	pub fn open(source: &Path) -> Result<Self> {
		let source = crate::fsx::entry_path(source)?;
		let directory = tempfile::Builder::new().prefix(crate::convert::TEMP_IN_PREFIX)
			.tempdir_in(source.parent().expect("resolved parent"))?;
		let path = directory.path().join(source.file_name().expect("resolved name"));
		let original = copy_snapshot(&source, &path)?;
		Ok(Self { path, original, _directory: directory })
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
	pub identity: Option<FileIdentity>,
	pub bytes: u64,
	pub sha256: String,
}

impl Fingerprint {
	pub fn of(path: &Path) -> Result<Self> {
		let mut file = regular_file(path)?;
		let before = file.metadata()?;
		let mut hash = Sha256::new();
		let bytes = io::copy(&mut file, &mut hash)?;
		let after = file.metadata()?;
		if before.len() != bytes || before.modified()? != after.modified()? || after.len() != bytes {
			return Err(changed(path));
		}
		Ok(Self { identity: FileIdentity::of(path), bytes, sha256: format!("{:x}", hash.finalize()) })
	}

	pub fn same_content(&self, other: &Self) -> bool {
		self.bytes == other.bytes && self.sha256 == other.sha256
	}

	pub fn matches(&self, path: &Path) -> Result<bool> {
		let current = Self::of(path)?;
		Ok(self.same_content(&current) && self.identity.as_ref().zip(current.identity.as_ref())
			.is_some_and(|(expected, actual)| expected.matches(actual)))
	}
}

fn regular_file(path: &Path) -> io::Result<File> {
	if !fs::symlink_metadata(path)?.file_type().is_file() {
		return Err(io::Error::other(format!("{} is not a regular file", path.display())));
	}
	let mut options = OpenOptions::new();
	options.read(true);
	#[cfg(unix)]
	{
		use std::os::unix::fs::OpenOptionsExt;
		options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
	}
	let file = options.open(path)?;
	if !file.metadata()?.is_file() {
		return Err(io::Error::other("input changed while opening"));
	}
	Ok(file)
}

fn changed(path: &Path) -> Error {
	Error::Io(io::Error::other(format!("{} changed while being read; original retained", path.display())))
}

/// Copy into a new file, checking that the independent snapshot matches a stable source.
pub fn copy_snapshot(source: &Path, target: &Path) -> Result<Fingerprint> {
	let before = Fingerprint::of(source)?;
	let mut input = regular_file(source)?;
	let mut options = OpenOptions::new();
	options.write(true).create_new(true);
	#[cfg(unix)]
	{
		use std::os::unix::fs::OpenOptionsExt;
		options.mode(0o600);
	}
	let mut output = options.open(target)?;
	io::copy(&mut input, &mut output)?;
	output.sync_all()?;
	let snapshot = Fingerprint::of(target)?;
	if !before.matches(source)? || !before.same_content(&snapshot) {
		return Err(changed(source));
	}
	Ok(before)
}

/// Claim first, then verify. Never unlink an unchecked public pathname.
/// On any ambiguous restore, the claimed file remains in a persistent recovery directory.
pub fn remove_owned(path: &Path, expected: &Fingerprint) -> Result<()> {
	let path = crate::fsx::entry_path(path)?;
	let parent = path.parent().expect("resolved parent");
	let directory = tempfile::Builder::new().prefix(".retype-recovery.").tempdir_in(parent)?.keep();
	let claimed = directory.join(path.file_name().expect("resolved name"));
	if let Err(error) = crate::fsx::rename_noreplace(&path, &claimed) {
		fs::remove_dir(&directory)?;
		return Err(error.into());
	}
	let verified = expected.matches(&claimed);
	if !matches!(verified, Ok(true)) {
		if crate::fsx::rename_noreplace(&claimed, &path).is_ok() {
			fs::remove_dir(&directory)?;
		}
		return Err(Error::Io(io::Error::other(format!(
			"{} no longer holds the expected file; preserved at {} or its original name",
			path.display(), claimed.display(),
		))));
	}
	/* A crash before unlink must leave the claimed source reachable. */
	crate::fsx::sync_directory(&directory)?;
	crate::fsx::sync_directory(parent)?;
	fs::remove_file(&claimed)?;
	crate::fsx::sync_directory(&directory)?;
	fs::remove_dir(&directory)?;
	crate::fsx::sync_directory(parent)?;
	Ok(())
}
