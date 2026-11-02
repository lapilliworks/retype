//! Job handles and state filters are identifiers a script types, so they are
//! checked before anything else happens.

use std::process::Command;

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

#[test]
fn a_handle_shorter_than_four_characters_is_a_usage_error() {
	let dir = tempfile::tempdir().unwrap();

	for command in ["get", "logs", "cancel", "resume"] {
		let output = retype()
			.args(["jobs", command, "01m"])
			.arg("--state-dir")
			.arg(dir.path())
			.arg("--no-spawn")
			.output()
			.unwrap();

		assert_eq!(output.status.code(), Some(2), "{command}: {output:?}");
		assert!(
			String::from_utf8_lossy(&output.stderr).contains("at least 4 characters"),
			"{command}: {}",
			String::from_utf8_lossy(&output.stderr)
		);
	}
}

#[test]
fn an_unknown_state_is_rejected_by_the_parser() {
	let dir = tempfile::tempdir().unwrap();

	let output = retype()
		.args(["jobs", "list", "--state", "bogus"])
		.arg("--state-dir")
		.arg(dir.path())
		.arg("--no-spawn")
		.output()
		.unwrap();

	assert_eq!(output.status.code(), Some(2), "{output:?}");
	let stderr = String::from_utf8_lossy(&output.stderr);
	assert!(stderr.contains("bogus"), "{stderr}");
	assert!(stderr.contains("needs-attention"), "{stderr}");
}

#[test]
fn a_state_filter_is_read_in_either_case() {
	let dir = tempfile::tempdir().unwrap();

	let output = retype()
		.args(["jobs", "list", "--state", "NEEDS-ATTENTION"])
		.arg("--state-dir")
		.arg(dir.path())
		.arg("--no-spawn")
		.output()
		.unwrap();

	// The value passed the parser, so the command reached the daemon and failed as
	// unavailable rather than as a usage error.
	assert_eq!(output.status.code(), Some(6), "{output:?}");
	let stderr = String::from_utf8_lossy(&output.stderr);
	assert!(!stderr.contains("invalid value"), "{stderr}");
}

#[test]
fn the_state_filter_lists_its_values() {
	let output = retype().args(["jobs", "list", "--help"]).output().unwrap();
	let help = String::from_utf8_lossy(&output.stdout);

	assert!(help.contains("needs-attention"), "{help}");
	assert!(help.contains("[possible values:"), "{help}");
}
