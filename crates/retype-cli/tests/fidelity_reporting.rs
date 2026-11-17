//! Shared fidelity reporting and ordinary execution, without external codecs.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use retype_core::axis::Axis;
use retype_core::fidelity::{Claim, Manifest};
use retype_core::job::Tier;
use serde_json::Value;

struct Fixture {
	dir: tempfile::TempDir,
	state: PathBuf,
	input: PathBuf,
}

impl Fixture {
	fn new() -> Self {
		let dir = tempfile::tempdir().unwrap();
		let state = dir.path().join("state");
		fs::create_dir(&state).unwrap();
		let input = dir.path().join("bundle.tar");
		let mut builder = tar::Builder::new(fs::File::create(&input).unwrap());
		let mut header = tar::Header::new_gnu();
		header.set_size(5);
		header.set_mode(0o600);
		header.set_cksum();
		builder
			.append_data(&mut header, "sample.txt", b"bytes".as_slice())
			.unwrap();
		builder.finish().unwrap();
		Self { dir, state, input }
	}

	fn run(&self, args: &[&str]) -> Output {
		Command::new(env!("CARGO_BIN_EXE_retype"))
			.args(args)
			.arg("--state-dir")
			.arg(&self.state)
			.current_dir(self.dir.path())
			.env_remove("RETYPE_CONFIG")
			.env_remove("RETYPE_FIDELITY_MANIFEST")
			.output()
			.unwrap()
	}

	fn json(&self, args: &[&str]) -> Value {
		let output = self.run(args);
		assert!(output.status.success(), "{output:?}");
		serde_json::from_slice(&output.stdout).unwrap()
	}

	fn record(&self, tier: Tier, version: &str) {
		let mut manifest = Manifest::default();
		/* Synthetic evidence exercises lookup and enforcement, not release fidelity. */
		manifest.record(Claim {
			source: "tar".to_string(),
			target: "zip".to_string(),
			backend: "zip".to_string(),
			tier,
			engine: None,
			version: version.to_string(),
			fingerprint: String::new(),
			engine_version: String::new(),
			engine_fingerprint: String::new(),
			fixture: "synthetic reporting fixture".to_string(),
			axes: vec![Axis::Depth],
			lost: Vec::new(),
			earned: 1,
		});
		manifest.save(&self.state.join("fidelity.toml")).unwrap();
	}
}

impl Drop for Fixture {
	fn drop(&mut self) {
		let output = self.run(&["daemon", "stop", "--force"]);
		if !output.status.success() && self.state.join("endpoint").exists() {
			eprintln!("test daemon cleanup failed: {output:?}");
		}
	}
}

#[test]
fn missing_evidence_is_unclaimed_on_every_cli_surface() {
	let fixture = Fixture::new();
	let explain = fixture.json(&["explain", "tar", "zip", "--json"]);
	assert_eq!(explain["outputs"][0]["tier"], "unclaimed");
	assert_eq!(explain["outputs"][0]["declared_tier"], "exact");

	let listing = fixture.run(&["list", "--tiers", "--json"]);
	assert!(listing.status.success(), "{listing:?}");
	let pair: Value = String::from_utf8(listing.stdout)
		.unwrap()
		.lines()
		.map(|line| serde_json::from_str::<Value>(line).unwrap())
		.find(|pair| pair["from"] == "tar" && pair["to"] == "zip")
		.unwrap();
	assert_eq!(pair["tier"], "unclaimed");
	assert_eq!(pair["declared_tier"], "exact");

	let converted = fixture.json(&["convert", fixture.input.to_str().unwrap(), "zip+keep", "--json"]);
	assert_eq!(converted["job"]["outputs"][0]["fidelity"]["state"], "new");
	let output = fixture.dir.path().join("bundle.zip");
	let why = fixture.json(&["why", output.to_str().unwrap(), "--json"]);
	assert_eq!(why["tier"], "unclaimed");
	assert_eq!(why["declared_tier"], "exact");
}

#[test]
fn a_stale_exact_claim_refuses_conversion_without_touching_the_source() {
	let fixture = Fixture::new();
	fixture.record(Tier::Exact, "another release");
	let original = fs::read(&fixture.input).unwrap();
	let explain = fixture.run(&["explain", "tar", "zip", "--json"]);
	assert_eq!(explain.status.code(), Some(3), "{explain:?}");
	let answer: Value = serde_json::from_slice(&explain.stdout).unwrap();
	assert_eq!(answer["outputs"][0]["tier"], "unverified");
	assert_eq!(answer["error"]["code"], "E_UNSUPPORTED");

	let converted = fixture.run(&["convert", fixture.input.to_str().unwrap(), "zip", "--json"]);
	assert_eq!(converted.status.code(), Some(3), "{converted:?}");
	let answer: Value = serde_json::from_slice(&converted.stdout).unwrap();
	assert_eq!(answer["error"]["code"], "E_UNSUPPORTED");
	assert_eq!(fs::read(&fixture.input).unwrap(), original);
	assert!(!fixture.dir.path().join("bundle.zip").exists());
}

#[test]
fn current_evidence_is_persisted_with_the_output() {
	let fixture = Fixture::new();
	fixture.record(Tier::Exact, env!("CARGO_PKG_VERSION"));
	let explain = fixture.json(&["explain", "tar", "zip", "--json"]);
	assert_eq!(explain["outputs"][0]["tier"], "exact");
	let converted = fixture.json(&["convert", fixture.input.to_str().unwrap(), "zip+keep", "--json"]);
	assert_eq!(converted["job"]["outputs"][0]["fidelity"]["state"], "earned");
	assert_eq!(
		converted["job"]["outputs"][0]["fidelity"]["claim"]["fixture"],
		"synthetic reporting fixture"
	);
}

#[test]
fn stale_converted_evidence_does_not_block_conversion() {
	let fixture = Fixture::new();
	fs::write(
		fixture.dir.path().join("retype.toml"),
		"version = 'retype-config-1'\n[formats.zip]\ntier = 'converted'\n",
	)
	.unwrap();
	fixture.record(Tier::Converted, "another release");
	let converted = Command::new(env!("CARGO_BIN_EXE_retype"))
		.args(["convert", fixture.input.to_str().unwrap(), "zip+keep", "--json"])
		.arg("--state-dir")
		.arg(&fixture.state)
		.env("RETYPE_CONFIG", fixture.dir.path().join("retype.toml"))
		.env_remove("RETYPE_FIDELITY_MANIFEST")
		.output()
		.unwrap();
	assert!(converted.status.success(), "{converted:?}");
	let answer: Value = serde_json::from_slice(&converted.stdout).unwrap();
	assert_eq!(answer["job"]["outputs"][0]["fidelity"]["state"], "unverified");
	assert!(fixture.input.exists());
}

#[test]
fn an_invalid_manifest_is_not_silently_ignored() {
	let fixture = Fixture::new();
	fs::write(fixture.state.join("fidelity.toml"), "invalid = [").unwrap();
	let output = fixture.run(&["convert", fixture.input.to_str().unwrap(), "zip", "--json"]);
	assert!(!output.status.success(), "{output:?}");
	let answer: Value = serde_json::from_slice(&output.stdout).unwrap();
	assert_eq!(answer["error"]["code"], "E_CFG_PARSE");
	assert!(fixture.input.exists());
	assert!(!fixture.dir.path().join("bundle.zip").exists());
}
