//! The test command helper must not wait on pipes or leave a timed-out child running.
#![cfg(unix)]

#[path = "support/commands.rs"]
mod commands;

use std::fs;
use std::process::Command;
use std::time::Duration;

use commands::bounded;

#[test]
fn a_command_can_write_more_than_a_pipe_buffer_and_preserve_its_status() {
	let mut command = Command::new("sh");
	command.args([
		"-c",
		"i=0; while [ \"$i\" -lt 10000 ]; do printf 1234567890; i=$((i + 1)); done; echo diagnostic >&2; exit 7",
	]);
	let output = bounded(&mut command, Duration::from_secs(10)).unwrap();
	assert_eq!(output.status.code(), Some(7));
	assert_eq!(output.stdout.len(), 100_000);
	assert_eq!(output.stderr.trim(), "diagnostic");
}

#[test]
fn a_timed_out_command_reports_its_output_and_stops() {
	let dir = tempfile::tempdir().unwrap();
	let marker = dir.path().join("pid");
	let mut command = Command::new("sh");
	command
		.args(["-c", "echo $$ > \"$1\"; echo probe-started >&2; sleep 5 & wait", "sh"])
		.arg(&marker);
	let error = match bounded(&mut command, Duration::from_millis(500)) {
		Ok(_) => panic!("a stalled command unexpectedly succeeded"),
		Err(error) => error,
	};
	assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
	assert!(error.to_string().contains("probe-started"), "{error}");
	assert!(error.to_string().contains("cleanup: Ok"), "{error}");
	let pid = fs::read_to_string(marker).unwrap().trim().parse::<i32>().unwrap();
	assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "the test command is still running");
}
