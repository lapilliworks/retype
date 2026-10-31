//! Writing archives, `docs/tools.md` §7.
//!
//! An archive is a container: the bytes go in unchanged, so there is no fidelity
//! parity question and nothing to hand to a third-party encoder. Path traversal,
//! symlinks and permissions are decisions we make here rather than flags we pass,
//! which is the other half of the reason these formats are native.
//!
//! Rules, deliberately chosen rather than inherited:
//!
//! - Everything under the directory is stored, including dotfiles. The ignore list
//!   in `docs/watch.md` is about what is worth watching, not about what the user
//!   asked to package
//! - A symbolic link refuses the archive. Storing what it points at lies about the
//!   tree, and storing the link itself is not portable through zip; failing names
//!   the file and costs one gesture
//! - Entries are sorted, so two runs on one directory produce the same archive
//! - Permissions are stored from the file, and restored by whoever unpacks it
//! - The archive is written to a temporary path and renamed into place, so an
//!   interrupted run leaves no partial file, `docs/errors.md` §5

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::convert::StagedOutput;
use crate::error::{Code, Diagnostic, Error, Result};
use crate::execute::{Cancellation, Report};
use crate::registry::Native;

/// How much of the refusal text a run keeps, matching the tool runner.
const NOTES: usize = 4 * 1024;

/// One thing to store.
struct Entry {
	/// The name inside the archive, with `/` separators.
	name: String,
	path: PathBuf,
	is_dir: bool,
	mode: u32,
}

/// Write an archive of the directory at `input` to `output`.
pub fn create(codec: Native, input: &Path, output: &Path, cancellation: &Cancellation) -> Result<Report> {
	if !input.is_dir() {
		return Err(Error::Diagnostic(Diagnostic::global(
			Code::Unsupported,
			format!("{} is not a directory, so it cannot be archived", input.display()),
		)));
	}

	let entries = collect(input, cancellation)?;
	let staged = StagedOutput::new(output)?;

	let notes = match codec {
		Native::Zip => write_zip(&entries, staged.temp(), cancellation)?,
		Native::Tar => write_tar(&entries, staged.temp(), None, cancellation)?,
		Native::TarGz => write_tar(&entries, staged.temp(), Some(()), cancellation)?,
	};

	// The temporary file is what the archive was written as, so its length is the
	// archive's; committing first would need a second stat of the same file.
	let bytes = fs::metadata(staged.temp())?.len();
	staged.commit()?;

	Ok(Report {
		output: output.to_path_buf(),
		bytes,
		stderr: notes
			.map(|notes| notes.chars().take(NOTES).collect())
			.unwrap_or_default(),
	})
}

/// Every entry below `root`, sorted, named relative to `root`'s own name.
fn collect(root: &Path, cancellation: &Cancellation) -> Result<Vec<Entry>> {
	let base = root
		.file_name()
		.map(|name| name.to_string_lossy().into_owned())
		.unwrap_or_else(|| "archive".to_string());

	let mut entries = Vec::new();
	walk(root, &base, &mut entries, cancellation)?;
	entries.sort_by(|left, right| left.name.cmp(&right.name));
	Ok(entries)
}

fn walk(root: &Path, base: &str, entries: &mut Vec<Entry>, cancellation: &Cancellation) -> Result<()> {
	if cancellation.is_cancelled() {
		return Err(Error::Diagnostic(Diagnostic::global(
			Code::JobCancelled,
			"the archive was cancelled",
		)));
	}

	let mut children: Vec<PathBuf> = fs::read_dir(root)?
		.map(|entry| entry.map(|entry| entry.path()))
		.collect::<std::io::Result<Vec<_>>>()?;
	children.sort();

	for path in children {
		let metadata = fs::symlink_metadata(&path)?;
		let name = path
			.file_name()
			.map(|name| name.to_string_lossy().into_owned())
			.unwrap_or_default();
		let name = format!("{base}/{name}");

		// `file_type` rather than `is_dir`, because a link to a directory must not
		// be walked into: that is how an archive escapes its own tree.
		let file_type = metadata.file_type();

		if file_type.is_symlink() {
			return Err(Error::Diagnostic(Diagnostic::global(
				Code::Unsupported,
				format!(
					"{} is a symbolic link: an archive stores what it is given, and following \
					 a link would store something the directory does not contain",
					path.display()
				),
			)));
		}

		if file_type.is_dir() {
			entries.push(Entry {
				name: name.clone(),
				path: path.clone(),
				is_dir: true,
				mode: mode_of(&metadata),
			});
			walk(&path, &name, entries, cancellation)?;
			continue;
		}

		if !file_type.is_file() {
			return Err(Error::Diagnostic(Diagnostic::global(
				Code::Unsupported,
				format!(
					"{} is not a file or a directory, so it cannot be archived",
					path.display()
				),
			)));
		}

		entries.push(Entry {
			name,
			path,
			is_dir: false,
			mode: mode_of(&metadata),
		});
	}

	Ok(())
}

fn write_zip(entries: &[Entry], target: &Path, cancellation: &Cancellation) -> Result<Option<String>> {
	let options = zip::write::SimpleFileOptions::default()
		.compression_method(zip::CompressionMethod::Deflated)
		.large_file(true);

	let mut writer = zip::ZipWriter::new(File::create(target)?);

	for entry in entries {
		if cancellation.is_cancelled() {
			return Err(Error::Diagnostic(Diagnostic::global(
				Code::JobCancelled,
				"the archive was cancelled",
			)));
		}

		let options = options.unix_permissions(entry.mode);

		if entry.is_dir {
			writer.add_directory(&entry.name, options).map_err(zip_failure)?;
			continue;
		}

		writer.start_file(&entry.name, options).map_err(zip_failure)?;
		let mut file = File::open(&entry.path)?;
		std::io::copy(&mut file, &mut writer)?;
	}

	writer.finish().map_err(zip_failure)?;
	Ok(None)
}

fn write_tar(
	entries: &[Entry],
	target: &Path,
	gzip: Option<()>,
	cancellation: &Cancellation,
) -> Result<Option<String>> {
	let file = File::create(target)?;

	match gzip {
		Some(()) => {
			let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
			let builder = write_tar_entries(entries, encoder, cancellation)?;
			builder.into_inner()?.finish()?;
		}
		None => {
			write_tar_entries(entries, file, cancellation)?;
		}
	}

	Ok(None)
}

fn write_tar_entries<W: Write>(entries: &[Entry], sink: W, cancellation: &Cancellation) -> Result<tar::Builder<W>> {
	let mut builder = tar::Builder::new(sink);

	for entry in entries {
		if cancellation.is_cancelled() {
			return Err(Error::Diagnostic(Diagnostic::global(
				Code::JobCancelled,
				"the archive was cancelled",
			)));
		}

		if entry.is_dir {
			let mut header = tar::Header::new_gnu();
			header.set_entry_type(tar::EntryType::Directory);
			header.set_size(0);
			header.set_mode(entry.mode);
			header.set_mtime(0);
			header.set_cksum();
			builder.append_data(&mut header, format!("{}/", entry.name), std::io::empty())?;
			continue;
		}

		let mut header = tar::Header::new_gnu();
		header.set_entry_type(tar::EntryType::Regular);
		header.set_size(fs::metadata(&entry.path)?.len());
		header.set_mode(entry.mode);
		header.set_mtime(0);
		header.set_cksum();
		builder.append_data(&mut header, &entry.name, File::open(&entry.path)?)?;
	}

	builder.finish()?;
	Ok(builder)
}

/// A zip failure, kept as a filesystem failure where that is what it was.
fn zip_failure(error: zip::result::ZipError) -> Error {
	match error {
		zip::result::ZipError::Io(error) => Error::Io(error),
		other => Error::Diagnostic(Diagnostic::global(
			Code::IoFailed,
			format!("the archive could not be written: {other}"),
		)),
	}
}

/// The mode to store. A platform without Unix modes stores the usual defaults, so
/// an archive made on Windows is readable rather than mode 000.
#[cfg(unix)]
fn mode_of(metadata: &fs::Metadata) -> u32 {
	use std::os::unix::fs::PermissionsExt;
	metadata.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn mode_of(metadata: &fs::Metadata) -> u32 {
	match metadata.is_dir() {
		true => 0o755,
		false => 0o644,
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::io::Read;

	/// A directory named the way an archive carries it, with a nested tree, a dotfile
	/// and an empty directory.
	fn tree() -> (tempfile::TempDir, PathBuf) {
		let dir = tempfile::tempdir().unwrap();
		let photos = dir.path().join("Photos");
		fs::create_dir_all(photos.join("sub/deeper")).unwrap();
		fs::create_dir_all(photos.join("empty")).unwrap();
		fs::write(photos.join("one.txt"), b"first").unwrap();
		fs::write(photos.join("sub/two.bin"), b"second").unwrap();
		fs::write(photos.join(".hidden"), b"third").unwrap();
		fs::write(photos.join("sub/deeper/three.txt"), b"fourth").unwrap();
		(dir, photos)
	}

	fn run(codec: Native, photos: &Path) -> Report {
		let output = photos.with_extension(codec.name());
		create(codec, photos, &output, &Cancellation::default()).expect("creates")
	}

	fn zip_contents(path: &Path) -> Vec<(String, Vec<u8>)> {
		let mut archive = zip::ZipArchive::new(File::open(path).unwrap()).unwrap();
		let mut entries = Vec::new();

		for index in 0..archive.len() {
			let mut entry = archive.by_index(index).unwrap();
			let mut bytes = Vec::new();
			entry.read_to_end(&mut bytes).unwrap();
			entries.push((entry.name().to_string(), bytes));
		}

		entries
	}

	fn tar_contents(path: &Path) -> Vec<(String, Vec<u8>)> {
		let mut archive = tar::Archive::new(File::open(path).unwrap());
		let mut entries = Vec::new();

		for entry in archive.entries().unwrap() {
			let mut entry = entry.unwrap();
			let name = entry.path().unwrap().display().to_string();
			let mut bytes = Vec::new();
			entry.read_to_end(&mut bytes).unwrap();
			entries.push((name, bytes));
		}

		entries
	}

	#[test]
	fn a_zip_holds_the_whole_tree_under_the_directory_name() {
		let (_dir, photos) = tree();
		let report = run(Native::Zip, &photos);
		let entries = zip_contents(&report.output);

		let names: Vec<&str> = entries.iter().map(|(name, _)| name.as_str()).collect();
		assert!(names.contains(&"Photos/one.txt"), "{names:?}");
		assert!(names.contains(&"Photos/sub/two.bin"), "{names:?}");
		assert!(names.contains(&"Photos/.hidden"), "{names:?}");
		assert!(names.contains(&"Photos/sub/deeper/three.txt"), "{names:?}");
		assert!(names.contains(&"Photos/empty/"), "{names:?}");

		let first = entries.iter().find(|(name, _)| name == "Photos/one.txt").unwrap();
		assert_eq!(first.1, b"first");
		assert_eq!(report.bytes, fs::metadata(&report.output).unwrap().len());
	}

	#[test]
	fn a_tar_holds_the_same_tree() {
		let (_dir, photos) = tree();
		let report = run(Native::Tar, &photos);
		let entries = tar_contents(&report.output);

		let names: Vec<&str> = entries.iter().map(|(name, _)| name.as_str()).collect();
		assert!(names.contains(&"Photos/one.txt"), "{names:?}");
		assert!(names.contains(&"Photos/sub/deeper/three.txt"), "{names:?}");

		let one = entries.iter().find(|(name, _)| name == "Photos/one.txt").unwrap();
		assert_eq!(one.1, b"first");
	}

	#[test]
	fn a_compressed_tar_is_a_gzip_stream_of_the_same_tree() {
		let (_dir, photos) = tree();
		let report = run(Native::TarGz, &photos);

		let raw = fs::read(&report.output).unwrap();
		assert_eq!(&raw[..2], &[0x1f, 0x8b], "a gzip magic number");

		let mut decompressed = Vec::new();
		flate2::read::GzDecoder::new(raw.as_slice())
			.read_to_end(&mut decompressed)
			.unwrap();

		let mut names = Vec::new();
		for entry in tar::Archive::new(decompressed.as_slice()).entries().unwrap() {
			names.push(entry.unwrap().path().unwrap().display().to_string());
		}

		assert!(names.iter().any(|name| name.ends_with("two.bin")), "{names:?}");
	}

	#[test]
	fn entries_are_sorted_so_two_runs_agree() {
		let (_dir, photos) = tree();
		let first = run(Native::Zip, &photos);
		let second = run(Native::Zip, &photos);

		assert_eq!(
			zip_contents(&first.output),
			zip_contents(&second.output),
			"the same tree produces the same entries in the same order"
		);
	}

	#[cfg(unix)]
	#[test]
	fn a_symlink_refuses_the_archive_rather_than_lying_about_the_tree() {
		let (_dir, photos) = tree();
		std::os::unix::fs::symlink("/etc/passwd", photos.join("link")).unwrap();
		let output = photos.with_extension("zip");

		let error = create(Native::Zip, &photos, &output, &Cancellation::default()).unwrap_err();

		assert_eq!(error.code(), Code::Unsupported);
		assert!(error.to_string().contains("symbolic link"), "{error}");
		assert!(!output.exists(), "nothing was written");
	}

	#[test]
	fn a_cancelled_archive_writes_nothing() {
		let (_dir, photos) = tree();
		let cancellation = Cancellation::default();
		cancellation.cancel();
		let output = photos.with_extension("zip");

		let error = create(Native::Zip, &photos, &output, &cancellation).unwrap_err();

		assert_eq!(error.code(), Code::JobCancelled);
		assert!(!output.exists(), "the staged file is discarded");
	}

	#[test]
	fn a_file_is_not_a_directory_to_archive() {
		let (_dir, photos) = tree();
		let output = photos.with_extension("zip");

		let error = create(Native::Zip, &photos.join("one.txt"), &output, &Cancellation::default()).unwrap_err();

		assert_eq!(error.code(), Code::Unsupported);
	}
}
