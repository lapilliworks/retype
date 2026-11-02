//! Helpers shared by the tests that drive the daemon.
//!
//! A test daemon must not raise a real desktop notification: Windows and macOS both
//! have a notifier the daemon will find, so every configuration a daemon reads in a
//! test sets `[notify] enabled = false`. `tests/notify.rs` is the one exception, and it
//! scripts the notifier instead.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// The number of event paths the daemon's watcher has looked at, `docs/watch.md` §3.
pub fn observed(state: &Path) -> u64 {
	let output = Command::new(env!("CARGO_BIN_EXE_retype"))
		.args(["daemon", "status", "--json"])
		.arg("--state-dir")
		.arg(state)
		.output()
		.expect("the status command runs");

	let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_default();

	status["observed"].as_u64().unwrap_or_default()
}

/// Wait until the watcher has looked at one more path than `before`, so a file
/// written after that snapshot is known by identity before it is renamed.
///
/// macOS reports a create and a rename inside one window as a create, which
/// carries no pair and no identity to correlate, so the create has to be seen on
/// its own.
pub fn observe(state: &Path, before: u64) {
	let deadline = Instant::now() + Duration::from_secs(20);

	while observed(state) <= before {
		assert!(Instant::now() < deadline, "the watcher did not see the new file");
		std::thread::sleep(Duration::from_millis(25));
	}
}
