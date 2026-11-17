//! Native installer rollback is checked against an isolated local release.
#![cfg(windows)]

#[path = "support/commands.rs"]
mod commands;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

#[test]
fn windows_installation_rolls_back_failed_publications() {
	let root = Path::new(env!("CARGO_MANIFEST_DIR"))
		.parent()
		.unwrap()
		.parent()
		.unwrap();
	let mut command = Command::new("powershell.exe");
	command
		/* Let Windows PowerShell find its own modules, not the parent PowerShell 7 modules. */
		.env_remove("PSModulePath")
		.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
		.arg(root.join("tests/install.ps1"))
		.arg("-Binary")
		.arg(env!("CARGO_BIN_EXE_retype"));
	let output = commands::bounded(&mut command, Duration::from_secs(60)).unwrap();
	assert!(output.status.success(), "{}\n{}", output.stdout, output.stderr);
	assert!(output.stdout.contains("Windows installer tests passed"));
}
