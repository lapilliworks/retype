//! Stable input snapshots and preservation-first filesystem ownership.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, Write};
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
		Self::open_cancellable(source, &crate::execute::Cancellation::default())
	}

	pub fn open_cancellable(source: &Path, cancellation: &crate::execute::Cancellation) -> Result<Self> {
		let source = crate::fsx::entry_path(source)?;
		let directory = tempfile::Builder::new()
			.prefix(crate::convert::TEMP_IN_PREFIX)
			.tempdir_in(source.parent().expect("resolved parent"))?;
		let path = directory.path().join(source.file_name().expect("resolved name"));
		let original = copy_snapshot_cancellable(&source, &path, cancellation)?;
		Ok(Self {
			path,
			original,
			_directory: directory,
		})
	}
}

/// Captured without reading contents; preparation must verify this before and after hashing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputStamp {
	identity: FileIdentity,
	bytes: u64,
	modified: std::time::Duration,
	modified_before_epoch: bool,
	changed: Option<(i64, i64)>,
}

impl InputStamp {
	pub fn of(path: &Path) -> Result<Self> {
		let file = regular_file(path)?;
		let metadata = file.metadata()?;
		#[cfg(unix)]
		let (identity, changed) = {
			use std::os::unix::fs::MetadataExt;
			(
				Some(FileIdentity::Unix {
					dev: metadata.dev(),
					ino: metadata.ino(),
					birth_ns: None,
				}),
				Some((metadata.ctime(), metadata.ctime_nsec())),
			)
		};
		#[cfg(windows)]
		let (identity, changed) = {
			use std::os::windows::io::AsRawHandle;
			use windows_sys::Win32::Storage::FileSystem::{
				FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandleEx,
			};
			let mut basic: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
			if unsafe {
				GetFileInformationByHandleEx(
					file.as_raw_handle() as _,
					FileBasicInfo,
					std::ptr::from_mut(&mut basic).cast(),
					std::mem::size_of::<FILE_BASIC_INFO>() as u32,
				)
			} == 0
			{
				return Err(io::Error::last_os_error().into());
			}
			(FileIdentity::of_open_file(&file), Some((basic.ChangeTime, 0)))
		};
		#[cfg(not(any(unix, windows)))]
		let (identity, changed) = (FileIdentity::of(path), None);
		// SystemTime's serializer rejects legitimate file dates before 1970.
		let (modified, modified_before_epoch) = match metadata.modified()?.duration_since(std::time::UNIX_EPOCH) {
			Ok(duration) => (duration, false),
			Err(error) => (error.duration(), true),
		};
		Ok(Self {
			identity: identity.ok_or_else(|| io::Error::other("cannot identify admitted input"))?,
			bytes: metadata.len(),
			modified,
			modified_before_epoch,
			changed,
		})
	}

	pub fn verify(&self, path: &Path) -> Result<()> {
		if &Self::of(path)? != self {
			return Err(crate::error::Diagnostic::global(
				crate::error::Code::JobInterrupted,
				format!(
					"{} changed or was replaced while the job waited; no output was written",
					path.display()
				),
			)
			.into());
		}
		Ok(())
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
		Self::of_cancellable(path, &crate::execute::Cancellation::default())
	}

	pub fn of_cancellable(path: &Path, cancellation: &crate::execute::Cancellation) -> Result<Self> {
		let mut file = regular_file(path)?;
		Self::of_open_file_cancellable(path, &mut file, cancellation)
	}

	#[cfg(any(test, windows))]
	pub(crate) fn of_open_file(path: &Path, file: &mut File) -> Result<Self> {
		Self::of_open_file_cancellable(path, file, &crate::execute::Cancellation::default())
	}

	pub(crate) fn of_open_file_cancellable(
		path: &Path,
		file: &mut File,
		cancellation: &crate::execute::Cancellation,
	) -> Result<Self> {
		let before = file.metadata()?;
		file.rewind()?;
		let mut hash = Sha256::new();
		let bytes = copy_bytes(file, &mut hash, cancellation)?;
		let after = file.metadata()?;
		if before.len() != bytes || before.modified()? != after.modified()? || after.len() != bytes {
			return Err(changed(path));
		}
		#[cfg(unix)]
		let identity = {
			use std::os::unix::fs::MetadataExt;
			Some(FileIdentity::Unix {
				dev: before.dev(),
				ino: before.ino(),
				birth_ns: None,
			})
		};
		/* The identity must describe the handle whose bytes were hashed. */
		#[cfg(windows)]
		let identity = FileIdentity::of_open_file(file);
		#[cfg(not(any(unix, windows)))]
		let identity = FileIdentity::of(path);
		Ok(Self {
			identity,
			bytes,
			sha256: format!("{:x}", hash.finalize()),
		})
	}

	pub fn same_content(&self, other: &Self) -> bool {
		self.bytes == other.bytes && self.sha256 == other.sha256
	}

	pub fn same_file_and_content(&self, other: &Self) -> bool {
		self.same_content(other)
			&& self
				.identity
				.as_ref()
				.zip(other.identity.as_ref())
				.is_some_and(|(expected, actual)| expected.matches(actual))
	}

	pub fn matches(&self, path: &Path) -> Result<bool> {
		self.matches_cancellable(path, &crate::execute::Cancellation::default())
	}

	pub fn matches_cancellable(&self, path: &Path, cancellation: &crate::execute::Cancellation) -> Result<bool> {
		Ok(self.same_file_and_content(&Self::of_cancellable(path, cancellation)?))
	}
}

/// Open a regular file without following its final component; Windows excludes mutation sharing.
pub(crate) fn open_guarded_file(path: &Path, mut options: OpenOptions) -> io::Result<File> {
	#[cfg(unix)]
	{
		use std::os::unix::fs::OpenOptionsExt;
		options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
	}
	#[cfg(windows)]
	{
		use std::os::windows::fs::OpenOptionsExt;
		use windows_sys::Win32::Storage::FileSystem::{
			FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_WRITE_THROUGH, FILE_SHARE_READ,
		};
		options
			.share_mode(FILE_SHARE_READ)
			.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_WRITE_THROUGH);
	}
	let file = options.open(path)?;
	let metadata = file.metadata()?;
	if !metadata.file_type().is_file() {
		return Err(io::Error::other(format!("{} is not a regular file", path.display())));
	}
	#[cfg(windows)]
	{
		use std::os::windows::fs::MetadataExt;
		use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
		if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
			return Err(io::Error::new(
				io::ErrorKind::Unsupported,
				format!("{} is a reparse point; a regular file is required", path.display()),
			));
		}
	}
	Ok(file)
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
	Error::Io(io::Error::other(format!(
		"{} changed while being read; original retained",
		path.display()
	)))
}

/// Copy into a new file, checking that the independent snapshot matches a stable source.
pub fn copy_snapshot(source: &Path, target: &Path) -> Result<Fingerprint> {
	copy_snapshot_cancellable(source, target, &crate::execute::Cancellation::default())
}

pub fn copy_snapshot_cancellable(
	source: &Path,
	target: &Path,
	cancellation: &crate::execute::Cancellation,
) -> Result<Fingerprint> {
	let before = Fingerprint::of_cancellable(source, cancellation)?;
	let mut input = regular_file(source)?;
	let attributes = crate::xattr::attributes(&input)?;
	let mut options = OpenOptions::new();
	options.write(true).create_new(true);
	#[cfg(unix)]
	{
		use std::os::unix::fs::OpenOptionsExt;
		options.mode(0o600);
	}
	let mut output = options.open(target)?;
	copy_bytes(&mut input, &mut output, cancellation)?;
	crate::xattr::copy_attributes(&attributes, &output)?;
	output.sync_all()?;
	let snapshot = Fingerprint::of_cancellable(target, cancellation)?;
	if !before.same_file_and_content(&Fingerprint::of_cancellable(source, cancellation)?)
		|| !before.same_content(&snapshot)
		|| attributes != crate::xattr::attributes(&input)?
		|| attributes != crate::xattr::attributes(&output)?
	{
		return Err(changed(source));
	}
	Ok(before)
}

fn copy_bytes(
	input: &mut impl Read,
	output: &mut impl Write,
	cancellation: &crate::execute::Cancellation,
) -> Result<u64> {
	let mut buffer = [0; 64 * 1024];
	let mut total = 0;
	loop {
		cancellation.commit(|| Ok(()))?;
		let read = input.read(&mut buffer)?;
		if read == 0 {
			return Ok(total);
		}
		output.write_all(&buffer[..read])?;
		total += read as u64;
	}
}

/// Claim first, then verify. Never unlink an unchecked public pathname.
/// Requires exclusive ownership: Unix renames do not revoke external writers' open handles.
/// On any ambiguous restore, the claimed file remains in a persistent recovery directory.
pub fn remove_owned(path: &Path, expected: &Fingerprint) -> Result<()> {
	remove_owned_cancellable(path, expected, &crate::execute::Cancellation::default())
}

pub fn remove_owned_cancellable(
	path: &Path,
	expected: &Fingerprint,
	cancellation: &crate::execute::Cancellation,
) -> Result<()> {
	cancellation.commit(|| Ok(()))?;
	if !crate::fsx::supports_durable_cleanup() {
		return Err(io::Error::new(
			io::ErrorKind::Unsupported,
			"source retained: directory durability unavailable",
		)
		.into());
	}
	let path = crate::fsx::entry_path(path)?;
	let parent = path.parent().expect("resolved parent");
	let directory = tempfile::Builder::new()
		.prefix(".retype-recovery.")
		.tempdir_in(parent)?
		.keep();
	let claimed = directory.join(path.file_name().expect("resolved name"));
	if let Err(error) = crate::fsx::rename_noreplace(&path, &claimed) {
		fs::remove_dir(&directory)?;
		return Err(error.into());
	}
	let verified = expected.matches_cancellable(&claimed, cancellation);
	if !matches!(verified, Ok(true)) {
		if crate::fsx::rename_noreplace(&claimed, &path).is_ok() {
			fs::remove_dir(&directory)?;
		}
		return Err(Error::Io(io::Error::other(format!(
			"{} no longer holds the expected file; preserved at {} or its original name",
			path.display(),
			claimed.display(),
		))));
	}
	/* A crash before unlink must leave the claimed source reachable. */
	crate::fsx::sync_directory(&directory)?;
	crate::fsx::sync_directory(parent)?;
	if cancellation.is_cancelled() {
		let _ = crate::fsx::rename_noreplace(&claimed, &path);
		return Err(crate::error::Diagnostic::global(
			crate::error::Code::JobCancelled,
			"cleanup cancelled; claimed data remains at its original or recovery path",
		)
		.into());
	}
	fs::remove_file(&claimed)?;
	crate::fsx::sync_directory(&directory)?;
	fs::remove_dir(&directory)?;
	crate::fsx::sync_directory(parent)?;
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn admission_can_journal_a_file_modified_before_the_unix_epoch() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::write(&source, b"original").unwrap();
		File::options()
			.write(true)
			.open(&source)
			.unwrap()
			.set_times(fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH - std::time::Duration::from_secs(1)))
			.unwrap();
		let stamp = InputStamp::of(&source).unwrap();
		let json = serde_json::to_string(&stamp).unwrap();
		let restored: InputStamp = serde_json::from_str(&json).unwrap();
		restored.verify(&source).unwrap();
		assert!(restored.modified_before_epoch);
	}

	#[test]
	fn admission_rejects_replacement_even_with_the_same_bytes_and_mtime() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		let replacement = dir.path().join("replacement");
		fs::write(&source, b"original").unwrap();
		let stamp = InputStamp::of(&source).unwrap();
		fs::write(&replacement, b"original").unwrap();
		File::options()
			.write(true)
			.open(&replacement)
			.unwrap()
			.set_times(fs::FileTimes::new().set_modified(fs::metadata(&source).unwrap().modified().unwrap()))
			.unwrap();
		fs::rename(&source, dir.path().join("old")).unwrap();
		fs::rename(replacement, &source).unwrap();
		assert!(stamp.verify(&source).is_err());
	}

	#[test]
	fn admission_rejects_same_size_edits_with_a_restored_modification_time() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::write(&source, b"original").unwrap();
		let modified = fs::metadata(&source).unwrap().modified().unwrap();
		let stamp = InputStamp::of(&source).unwrap();
		fs::write(&source, b"modified").unwrap();
		File::options()
			.write(true)
			.open(&source)
			.unwrap()
			.set_times(fs::FileTimes::new().set_modified(modified))
			.unwrap();
		assert_eq!(
			stamp.verify(&source).unwrap_err().code(),
			crate::error::Code::JobInterrupted
		);
	}

	#[test]
	fn a_cancelled_snapshot_does_not_copy_the_input() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		let target = dir.path().join("target");
		fs::write(&source, b"original").unwrap();
		let cancellation = crate::execute::Cancellation::default();
		cancellation.cancel();
		assert_eq!(
			copy_snapshot_cancellable(&source, &target, &cancellation)
				.unwrap_err()
				.code(),
			crate::error::Code::JobCancelled
		);
		assert!(!target.exists());
	}

	#[test]
	fn equal_contents_do_not_make_a_replacement_the_same_file() {
		let dir = tempfile::tempdir().unwrap();
		let first = dir.path().join("first");
		let replacement = dir.path().join("replacement");
		fs::write(&first, b"same bytes").unwrap();
		fs::write(&replacement, b"same bytes").unwrap();
		let expected = Fingerprint::of(&first).unwrap();
		let other = Fingerprint::of(&replacement).unwrap();

		assert!(expected.same_content(&other));
		assert!(!expected.same_file_and_content(&other));
		assert!(expected.same_file_and_content(&Fingerprint::of(&first).unwrap()));
	}

	#[test]
	fn fingerprinting_an_open_file_rewinds_it_before_each_read() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("original");
		fs::write(&path, b"original bytes").unwrap();
		let mut file = regular_file(&path).unwrap();
		let first = Fingerprint::of_open_file(&path, &mut file).unwrap();
		let second = Fingerprint::of_open_file(&path, &mut file).unwrap();
		assert_eq!(first, second);
		assert_eq!(first.bytes, b"original bytes".len() as u64);
	}

	#[test]
	#[cfg(any(unix, windows))]
	fn a_fingerprint_identifies_the_open_file_after_its_path_is_replaced() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("original");
		let moved = dir.path().join("opened-original");
		let replacement = dir.path().join("replacement");
		fs::write(&path, b"same bytes").unwrap();
		fs::write(&replacement, b"same bytes").unwrap();
		let mut opened = regular_file(&path).unwrap();
		let original_id = FileIdentity::of(&path).unwrap();

		crate::fsx::rename(&path, &moved).unwrap();
		crate::fsx::rename(&replacement, &path).unwrap();
		let fingerprint = Fingerprint::of_open_file(&path, &mut opened).unwrap();
		let other = Fingerprint::of(&path).unwrap();
		assert!(original_id.matches(fingerprint.identity.as_ref().unwrap()));
		assert!(fingerprint.same_content(&other));
		assert!(!fingerprint.same_file_and_content(&other));
		assert!(fingerprint.matches(&moved).unwrap());
		assert!(!fingerprint.matches(&path).unwrap());
	}
}
