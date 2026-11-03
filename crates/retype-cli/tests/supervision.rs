//! A supervised daemon refuses to be stopped, and `--now` cancels what is running,

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

mod support;

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

fn slow_config(dir: &Path) -> PathBuf {
	let (command, layout) = match cfg!(windows) {
		true => (
			"cmd",
			"layout = ['/c', 'ping -n 30 127.0.0.1', '{input}', '{ops}', '{output}']",
		),
		false => ("sh", "layout = ['-c', 'sleep 30', '{input}', '{ops}', '{output}']"),
	};

	let config = dir.join("retype.toml");
	fs::write(
		&config,
		format!(
			"\
version = 'retype-config-1'

[notify]
enabled = false

[backends]
prefer = ['slow']

[tools.slow]
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

	config
}

struct Daemon(Child);

impl Drop for Daemon {
	fn drop(&mut self) {
		let _ = self.0.kill();
		let _ = self.0.wait();
	}
}

/// Start a daemon, optionally with the environment a supervisor would set.
fn start(state: &Path, config: &Path, supervised: bool) -> Daemon {
	let mut command = retype();
	command
		.args(["daemon", "--detached"])
		.arg("--state-dir")
		.arg(state)
		.env("RETYPE_CONFIG", config)
		// A Konsole session on a systemd desktop and a launchd-launched app set
		// these for every process in the session, so the test says which it wants
		// rather than inheriting whatever started it.
		.env_remove("INVOCATION_ID")
		.env_remove("XPC_SERVICE_NAME")
		.stdout(Stdio::null())
		.stderr(Stdio::null());

	if supervised {
		command.env("INVOCATION_ID", "test-unit");
	}

	let child = command.spawn().unwrap();

	let endpoint = state.join("endpoint");
	let deadline = Instant::now() + Duration::from_secs(20);
	while !endpoint.exists() {
		assert!(Instant::now() < deadline, "the daemon did not start");
		std::thread::sleep(Duration::from_millis(50));
	}

	Daemon(child)
}

#[test]
fn a_supervised_daemon_refuses_to_stop_and_says_why() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let config = slow_config(dir.path());
	let daemon = start(&state, &config, true);

	let refused = retype()
		.args(["daemon", "stop"])
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.output()
		.unwrap();

	assert_eq!(refused.status.code(), Some(3), "a refusal is a diagnosis");
	assert!(
		String::from_utf8_lossy(&refused.stderr).contains("E_IPC_SUPERVISED"),
		"{}",
		String::from_utf8_lossy(&refused.stderr)
	);
	assert!(
		String::from_utf8_lossy(&refused.stderr).contains("systemd"),
		"the message names the supervisor: {}",
		String::from_utf8_lossy(&refused.stderr)
	);

	// Still there, which is the point of refusing.
	let status = retype()
		.args(["daemon", "status", "--json"])
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.output()
		.unwrap();

	assert!(status.status.success(), "the daemon is still answering");
	assert!(
		String::from_utf8_lossy(&status.stdout).contains("\"systemd\""),
		"{}",
		String::from_utf8_lossy(&status.stdout)
	);

	let forced = retype()
		.args(["daemon", "stop", "--force"])
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.output()
		.unwrap();

	assert!(forced.status.success(), "{}", String::from_utf8_lossy(&forced.stderr));
	drop(daemon);
}

#[test]
fn a_stop_now_cancels_a_running_job_and_records_it() {
	let dir = tempfile::tempdir().unwrap();
	let photos = dir.path().join("photos");
	fs::create_dir(&photos).unwrap();
	let state = dir.path().join("state");
	let config = slow_config(dir.path());

	// Registration first: the daemon reads the file when it starts, so a rename
	// before that is a rename in a directory nobody is watching.
	retype()
		.args(["watch", photos.to_str().unwrap()])
		.arg("--state-dir")
		.arg(&state)
		.output()
		.unwrap();

	let _daemon = start(&state, &config, false);

	let source = photos.join("photo.png");
	let before = support::observed(&state);
	fs::write(&source, b"pixels").unwrap();
	support::observe(&state, before);
	fs::rename(&source, photos.join("photo.webp")).unwrap();

	// Wait until the job is running rather than merely queued, so the cancel has a
	// process to kill.
	let journal = state.join("jobs.jsonl");
	let deadline = Instant::now() + Duration::from_secs(20);
	loop {
		let recorded = fs::read_to_string(&journal).unwrap_or_default();
		if recorded.contains("\"state\":\"running\"") {
			break;
		}
		assert!(Instant::now() < deadline, "the job never started: {recorded}");
		std::thread::sleep(Duration::from_millis(50));
	}

	let stopped = retype()
		.args(["daemon", "stop", "--now"])
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.output()
		.unwrap();

	assert!(stopped.status.success(), "{}", String::from_utf8_lossy(&stopped.stderr));
	assert!(
		String::from_utf8_lossy(&stopped.stdout).contains("cancelled"),
		"{}",
		String::from_utf8_lossy(&stopped.stdout)
	);

	let recorded = fs::read_to_string(&journal).unwrap();
	let cancelled = recorded
		.lines()
		.filter(|line| line.contains("\"state\":\"cancelled\""))
		.count();

	assert!(cancelled >= 1, "the cancel was recorded: {recorded}");

	// The journal is append-only, so the `running` record stays; what matters is that
	// the job's last word is `cancelled` rather than a record that never ended.
	let last = recorded.lines().last().unwrap_or_default();
	assert!(
		last.contains("\"state\":\"cancelled\""),
		"the job ends cancelled: {last}"
	);
}
