//! Finding the daemon, and starting one when it is missing.
//!
//! The endpoint is read from a file the daemon publishes rather than derived
//! here: deriving it would mean two places that compute a pipe name from the
//! machine identity, and a first-run race between them would leave a client
//! looking for a name nobody listens on.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use retype_ipc::Client;
use serde_json::Value;

use crate::error::{Error, Result};
use crate::state;

/// How long a client waits for a daemon it just started, `docs/dev/architecture/ipc.md` §2.
const START_TIMEOUT: Duration = Duration::from_secs(3);
const FIRST_DELAY: Duration = Duration::from_millis(20);
const MAX_DELAY: Duration = Duration::from_millis(250);
const CLIENT_NAME: &str = concat!("retype-cli/", env!("CARGO_PKG_VERSION"));

/// Connect to a running daemon. Never starts one.
pub fn connect(state_dir: &Path) -> Result<Client> {
	let endpoint = state::read_endpoint(state_dir)?;
	crate::verbose::note(format!("daemon at {endpoint}"));
	let mut client = Client::connect(Path::new(&endpoint))?;
	client.hello(CLIENT_NAME)?;
	Ok(client)
}

/// Connect, starting a daemon if none answers and starting one is allowed.
pub fn connect_or_spawn(state_dir: &Path, allow_spawn: bool) -> Result<Client> {
	if let Ok(client) = connect(state_dir) {
		return Ok(client);
	}

	if !allow_spawn || spawn_suppressed() {
		return Err(Error::DaemonUnavailable);
	}

	crate::verbose::note(format!("no daemon for {}, starting one", state_dir.display()));
	let log = spawn_detached(state_dir)?;
	wait_for(state_dir, &log)
}

/// Start a daemon without a terminal, pointing its output at a log file so a
/// failure to start can be reported with something to read.
pub fn spawn_detached(state_dir: &Path) -> Result<PathBuf> {
	state::ensure_private_directory(state_dir)?;

	let log = state_dir.join("daemon.log");
	let stdout = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
	let stderr = stdout.try_clone()?;

	let mut command = Command::new(std::env::current_exe()?);
	command
		.arg("daemon")
		.arg("--detached")
		.arg("--state-dir")
		.arg(state_dir)
		.stdin(Stdio::null())
		.stdout(stdout)
		.stderr(stderr);

	// The daemon reads configuration too, and it has to read the same file the
	// client did rather than guess at one.
	if let Some(config) = crate::config::user_path() {
		command.env("RETYPE_CONFIG", config);
	}

	// This daemon was started by the CLI, not by the supervisor that started the
	// CLI's shell. The variables are inherited down the whole session, and a
	// systemd user unit for the terminal or a launchd-launched app sets them, so
	// passing them on would claim a supervision that would never restart it.
	command.env_remove("INVOCATION_ID").env_remove("XPC_SERVICE_NAME");

	// Without this the child inherits the console and dies with it.
	#[cfg(windows)]
	{
		use std::os::windows::process::CommandExt;
		const DETACHED_PROCESS: u32 = 0x0000_0008;
		command.creation_flags(DETACHED_PROCESS);
		detach_handles()?;
	}

	command.spawn()?;
	Ok(log)
}

/// Stop the daemon from inheriting the standard handles this process was given.
///
/// Windows hands every inheritable handle to a child, and the handles a caller
/// uses to read this process's output must be inheritable or they could not have
/// been passed to it at all. A daemon holding one would keep that pipe open for
/// as long as it lives: `retype convert | cat` would not see the end of the output
/// until the daemon stopped, and a caller that waits for the end of the output
/// before it stops the daemon would wait for itself. The daemon's own standard
/// handles are the log file and the null device, set above, so it needs none of
/// ours.
#[cfg(windows)]
fn detach_handles() -> std::io::Result<()> {
	use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation};
	use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};

	for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
		let handle = unsafe { GetStdHandle(which) };

		// A process with no console has no standard handles to clear.
		if handle.is_null() || handle == INVALID_HANDLE_VALUE {
			continue;
		}

		if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
			return Err(std::io::Error::last_os_error());
		}
	}

	Ok(())
}

/// Wait for a daemon that was just spawned to answer.
pub fn wait_for(state_dir: &Path, log: &Path) -> Result<Client> {
	let deadline = Instant::now() + START_TIMEOUT;
	let mut delay = FIRST_DELAY;

	while Instant::now() < deadline {
		if let Ok(client) = connect(state_dir) {
			return Ok(client);
		}

		std::thread::sleep(delay);
		delay = (delay * 2).min(MAX_DELAY);
	}

	Err(Error::DaemonStartFailed(START_TIMEOUT, log.to_path_buf()))
}

/// Wait until the daemon has removed its endpoint, which it does after the
/// journal is flushed and the socket is gone, `docs/dev/architecture/ipc.md` §2.
pub fn wait_for_stop(state_dir: &Path, timeout: Duration) -> bool {
	let deadline = Instant::now() + timeout;

	while Instant::now() < deadline {
		if matches!(state::read_endpoint(state_dir), Err(Error::DaemonUnavailable)) {
			return true;
		}
		std::thread::sleep(FIRST_DELAY);
	}

	false
}

fn spawn_suppressed() -> bool {
	std::env::var_os("RETYPE_NO_SPAWN").is_some_and(|value| value != "0" && !value.is_empty())
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
		("watches".to_string(), watch_line(&status["watches"])),
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

/// A one-line account of the registrations, with a missing or paused one named
/// rather than silently leaving `daemon status` looking healthy.
fn watch_line(watches: &Value) -> String {
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
			match (
				watch["available"].as_bool().unwrap_or(false),
				watch["paused"].as_bool().unwrap_or(false),
			) {
				(false, _) => format!("{path} (missing)"),
				(_, true) => format!("{path} (paused)"),
				_ => path.to_string(),
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
	fn a_missing_daemon_is_refused_before_spawning() {
		let dir = tempfile::tempdir().unwrap();

		let error = connect_or_spawn(dir.path(), false).unwrap_err();

		assert!(matches!(error, Error::DaemonUnavailable));
		assert_eq!(error.exit_code(), 6);
	}

	#[test]
	fn a_stale_endpoint_does_not_throw() {
		let dir = tempfile::tempdir().unwrap();
		state::publish_endpoint(dir.path(), "/tmp/definitely-not-a-socket").unwrap();

		let error = connect(dir.path()).unwrap_err();

		assert!(error.exit_code() == 6, "{error}");
	}

	#[test]
	fn the_endpoint_survives_being_written_and_read() {
		let dir = tempfile::tempdir().unwrap();

		state::publish_endpoint(dir.path(), r"\\.\pipe\retype-x-y").unwrap();

		assert_eq!(state::read_endpoint(dir.path()).unwrap(), r"\\.\pipe\retype-x-y");
	}
}
