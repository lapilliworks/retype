//! Finding the daemon, through the shared client crate.
//!
//! The discovery and spawn rules live in `retype-client`; this module only adds
//! the command line's own config path and its human-readable status rendering.

use std::path::{Path, PathBuf};
use std::time::Duration;

use retype_client::{Client, Context, Daemon};
use serde_json::Value;

use crate::error::Result;

/// The context this process uses: the state and configuration it was given, and
/// the CLI binary as the daemon launcher.
pub(crate) fn context(state_dir: &Path, allow_spawn: bool) -> Result<Context> {
	Ok(Context::new(state_dir)?
		.with_config(crate::config::user_path())
		.with_daemon(Daemon::current_exe()?)
		.with_spawn(allow_spawn))
}

/// Connect to a running daemon. Never starts one.
pub fn connect(state_dir: &Path) -> Result<Client> {
	Ok(retype_client::connect(&context(state_dir, false)?)?)
}

/// Connect, starting a daemon if none answers and starting one is allowed.
pub fn connect_or_spawn(state_dir: &Path, allow_spawn: bool) -> Result<Client> {
	Ok(retype_client::connect_or_spawn(&context(state_dir, allow_spawn)?)?)
}

/// Start a daemon without a terminal and return its log path.
pub fn spawn_detached(state_dir: &Path) -> Result<PathBuf> {
	Ok(retype_client::spawn_detached(&context(state_dir, true)?)?)
}

/// Wait for a daemon that was just spawned to answer.
pub fn wait_for(state_dir: &Path, log: &Path) -> Result<Client> {
	Ok(retype_client::wait_for(&context(state_dir, true)?, log)?)
}

/// Wait until the daemon has removed its endpoint.
pub fn wait_for_stop(state_dir: &Path, timeout: Duration) -> bool {
	retype_client::wait_for_stop(state_dir, timeout)
}

/// `docs/dev/architecture/ipc.md` §1: the daemon's own account of itself, minus what a human does
/// not need on one line.
pub fn describe_status(status: &Value) -> Vec<(String, String)> {
	let field = |name: &str| match &status[name] {
		Value::Null => "-".to_string(),
		Value::String(text) => text.clone(),
		other => other.to_string(),
	};

	vec![
		("pid".to_string(), field("pid")),
		("machine".to_string(), field("machine")),
		(
			"uptime".to_string(),
			format_uptime(status["uptime_ms"].as_u64().unwrap_or_default()),
		),
		("jobs".to_string(), field("jobs")),
		("parked".to_string(), field("parked")),
		("dropped".to_string(), field("dropped")),
		("placeholders".to_string(), field("placeholders")),
		("primed".to_string(), field("primed")),
		("observed".to_string(), field("observed")),
		(
			"watches".to_string(),
			watch_line(&status["watches"], &status["watch_error"]),
		),
		("journal".to_string(), field("journal")),
		("endpoint".to_string(), field("endpoint")),
		(
			"supervised".to_string(),
			match &status["supervised"] {
				Value::Null => "no".to_string(),
				Value::String(name) => name.clone(),
				other => other.to_string(),
			},
		),
	]
}

/// Registrations are not proof that their filesystem watchers are installed.
fn watch_line(watches: &Value, error: &Value) -> String {
	if let Some(error) = error.as_str() {
		return format!("unavailable: {error}");
	}
	let Some(watches) = watches.as_array() else {
		return "-".to_string();
	};

	if watches.is_empty() {
		return "none".to_string();
	}

	watches
		.iter()
		.map(|watch| {
			let path = watch["path"].as_str().unwrap_or_default();
			let state = watch["status"].as_str().unwrap_or("unverified");
			match watch["problem"].as_str() {
				Some(problem) => format!("{path} ({state}: {problem})"),
				None => format!("{path} ({state})"),
			}
		})
		.collect::<Vec<_>>()
		.join(", ")
}

fn format_uptime(milliseconds: u64) -> String {
	let seconds = milliseconds / 1000;
	let (hours, minutes, seconds) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);

	if hours > 0 {
		format!("{hours}h {minutes}m")
	} else if minutes > 0 {
		format!("{minutes}m {seconds}s")
	} else {
		format!("{seconds}s")
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn uptime_is_readable_at_every_scale() {
		assert_eq!(format_uptime(1_500), "1s");
		assert_eq!(format_uptime(91_000), "1m 31s");
		assert_eq!(format_uptime(3_723_000), "1h 2m");
	}

	#[test]
	fn watcher_status_distinguishes_readiness_from_registration() {
		let watches = serde_json::json!([
			{"path": "/ready", "status": "ready"},
			{"path": "/broken", "status": "error", "problem": "permission denied"},
			{"path": "/old-daemon"}
		]);
		let line = watch_line(&watches, &Value::Null);
		assert!(line.contains("/ready (ready)"), "{line}");
		assert!(line.contains("/broken (error: permission denied)"), "{line}");
		assert!(line.contains("/old-daemon (unverified)"), "{line}");
		assert_eq!(
			watch_line(&watches, &serde_json::json!("bad registry")),
			"unavailable: bad registry"
		);
	}

	#[test]
	fn a_missing_daemon_is_refused_before_spawning() {
		let dir = tempfile::tempdir().unwrap();

		let error = connect_or_spawn(dir.path(), false).unwrap_err();

		assert!(matches!(error, crate::error::Error::DaemonUnavailable));
		assert_eq!(error.exit_code(), 6);
	}

	#[test]
	fn a_stale_endpoint_does_not_throw() {
		let dir = tempfile::tempdir().unwrap();
		crate::state::publish_endpoint(dir.path(), "/tmp/definitely-not-a-socket").unwrap();

		let error = connect(dir.path()).unwrap_err();

		assert!(error.exit_code() == 6, "{error}");
	}

	#[test]
	fn the_endpoint_survives_being_written_and_read() {
		let dir = tempfile::tempdir().unwrap();

		crate::state::publish_endpoint(dir.path(), r"\\.\pipe\retype-x-y").unwrap();

		assert_eq!(crate::state::read_endpoint(dir.path()).unwrap(), r"\\.\pipe\retype-x-y");
	}
}
