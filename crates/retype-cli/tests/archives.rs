//! Archives convert into archives, `docs/tools.md` §7.
//!
//! The gesture is `notes.zip` to `notes.tar.gz`: the entries keep their bytes and
//! only the container changes. Nothing here extracts to a directory, because that is
//! not a conversion.

use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

/// A zip holding two files and a directory, written here so the test does not depend
/// on a converter or a shell tool.
fn zip(dir: &Path) -> std::path::PathBuf {
	let path = dir.join("notes.zip");
	let mut writer = zip::ZipWriter::new(fs::File::create(&path).unwrap());

	for name in ["notes/", "notes/one.txt", "notes/sub/two.txt"] {
		let options = zip::write::SimpleFileOptions::default().unix_permissions(0o644);

		if name.ends_with('/') {
			writer.add_directory(name, options).unwrap();
			continue;
		}

		writer.start_file(name, options).unwrap();
		writer.write_all(name.as_bytes()).unwrap();
	}

	writer.finish().unwrap();
	path
}

/// The daemon a `convert` causes, stopped when the test ends: an autostarted daemon
/// outlives its client, and a leaked one keeps a watcher and a timer alive for every
/// later run.
struct Autostarted {
	state: std::path::PathBuf,
}

impl Drop for Autostarted {
	fn drop(&mut self) {
		let _ = retype()
			.args(["daemon", "stop", "--force"])
			.arg("--state-dir")
			.arg(&self.state)
			.output();
	}
}

fn convert(input: &Path, intent: &str, state: &Path) -> std::process::Output {
	retype()
		.args(["convert"])
		.arg(input)
		.arg(intent)
		.arg("--state-dir")
		.arg(state)
		.output()
		.unwrap()
}

#[test]
fn a_zip_becomes_a_tarball_with_its_entries_intact() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let _daemon = Autostarted { state: state.clone() };
	let input = zip(dir.path());

	let ran = convert(&input, "tar.gz", &state);
	assert!(ran.status.success(), "{}", String::from_utf8_lossy(&ran.stderr));

	let output = dir.path().join("notes.tar.gz");
	let bytes = fs::read(&output).unwrap();
	assert_eq!(&bytes[..2], &[0x1f, 0x8b], "a gzip stream");

	let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice()));
	let mut names = Vec::new();

	for entry in archive.entries().unwrap() {
		let mut entry = entry.unwrap();
		let mut held = String::new();
		entry.read_to_string(&mut held).unwrap();
		names.push(entry.path().unwrap().display().to_string());

		if entry.path().unwrap().display().to_string() == "notes/one.txt" {
			assert_eq!(held, "notes/one.txt", "the bytes are the source's bytes");
		}
	}

	assert!(names.contains(&"notes/one.txt".to_string()), "{names:?}");
	assert!(names.contains(&"notes/sub/two.txt".to_string()), "{names:?}");
}

#[test]
fn a_tarball_becomes_a_zip() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let _daemon = Autostarted { state: state.clone() };

	// A tar of a directory, built here rather than by a tool.
	let tarball = dir.path().join("notes.tar");
	{
		let mut builder = tar::Builder::new(fs::File::create(&tarball).unwrap());
		let mut header = tar::Header::new_gnu();
		header.set_entry_type(tar::EntryType::Regular);
		header.set_size(5);
		header.set_mode(0o644);
		header.set_cksum();
		builder
			.append_data(&mut header, "notes/one.txt", b"first".as_slice())
			.unwrap();
		builder.finish().unwrap();
	}

	let ran = convert(&tarball, "zip", &state);
	assert!(ran.status.success(), "{}", String::from_utf8_lossy(&ran.stderr));

	let mut zip = zip::ZipArchive::new(fs::File::open(dir.path().join("notes.zip")).unwrap()).unwrap();
	let mut held = String::new();
	zip.by_name("notes/one.txt").unwrap().read_to_string(&mut held).unwrap();
	assert_eq!(held, "first");
}

#[test]
fn a_directory_is_not_an_archive_and_says_so() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let _daemon = Autostarted { state: state.clone() };
	let tree = dir.path().join("Photos");
	fs::create_dir(&tree).unwrap();
	fs::write(tree.join("one.txt"), b"first").unwrap();

	let ran = convert(&tree, "zip", &state);

	assert_eq!(ran.status.code(), Some(3), "a diagnosis, not a crash");
	assert!(
		String::from_utf8_lossy(&ran.stderr).contains("not an archive"),
		"{}",
		String::from_utf8_lossy(&ran.stderr)
	);
}

#[test]
fn repacking_an_archive_into_its_own_format_is_a_no_op() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let _daemon = Autostarted { state: state.clone() };
	let input = zip(dir.path());

	let ran = convert(&input, "zip", &state);

	assert_eq!(ran.status.code(), Some(3));
	assert!(
		String::from_utf8_lossy(&ran.stderr).contains("E_NOOP"),
		"{}",
		String::from_utf8_lossy(&ran.stderr)
	);
}
