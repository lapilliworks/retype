//! `retype list`: what the configuration declares, and the pairwise view that
//! `--tiers` adds.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

fn config(dir: &Path) -> PathBuf {
	let (command, layout) = match cfg!(windows) {
		true => ("cmd", "layout = ['/c', 'copy', '{input}', '{ops}', '{output}']"),
		false => ("cp", "layout = ['{input}', '{ops}', '{output}']"),
	};

	let path = dir.join("retype.toml");
	fs::write(
		&path,
		format!(
			"\
version = 'retype-config-1'

[backends]
prefer = ['copy']

[tools.copy]
command = '{command}'
{layout}

[formats.png]
family = 'image'
lossy = false
alpha = true
tier = 'exact'

[formats.webp]
family = 'image'
lossy = true
alpha = true
tier = 'converted'
"
		),
	)
	.unwrap();

	path
}

fn run(config: &Path, args: &[&str]) -> String {
	let output = retype().args(args).env("RETYPE_CONFIG", config).output().unwrap();

	assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

	String::from_utf8_lossy(&output.stdout).to_string()
}

#[test]
fn list_names_the_formats_the_operations_and_the_backends() {
	let dir = tempfile::tempdir().unwrap();
	let config = config(dir.path());
	let stdout = run(&config, &["list"]);

	assert!(stdout.contains("FORMAT"), "{stdout}");
	assert!(stdout.contains("png"), "{stdout}");
	assert!(stdout.contains("OPERATION"), "{stdout}");
	assert!(stdout.contains("quality"), "{stdout}");
	assert!(stdout.contains("BACKEND"), "{stdout}");
	assert!(stdout.contains("copy"), "{stdout}");
}

#[test]
fn tiers_claims_one_line_per_pair() {
	let dir = tempfile::tempdir().unwrap();
	let config = config(dir.path());
	let stdout = run(&config, &["list", "--tiers"]);

	assert!(stdout.contains("FROM"), "{stdout}");
	assert!(stdout.contains("TIER"), "{stdout}");

	let pair = stdout
		.lines()
		.find(|line| line.starts_with("png") && line.contains("webp"))
		.unwrap_or_default();

	assert!(pair.contains("converted"), "{pair}");
	assert!(pair.contains("copy"), "{pair}");
}

#[test]
fn a_pair_nothing_can_carry_says_why() {
	let dir = tempfile::tempdir().unwrap();
	let quiet = dir.path().join("quiet.toml");
	let text = fs::read_to_string(config(dir.path())).unwrap();
	// A tool that is not installed: present in `prefer`, absent from the machine.
	fs::write(
		&quiet,
		text.replace("command = 'cp'", "command = 'cp-not-installed'")
			.replace("command = 'cmd'", "command = 'cmd-not-installed'"),
	)
	.unwrap();

	let stdout = run(&quiet, &["list", "--tiers"]);
	let pair = stdout
		.lines()
		.find(|line| line.starts_with("png") && line.contains("webp"))
		.unwrap_or_default();

	assert!(pair.contains("(E_TOOL_MISSING)"), "{pair}");
}

fn kinds(stdout: &str) -> std::collections::BTreeSet<String> {
	let mut kinds = std::collections::BTreeSet::new();

	for line in stdout.lines() {
		let value: serde_json::Value = serde_json::from_str(line).expect("an object");
		kinds.insert(value["kind"].as_str().unwrap_or_default().to_string());
	}

	kinds
}

#[test]
fn json_writes_one_object_per_line() {
	let dir = tempfile::tempdir().unwrap();
	let config = config(dir.path());

	let listed = kinds(&run(&config, &["list", "--json"]));
	assert!(listed.contains("format"), "{listed:?}");
	assert!(listed.contains("operation"), "{listed:?}");
	assert!(listed.contains("backend"), "{listed:?}");

	// `--tiers` replaces the listing rather than adding to it, in JSON as in text.
	let pairs = kinds(&run(&config, &["list", "--tiers", "--json"]));
	assert_eq!(pairs, ["pair".to_string()].into_iter().collect(), "{pairs:?}");
}
