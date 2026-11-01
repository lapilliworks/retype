//! `retype why`, `docs/cli.md` §5 and `docs/provenance.md` §8.
//!
//! The daemon answers, because the journal is what it holds. The command prints
//! which store answered and how sure it is, since a path match and an identity
//! match are not the same claim and the difference matters to someone deciding
//! whether to re-encode.

use std::path::Path;
use std::process::ExitCode;

use retype_core::fidelity::{Claim, Manifest, State};
use retype_core::registry::Registry;
use retype_core::tools;
use serde_json::Value;

use crate::client;
use crate::error::Result;
use crate::output;

pub fn run(
	registry: &Registry,
	path: &str,
	json: bool,
	quiet: bool,
	state_dir: &Path,
	allow_spawn: bool,
) -> Result<ExitCode> {
	let mut client = client::connect_or_spawn(state_dir, allow_spawn)?;
	let mut answer = client.call("why", serde_json::json!({"path": path}))?;

	if let Some(claim) = claim(registry, state_dir, &answer) {
		answer["claim"] = claim;
	}

	if json {
		output::success(answer);
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

	if let Some(claim) = answer.get("claim") {
		if let Some(state) = claim.get("state").and_then(Value::as_str) {
			line("claim", state);
		}
	}

	if let Some(bytes) = answer.get("bytes").and_then(Value::as_u64) {
		line("bytes", &bytes.to_string());
	}

	if let Some(created) = answer.get("created").and_then(Value::as_i64) {
		line("converted", &ago(created));
	}

	Ok(ExitCode::SUCCESS)
}

/// The claim the release makes for this pair, and whether the installed build is
/// the one it was earned with, `docs/fidelity.md` §5.
fn claim(registry: &Registry, state_dir: &Path, answer: &Value) -> Option<Value> {
	let source = answer.get("source").and_then(Value::as_str)?;
	let target = answer.get("target").and_then(Value::as_str)?;
	let backend = answer.get("backend").and_then(Value::as_str)?;
	let manifest = find_manifest(state_dir).and_then(|path| Manifest::open(&path).ok())?;
	let claim: &Claim = manifest.get(source, target, backend)?;

	let (version, fingerprint) = match registry.tool(backend) {
		Some(tool) => {
			let status = tools::probe(tool);
			(
				status.version.as_ref().map(ToString::to_string).unwrap_or_default(),
				status.fingerprint().unwrap_or_default(),
			)
		}
		None => (env!("CARGO_PKG_VERSION").to_string(), String::new()),
	};

	let state = match claim.is_current(&version, &fingerprint) {
		true => State::Earned,
		false => State::Unverified,
	};

	Some(serde_json::json!({
		"tier": tier(claim),
		"state": state.as_str(),
		"earned": claim.earned,
		"version": claim.version,
		"fingerprint": claim.fingerprint,
		"engine": claim.engine,
	}))
}

fn tier(claim: &Claim) -> &'static str {
	match claim.tier {
		retype_core::job::Tier::Exact => "exact",
		retype_core::job::Tier::Converted => "converted",
		retype_core::job::Tier::Delegated => "delegated",
	}
}

/// The release manifest, when this installation has one.
fn find_manifest(state_dir: &Path) -> Option<std::path::PathBuf> {
	if let Some(configured) = std::env::var_os("RETYPE_FIDELITY_MANIFEST") {
		let path = std::path::PathBuf::from(configured);
		if path.is_file() {
			return Some(path);
		}
	}

	for candidate in [
		state_dir.join("fidelity.toml"),
		std::path::PathBuf::from("fidelity.toml"),
	] {
		if candidate.is_file() {
			return Some(candidate);
		}
	}

	if let Ok(executable) = std::env::current_exe() {
		for ancestor in executable.ancestors().skip(1) {
			let candidate = ancestor.join("share/retype/fidelity.toml");
			if candidate.is_file() {
				return Some(candidate);
			}
		}
	}

	None
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
