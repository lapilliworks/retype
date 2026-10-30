use std::process::Command;

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

#[test]
fn explains_an_intent_without_a_file() {
	let output = retype().args(["explain", "holiday.heic", "webp"]).output().unwrap();
	let stdout = String::from_utf8_lossy(&output.stdout);

	assert!(stdout.contains("holiday.webp"), "{stdout}");
	assert!(stdout.contains("backend"), "{stdout}");
	assert!(stdout.contains("tier"), "{stdout}");

	// The explanation is printed either way; only whether the machine has the
	// backend changes the exit code.
	assert!(
		matches!(output.status.code(), Some(0) | Some(4)),
		"unexpected exit: {:?}",
		output.status.code()
	);
}

#[test]
fn a_conflicting_intent_is_a_diagnosis() {
	let output = retype()
		.args(["explain", "holiday.heic", "webp+q80+size5mb"])
		.output()
		.unwrap();

	assert_eq!(output.status.code(), Some(3));
	assert!(String::from_utf8_lossy(&output.stderr).contains("E_EXCLUSIVE"));
}

#[test]
fn an_alias_prints_as_the_format_it_names() {
	let output = retype().args(["explain", "photo.jpeg", "png"]).output().unwrap();
	let stdout = String::from_utf8_lossy(&output.stdout);

	assert!(stdout.contains("photo.jpg"), "{stdout}");
	assert!(!stdout.contains("jpeg"), "{stdout}");
}

#[test]
fn an_unknown_source_format_is_refused() {
	let output = retype().args(["explain", "clip.mov", "webp"]).output().unwrap();

	assert_eq!(output.status.code(), Some(3));
	assert!(String::from_utf8_lossy(&output.stderr).contains("E_FORMAT_UNKNOWN"));
}

#[test]
fn json_carries_the_envelope_version() {
	let output = retype()
		.args(["--json", "explain", "holiday.heic", "webp"])
		.output()
		.unwrap();
	let stdout = String::from_utf8_lossy(&output.stdout);

	assert!(stdout.contains("\"v\":\"retype-cli-json-1\""), "{stdout}");
	assert!(stdout.contains("\"outputs\""), "{stdout}");
}

#[test]
fn a_name_with_no_declared_suffix_is_not_ours() {
	let output = retype().args(["explain", "notes.txt", "png"]).output().unwrap();

	assert_eq!(output.status.code(), Some(3));
	assert!(String::from_utf8_lossy(&output.stderr).contains("E_FORMAT_UNKNOWN"));
}

#[test]
fn quiet_still_reports_diagnostics() {
	let output = retype()
		.args(["--quiet", "explain", "holiday.heic", "webp+q80+size5mb"])
		.output()
		.unwrap();

	assert_eq!(output.status.code(), Some(3));
	assert!(output.stdout.is_empty(), "nothing to say on stdout");
	assert!(String::from_utf8_lossy(&output.stderr).contains("E_EXCLUSIVE"));
}
