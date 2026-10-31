//! `retype doctor --report`: one file a user can read and send.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use retype_core::job::JobId;
use retype_core::undo::{Store, Subject};

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

/// A state directory with a journal, an undo record and a log, which is what a
/// report is supposed to show.
fn state(dir: &Path) -> PathBuf {
	let state = dir.join("state");
	fs::create_dir_all(state.join("logs")).unwrap();

	let done = format!("01J{:023}", 1);
	let failed = format!("01J{:023}", 2);

	let journal = [
		format!(r#"{{"v":"retype-journal-1","kind":"job","id":"{done}","state":"done","intent":"webp"}}"#),
		format!(r#"{{"v":"retype-journal-1","kind":"job","id":"{failed}","state":"failed","intent":"jpg"}}"#),
	];
	fs::write(state.join("jobs.jsonl"), journal.join("\n") + "\n").unwrap();

	fs::write(
		state.join("logs").join(format!("{failed}.log")),
		"magick: no such file\n",
	)
	.unwrap();

	let source = dir.join("photo.heic");
	fs::write(&source, b"heic bytes").unwrap();

	Store::open(&state)
		.save(Subject {
			id: JobId::parse(&done).unwrap(),
			intent: "webp".to_string(),
			machine: "machine".to_string(),
			bytes: source.clone(),
			source_path: source,
			source_ext: "heic".to_string(),
			outputs: Vec::new(),
		})
		.unwrap();

	state
}

#[test]
fn the_report_carries_the_build_the_tools_and_what_happened() {
	let dir = tempfile::tempdir().unwrap();
	let state = state(dir.path());
	let report = dir.path().join("report.txt");

	let ran = retype()
		.args(["doctor", "--report"])
		.arg(&report)
		.arg("--state-dir")
		.arg(&state)
		.output()
		.unwrap();

	assert!(ran.status.success(), "{}", String::from_utf8_lossy(&ran.stderr));

	let written = fs::read_to_string(&report).unwrap();

	for section in ["## build", "## machine", "## configuration", "## tools", "## state"] {
		assert!(written.contains(section), "{section} is missing from the report");
	}

	assert!(written.contains("retype    0.1.0"), "{written}");
	assert!(written.contains("Read it before sending it"), "{written}");
	assert!(written.contains("magick"), "the probed tools: {written}");
	assert!(written.contains("2 record(s), the last 2 below"), "{written}");
	assert!(written.contains(r#""intent":"webp""#), "the journal tail: {written}");
	assert!(written.contains("1 record(s),"), "the undo summary: {written}");
	assert!(written.contains("magick: no such file"), "the log tail: {written}");
}

#[test]
fn the_report_lands_in_the_current_directory_when_no_path_is_given() {
	let dir = tempfile::tempdir().unwrap();
	let state = state(dir.path());
	let work = dir.path().join("work");
	fs::create_dir(&work).unwrap();

	let ran = retype()
		.current_dir(&work)
		.args(["doctor", "--report"])
		.arg("--state-dir")
		.arg(&state)
		.output()
		.unwrap();

	assert!(ran.status.success(), "{}", String::from_utf8_lossy(&ran.stderr));

	let written: Vec<PathBuf> = fs::read_dir(&work)
		.unwrap()
		.map(|entry| entry.unwrap().path())
		.collect();

	assert_eq!(written.len(), 1, "{written:?}");
	let name = written[0].file_name().unwrap().to_string_lossy();
	assert!(name.starts_with("retype-report-"), "{name}");
	assert!(name.ends_with(".txt"), "{name}");
}

#[test]
fn a_report_and_a_snippet_together_are_a_usage_error() {
	let dir = tempfile::tempdir().unwrap();
	let state = state(dir.path());

	let ran = retype()
		.args(["doctor", "--report", "--emit"])
		.arg("--state-dir")
		.arg(&state)
		.output()
		.unwrap();

	assert_eq!(ran.status.code(), Some(2), "usage is exit 2");
	assert!(fs::read_dir(dir.path()).unwrap().all(|entry| {
		let path = entry.unwrap().path();
		!path
			.file_name()
			.unwrap()
			.to_string_lossy()
			.starts_with("retype-report-")
	}));
}
