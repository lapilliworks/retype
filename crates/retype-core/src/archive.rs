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

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

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
///
/// `tool` is the executable that carries a codec this build does not write, and it is
/// `Some` exactly when one side of the repack is `7z`, `docs/tools.md` §7.
pub fn repack(
	tool: Option<&str>,
	from: Native,
	to: Native,
	input: &Path,
	output: &Path,
	cancellation: &Cancellation,
) -> Result<Report> {
	// Absolute, because a repack through a tool changes the child's working directory:
	// a relative path would then mean something else to the tool than it did here.
	let input = &std::path::absolute(input)?;
	let output_path = std::path::absolute(output)?;
	let output = output_path.as_path();

	let staged = StagedOutput::new(output)?;

	let stderr = match tool {
		Some(tool) if from.needs_tool() || to.needs_tool() => {
			through(tool, from, to, input, staged.temp(), cancellation)?
		}
		_ => {
			let mut source = Source::open(from, input)?;
			let mut sink = Sink::open(to, staged.temp())?;

			let mut budget = MAX_BYTES;
			source.each(&mut sink, cancellation, &mut budget)?;
			sink.commit()?;
			String::new()
		}
	};

	// The staged file is the archive, so its length is the answer; committing first
	// would only move it.
	let bytes = fs::metadata(staged.temp())?.len();
	staged.commit()?;

	Ok(Report {
		output: output.to_path_buf(),
		bytes,
		stderr,
	})
}

/// Repack a container that needs a tool, `docs/tools.md` §7.
///
/// The rules are the same ones the native path applies, and they are applied here
/// rather than by the tool: the container's bytes are 7-Zip's, the decisions are ours.
/// A tree is the only shape both sides understand, so a repack through a tool
/// materialises into a staged directory and then out of it, and the directory never
/// outlives the job.
fn through(
	tool: &str,
	from: Native,
	to: Native,
	input: &Path,
	target: &Path,
	cancellation: &Cancellation,
) -> Result<String> {
	let staged = StagedTree::new(target)?;
	let tree = staged.path();
	let mut notes = String::new();

	// In: the source as a tree.
	match from {
		Native::SevenZip => notes = extract(tool, input, tree, cancellation)?,
		other => {
			let mut source = Source::open(other, input)?;
			let mut sink = Sink::Tree(TreeSink::new(tree)?);

			let mut budget = MAX_BYTES;
			source.each(&mut sink, cancellation, &mut budget)?;
			sink.commit()?;
		}
	}

	// Out: the tree as the target.
	match to {
		Native::SevenZip => notes = create(tool, tree, target, cancellation)?,
		other => {
			let mut source = Source::tree(tree)?;
			let mut sink = Sink::open(other, target)?;

			let mut budget = MAX_BYTES;
			source.each(&mut sink, cancellation, &mut budget)?;
			sink.commit()?;
		}
	}

	Ok(notes)
}

/// The source archive, opened for reading.
enum Source {
	Zip(zip::ZipArchive<File>),
	Tar(Box<tar::Archive<TarSource>>),
	/// A checked directory produced by a tool, listed in the order it will be written.
	Tree {
		entries: Vec<TreeEntry>,
		root: PathBuf,
		at: usize,
	},
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
			// A codec with no reader here is always on the other side of a tool, and
			// `through` is what handles it, `docs/tools.md` §7.
			Native::SevenZip => Err(Error::Diagnostic(Diagnostic::global(
				Code::InternalInvariant,
				"a 7z source reached the native reader",
			))),
		}
	}

	/// Open a tree that a tool wrote, ready to be written into another container.
	fn tree(root: &Path) -> Result<Self> {
		let mut entries = Vec::new();
		walk(root, root, &mut entries)?;

		// The order is the tree's own, sorted by name, so repacking the same archive
		// twice produces the same bytes.
		entries.sort_by(|left, right| left.name.cmp(&right.name));

		Ok(Self::Tree {
			entries,
			root: root.to_path_buf(),
			at: 0,
		})
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
			Self::Tree { entries, root, at } => {
				while *at < entries.len() {
					stop_if_cancelled(cancellation)?;

					let entry = entries[*at].clone();
					*at += 1;

					if entry.directory {
						sink.directory(&entry.name, entry.mode)?;
						continue;
					}

					spend(&entry.name, entry.size, budget)?;

					let mut file = File::open(root.join(&entry.name))
						.map_err(|error| unreadable(&root.join(&entry.name), &error))?;

					sink.file(&entry.name, entry.mode, entry.size, &mut file)?;
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
	/// Write the entries into a directory a tool can archive from.
	Tree(TreeSink),
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
			Native::SevenZip => Err(Error::Diagnostic(Diagnostic::global(
				Code::InternalInvariant,
				"a 7z target reached the native writer",
			))),
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
			Self::Tree(tree) => tree.directory(&name, mode),
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
			Self::Tree(tree) => tree.file(&name, mode, size, bytes),
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
			Self::Tree(_) => {}
		}

		Ok(())
	}
}

/// One checked entry of a tree a tool wrote or will read.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TreeEntry {
	/// Relative, with `/` separators, and already through `archive_name`.
	name: String,
	directory: bool,
	mode: u32,
	size: u64,
}

/// Write archive entries into a directory, keeping their names and modes.
struct TreeSink {
	root: PathBuf,
}

impl TreeSink {
	fn new(root: &Path) -> Result<Self> {
		fs::create_dir_all(root)?;
		Ok(Self {
			root: root.to_path_buf(),
		})
	}

	fn directory(&self, name: &str, mode: u32) -> Result<()> {
		let path = self.root.join(name);
		fs::create_dir_all(&path)?;
		set_mode(&path, mode);
		Ok(())
	}

	fn file(&self, name: &str, mode: u32, size: u64, bytes: &mut dyn Read) -> Result<()> {
		let path = self.root.join(name);

		if let Some(parent) = path.parent() {
			fs::create_dir_all(parent)?;
		}

		let mut file = File::create(&path)?;
		let copied = std::io::copy(&mut bytes.take(size), &mut file).map_err(|error| unwritable(&error))?;

		if copied != size {
			return Err(short(name, size, copied));
		}

		set_mode(&path, mode);
		Ok(())
	}
}

/// Collect every entry of a tree, refusing what the native path refuses.
fn walk(root: &Path, at: &Path, entries: &mut Vec<TreeEntry>) -> Result<()> {
	let mut children: Vec<PathBuf> = fs::read_dir(at)?
		.map(|entry| entry.map(|entry| entry.path()))
		.collect::<std::io::Result<Vec<_>>>()?;

	children.sort();

	for path in children {
		let relative = path.strip_prefix(root).unwrap_or(&path);
		let name = relative
			.components()
			.map(|component| component.as_os_str().to_string_lossy().into_owned())
			.collect::<Vec<_>>()
			.join("/");

		let metadata = fs::symlink_metadata(&path)?;

		if metadata.file_type().is_symlink() {
			return Err(link_refused(&name));
		}

		if metadata.is_dir() {
			entries.push(TreeEntry {
				name,
				directory: true,
				mode: mode_of(&metadata),
				size: 0,
			});
			walk(root, &path, entries)?;
			continue;
		}

		if !metadata.is_file() {
			return Err(Error::Diagnostic(Diagnostic::global(
				Code::Unsupported,
				format!("'{name}' is not a file or a directory, which is all an archive carries"),
			)));
		}

		entries.push(TreeEntry {
			name,
			directory: false,
			mode: mode_of(&metadata),
			size: metadata.len(),
		});
	}

	Ok(())
}

fn mode_of(metadata: &fs::Metadata) -> u32 {
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		metadata.permissions().mode() & 0o7777
	}
	#[cfg(not(unix))]
	{
		let _ = metadata;
		0o644
	}
}

fn set_mode(path: &Path, mode: u32) {
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777));
	}
	#[cfg(not(unix))]
	{
		let _ = (path, mode);
	}
}

/// A directory staged beside the output for a repack that needs a tree.
///
/// It is removed when the job ends, whether it succeeded or not, so a failed repack
/// leaves nothing behind in the user's directory.
struct StagedTree {
	path: PathBuf,
}

impl StagedTree {
	fn new(target: &Path) -> Result<Self> {
		let parent = target.parent().ok_or_else(|| Error::NoParent(target.to_path_buf()))?;
		let name = target
			.file_name()
			.ok_or_else(|| Error::NoFileName(target.to_path_buf()))?;

		static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
		let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

		let path = parent.join(format!(
			"{}tree-{}-{unique}",
			crate::convert::TEMP_PREFIX,
			name.to_string_lossy()
		));

		// A leftover from a process that was killed must not be read as this job's tree.
		let _ = fs::remove_dir_all(&path);
		fs::create_dir_all(&path)?;

		Ok(Self { path })
	}

	fn path(&self) -> &Path {
		&self.path
	}
}

impl Drop for StagedTree {
	fn drop(&mut self) {
		let _ = fs::remove_dir_all(&self.path);
	}
}

/// List a 7z archive, and refuse it before a byte is extracted.
///
/// The listing is where the rules are applied: `docs/tools.md` §7 wants the declared
/// sizes checked against the budget before anything is copied, and wants names and
/// entry kinds judged by this build rather than by the tool that will unpack them.
fn listing(tool: &str, input: &Path, cancellation: &Cancellation) -> Result<Vec<TreeEntry>> {
	let text = seven(tool, &["l", "-slt", "-ba", "--"], Some(input), None, cancellation)?;

	parse_listing(&text)
}

/// The rules, applied to a listing rather than to a running tool.
///
/// Split out because these are the decisions a repack through a tool exists to keep:
/// names, entry kinds and the budget are judged here, and a listing is the whole input
/// they need, `docs/tools.md` §7.
fn parse_listing(text: &str) -> Result<Vec<TreeEntry>> {
	let mut entries = Vec::new();
	let mut budget = MAX_BYTES;
	let mut fields: BTreeMap<String, String> = BTreeMap::new();

	for line in text.lines().chain(std::iter::once("")) {
		match line.split_once(" = ") {
			Some((key, value)) => {
				fields.insert(key.trim().to_string(), value.trim().to_string());
			}
			None => {
				if fields.is_empty() {
					continue;
				}

				let Some(path) = fields.remove("Path") else {
					fields.clear();
					continue;
				};

				let attributes = fields.remove("Attributes").unwrap_or_default();
				let mode_text = attributes.split_whitespace().nth(1).unwrap_or_default().to_string();
				let kind = mode_text.chars().next().unwrap_or('-');

				// A directory keeps its entry; anything else that is not a regular file
				// is refused by name, because carrying it is how a repack of an archive
				// becomes a traversal for whoever unpacks it next.
				let directory = kind == 'd' || attributes.starts_with('D');

				if !directory && kind != '-' {
					return Err(Error::Diagnostic(Diagnostic::global(
						Code::Unsupported,
						format!(
							"'{path}' is '{}', and an archive of files and directories is all this carries",
							mode_text
						),
					)));
				}

				let name = archive_name(&path)?;
				let size = fields
					.remove("Size")
					.and_then(|size| size.parse::<u64>().ok())
					.unwrap_or(0);

				if !directory {
					spend(&name, size, &mut budget)?;
				}

				entries.push(TreeEntry {
					name,
					directory,
					mode: octal_mode(&mode_text),
					size,
				});

				fields.clear();
			}
		}
	}

	Ok(entries)
}

/// `-rwxr-xr-x` as a number, which is what a container and a file both want.
fn octal_mode(text: &str) -> u32 {
	let mut mode = 0;

	for character in text.chars().skip(1).take(9) {
		mode <<= 1;

		if character != '-' {
			mode |= 1;
		}
	}

	// rwx rwx rwx is the low nine bits; the setuid and sticky bits are not carried by
	// this listing, so a setuid file comes back as an ordinary executable.
	mode & 0o777
}

/// Unpack a 7z archive into `tree`, after its listing has been refused or accepted.
fn extract(tool: &str, input: &Path, tree: &Path, cancellation: &Cancellation) -> Result<String> {
	// The listing is the check; the extraction follows it, so an archive that fails a
	// rule is refused before it writes anything.
	let _ = listing(tool, input, cancellation)?;

	let destination = format!("-o{}", tree.display());

	seven(
		tool,
		&["x", "-bd", "-bso0", "-bsp0", "-y", &destination, "--"],
		Some(input),
		None,
		cancellation,
	)
}

/// Create a 7z archive from a tree that has already been checked.
fn create(tool: &str, tree: &Path, target: &Path, cancellation: &Cancellation) -> Result<String> {
	let name = target.to_string_lossy().into_owned();

	// The tool expands the wildcard itself, so no shell is involved and no argument
	// list grows with the number of entries.
	seven(
		tool,
		&["a", "-t7z", "-bd", "-bso0", "-bsp0", "-r", "--", &name, "*"],
		None,
		Some(tree),
		cancellation,
	)
}

/// Run the tool, answering with its stderr tail.
fn seven(
	tool: &str,
	args: &[&str],
	input: Option<&Path>,
	cwd: Option<&Path>,
	cancellation: &Cancellation,
) -> Result<String> {
	let mut argv: Vec<OsString> = args.iter().map(OsString::from).collect();

	if let Some(input) = input {
		argv.push(input.as_os_str().to_owned());
	}

	crate::execute::run_child(tool, Path::new(tool), &argv, cwd, None, cancellation)
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

		let report = repack(None, Native::Zip, Native::TarGz, &input, &output, &cost()).expect("repacks");

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
		repack(None, Native::Tar, Native::Zip, &tarball, &output, &cost()).expect("repacks");

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
		let error = repack(None, Native::Zip, Native::Tar, &input, &output, &cost()).unwrap_err();

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
		let error = repack(None, Native::Tar, Native::Zip, &input, &output, &cost()).unwrap_err();

		assert_eq!(error.code(), Code::Unsupported);
		assert!(error.to_string().contains("symbolic link"), "{error}");
	}

	#[test]
	fn a_directory_is_not_an_archive_to_repack() {
		let (dir, tree) = tree();
		let output = dir.path().join("tree.zip");

		let error = repack(None, Native::Zip, Native::Zip, &tree, &output, &cost()).unwrap_err();

		assert_eq!(error.code(), Code::IoFailed);
	}

	#[test]
	fn a_cancelled_repack_writes_nothing() {
		let (dir, tree) = tree();
		let input = zip_of(&tree);
		let output = dir.path().join("cancelled.tar.gz");

		let cancellation = Cancellation::default();
		cancellation.cancel();

		let error = repack(None, Native::Zip, Native::TarGz, &input, &output, &cancellation).unwrap_err();

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

#[cfg(test)]
mod seven_tests {
	use super::*;

	/// The 7-Zip console build this host has, under whichever of its names it carries.
	fn seven_zip() -> Option<&'static str> {
		let path = std::env::var_os("PATH").unwrap_or_default();

		["7zz", "7z", "7za"]
			.into_iter()
			.find(|name| crate::tools::find_in_path(&path, name).is_some())
	}

	#[test]
	fn zip_into_seven_zip_through_the_tool() {
		let Some(tool) = seven_zip() else {
			eprintln!("no 7z on this machine, so this test proves nothing");
			return;
		};

		let dir = std::env::temp_dir().join(format!("retype-7z-{}", std::process::id()));
		let _ = fs::remove_dir_all(&dir);
		fs::create_dir_all(&dir).unwrap();

		let zip_path = dir.join("in.zip");
		{
			let file = File::create(&zip_path).unwrap();
			let mut writer = zip::ZipWriter::new(file);
			writer
				.start_file("a.txt", zip::write::SimpleFileOptions::default())
				.unwrap();
			writer.write_all(b"hello").unwrap();
			writer
				.add_directory("sub", zip::write::SimpleFileOptions::default())
				.unwrap();
			writer
				.start_file("sub/b.bin", zip::write::SimpleFileOptions::default())
				.unwrap();
			writer.write_all(b"x").unwrap();
			writer.finish().unwrap();
		}

		let out = dir.join("out.7z");
		let cancellation = Cancellation::default();

		let report = repack(
			Some(tool),
			Native::Zip,
			Native::SevenZip,
			&zip_path,
			&out,
			&cancellation,
		);

		match report {
			Ok(report) => {
				assert!(report.bytes > 0);
				assert!(out.exists());
			}
			Err(error) => panic!("{error:?}"),
		}

		let _ = fs::remove_dir_all(&dir);
	}
}

#[cfg(test)]
mod seven_rule_tests {
	use super::*;

	fn entry(path: &str, attributes: &str, size: u64) -> String {
		format!("Path = {path}\nSize = {size}\nAttributes = {attributes}\n\n")
	}

	#[test]
	fn a_listing_is_read_into_entries_with_their_modes() {
		let text = entry("a.txt", "A -rwxr-xr-x", 6) + &entry("sub", "D drwxr-xr-x", 0);

		let entries = parse_listing(&text).expect("a listing");

		assert_eq!(entries.len(), 2);
		assert_eq!(entries[0].name, "a.txt");
		assert_eq!(entries[0].mode, 0o755);
		assert_eq!(entries[0].size, 6);
		assert!(entries[1].directory);
		assert_eq!(entries[1].mode, 0o755);
	}

	#[test]
	fn a_name_that_would_escape_is_refused_from_a_listing() {
		let error = parse_listing(&entry("../evil.txt", "A -rw-r--r--", 6)).unwrap_err();

		assert_eq!(error.code(), Code::Unsupported);
	}

	#[test]
	fn an_absolute_name_is_refused_from_a_listing() {
		let error = parse_listing(&entry("/etc/passwd", "A -rw-r--r--", 6)).unwrap_err();

		assert_eq!(error.code(), Code::Unsupported);
	}

	#[test]
	fn a_symbolic_link_in_a_listing_is_refused_by_name() {
		let error = parse_listing(&entry("link", "A lrwxrwxrwx", 5)).unwrap_err();

		assert_eq!(error.code(), Code::Unsupported);
		assert!(format!("{error}").contains("link"), "{error}");
	}

	#[test]
	fn a_listing_that_expands_beyond_the_budget_is_refused_before_anything_is_copied() {
		let text = entry("big.bin", "A -rw-r--r--", MAX_BYTES) + &entry("more.bin", "A -rw-r--r--", 1);

		let error = parse_listing(&text).unwrap_err();

		assert_eq!(error.code(), Code::Unsupported);
		assert!(format!("{error}").contains("limit"), "{error}");
	}

	#[test]
	fn a_device_entry_is_refused() {
		let error = parse_listing(&entry("dev/null", "A crw-rw-rw-", 0)).unwrap_err();

		assert_eq!(error.code(), Code::Unsupported);
	}
}
