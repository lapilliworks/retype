use std::process::Command;

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

#[test]
fn doctor_lists_tools_and_succeeds() {
	let output = retype().arg("doctor").output().unwrap();
	let stdout = String::from_utf8_lossy(&output.stdout);

	assert!(output.status.success(), "doctor should succeed without --check");
	assert!(stdout.contains("tool"), "header missing: {stdout}");
	assert!(stdout.contains("magick"));
	assert!(stdout.contains("pdf"));
}

#[test]
fn check_fails_for_an_unknown_tool() {
	let output = retype()
		.args(["doctor", "--check", "--require", "definitely-not-installed"])
		.output()
		.unwrap();

	assert_eq!(output.status.code(), Some(4));
}

#[test]
fn check_fails_for_an_absent_tool() {
	// A tool name that exists in the table but will not be on PATH in CI.
	let output = retype()
		.args(["doctor", "--check", "--require", "magick"])
		.env("PATH", "")
		.output()
		.unwrap();

	assert_eq!(output.status.code(), Some(4));
}
