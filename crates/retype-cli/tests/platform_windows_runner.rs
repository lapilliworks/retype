//! Native checks for Windows platform-runner probe reporting.
#![cfg(windows)]

#[path = "support/commands.rs"]
mod commands;
#[path = "support/tools.rs"]
mod tools;

use std::io;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

#[test]
fn windows_probe_failures_and_timeouts_are_reported() {
	let root = Path::new(env!("CARGO_MANIFEST_DIR"))
		.parent()
		.unwrap()
		.parent()
		.unwrap();
	let scratch = tempfile::tempdir().unwrap();
	let mut command = Command::new("pwsh");
	command
		.args(["-NoProfile", "-NonInteractive", "-File"])
		.arg(root.join("crates/retype-cli/tests/support/windows_runner_probe.ps1"))
		.arg(root.join("scripts/test-windows.ps1"))
		.arg(scratch.path());
	let result = match commands::bounded(&mut command, Duration::from_secs(30)) {
		Ok(result) => result,
		Err(error) if error.kind() == io::ErrorKind::NotFound => {
			tools::unavailable("PowerShell 7: install pwsh on PATH");
			return;
		}
		Err(error) => panic!("could not run the Windows probe checks: {error}"),
	};
	assert!(
		result.status.success(),
		"Windows probe checks failed: stdout: {}\nstderr: {}",
		result.stdout,
		result.stderr
	);
	assert!(result.stdout.contains("Windows probe reporting checks passed."));
}
