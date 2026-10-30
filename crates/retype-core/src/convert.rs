//! Atomic output writing and source restoration.
//!
//! Two rules from the specifications live here.
//!
//! `docs/errors.md` §5: an output is written to a temporary file in its target
//! directory and renamed into place only when complete, so a path is either
//! absent or correct, never half written.
//!
//!: when a conversion fails, the renamed path still holds the source
//! format's bytes. The source extension is restored, so that no path carries a
//! format it does not contain.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Implementations compiled into this build, `docs/config.md` V12. An operation
/// may only name one of these, so a configuration cannot ask for a backend that
/// does not exist. Empty until the image vertical lands its own encoder.
pub const NATIVE: &[&str] = &[];

/// Prefix for temporary output files. Never a valid intent, never a user name.
pub const TEMP_PREFIX: &str = ".retype-tmp.";

/// Prefix for a temporary input name, used only when the source name is taken.
pub const TEMP_IN_PREFIX: &str = ".retype-in.";

/// Whether a path is one of our temporary files.
pub fn is_temp_file(path: &Path) -> bool {
	path.file_name()
		.and_then(|name| name.to_str())
		.is_some_and(|name| name.starts_with(TEMP_PREFIX))
}

/// Write `target` atomically.
///
/// `write` is handed a temporary path in the same directory as `target`. If it
/// returns `Ok`, the temporary file is renamed onto `target`. If it returns
/// `Err` or panics, the temporary file is removed and `target` is untouched.
pub fn write_atomically(target: &Path, write: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
	let staged = StagedOutput::new(target)?;
	write(staged.temp())?;
	staged.commit()
}

/// A temporary file that becomes `target` on commit and disappears otherwise.
pub struct StagedOutput {
	temp: PathBuf,
	target: PathBuf,
	committed: bool,
}

impl StagedOutput {
	pub fn new(target: &Path) -> Result<Self> {
		let parent = target.parent().ok_or_else(|| Error::NoParent(target.to_path_buf()))?;
		let name = target
			.file_name()
			.ok_or_else(|| Error::NoFileName(target.to_path_buf()))?;

		let temp = parent.join(format!("{TEMP_PREFIX}{}", name.to_string_lossy()));
		Ok(Self {
			temp,
			target: target.to_path_buf(),
			committed: false,
		})
	}

	/// Path the writer should create.
	pub fn temp(&self) -> &Path {
		&self.temp
	}

	/// Rename the temporary file onto the target.
	pub fn commit(mut self) -> Result<()> {
		fs::rename(&self.temp, &self.target)?;
		self.committed = true;
		Ok(())
	}
}

impl Drop for StagedOutput {
	fn drop(&mut self) {
		if !self.committed {
			let _ = fs::remove_file(&self.temp);
		}
	}
}

/// Where a converter should read from, and where the result belongs.
///
/// A rename leaves the source bytes at the new name, so a converter that reads
/// the file directly would see the wrong extension. Rather than lie about the
/// content for the duration of the conversion, the file is renamed back to its
/// source name first: the name matches the bytes at every instant, a crash
/// leaves a correctly named file, and filesystems without hard links need no
/// copy.
///
/// The temporary link exists only for the case where the source name is taken
/// by something else and the file therefore cannot be renamed back.
#[derive(Debug)]
pub enum PreparedInput {
	/// The file is at its source name. The converter reads it there.
	Restored { input: PathBuf, target: PathBuf },
	/// The file could not be renamed back, so its bytes are reachable through
	/// a temporary link, or a copy where links are unsupported.
	Linked { input: PathBuf, target: PathBuf },
}

impl PreparedInput {
	pub fn input(&self) -> &Path {
		match self {
			Self::Restored { input, .. } | Self::Linked { input, .. } => input,
		}
	}

	pub fn target(&self) -> &Path {
		match self {
			Self::Restored { target, .. } | Self::Linked { target, .. } => target,
		}
	}

	/// Whether the input is a temporary name that must be removed afterwards.
	pub fn is_temporary(&self) -> bool {
		matches!(self, Self::Linked { .. })
	}
}

impl Drop for PreparedInput {
	fn drop(&mut self) {
		if let Self::Linked { input, .. } = self {
			let _ = fs::remove_file(input);
		}
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
			let input = temporary_input_path(target, stem, source_format)?;
			if fs::hard_link(target, &input).is_err() {
				// Filesystems without hard links, such as exFAT camera media.
				fs::copy(target, &input)?;
			}
			Ok(PreparedInput::Linked {
				input,
				target: target.to_path_buf(),
			})
		}
		Err(error) => Err(error),
	}
}

fn temporary_input_path(target: &Path, stem: &str, source_format: &str) -> Result<PathBuf> {
	let parent = target.parent().ok_or_else(|| Error::NoParent(target.to_path_buf()))?;
	Ok(parent.join(format!("{TEMP_IN_PREFIX}{stem}.{source_format}")))
}

/// Restore a path to the source extension after a failed conversion.
///
/// Returns the restored path. Refuses to overwrite an existing file rather than
/// losing whichever bytes are already there.
pub fn restore_source_name(path: &Path, stem: &str, source_format: &str) -> Result<PathBuf> {
	let parent = path.parent().ok_or_else(|| Error::NoParent(path.to_path_buf()))?;

	let restored = parent.join(format!("{stem}.{source_format}"));
	if restored.exists() {
		return Err(Error::Exists(restored));
	}
	fs::rename(path, &restored)?;
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
	fn temp_file_detection() {
		assert!(is_temp_file(Path::new("/dir/.retype-tmp.photo.jpg")));
		assert!(!is_temp_file(Path::new("/dir/photo.jpg")));
		assert!(!is_temp_file(Path::new("/dir/.hidden")));
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
	fn input_falls_back_to_a_link_when_the_source_name_is_taken() {
		let dir = tempdir();
		let target = dir.path().join("photo.jpg");
		let occupied = dir.path().join("photo.heic");
		fs::write(&target, b"heic bytes").unwrap();
		fs::write(&occupied, b"someone else").unwrap();

		let prepared = prepare_input(&target, "photo", "heic").unwrap();

		assert!(prepared.is_temporary());
		assert_eq!(prepared.input(), dir.path().join(".retype-in.photo.heic"));
		assert_eq!(prepared.target(), target);
		assert_eq!(fs::read(prepared.input()).unwrap(), b"heic bytes");
		// The occupied name is untouched, and the target still holds the bytes.
		assert_eq!(fs::read(&occupied).unwrap(), b"someone else");
		assert!(target.exists());
	}

	#[cfg(unix)]
	#[test]
	fn linked_input_shares_the_inode() {
		use std::os::unix::fs::MetadataExt;

		let dir = tempdir();
		let target = dir.path().join("photo.jpg");
		fs::write(dir.path().join("photo.heic"), b"occupied").unwrap();
		fs::write(&target, b"heic bytes").unwrap();

		let prepared = prepare_input(&target, "photo", "heic").unwrap();
		let input_meta = fs::metadata(prepared.input()).unwrap();
		let target_meta = fs::metadata(&target).unwrap();

		assert_eq!(input_meta.ino(), target_meta.ino(), "a link, not a copy");
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
 // The regression for: the renamed path holds the source bytes, the
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
