//! `retype why`, `docs/cli.md` §6 and `docs/provenance.md` §8.
//!
//! The daemon answers, because the journal is what it holds. The command prints
//! which store answered and how sure it is, since a path match and an identity
//! match are not the same claim and the difference matters to someone deciding
//! whether to re-encode.

use std::path::Path;
use std::process::ExitCode;

use serde_json::Value;

use crate::client;
use crate::error::Result;

pub fn run(path: &str, json: bool, quiet: bool, state_dir: &Path, allow_spawn: bool) -> Result<ExitCode> {
	let mut client = client::connect_or_spawn(state_dir, allow_spawn)?;
	let answer = client.call("why", serde_json::json!({"path": path}))?;

	if json {
		println!("{answer}");
		return Ok(ExitCode::SUCCESS);
	}

	// Nothing to explain is not a failure: the file may simply predate the
	// journal, and saying so is the honest answer, `docs/provenance.md` §6.
	if answer.get("confidence").and_then(Value::as_str) == Some("none") {
		if !quiet {
			eprintln!("{path}: no conversion in the journal produced this file");
			eprintln!("  retype records a conversion for twice the undo retention, or thirty days");
		}

		return Ok(ExitCode::SUCCESS);
	}

	// What the answer does not know is left out rather than printed as a dash: a
	// record read from the file carries the intent and nothing else.
	let line = |name: &str, value: &str| println!("{name:<11}{value}");
	let field = |name: &str| answer.get(name).and_then(Value::as_str).map(str::to_string);

	line("path", path);
	line("store", field("store").unwrap_or_default().as_str());
	line("confidence", field("confidence").unwrap_or_default().as_str());

	if let Some(intent) = field("intent") {
		line("intent", &intent);
	}

	if let Some(backend) = field("backend") {
		line("backend", &backend);
	}

	if let Some(engine) = answer.get("engine").and_then(Value::as_object) {
		line(
			"engine",
			&format!(
				"{} via {}",
				engine.get("name").and_then(Value::as_str).unwrap_or_default(),
				engine.get("candidate").and_then(Value::as_str).unwrap_or_default()
			),
		);
	}

	if let Some(tier) = field("tier") {
		line("tier", &tier);
	}

	if let Some(bytes) = answer.get("bytes").and_then(Value::as_u64) {
		line("bytes", &bytes.to_string());
	}

	if let Some(created) = answer.get("created").and_then(Value::as_i64) {
		line("converted", &ago(created));
	}

	Ok(ExitCode::SUCCESS)
}

/// How long ago a record was written, in the units a person reads. A date
/// formatter for one field would be a dependency and a locale question.
fn ago(created_ms: i64) -> String {
	let seconds = ((retype_core::job::now_ms() - created_ms).max(0) / 1000) as u64;

	match seconds {
		0..60 => format!("{seconds}s ago"),
		60..3_600 => format!("{}m ago", seconds / 60),
		3_600..86_400 => format!("{}h ago", seconds / 3600),
		_ => format!("{}d ago", seconds / 86_400),
	}
}
