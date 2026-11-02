//! The document vertical end to end, with pandoc and whatever PDF engine this
//! machine has.
//!
//! Skipped, not failed, where pandoc is absent: the point is that the shipped
//! declarations produce a readable file of the target format when the tool is
//! there.

#[path = "support/tools.rs"]
mod tool_support;

use std::fs;
use std::path::Path;
use std::process::Command;

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

fn pandoc() -> bool {
	Command::new("pandoc")
		.arg("--version")
		.stdout(std::process::Stdio::null())
		.stderr(std::process::Stdio::null())
		.status()
		.is_ok_and(|status| status.success())
}

/// A markdown source with the shapes that matter: a heading, emphasis, a list, a
/// table and a code block.
const SOURCE: &str = "\
# Quarterly report

A paragraph with **emphasis** and `code`.

- one
- two

| a | b |
|---|---|
| 1 | 2 |

```rust
fn main() {}
```
";

/// The daemon a `convert` causes, stopped when the test ends: an autostarted daemon
/// outlives its client, and a leaked one keeps a watcher and a timer alive for every
/// later run.
struct Autostarted {
	state: std::path::PathBuf,
}

impl Drop for Autostarted {
	fn drop(&mut self) {
		let _ = retype()
			.args(["daemon", "stop", "--force"])
			.arg("--state-dir")
			.arg(&self.state)
			.output();
	}
}

fn convert(dir: &Path, state: &Path, target: &str) -> (std::process::Output, std::path::PathBuf) {
	let source = dir.join("report.md");
	fs::write(&source, SOURCE).unwrap();

	let output = retype()
		.args(["convert"])
		.arg(&source)
		.arg(target)
		.arg("--state-dir")
		.arg(state)
		.output()
		.unwrap();

	(output, dir.join(format!("report.{target}")))
}

#[test]
fn markdown_becomes_a_readable_document_in_every_declared_target() {
	if !pandoc() {
		tool_support::unavailable("Pandoc on PATH");
		return;
	}

	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let _daemon = Autostarted { state: state.clone() };

	// Each target is checked by its first bytes rather than by its size: a PDF that
	// is not a PDF is the failure worth catching.
	for (target, magic) in [
		("html", b"<!DOCTYPE html".as_slice()),
		("docx", b"PK\x03\x04".as_slice()),
		("odt", b"PK\x03\x04".as_slice()),
		("epub", b"PK\x03\x04".as_slice()),
		("pdf", b"%PDF".as_slice()),
	] {
		let (ran, path) = convert(dir.path(), &state, target);

		assert!(
			ran.status.success(),
			"{target}: {}",
			String::from_utf8_lossy(&ran.stderr)
		);

		let bytes = fs::read(&path).unwrap_or_else(|error| panic!("{target} was not written to {path:?}: {error}"));
		assert!(
			bytes.starts_with(magic),
			"{target} does not start with {:?}",
			String::from_utf8_lossy(magic)
		);
	}
}

#[test]
fn the_pdf_engine_is_recorded_with_the_job() {
	if !pandoc() {
		tool_support::unavailable("Pandoc on PATH");
		return;
	}

	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let _daemon = Autostarted { state: state.clone() };
	let source = dir.path().join("report.md");
	fs::write(&source, SOURCE).unwrap();

	let explained = retype()
		.args(["convert"])
		.arg(&source)
		.arg("pdf")
		.arg("--dry-run")
		.arg("--json")
		.arg("--state-dir")
		.arg(&state)
		.output()
		.unwrap();

	assert!(
		explained.status.success(),
		"stdout: {}\nstderr: {}",
		String::from_utf8_lossy(&explained.stdout),
		String::from_utf8_lossy(&explained.stderr)
	);

	let printed = String::from_utf8_lossy(&explained.stdout);
	assert!(
		printed.contains("\"name\":\"pdf\"") && printed.contains("\"resolved\":"),
		"the engine is part of the plan: {printed}"
	);
}

#[test]
fn a_pdf_is_a_target_and_never_a_source() {
	// pandoc has no PDF reader, so the honest answer is a diagnosis before a tool is
	// chosen rather than a conversion that fails after the work has started.
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let _daemon = Autostarted { state: state.clone() };
	let source = dir.path().join("report.pdf");
	fs::write(&source, b"%PDF-1.7\n").unwrap();

	let ran = retype()
		.args(["convert"])
		.arg(&source)
		.arg("md")
		.arg("--state-dir")
		.arg(&state)
		.output()
		.unwrap();

	assert_eq!(ran.status.code(), Some(3), "a diagnosis is exit 3");
	assert!(
		String::from_utf8_lossy(&ran.stderr).contains("written but not read"),
		"{}",
		String::from_utf8_lossy(&ran.stderr)
	);
}
