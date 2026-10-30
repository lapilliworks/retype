//! `retype why`: the record follows the file through a rename, and a file no job
//! produced is answered rather than guessed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

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

	fn run(&self, args: &[&str]) -> std::process::Output {
		retype()
			.args(args)
			.arg("--state-dir")
			.arg(&self.state)
			.env("RETYPE_CONFIG", &self.config)
			.output()
			.unwrap()
	}

	/// Convert `photo.png` to `photo.webp` through the daemon.
	fn converted(&self) -> PathBuf {
		let input = self.dir.path().join("photo.png");
		fs::write(&input, b"pixels").unwrap();

		let output = self.run(&["convert", input.to_str().unwrap(), "webp"]);
		assert_eq!(
			output.status.code(),
			Some(0),
			"{}",
			String::from_utf8_lossy(&output.stderr)
		);

		self.dir.path().join("photo.webp")
	}
}

#[test]
fn the_record_names_the_intent_the_backend_and_the_store() {
	let fixture = Fixture::new();
	let produced = fixture.converted();

	let asked = fixture.run(&["why", produced.to_str().unwrap()]);
	let stdout = String::from_utf8_lossy(&asked.stdout);

	assert_eq!(
		asked.status.code(),
		Some(0),
		"{}",
		String::from_utf8_lossy(&asked.stderr)
	);
	assert!(stdout.contains("journal-identity"), "{stdout}");
	assert!(stdout.contains("exact"), "{stdout}");
	assert!(stdout.contains("intent     webp"), "{stdout}");
	assert!(stdout.contains("backend    copy"), "{stdout}");
}

#[test]
fn the_answer_is_json_with_a_stable_shape() {
	let fixture = Fixture::new();
	let produced = fixture.converted();

	let asked = fixture.run(&["why", produced.to_str().unwrap(), "--json"]);
	let answer: serde_json::Value = serde_json::from_slice(&asked.stdout).expect("an object");

	assert_eq!(answer["store"], "journal-identity");
	assert_eq!(answer["confidence"], "exact");
	assert_eq!(answer["intent"], "webp");
	assert_eq!(answer["backend"], "copy");
	assert_eq!(answer["tier"], "converted");
	assert!(answer["job"].is_string(), "{answer}");
	assert!(answer["created"].is_i64(), "{answer}");
}

#[test]
fn renaming_the_file_keeps_the_record_pointing_at_it() {
	let fixture = Fixture::new();
	let produced = fixture.converted();
	let renamed = fixture.dir.path().join("holiday.webp");
	fs::rename(&produced, &renamed).unwrap();

	let asked = fixture.run(&["why", renamed.to_str().unwrap(), "--json"]);
	let answer: serde_json::Value = serde_json::from_slice(&asked.stdout).expect("an object");

	assert_eq!(answer["confidence"], "exact", "{answer}");
	assert_eq!(answer["store"], "journal-identity");
}

#[test]
fn a_file_no_job_produced_is_said_so() {
	let fixture = Fixture::new();
	fixture.converted();

	let stranger = fixture.dir.path().join("stranger.webp");
	fs::write(&stranger, b"untouched").unwrap();

	let asked = fixture.run(&["why", stranger.to_str().unwrap()]);
	let stderr = String::from_utf8_lossy(&asked.stderr);

	assert_eq!(asked.status.code(), Some(0));
	assert!(
		stderr.contains("no conversion in the journal produced this file"),
		"{stderr}"
	);
}
