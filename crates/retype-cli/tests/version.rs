//! `retype version` and `retype completions`, `docs/cli.md` §1.
//!
//! Both are support surfaces: a bug report names the schemas it came from, and a
//! shell completion is what makes the command usable without the manual.

use std::process::Command;

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

#[test]
fn version_names_the_build_and_every_schema_it_reads() {
	let ran = retype().arg("version").output().unwrap();
	assert!(ran.status.success());

	let printed = String::from_utf8_lossy(&ran.stdout);
	assert!(printed.starts_with("retype "), "{printed}");

	for schema in [
		"retype-grammar-1",
		"retype-config-1",
		"retype-ipc-1",
		"retype-journal-1",
		"retype-undo-1",
		"retype-watches-1",
		"retype-bench-1",
		"retype-fidelity-1",
		"retype-fixtures-1",
	] {
		assert!(printed.contains(schema), "{schema} is missing from: {printed}");
	}
}

#[test]
fn version_as_json_carries_the_same_facts() {
	let ran = retype().args(["version", "--json"]).output().unwrap();
	assert!(ran.status.success());

	let printed = String::from_utf8_lossy(&ran.stdout);
	let parsed: serde_json::Value = serde_json::from_str(printed.trim()).expect("one JSON object");

	assert_eq!(parsed["name"], "retype");
	assert_eq!(parsed["v"], "retype-cli-json-1");
	assert_eq!(parsed["schemas"]["config"], "retype-config-1");
}

#[test]
fn a_completion_script_is_written_for_the_installed_name() {
	let ran = retype().args(["completions", "bash"]).output().unwrap();
	assert!(ran.status.success());

	let printed = String::from_utf8_lossy(&ran.stdout);
	assert!(printed.contains("_retype()"), "{printed}");
	assert!(printed.contains("convert"), "{printed}");
}

#[test]
fn a_shell_this_build_does_not_know_is_a_usage_error() {
	let ran = retype().args(["completions", "tcsh"]).output().unwrap();

	assert_eq!(ran.status.code(), Some(2), "clap rejects the value");
}

#[test]
fn the_version_flag_answers_in_json_when_asked() {
	let ran = retype().args(["--json", "--version"]).output().unwrap();
	assert!(ran.status.success());

	let parsed: serde_json::Value = serde_json::from_slice(&ran.stdout).expect("one JSON object, not plain text");
	assert_eq!(parsed["v"], "retype-cli-json-1");
	assert_eq!(parsed["ok"], true);
	assert_eq!(parsed["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn a_parser_error_is_an_envelope_when_json_was_asked_for() {
	let ran = retype().args(["--json", "uncompressed"]).output().unwrap();

	assert_eq!(ran.status.code(), Some(2));
	assert!(ran.stderr.is_empty(), "{}", String::from_utf8_lossy(&ran.stderr));

	let parsed: serde_json::Value = serde_json::from_slice(&ran.stdout).expect("one JSON object");
	assert_eq!(parsed["v"], "retype-cli-json-1");
	assert_eq!(parsed["ok"], false);
	assert_eq!(parsed["error"]["code"], "E_USAGE_ARGS");
	assert_eq!(parsed["error"]["category"], "usage");
}
