//! Atomic output writing and source restoration.
//!
//! Two rules from the specifications live here.
//!
//! `docs/errors.md` §5: an output is written to a temporary file in its target
//! directory and renamed into place only when complete, so a path is either
//! absent or correct, never half written.
//!
//! When a conversion fails, the renamed path still holds the source
//! format's bytes. The source extension is restored, so that no path carries a
//! format it does not contain.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Prefix for temporary output files. Never a valid intent, never a user name.
pub const TEMP_PREFIX: &str = ".retype-tmp.";

/// Prefix for a temporary input name, used only when the source name is taken.
pub const TEMP_IN_PREFIX: &str = ".retype-in.";

/// Whether a path is one of our temporary files.
pub fn is_temp_file(path: &Path) -> bool {
	path.components().any(|component| {
		component.as_os_str().to_str().is_some_and(|name| {
			name.starts_with(TEMP_PREFIX) || name.starts_with(TEMP_IN_PREFIX) || name.starts_with(".retype-recovery.")
		})
	})
}

/// Write `target` atomically.
///
/// `write` is handed a temporary path in the same directory as `target`. If it
/// returns `Err` or panics, `target` is untouched. A publication or sync error may
/// occur after the rename, so callers must not assume an error means no target exists.
pub fn write_atomically(target: &Path, write: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
	let staged = StagedOutput::replacing(target)?;
	write(staged.temp())?;
	staged.commit()
}

#[derive(Debug, Clone, Copy)]
pub enum Publication {
	Create,
	Replace,
}

/// A temporary file that becomes `target` on commit and is normally removed on drop.
pub struct StagedOutput {
	temp: PathBuf,
	target: PathBuf,
	_directory: tempfile::TempDir,
	should_replace: bool,
}

impl StagedOutput {
	pub fn new(target: &Path) -> Result<Self> {
		let target = crate::fsx::entry_path(target)?;
		let parent = target.parent().ok_or_else(|| Error::NoParent(target.clone()))?;
		let name = target.file_name().ok_or_else(|| Error::NoFileName(target.clone()))?;
		/* A private directory allows tools that require an absent output or replace it themselves. */
		let directory = tempfile::Builder::new().prefix(TEMP_PREFIX).tempdir_in(parent)?;
		Ok(Self {
			temp: directory.path().join(name),
			target,
			_directory: directory,
			should_replace: false,
		})
	}

	pub fn for_publication(target: &Path, publication: Publication) -> Result<Self> {
		match publication {
			Publication::Create => Self::new(target),
			Publication::Replace => Self::replacing(target),
		}
	}

	pub fn replacing(target: &Path) -> Result<Self> {
		let mut staged = Self::new(target)?;
		staged.should_replace = true;
		Ok(staged)
	}

	pub fn target(&self) -> &Path {
		&self.target
	}

	/// Path the writer should create.
	pub fn temp(&self) -> &Path {
		&self.temp
	}

	/// Rename the temporary file onto the target.
	pub fn commit(self) -> Result<()> {
		self.commit_with_expected(None)
	}

	/// Publish only the file and bytes identified before the publication checkpoint.
	pub(crate) fn commit_checked(self, expected: &crate::safety::Fingerprint) -> Result<()> {
		self.commit_with_expected(Some(expected))
	}

	fn commit_with_expected(self, expected: Option<&crate::safety::Fingerprint>) -> Result<()> {
		#[cfg(windows)]
		{
			self.commit_windows(expected)
		}
		#[cfg(not(windows))]
		{
			self.commit_path(expected)
		}
	}

	#[cfg(windows)]
	fn commit_windows(self, expected: Option<&crate::safety::Fingerprint>) -> Result<()> {
		let mut file = open_publication_file(&self.temp)?;
		if let Some(expected) = expected {
			let actual = crate::safety::Fingerprint::of_open_file(&self.temp, &mut file)?;
			if !expected.same_file_and_content(&actual) {
				return Err(changed_staged(&self.temp));
			}
		}
		file.sync_all()?;
		crate::fsx::sync_directory(self._directory.path())?;
		let policy = if self.should_replace {
			Publication::Replace
		} else {
			Publication::Create
		};
		crate::fsx::rename_open(&file, &self.target, policy)?;
		file.sync_all()?;
		if let Some(expected) = expected {
			if !expected.matches(&self.target)? {
				return Err(changed_staged(&self.target));
			}
		}
		drop(file);
		crate::fsx::sync_directory(self.target.parent().expect("resolved parent"))?;
		Ok(())
	}

	#[cfg(not(windows))]
	fn commit_path(self, expected: Option<&crate::safety::Fingerprint>) -> Result<()> {
		if !fs::symlink_metadata(&self.temp)?.file_type().is_file() {
			return Err(Error::Io(std::io::Error::other("staged output is not a regular file")));
		}
		if let Some(expected) = expected {
			if !expected.matches(&self.temp)? {
				return Err(changed_staged(&self.temp));
			}
		}
		crate::fsx::sync_file(&self.temp)?;
		crate::fsx::sync_directory(self._directory.path())?;
		if self.should_replace {
			crate::fsx::rename(&self.temp, &self.target)?;
		} else {
			crate::fsx::rename_noreplace(&self.temp, &self.target)?;
		}
		crate::fsx::sync_directory(self.target.parent().expect("resolved parent"))?;
		if let Some(expected) = expected {
			if !expected.matches(&self.target)? {
				return Err(changed_staged(&self.target));
			}
		}
		Ok(())
	}
}

#[cfg(windows)]
fn open_publication_file(path: &Path) -> Result<fs::File> {
	use std::os::windows::fs::OpenOptionsExt;
	use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
	use windows_sys::Win32::Storage::FileSystem::DELETE;

	let mut options = fs::OpenOptions::new();
	options
		.read(true)
		.write(true)
		.access_mode(GENERIC_READ | GENERIC_WRITE | DELETE);
	Ok(crate::safety::open_guarded_file(path, options)?)
}

fn changed_staged(path: &Path) -> Error {
	Error::Io(std::io::Error::other(format!(
		"{} changed during publication; recovery retained",
		path.display()
	)))
}

/// Where a converter should read from, and where the result belongs.
///
/// A rename leaves the source bytes at the new name, so a converter that reads
/// the file directly would see the wrong extension. Rather than lie about the
/// content for the duration of the conversion, the file is renamed back to its
/// source name first: the name matches the bytes at every instant, a crash
/// leaves a correctly named file.
///
/// An independent temporary copy is used when the source name is occupied.
/// The original target remains untouched and cannot be overwritten by ordinary publication.
#[derive(Debug)]
pub enum PreparedInput {
	/// The file is at its source name. The converter reads it there.
	Restored { input: PathBuf, target: PathBuf },
	/// The source name is occupied, so read an independently owned snapshot.
	Snapshot {
		input: PathBuf,
		target: PathBuf,
		directory: tempfile::TempDir,
	},
}

impl PreparedInput {
	pub fn input(&self) -> &Path {
		match self {
			Self::Restored { input, .. } | Self::Snapshot { input, .. } => input,
		}
	}

	pub fn target(&self) -> &Path {
		match self {
			Self::Restored { target, .. } | Self::Snapshot { target, .. } => target,
		}
	}

	/// Whether the input is a temporary name that must be removed afterwards.
	pub fn is_temporary(&self) -> bool {
		matches!(self, Self::Snapshot { .. })
	}
}

/// Prepare the input for a conversion whose target already holds the source
/// bytes, which is what a rename produces.
pub fn prepare_input(target: &Path, stem: &str, source_format: &str) -> Result<PreparedInput> {
	match restore_source_name(target, stem, source_format) {
		Ok(restored) => Ok(PreparedInput::Restored {
			input: restored,
			target: target.to_path_buf(),
		}),
		Err(Error::Exists(_)) => {
			let parent = target.parent().ok_or_else(|| Error::NoParent(target.to_path_buf()))?;
			let directory = tempfile::Builder::new().prefix(TEMP_IN_PREFIX).tempdir_in(parent)?;
			let input = directory.path().join(format!("{stem}.{source_format}"));
			crate::safety::copy_snapshot(target, &input)?;
			Ok(PreparedInput::Snapshot {
				input,
				target: target.to_path_buf(),
				directory,
			})
		}
		Err(error) => Err(error),
	}
}

/// Restore a path to the source extension after a failed conversion.
///
/// Returns the restored path. Refuses to overwrite an existing file rather than
/// losing whichever bytes are already there.
pub fn restore_source_name(path: &Path, stem: &str, source_format: &str) -> Result<PathBuf> {
	let parent = path.parent().ok_or_else(|| Error::NoParent(path.to_path_buf()))?;

	let restored = parent.join(format!("{stem}.{source_format}"));
	if let Err(error) = crate::fsx::rename_noreplace(path, &restored) {
		if error.kind() == std::io::ErrorKind::AlreadyExists {
			return Err(Error::Exists(restored));
		}
		return Err(Error::Io(error));
	}
	crate::fsx::sync_file(&restored)?;
	crate::fsx::sync_directory(parent)?;
	Ok(restored)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn tempdir() -> tempfile::TempDir {
		tempfile::tempdir().expect("temp dir")
	}

	#[test]
	fn writes_target() {
		let dir = tempdir();
		let target = dir.path().join("out.webp");

		write_atomically(&target, |temp| {
			fs::write(temp, b"converted")?;
			Ok(())
		})
		.unwrap();

		assert_eq!(fs::read(&target).unwrap(), b"converted");
	}

	#[test]
	fn leaves_no_temp_on_success() {
		let dir = tempdir();
		let target = dir.path().join("out.webp");

		write_atomically(&target, |temp| {
			fs::write(temp, b"x")?;
			Ok(())
		})
		.unwrap();

		let names: Vec<_> = fs::read_dir(dir.path())
			.unwrap()
			.map(|entry| entry.unwrap().file_name())
			.collect();
		assert_eq!(names, vec![std::ffi::OsString::from("out.webp")]);
	}

	#[test]
	fn failure_leaves_no_temp_and_no_target() {
		let dir = tempdir();
		let target = dir.path().join("out.webp");

		let result = write_atomically(&target, |temp| {
			fs::write(temp, b"partial")?;
			Err(Error::Io(std::io::Error::other("backend failed")))
		});

		assert!(result.is_err());
		assert!(!target.exists());
		assert!(fs::read_dir(dir.path()).unwrap().next().is_none());
	}

	#[test]
	fn failure_keeps_existing_target() {
		let dir = tempdir();
		let target = dir.path().join("out.webp");
		fs::write(&target, b"original").unwrap();

		let result = write_atomically(&target, |_temp| Err(Error::Io(std::io::Error::other("nope"))));

		assert!(result.is_err());
		assert_eq!(fs::read(&target).unwrap(), b"original");
	}

	#[test]
	fn success_overwrites_existing_target() {
		let dir = tempdir();
		let target = dir.path().join("out.webp");
		fs::write(&target, b"old").unwrap();

		write_atomically(&target, |temp| {
			fs::write(temp, b"new")?;
			Ok(())
		})
		.unwrap();

		assert_eq!(fs::read(&target).unwrap(), b"new");
	}

	#[test]
	fn checked_publication_rejects_a_same_content_staged_replacement() {
		let dir = tempdir();
		let target = dir.path().join("out.jpg");
		let staged = StagedOutput::new(&target).unwrap();
		fs::write(staged.temp(), b"validated bytes").unwrap();
		let fingerprint = crate::safety::Fingerprint::of(staged.temp()).unwrap();
		let original = dir.path().join("original-stage.jpg");
		fs::rename(staged.temp(), &original).unwrap();
		fs::copy(&original, staged.temp()).unwrap();

		let error = staged.commit_checked(&fingerprint).unwrap_err();
		assert!(error.to_string().contains("changed during publication"));
		assert!(!target.exists());
		assert_eq!(fs::read(original).unwrap(), b"validated bytes");
	}

	#[test]
	fn checked_publication_never_replaces_an_occupied_target() {
		let dir = tempdir();
		let target = dir.path().join("out.jpg");
		fs::write(&target, b"unrelated").unwrap();
		let staged = StagedOutput::new(&target).unwrap();
		fs::write(staged.temp(), b"validated bytes").unwrap();
		let fingerprint = crate::safety::Fingerprint::of(staged.temp()).unwrap();

		let error = staged.commit_checked(&fingerprint).unwrap_err();
		assert!(matches!(error, Error::Io(ref error) if error.kind() == std::io::ErrorKind::AlreadyExists));
		assert_eq!(fs::read(target).unwrap(), b"unrelated");
	}

	#[test]
	fn checked_publication_preserves_identity_for_explicit_replacement() {
		let dir = tempdir();
		let target = dir.path().join("out-\u{6f22}.jpg");
		fs::write(&target, b"old").unwrap();
		let staged = StagedOutput::replacing(&target).unwrap();
		fs::write(staged.temp(), b"validated bytes").unwrap();
		let fingerprint = crate::safety::Fingerprint::of(staged.temp()).unwrap();

		staged.commit_checked(&fingerprint).unwrap();
		assert!(fingerprint.matches(&target).unwrap());
		assert_eq!(fs::read(target).unwrap(), b"validated bytes");
	}

	#[test]
	#[cfg(windows)]
	fn windows_publication_handle_blocks_stage_mutation_until_commit() {
		use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;
		let dir = tempdir();
		let target = dir.path().join("out.jpg");
		let staged = StagedOutput::new(&target).unwrap();
		fs::write(staged.temp(), b"validated bytes").unwrap();
		let fingerprint = crate::safety::Fingerprint::of(staged.temp()).unwrap();
		let file = open_publication_file(staged.temp()).unwrap();
		let moved = dir.path().join("other.jpg");
		let write_error = fs::OpenOptions::new().write(true).open(staged.temp()).unwrap_err();
		let rename_error = crate::fsx::rename_noreplace(staged.temp(), &moved).unwrap_err();
		assert_eq!(write_error.raw_os_error(), Some(ERROR_SHARING_VIOLATION as i32));
		assert_eq!(rename_error.raw_os_error(), Some(ERROR_SHARING_VIOLATION as i32));
		assert!(!moved.exists());
		drop(file);
		staged.commit_checked(&fingerprint).unwrap();
		assert!(fingerprint.matches(&target).unwrap());
	}

	#[test]
	fn temp_file_detection() {
		assert!(is_temp_file(Path::new("/dir/.retype-tmp.photo.jpg")));
		assert!(!is_temp_file(Path::new("/dir/photo.jpg")));
		assert!(!is_temp_file(Path::new("/dir/.hidden")));
		assert!(is_temp_file(Path::new("/dir/.retype-tmp.job/photo.webp")));
		assert!(is_temp_file(Path::new("/dir/.retype-in.job/photo.png")));
		assert!(is_temp_file(Path::new("/dir/.retype-recovery.job/photo.png")));
	}

	#[test]
	fn restores_source_extension() {
		let dir = tempdir();
		let renamed = dir.path().join("photo.jpg");
		fs::write(&renamed, b"heic bytes").unwrap();

		let restored = restore_source_name(&renamed, "photo", "heic").unwrap();

		assert_eq!(restored, dir.path().join("photo.heic"));
		assert_eq!(fs::read(&restored).unwrap(), b"heic bytes");
		assert!(!renamed.exists());
	}

	#[test]
	fn restore_refuses_to_clobber() {
		let dir = tempdir();
		let renamed = dir.path().join("photo.jpg");
		let occupied = dir.path().join("photo.heic");
		fs::write(&renamed, b"new").unwrap();
		fs::write(&occupied, b"existing").unwrap();

		let error = restore_source_name(&renamed, "photo", "heic").unwrap_err();

		assert!(matches!(error, Error::Exists(_)));
		assert_eq!(fs::read(&occupied).unwrap(), b"existing");
		assert!(renamed.exists());
	}

	#[test]
	fn input_is_restored_to_the_source_name() {
		let dir = tempdir();
		let target = dir.path().join("photo.jpg");
		fs::write(&target, b"heic bytes").unwrap();

		let prepared = prepare_input(&target, "photo", "heic").unwrap();

		assert_eq!(prepared.input(), dir.path().join("photo.heic"));
		assert_eq!(prepared.target(), target);
		assert!(!prepared.is_temporary());
		// The bytes moved with the name, and no temporary file exists.
		assert_eq!(fs::read(prepared.input()).unwrap(), b"heic bytes");
		assert!(!target.exists());
		assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
	}

	#[test]
	fn input_falls_back_to_an_independent_snapshot_when_the_source_name_is_taken() {
		let dir = tempdir();
		let target = dir.path().join("photo.jpg");
		let occupied = dir.path().join("photo.heic");
		fs::write(&target, b"heic bytes").unwrap();
		fs::write(&occupied, b"someone else").unwrap();

		let prepared = prepare_input(&target, "photo", "heic").unwrap();

		assert!(prepared.is_temporary());
		assert_eq!(prepared.input().file_name().unwrap(), "photo.heic");
		assert_ne!(prepared.input().parent().unwrap(), dir.path());
		assert_eq!(prepared.target(), target);
		assert_eq!(fs::read(prepared.input()).unwrap(), b"heic bytes");
		// The occupied name is untouched, and the target still holds the bytes.
		assert_eq!(fs::read(&occupied).unwrap(), b"someone else");
		assert!(target.exists());
	}

	#[cfg(unix)]
	#[test]
	fn fallback_input_does_not_share_the_mutable_source_inode() {
		use std::os::unix::fs::MetadataExt;

		let dir = tempdir();
		let target = dir.path().join("photo.jpg");
		fs::write(dir.path().join("photo.heic"), b"occupied").unwrap();
		fs::write(&target, b"heic bytes").unwrap();

		let prepared = prepare_input(&target, "photo", "heic").unwrap();
		let input_meta = fs::metadata(prepared.input()).unwrap();
		let target_meta = fs::metadata(&target).unwrap();

		assert_ne!(
			input_meta.ino(),
			target_meta.ino(),
			"an independent copy, not a hard link"
		);
	}

	#[test]
	fn temporary_input_is_removed_on_drop() {
		let dir = tempdir();
		let target = dir.path().join("photo.jpg");
		fs::write(dir.path().join("photo.heic"), b"occupied").unwrap();
		fs::write(&target, b"heic bytes").unwrap();

		let input = {
			let prepared = prepare_input(&target, "photo", "heic").unwrap();
			prepared.input().to_path_buf()
		};

		assert!(!input.exists());
		assert!(target.exists(), "the target is never removed by cleanup");
	}

	#[test]
	fn restores_a_compound_source_format() {
		// The stem comes from the caller, which took it from the registry, so a
		// format with a dot in it needs no list of known compounds here.
		let dir = tempdir();
		let renamed = dir.path().join("backup.zip");
		fs::write(&renamed, b"tar bytes").unwrap();

		let restored = restore_source_name(&renamed, "backup", "tar.gz").unwrap();

		assert_eq!(restored, dir.path().join("backup.tar.gz"));
	}

	#[test]
	fn failed_conversion_then_restore_round_trip() {
		// The regression: the renamed path holds the source bytes, the
		// conversion fails, and the name goes back to the source extension.
		let dir = tempdir();
		let renamed = dir.path().join("photo.jpg");
		fs::write(&renamed, b"heic bytes").unwrap();

		let conversion = write_atomically(&renamed, |temp| {
			fs::write(temp, b"partial")?;
			Err(Error::Io(std::io::Error::other("encoder refused")))
		});
		assert!(conversion.is_err());

		let restored = restore_source_name(&renamed, "photo", "heic").unwrap();

		assert_eq!(restored, dir.path().join("photo.heic"));
		assert_eq!(fs::read(&restored).unwrap(), b"heic bytes");
		assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
	}
}
