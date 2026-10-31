//! Archives are written by this build rather than by a tool, `docs/tools.md` §7.

use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::Command;

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

fn photos(dir: &Path) -> std::path::PathBuf {
	let photos = dir.join("Photos");
	fs::create_dir_all(photos.join("sub")).unwrap();
	fs::write(photos.join("one.txt"), b"first").unwrap();
	fs::write(photos.join("sub/two.txt"), b"second").unwrap();
	photos
}

#[test]
fn a_directory_becomes_a_zip_that_holds_what_it_contained() {
	let dir = tempfile::tempdir().unwrap();
	let photos = photos(dir.path());
	let state = dir.path().join("state");

	let ran = retype()
		.args(["convert"])
		.arg(&photos)
		.arg("zip")
		.arg("--state-dir")
		.arg(&state)
		.output()
		.unwrap();

	assert!(ran.status.success(), "{}", String::from_utf8_lossy(&ran.stderr));

	let archive = dir.path().join("Photos.zip");
	let mut zip = zip::ZipArchive::new(fs::File::open(&archive).unwrap()).unwrap();

	let mut one = String::new();
	zip.by_name("Photos/one.txt").unwrap().read_to_string(&mut one).unwrap();
	assert_eq!(one, "first");
	assert!(zip.by_name("Photos/sub/two.txt").is_ok());
}

#[test]
fn the_same_directory_becomes_a_tarball_by_its_extension() {
	let dir = tempfile::tempdir().unwrap();
	let photos = photos(dir.path());
	let state = dir.path().join("state");

	for (intent, magic) in [("tar", b"Photos/one".as_slice()), ("tar.gz", &[0x1f, 0x8b])] {
		let ran = retype()
			.args(["convert"])
			.arg(&photos)
			.arg(intent)
			.arg("--state-dir")
			.arg(&state)
			.output()
			.unwrap();

		assert!(
			ran.status.success(),
			"{intent}: {}",
			String::from_utf8_lossy(&ran.stderr)
		);

		let written = fs::read(dir.path().join(format!("Photos.{intent}"))).unwrap();

		if intent == "tar.gz" {
			assert_eq!(&written[..2], magic, "a gzip magic number");
			continue;
		}

		assert!(
			written.windows(magic.len()).any(|window| window == magic),
			"the tar holds the tree"
		);
	}
}

#[test]
fn an_archive_has_nothing_to_run_and_says_so() {
	let dir = tempfile::tempdir().unwrap();
	let photos = photos(dir.path());
	let state = dir.path().join("state");

	let explained = retype()
		.args(["convert"])
		.arg(&photos)
		.arg("zip")
		.arg("--dry-run")
		.arg("--state-dir")
		.arg(&state)
		.output()
		.unwrap();

	assert!(
		explained.status.success(),
		"{}",
		String::from_utf8_lossy(&explained.stderr)
	);

	let printed = String::from_utf8_lossy(&explained.stdout);
	assert!(printed.contains("nothing to run"), "{printed}");
	assert!(printed.contains("exact"), "{printed}");
	assert!(!dir.path().join("Photos.zip").exists(), "a dry run writes nothing");
}
