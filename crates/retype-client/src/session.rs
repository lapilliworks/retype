//! Finding the daemon, and starting one when it is missing.
//!
//! The endpoint is read from a file the daemon publishes rather than derived
//! here: deriving it would mean two places that compute a pipe name from the
//! machine identity, and a first-run race between them would leave a client
//! looking for a name nobody listens on.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use retype_ipc::Client;

use crate::error::{Error, Result};
use crate::state;

/// How long a client waits for a daemon it just started, `docs/dev/architecture/ipc.md` §2.
const START_TIMEOUT: Duration = Duration::from_secs(3);
const FIRST_DELAY: Duration = Duration::from_millis(20);
const MAX_DELAY: Duration = Duration::from_millis(250);

/// The CLI or sidecar that can run `daemon --detached`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Daemon {
	program: PathBuf,
}

impl Daemon {
	pub fn at(program: impl Into<PathBuf>) -> Self {
		Self {
			program: program.into(),
		}
	}

	pub fn current_exe() -> Result<Self> {
		Ok(Self::at(std::env::current_exe()?))
	}

	/// An explicit path, then `RETYPE_DAEMON`, then this executable.
	///
	/// A GUI names its bundled CLI here. The CLI falls back to itself, which is
	/// what `retype daemon` has always done.
	pub fn discover(explicit: Option<PathBuf>) -> Result<Self> {
		if let Some(path) = explicit {
			return Ok(Self::at(path));
		}

		if let Some(configured) = std::env::var_os("RETYPE_DAEMON") {
			if !configured.is_empty() {
				return Ok(Self::at(PathBuf::from(configured)));
			}
		}

		Self::current_exe()
	}

	pub fn program(&self) -> &Path {
		&self.program
	}
}

/// Everything a client needs to reach or start a daemon.
#[derive(Debug, Clone)]
pub struct Context {
	pub state_dir: PathBuf,
	/// The user configuration the daemon must read.
	pub config: Option<PathBuf>,
	pub daemon: Daemon,
	pub allow_spawn: bool,
	pub client: String,
}

impl Context {
	pub fn new(state_dir: impl Into<PathBuf>) -> Result<Self> {
		Ok(Self {
			state_dir: state_dir.into(),
			config: retype_config::user_path(None),
			daemon: Daemon::discover(None)?,
			allow_spawn: true,
			client: concat!("retype-client/", env!("CARGO_PKG_VERSION")).to_string(),
		})
	}

	pub fn with_daemon(mut self, daemon: Daemon) -> Self {
		self.daemon = daemon;
		self
	}

	pub fn with_config(mut self, config: Option<PathBuf>) -> Self {
		self.config = config;
		self
	}

	pub fn with_client(mut self, client: impl Into<String>) -> Self {
		self.client = client.into();
		self
	}

	pub fn with_spawn(mut self, allow_spawn: bool) -> Self {
		self.allow_spawn = allow_spawn;
		self
	}
}

impl Default for Context {
	fn default() -> Self {
		Self {
			state_dir: PathBuf::new(),
			config: None,
			daemon: Daemon::at("retype"),
			allow_spawn: true,
			client: concat!("retype-client/", env!("CARGO_PKG_VERSION")).to_string(),
		}
	}
}

/// Connect to a running daemon. Never starts one.
pub fn connect(context: &Context) -> Result<Client> {
	let endpoint = state::read_endpoint(&context.state_dir)?;
	let mut client = Client::connect(Path::new(&endpoint))?;
	client.hello(&context.client)?;
	Ok(client)
}

/// Connect, starting a daemon if none answers and starting one is allowed.
pub fn connect_or_spawn(context: &Context) -> Result<Client> {
	match connect(context) {
		Ok(client) => return Ok(client),
		Err(Error::Client(error)) if error.is_connection_busy() => return Err(Error::Client(error)),
		Err(_) => {}
	}

	if !context.allow_spawn || spawn_suppressed() {
		return Err(Error::DaemonUnavailable);
	}

	let log = spawn_detached(context)?;
	wait_for(context, &log)
}

/// Start a daemon without a terminal, pointing its output at a log file so a
/// failure to start can be reported with something to read.
pub fn spawn_detached(context: &Context) -> Result<PathBuf> {
	state::ensure_private_directory(&context.state_dir)?;

	let log = context.state_dir.join("daemon.log");
	let stdout = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
	let stderr = stdout.try_clone()?;

	let mut command = Command::new(context.daemon.program());
	command
		.arg("daemon")
		.arg("--detached")
		.arg("--state-dir")
		.arg(&context.state_dir)
		.stdin(Stdio::null())
		.stdout(stdout)
		.stderr(stderr);

	// The daemon reads configuration too, and it has to read the same file the
	// client did rather than guess at one.
	if let Some(config) = &context.config {
		command.env("RETYPE_CONFIG", config);
	}

	// This daemon was started by a client, not by the supervisor that started the
	// client's shell. The variables are inherited down the whole session, and a
	// systemd user unit for the terminal or a launchd-launched app sets them, so
	// passing them on would claim a supervision that would never restart it.
	command.env_remove("INVOCATION_ID").env_remove("XPC_SERVICE_NAME");

	// A foreground client's Ctrl+C must not also reach the detached daemon.
	#[cfg(unix)]
	{
		use std::os::unix::process::CommandExt;
		command.process_group(0);
	}

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
pub fn wait_for(context: &Context, log: &Path) -> Result<Client> {
	let deadline = Instant::now() + START_TIMEOUT;
	let mut delay = FIRST_DELAY;

	while Instant::now() < deadline {
		if let Ok(client) = connect(context) {
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

/// The command line used to start a daemon, for a log line or a diagnostic.
pub fn daemon_command(context: &Context) -> Vec<OsString> {
	vec![
		context.daemon.program().as_os_str().to_os_string(),
		"daemon".into(),
		"--detached".into(),
	]
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn an_explicit_daemon_wins_over_the_environment() {
		let explicit = Daemon::discover(Some(PathBuf::from("/tmp/bundled-retype"))).unwrap();

		assert_eq!(explicit.program(), Path::new("/tmp/bundled-retype"));
	}

	#[test]
	fn a_context_names_the_daemon_it_will_start() {
		let context = Context::new("/tmp/retype-state")
			.unwrap()
			.with_daemon(Daemon::at("/tmp/retype"))
			.with_client("retype-gui/0.1.0")
			.with_spawn(false);

		assert_eq!(context.state_dir, PathBuf::from("/tmp/retype-state"));
		assert_eq!(context.daemon.program(), Path::new("/tmp/retype"));
		assert_eq!(context.client, "retype-gui/0.1.0");
		assert!(!context.allow_spawn);
	}
}
