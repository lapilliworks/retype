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
fn explains_the_actions_in_words() {
	let output = retype()
		.args(["explain", "holiday.heic", "webp+q80+cap1080+rotate90"])
		.output()
		.unwrap();
	let stdout = String::from_utf8_lossy(&output.stdout);

	assert!(
		stdout.contains("convert from heic to webp with quality at 80, then cap to 1080, then rotate 90 degrees"),
		"{stdout}"
	);
}

#[test]
fn a_preset_expands_into_the_operations_it_stands_for() {
	let output = retype()
		.args(["explain", "holiday.heic", "jpg+thumbnail512"])
		.output()
		.unwrap();
	let stdout = String::from_utf8_lossy(&output.stdout);

	assert!(stdout.contains("jpg+cap512+quality85"), "{stdout}");
	assert!(stdout.contains("with cap to 512, then quality at 85"), "{stdout}");
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
fn an_alias_names_the_format_it_means() {
	let output = retype().args(["explain", "photo.jpeg", "png"]).output().unwrap();
	let stdout = String::from_utf8_lossy(&output.stdout);

	assert!(stdout.contains("convert from jpg to png"), "{stdout}");
	assert!(stdout.contains("photo.png"), "{stdout}");
}

#[test]
fn a_bare_format_names_a_source_without_a_file() {
	let output = retype().args(["explain", "heic", "jpg"]).output().unwrap();
	let stdout = String::from_utf8_lossy(&output.stdout);

	assert!(stdout.contains("convert from heic to jpg"), "{stdout}");
	assert!(stdout.contains("input.jpg"), "{stdout}");
	assert!(matches!(output.status.code(), Some(0) | Some(4)), "{stdout}");
}

#[test]
fn an_alias_is_a_bare_format_too() {
	let output = retype().args(["explain", "jpeg", "png"]).output().unwrap();
	let stdout = String::from_utf8_lossy(&output.stdout);

	assert!(stdout.contains("convert from jpg to png"), "{stdout}");
	assert!(stdout.contains("input.png"), "{stdout}");
}

#[test]
fn an_unknown_source_format_is_refused() {
	let output = retype().args(["explain", "notes.txt", "webp"]).output().unwrap();

	assert_eq!(output.status.code(), Some(3));
	assert!(String::from_utf8_lossy(&output.stderr).contains("E_FORMAT_UNKNOWN"));
}

/// A video source and an image target is a diagnosis, not a plan: no single tool
/// carries both families, and the answer says which.
#[test]
fn a_cross_family_conversion_is_unsupported() {
	let output = retype().args(["explain", "clip.mov", "webp"]).output().unwrap();

	assert_eq!(output.status.code(), Some(3));
	assert!(String::from_utf8_lossy(&output.stderr).contains("E_UNSUPPORTED"));
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
