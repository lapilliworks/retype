//! `retype jobs logs`: the full output in a file, and the tail in the record for
//! when the file is gone.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

/// The tool's command and layout: a shell script on Unix, and on Windows a batch
/// file run through `cmd`, because Windows cannot start a script directly.
#[cfg(not(windows))]
fn failing_tool(dir: &Path) -> (String, String) {
	use std::os::unix::fs::PermissionsExt;

	let script = dir.join("failing.sh");
	fs::write(&script, "#!/bin/sh\necho 'cannot read the input' >&2\nexit 3\n").unwrap();

	let mut permissions = fs::metadata(&script).unwrap().permissions();
	permissions.set_mode(0o755);
	fs::set_permissions(&script, permissions).unwrap();

	(
		script.display().to_string(),
		"layout = ['{input}', '{ops}', '{output}']".to_string(),
	)
}

#[cfg(windows)]
fn failing_tool(dir: &Path) -> (String, String) {
	let script = dir.join("failing.cmd");
	fs::write(&script, "@echo off\r\necho cannot read the input 1>&2\r\nexit /b 3\r\n").unwrap();

	(
		"cmd".to_string(),
		format!(
			"layout = ['/c', '{}', '{{input}}', '{{ops}}', '{{output}}']",
			script.display()
		),
	)
}

/// A tool that says something and fails, which is the case a log exists for.
fn failing_config(dir: &Path) -> PathBuf {
	let (command, layout) = failing_tool(dir);

	let config = dir.join("retype.toml");
	fs::write(
		&config,
		format!(
			"\
version = 'retype-config-1'

[backends]
prefer = ['failing']

[tools.failing]
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

impl Drop for Fixture {
	fn drop(&mut self) {
		let _ = retype()
			.args(["daemon", "stop"])
			.arg("--state-dir")
			.arg(&self.state)
			.env("RETYPE_CONFIG", &self.config)
			.output();
	}
}

impl Fixture {
	fn new() -> Self {
		let dir = tempfile::tempdir().unwrap();
		let config = failing_config(dir.path());
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

	/// Convert, which fails, and answer with the job's id.
	fn failed(&self) -> String {
		let input = self.dir.path().join("photo.png");
		fs::write(&input, b"pixels").unwrap();

		let converted = self.run(&["convert", input.to_str().unwrap(), "webp"]);
		assert_eq!(
			converted.status.code(),
			Some(4),
			"{}",
			String::from_utf8_lossy(&converted.stderr)
		);

		// The listing prints the shortest handle; the log file is named by the full id.
		let listed = self.run(&["jobs", "list", "--json"]);
		String::from_utf8_lossy(&listed.stdout)
			.lines()
			.filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
			.find_map(|job| job["id"].as_str().map(str::to_string))
			.expect("a job id")
	}
}

#[test]
fn the_log_file_holds_what_the_tool_said() {
	let fixture = Fixture::new();
	let id = fixture.failed();

	let log = fixture.state.join("logs").join(format!("{id}.log"));
	assert!(log.exists(), "a job writes its tool output to {}", log.display());
	assert!(fs::read_to_string(&log).unwrap().contains("cannot read the input"));

	let printed = fixture.run(&["jobs", "logs", &id]);
	let stdout = String::from_utf8_lossy(&printed.stdout);

	assert_eq!(
		printed.status.code(),
		Some(0),
		"{}",
		String::from_utf8_lossy(&printed.stderr)
	);
	assert!(stdout.contains("cannot read the input"), "{stdout}");
}

#[test]
fn the_record_still_answers_when_the_log_is_gone() {
	let fixture = Fixture::new();
	let id = fixture.failed();

	fs::remove_file(fixture.state.join("logs").join(format!("{id}.log"))).unwrap();

	let printed = fixture.run(&["jobs", "logs", &id]);
	let stderr = String::from_utf8_lossy(&printed.stderr);

	assert_eq!(printed.status.code(), Some(0));
	assert!(stderr.contains("cannot read the input"), "{stderr}");
}
