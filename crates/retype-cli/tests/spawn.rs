//! The binary end to end, which is the only way to test auto-start: start a
//! daemon, ask it something, stop it.

use std::path::{Path, PathBuf};
use std::process::Command;

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

/// Stops the daemon when the test ends, including when it fails.
struct Daemon {
	state: PathBuf,
}

impl Daemon {
	fn start(state: &Path) -> Self {
		let output = retype()
			.arg("daemon")
			.arg("start")
			.arg("--state-dir")
			.arg(state)
			.arg("--json")
			.output()
			.unwrap();

		assert!(
			output.status.success(),
			"daemon start failed: {}",
			String::from_utf8_lossy(&output.stderr)
		);

		Self {
			state: state.to_path_buf(),
		}
	}
}

impl Drop for Daemon {
	fn drop(&mut self) {
		let _ = retype()
			.arg("daemon")
			.arg("stop")
			.arg("--state-dir")
			.arg(&self.state)
			.output();
	}
}

#[test]
fn the_binary_starts_a_daemon_and_stops_it() {
	let dir = tempfile::tempdir().unwrap();
	let daemon = Daemon::start(dir.path());

	let status = retype()
		.args(["daemon", "status"])
		.arg("--state-dir")
		.arg(dir.path())
		.arg("--json")
		.output()
		.unwrap();
	assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));

	let value: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
	assert!(value["pid"].as_u64().unwrap() > 0);
	assert_eq!(value["jobs"], 0);
	assert!(
		dir.path().join("endpoint").exists(),
		"the daemon published where it listens"
	);

	let stop = retype()
		.args(["daemon", "stop"])
		.arg("--state-dir")
		.arg(dir.path())
		.output()
		.unwrap();
	assert!(stop.status.success(), "{}", String::from_utf8_lossy(&stop.stderr));
	assert!(
		!dir.path().join("endpoint").exists(),
		"a stopped daemon leaves no endpoint"
	);

	drop(daemon);
}

#[test]
fn starting_twice_is_not_an_error() {
	let dir = tempfile::tempdir().unwrap();
	let daemon = Daemon::start(dir.path());

	let again = retype()
		.args(["daemon", "start"])
		.arg("--state-dir")
		.arg(dir.path())
		.output()
		.unwrap();

	assert!(again.status.success(), "{}", String::from_utf8_lossy(&again.stderr));

	drop(daemon);
}

#[test]
fn suppressed_autostart_reports_the_daemon_missing() {
	let dir = tempfile::tempdir().unwrap();

	let output = retype()
		.args(["daemon", "status"])
		.arg("--state-dir")
		.arg(dir.path())
		.env("RETYPE_NO_AUTOSTART", "1")
		.output()
		.unwrap();

	assert_eq!(output.status.code(), Some(6));
	assert!(String::from_utf8_lossy(&output.stderr).contains("not running"));
}

#[test]
fn no_spawn_is_honoured_on_the_command_line() {
	let dir = tempfile::tempdir().unwrap();

	let output = retype()
		.args(["daemon", "status", "--no-spawn"])
		.arg("--state-dir")
		.arg(dir.path())
		.output()
		.unwrap();

	assert_eq!(output.status.code(), Some(6));
}
