//! `retype convert` end to end, with a copy tool instead of a converter so the
//! test does not depend on one being installed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

/// A configuration whose only backend copies the input to the output.
fn copy_config(dir: &Path) -> PathBuf {
	let (command, layout) = match cfg!(windows) {
		true => ("cmd", "layout = ['/c', 'copy', '{input}', '{ops}', '{output}']"),
		false => ("cp", "layout = ['{input}', '{ops}', '{output}']"),
	};

	let config = dir.join("retype.toml");
	fs::write(
		&config,
		format!(
			"\
version = 'retype-config-1'

[backends]
prefer = ['copy']

[tools.copy]
command = '{command}'
{layout}

[formats.png]
family = 'image'
lossy = false
alpha = true
tier = 'exact'

[formats.webp]
family = 'image'
lossy = true
alpha = true
tier = 'converted'
"
		),
	)
	.unwrap();

	config
}

struct Fixture {
	dir: tempfile::TempDir,
	config: PathBuf,
	state: PathBuf,
}

impl Fixture {
	fn new() -> Self {
		let dir = tempfile::tempdir().unwrap();
		let config = copy_config(dir.path());
		let state = dir.path().join("state");

		Self { dir, config, state }
	}

	fn input(&self) -> PathBuf {
		let path = self.dir.path().join("photo.png");
		fs::write(&path, b"pixels").unwrap();
		path
	}

	fn convert(&self, args: &[&str]) -> std::process::Output {
		retype()
			.args(args)
			.arg("--state-dir")
			.arg(&self.state)
			.env("RETYPE_CONFIG", &self.config)
			.output()
			.unwrap()
	}
}

#[test]
fn converts_and_writes_beside_the_input() {
	let fixture = Fixture::new();
	let input = fixture.input();

	let output = fixture.convert(&["convert", input.to_str().unwrap(), "webp"]);

	assert_eq!(
		output.status.code(),
		Some(0),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	assert_eq!(fs::read(fixture.dir.path().join("photo.webp")).unwrap(), b"pixels");
	assert!(String::from_utf8_lossy(&output.stdout).contains("photo.webp"));
}

#[test]
fn the_journal_records_the_job() {
	let fixture = Fixture::new();
	let input = fixture.input();

	fixture.convert(&["convert", input.to_str().unwrap(), "webp"]);

	let journal = fs::read_to_string(fixture.state.join("jobs.jsonl")).expect("the journal exists");
	assert!(journal.contains("\"retype-journal-1\""), "{journal}");
	assert!(journal.contains("\"state\":\"done\""), "{journal}");
	assert!(journal.contains("photo.webp"), "{journal}");
}

#[test]
fn an_existing_output_is_refused_without_force() {
	let fixture = Fixture::new();
	let input = fixture.input();
	let target = fixture.dir.path().join("photo.webp");
	fs::write(&target, b"older").unwrap();

	let output = fixture.convert(&["convert", input.to_str().unwrap(), "webp"]);

	assert_eq!(output.status.code(), Some(5));
	assert_eq!(fs::read(&target).unwrap(), b"older", "the existing file is untouched");

	let forced = fixture.convert(&["convert", input.to_str().unwrap(), "webp", "--force"]);
	assert_eq!(forced.status.code(), Some(0));
	assert_eq!(fs::read(&target).unwrap(), b"pixels");
}

#[test]
fn dry_run_prints_the_plan_and_writes_nothing() {
	let fixture = Fixture::new();
	let input = fixture.input();

	let output = fixture.convert(&["convert", input.to_str().unwrap(), "webp", "--dry-run"]);
	let stdout = String::from_utf8_lossy(&output.stdout);

	assert_eq!(output.status.code(), Some(0));
	assert!(stdout.contains("photo.webp"), "{stdout}");
	assert!(stdout.contains("backend"), "{stdout}");
	assert!(!fixture.dir.path().join("photo.webp").exists());
	assert!(!fixture.state.join("jobs.jsonl").exists(), "a dry run journals nothing");
}

#[test]
fn an_output_path_needs_one_output() {
	let fixture = Fixture::new();
	let input = fixture.input();

	let output = fixture.convert(&["convert", input.to_str().unwrap(), "webp,png", "-o", "one.webp"]);

	assert_eq!(output.status.code(), Some(3));
	assert!(String::from_utf8_lossy(&output.stderr).contains("E_USAGE_OUTPUT_COUNT"));
}

#[test]
fn a_name_with_no_declared_suffix_is_refused() {
	let fixture = Fixture::new();
	let input = fixture.dir.path().join("notes.txt");
	fs::write(&input, b"words").unwrap();

	let output = fixture.convert(&["convert", input.to_str().unwrap(), "webp"]);

	assert_eq!(output.status.code(), Some(3));
	assert!(String::from_utf8_lossy(&output.stderr).contains("E_FORMAT_UNKNOWN"));
}
