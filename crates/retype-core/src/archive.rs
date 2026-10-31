//! Repacking archives, `docs/tools.md` §7.
//!
//! An archive is a container, and converting one into another is a conversion like
//! any other: the entries keep their names, their bytes and their permissions, and
//! only the container changes. Nothing is decompressed to a directory, nothing is
//! compressed from one, and no file outside the archive is involved, which is why
//! this is `exact` and why it belongs in this program rather than in another tool's
//! argv.
//!
//! Rules, chosen here rather than inherited from a format's looseness:
//!
//! - Entry names must be relative and free of `..`. A name that would escape the
//!   archive is refused rather than copied into another container
//! - A symbolic link refuses the archive. Carrying one of those is how a repack
//!   becomes a traversal for whoever unpacks it next
//! - Directories, files and their permission bits are carried; devices, FIFOs and
//!   anything else are refused by name
//! - The declared size of each entry is checked against a budget before a byte is
//!   copied, so an archive that expands without bound stops before it fills the disk
//! - Entries are written in the order the source lists them, so a repack of the same
//!   archive twice produces the same bytes

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;

use crate::convert::StagedOutput;
use crate::error::{Code, Diagnostic, Error, Result};
use crate::execute::{Cancellation, Report};
use crate::registry::Native;

/// How many entries one repack will carry.
pub const MAX_ENTRIES: usize = 100_000;

/// How many bytes one repack will carry, counted as the source declares them. A
/// fixed limit, documented as one: an archive is not a place to discover that a
/// small file was a large one.
pub const MAX_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Convert the archive at `input` into the archive format `to` at `output`.
pub fn repack(from: Native, to: Native, input: &Path, output: &Path, cancellation: &Cancellation) -> Result<Report> {
	let staged = StagedOutput::new(output)?;

	let mut source = Source::open(from, input)?;
	let mut sink = Sink::open(to, staged.temp())?;

	let mut budget = MAX_BYTES;
	source.each(&mut sink, cancellation, &mut budget)?;
	sink.commit()?;

	// The staged file is the archive, so its length is the answer; committing first
	// would only move it.
	let bytes = fs::metadata(staged.temp())?.len();
	staged.commit()?;

	Ok(Report {
		output: output.to_path_buf(),
		bytes,
		stderr: String::new(),
	})
}

/// The source archive, opened for reading.
enum Source {
	Zip(zip::ZipArchive<File>),
	Tar(Box<tar::Archive<TarSource>>),
}

enum TarSource {
	Plain(File),
	Gzip(Box<flate2::read::GzDecoder<File>>),
}

impl Read for TarSource {
	fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
		match self {
			Self::Plain(file) => file.read(buffer),
			Self::Gzip(decoder) => decoder.read(buffer),
		}
	}
}

impl Source {
	fn open(codec: Native, path: &Path) -> Result<Self> {
		let file = File::open(path).map_err(|error| unreadable(path, &error))?;

		match codec {
			Native::Zip => Ok(Self::Zip(
				zip::ZipArchive::new(file).map_err(|error| unreadable(path, &error))?,
			)),
			Native::Tar => Ok(Self::Tar(Box::new(tar::Archive::new(TarSource::Plain(file))))),
			Native::TarGz => Ok(Self::Tar(Box::new(tar::Archive::new(TarSource::Gzip(Box::new(
				flate2::read::GzDecoder::new(file),
			)))))),
		}
	}

	/// Hand every entry to `sink`, in the order the source lists them.
	fn each(&mut self, sink: &mut Sink, cancellation: &Cancellation, budget: &mut u64) -> Result<()> {
		match self {
			Self::Zip(archive) => {
				for index in 0..archive.len() {
					stop_if_cancelled(cancellation)?;

					let mut entry = archive.by_index(index).map_err(|error| malformed("the zip", &error))?;

					let name = entry.name().to_string();
					let mode = entry.unix_mode().unwrap_or(0o644);

					if entry.is_dir() {
						sink.directory(&name, mode)?;
						continue;
					}

					if is_link(mode) {
						return Err(link_refused(&name));
					}

					let size = entry.size();
					spend(&name, size, budget)?;
					sink.file(&name, mode, size, &mut entry)?;
				}
			}
			Self::Tar(archive) => {
				for entry in archive.entries().map_err(|error| malformed("the tar", &error))? {
					stop_if_cancelled(cancellation)?;

					let mut entry = entry.map_err(|error| malformed("the tar", &error))?;
					let header = entry.header().clone();

					let path = entry.path().map_err(|error| malformed("the tar", &error))?;
					let name = path.to_string_lossy().into_owned();
					let mode = header.mode().unwrap_or(0o644);

					match header.entry_type() {
						tar::EntryType::Directory => {
							sink.directory(&name, mode)?;
							continue;
						}
						tar::EntryType::Regular | tar::EntryType::Continuous => {}
						tar::EntryType::Symlink | tar::EntryType::Link => return Err(link_refused(&name)),
						other => {
							return Err(Error::Diagnostic(Diagnostic::global(
								Code::Unsupported,
								format!(
									"'{name}' is {other:?}, and an archive of files and directories is all this carries"
								),
							)));
						}
					}

					let size = header.size().map_err(|error| malformed("the tar", &error))?;
					spend(&name, size, budget)?;
					sink.file(&name, mode, size, &mut entry)?;
				}
			}
		}

		Ok(())
	}
}

/// The target archive, written as the source is walked.
enum Sink {
	Zip(Box<zip::ZipWriter<File>>),
	Tar(Box<tar::Builder<TarSink>>),
}

enum TarSink {
	Plain(File),
	Gzip(Box<flate2::write::GzEncoder<File>>),
}

impl Write for TarSink {
	fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
		match self {
			Self::Plain(file) => file.write(buffer),
			Self::Gzip(encoder) => encoder.write(buffer),
		}
	}

	fn flush(&mut self) -> std::io::Result<()> {
		match self {
			Self::Plain(file) => file.flush(),
			Self::Gzip(encoder) => encoder.flush(),
		}
	}
}

impl Sink {
	/// Open the output through a staged file, so a failed repack leaves nothing.
	fn open(codec: Native, target: &Path) -> Result<Self> {
		let file = File::create(target)?;

		match codec {
			Native::Zip => Ok(Self::Zip(Box::new(zip::ZipWriter::new(file)))),
			Native::Tar => Ok(Self::Tar(Box::new(tar::Builder::new(TarSink::Plain(file))))),
			Native::TarGz => Ok(Self::Tar(Box::new(tar::Builder::new(TarSink::Gzip(Box::new(
				flate2::write::GzEncoder::new(file, flate2::Compression::default()),
			)))))),
		}
	}

	fn directory(&mut self, name: &str, mode: u32) -> Result<()> {
		let name = archive_name(name)?;

		match self {
			Self::Zip(writer) => writer
				.add_directory(&name, zip_options(mode))
				.map_err(|error| unwritable(&error)),
			Self::Tar(builder) => {
				let mut header = tar::Header::new_gnu();
				header.set_entry_type(tar::EntryType::Directory);
				header.set_size(0);
				header.set_mode(mode);
				header.set_mtime(0);
				header.set_cksum();

				builder
					.append_data(&mut header, format!("{name}/"), std::io::empty())
					.map_err(|error| unwritable(&error))
			}
		}
	}

	/// Copy one file, exactly `size` bytes of it.
	fn file(&mut self, name: &str, mode: u32, size: u64, bytes: &mut dyn Read) -> Result<()> {
		let name = archive_name(name)?;

		match self {
			Self::Zip(writer) => {
				writer
					.start_file(&name, zip_options(mode))
					.map_err(|error| unwritable(&error))?;

				let copied = std::io::copy(&mut bytes.take(size), writer).map_err(|error| unwritable(&error))?;

				match copied == size {
					true => Ok(()),
					false => Err(short(&name, size, copied)),
				}
			}
			Self::Tar(builder) => {
				let mut header = tar::Header::new_gnu();
				header.set_entry_type(tar::EntryType::Regular);
				header.set_size(size);
				header.set_mode(mode);
				header.set_mtime(0);
				header.set_cksum();

				builder
					.append_data(&mut header, &name, bytes.take(size))
					.map_err(|error| unwritable(&error))
			}
		}
	}

	/// Close the container. A zip keeps its directory at the end; a gzip stream keeps
	/// its checksum there, so neither is complete until this runs.
	fn commit(self) -> Result<()> {
		match self {
			Self::Zip(writer) => {
				(*writer).finish().map_err(|error| unwritable(&error))?;
			}
			Self::Tar(builder) => {
				let mut builder = builder;
				builder.finish().map_err(|error| unwritable(&error))?;

				if let TarSink::Gzip(encoder) = builder.into_inner().map_err(|error| unwritable(&error))? {
					encoder.finish().map_err(|error| unwritable(&error))?;
				}
			}
		}

		Ok(())
	}
}

fn zip_options(mode: u32) -> zip::write::SimpleFileOptions {
	zip::write::SimpleFileOptions::default()
		.compression_method(zip::CompressionMethod::Deflated)
		.large_file(true)
		.unix_permissions(mode)
}

/// The name an entry is allowed to have: relative, not empty, and with `/`
/// separators, so a Windows-made zip becomes a tar a Unix can read.
fn archive_name(name: &str) -> Result<String> {
	if name.is_empty() {
		return Err(refused(name, "an entry with no name has nowhere to go"));
	}

	let path = Path::new(name);

	if path.is_absolute() || path.has_root() {
		return Err(refused(name, "an absolute name would escape the archive"));
	}

	if path
		.components()
		.any(|part| matches!(part, std::path::Component::ParentDir))
	{
		return Err(refused(name, "a name with '..' would escape the archive"));
	}

	Ok(name.replace('\\', "/"))
}

/// Take `size` out of the budget, before any of it is copied.
fn spend(name: &str, size: u64, budget: &mut u64) -> Result<()> {
	if size > *budget {
		return Err(refused(
			name,
			&format!("the archive expands beyond the {MAX_BYTES} byte limit"),
		));
	}

	*budget -= size;
	Ok(())
}

fn stop_if_cancelled(cancellation: &Cancellation) -> Result<()> {
	match cancellation.is_cancelled() {
		true => Err(Error::Diagnostic(Diagnostic::global(
			Code::JobCancelled,
			"the repack was cancelled",
		))),
		false => Ok(()),
	}
}

fn is_link(mode: u32) -> bool {
	mode & 0o170_000 == 0o120_000
}

fn refused(name: &str, why: &str) -> Error {
	Error::Diagnostic(Diagnostic::global(
		Code::Unsupported,
		format!("'{name}' cannot be carried into another archive: {why}"),
	))
}

fn link_refused(name: &str) -> Error {
	refused(
		name,
		"a symbolic link is how a repack becomes a traversal for whoever unpacks it next",
	)
}

fn short(name: &str, declared: u64, copied: u64) -> Error {
	Error::Diagnostic(Diagnostic::global(
		Code::IoFailed,
		format!("'{name}' declares {declared} bytes and holds {copied}"),
	))
}

fn unreadable(path: &Path, error: &dyn std::fmt::Display) -> Error {
	Error::Diagnostic(Diagnostic::global(
		Code::IoFailed,
		format!("{} could not be read as an archive: {error}", path.display()),
	))
}

fn malformed(what: &str, error: &dyn std::fmt::Display) -> Error {
	Error::Diagnostic(Diagnostic::global(
		Code::IoFailed,
		format!("{what} is damaged: {error}"),
	))
}

fn unwritable(error: &dyn std::fmt::Display) -> Error {
	Error::Diagnostic(Diagnostic::global(
		Code::IoFailed,
		format!("the archive could not be written: {error}"),
	))
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::io::Read;

	/// A directory tree to put into an archive, next to the archive itself.
	fn tree() -> (tempfile::TempDir, std::path::PathBuf) {
		let dir = tempfile::tempdir().unwrap();
		let tree = dir.path().join("tree");
		fs::create_dir_all(tree.join("sub")).unwrap();
		fs::write(tree.join("one.txt"), b"first").unwrap();
		fs::write(tree.join("sub/two.bin"), [0u8, 1, 2, 3]).unwrap();
		fs::write(tree.join(".hidden"), b"third").unwrap();
		(dir, tree)
	}

	/// A zip of that tree, written with the crate that is also the reader, so the
	/// test is about this module rather than about the format.
	fn zip_of(tree: &Path) -> std::path::PathBuf {
		let path = tree.with_extension("zip");
		let mut writer = zip::ZipWriter::new(File::create(&path).unwrap());

		for (name, bytes) in [
			("tree/one.txt", b"first".as_slice()),
			("tree/sub/two.bin", &[0u8, 1, 2, 3]),
			("tree/.hidden", b"third"),
		] {
			writer.start_file(name, zip_options(0o644)).unwrap();
			writer.write_all(bytes).unwrap();
		}

		writer.finish().unwrap();
		path
	}

	fn cost() -> Cancellation {
		Cancellation::default()
	}

	/// What a tar holds, decompressed first where it was compressed.
	fn entries(codec: Native, path: &Path) -> Vec<(String, Vec<u8>)> {
		let file = File::open(path).unwrap();
		let source: Box<dyn Read> = match codec {
			Native::TarGz => Box::new(flate2::read::GzDecoder::new(file)),
			_ => Box::new(file),
		};

		let mut archive = tar::Archive::new(source);
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

	fn zip_entries(path: &Path) -> Vec<(String, Vec<u8>)> {
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

	#[test]
	fn a_zip_becomes_a_tarball_with_the_same_entries() {
		let (dir, tree) = tree();
		let input = zip_of(&tree);
		let output = dir.path().join("tree.tar.gz");

		let report = repack(Native::Zip, Native::TarGz, &input, &output, &cost()).expect("repacks");

		let entries = entries(Native::TarGz, &output);
		let names: Vec<&str> = entries.iter().map(|(name, _)| name.as_str()).collect();

		assert!(names.contains(&"tree/one.txt"), "{names:?}");
		assert!(names.contains(&"tree/sub/two.bin"), "{names:?}");
		assert!(names.contains(&"tree/.hidden"), "{names:?}");

		let one = entries.iter().find(|(name, _)| name == "tree/one.txt").unwrap();
		assert_eq!(one.1, b"first");
		assert_eq!(report.bytes, fs::metadata(&output).unwrap().len());
	}

	#[test]
	fn a_tarball_becomes_a_zip_with_the_same_entries() {
		let (dir, _tree) = tree();
		let tarball = dir.path().join("tree.tar");
		tar_of_tree(&tarball);

		let output = dir.path().join("tree.zip");
		repack(Native::Tar, Native::Zip, &tarball, &output, &cost()).expect("repacks");

		let entries = zip_entries(&output);
		let names: Vec<&str> = entries.iter().map(|(name, _)| name.as_str()).collect();
		assert!(names.contains(&"tree/one.txt"), "{names:?}");

		let binary = entries.iter().find(|(name, _)| name == "tree/sub/two.bin").unwrap();
		assert_eq!(binary.1, [0u8, 1, 2, 3]);
	}

	/// A tar of the same entries, so a repack test has a source that is not a zip.
	fn tar_of_tree(target: &Path) {
		let mut builder = tar::Builder::new(File::create(target).unwrap());

		for (name, bytes) in [
			("tree/one.txt", b"first".as_slice()),
			("tree/sub/two.bin", &[0u8, 1, 2, 3]),
			("tree/.hidden", b"third"),
		] {
			let mut header = tar::Header::new_gnu();
			header.set_entry_type(tar::EntryType::Regular);
			header.set_size(bytes.len() as u64);
			header.set_mode(0o644);
			header.set_cksum();
			builder.append_data(&mut header, name, bytes).unwrap();
		}

		builder.finish().unwrap();
	}

	#[test]
	fn a_name_that_would_escape_is_refused() {
		let (dir, _tree) = tree();
		let input = dir.path().join("evil.zip");

		let mut writer = zip::ZipWriter::new(File::create(&input).unwrap());
		writer.start_file("../outside.txt", zip_options(0o644)).unwrap();
		writer.write_all(b"nope").unwrap();
		writer.finish().unwrap();

		let output = dir.path().join("evil.tar");
		let error = repack(Native::Zip, Native::Tar, &input, &output, &cost()).unwrap_err();

		assert_eq!(error.code(), Code::Unsupported);
		assert!(error.to_string().contains(".."), "{error}");
		assert!(!output.exists(), "nothing was written");
	}

	#[test]
	fn a_symlink_in_a_tar_is_refused() {
		let (dir, _tree) = tree();
		let input = dir.path().join("link.tar");

		let mut builder = tar::Builder::new(File::create(&input).unwrap());
		let mut header = tar::Header::new_gnu();
		header.set_entry_type(tar::EntryType::Symlink);
		header.set_size(0);
		header.set_mode(0o777);
		header.set_cksum();
		builder.append_link(&mut header, "tree/link", "/etc/passwd").unwrap();
		builder.finish().unwrap();

		let output = dir.path().join("link.zip");
		let error = repack(Native::Tar, Native::Zip, &input, &output, &cost()).unwrap_err();

		assert_eq!(error.code(), Code::Unsupported);
		assert!(error.to_string().contains("symbolic link"), "{error}");
	}

	#[test]
	fn a_directory_is_not_an_archive_to_repack() {
		let (dir, tree) = tree();
		let output = dir.path().join("tree.zip");

		let error = repack(Native::Zip, Native::Zip, &tree, &output, &cost()).unwrap_err();

		assert_eq!(error.code(), Code::IoFailed);
	}

	#[test]
	fn a_cancelled_repack_writes_nothing() {
		let (dir, tree) = tree();
		let input = zip_of(&tree);
		let output = dir.path().join("cancelled.tar.gz");

		let cancellation = Cancellation::default();
		cancellation.cancel();

		let error = repack(Native::Zip, Native::TarGz, &input, &output, &cancellation).unwrap_err();

		assert_eq!(error.code(), Code::JobCancelled);
		assert!(!output.exists(), "the staged file is discarded");
	}

	#[test]
	fn a_name_from_windows_is_written_with_forward_slashes() {
		assert_eq!(archive_name("tree\\sub\\two.bin").unwrap(), "tree/sub/two.bin");
	}

	#[test]
	fn an_absolute_or_traversing_name_is_refused() {
		for name in ["/etc/passwd", "tree/../../etc/passwd", ""] {
			assert!(archive_name(name).is_err(), "{name} was accepted");
		}

		assert!(archive_name("tree/one.txt").is_ok());
	}
}
