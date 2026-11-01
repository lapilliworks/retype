//! Stopping on a signal, `docs/dev/architecture/ipc.md` §2: `SIGTERM` drains, a second one cancels.
//!
//! The tool sleeps for a moment and then copies, so a job can be caught in flight, and
//! a drain has something to wait for.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

/// A tool that takes two seconds to copy: long enough to signal, short enough to drain.
fn slow_config(dir: &Path, seconds: u32) -> PathBuf {
	let config = dir.join("retype.toml");
	fs::write(
		&config,
		format!(
			"\
version = 'retype-config-1'

[backends]
prefer = ['slow']

[tools.slow]
command = 'sh'
layout = ['-c', 'sleep {seconds}; cp \"$1\" \"$2\"', 'slow', '{{input}}', '{{ops}}', '{{output}}']

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

struct Fixture {
	_dir: tempfile::TempDir,
	photos: PathBuf,
	state: PathBuf,
	daemon: Daemon,
}

struct Daemon(Child);

impl Daemon {
	fn pid(&self) -> i32 {
		self.0.id() as i32
	}

	/// Wait for the process to end, answering with its exit code.
	fn wait(&mut self, within: Duration) -> Option<i32> {
		let deadline = Instant::now() + within;

		loop {
			match self.0.try_wait().unwrap() {
				Some(status) => return Some(status.code().unwrap_or(-1)),
				None if Instant::now() >= deadline => return None,
				None => std::thread::sleep(Duration::from_millis(50)),
			}
		}
	}

	fn alive(&mut self) -> bool {
		self.0.try_wait().unwrap().is_none()
	}
}

impl Drop for Daemon {
	fn drop(&mut self) {
		let _ = self.0.kill();
		let _ = self.0.wait();
	}
}

impl Fixture {
	fn new(seconds: u32) -> Self {
		let dir = tempfile::tempdir().unwrap();
		let photos = dir.path().join("photos");
		fs::create_dir(&photos).unwrap();
		let state = dir.path().join("state");
		let config = slow_config(dir.path(), seconds);

		retype()
			.args(["watch", photos.to_str().unwrap()])
			.arg("--state-dir")
			.arg(&state)
			.output()
			.unwrap();

		let log = fs::File::create(dir.path().join("daemon.err")).unwrap();

		let daemon = Daemon(
			retype()
				.args(["daemon", "--detached"])
				.arg("--state-dir")
				.arg(&state)
				.env("RETYPE_CONFIG", &config)
				.stdout(Stdio::null())
				.stderr(Stdio::from(log))
				.spawn()
				.unwrap(),
		);

		let deadline = Instant::now() + Duration::from_secs(20);
		while !state.join("endpoint").exists() {
			assert!(Instant::now() < deadline, "the daemon did not start");
			std::thread::sleep(Duration::from_millis(50));
		}

		Self {
			_dir: dir,
			photos,
			state,
			daemon,
		}
	}

	/// Start a job and wait until it is running, so the signal arrives mid-flight.
	fn start_job(&self) {
		let source = self.photos.join("slow.png");
		fs::write(&source, b"pixels").unwrap();
		fs::rename(&source, self.photos.join("slow.webp")).unwrap();

		let deadline = Instant::now() + Duration::from_secs(20);
		loop {
			if self.state_of_job().as_deref() == Some("running") {
				return;
			}

			assert!(Instant::now() < deadline, "no running job appeared");
			std::thread::sleep(Duration::from_millis(50));
		}
	}

	/// The last state recorded for the one job this fixture makes.
	fn state_of_job(&self) -> Option<String> {
		let journal = fs::read_to_string(self.state.join("jobs.jsonl")).unwrap_or_default();
		let last = journal.lines().last()?;
		let record: serde_json::Value = serde_json::from_str(last).ok()?;

		Some(record["state"].as_str()?.to_string())
	}

	fn signal(&self, signal: i32) {
		assert_eq!(
			unsafe { libc::kill(self.daemon.pid(), signal) },
			0,
			"the daemon did not take {}",
			signal
		);
	}

	fn journal(&self) -> String {
		fs::read_to_string(self.state.join("jobs.jsonl")).unwrap_or_default()
	}

	fn daemon_log(&self) -> String {
		fs::read_to_string(self._dir.path().join("daemon.err")).unwrap_or_default()
	}
}

#[test]
fn a_stop_signal_drains_the_job_it_interrupted() {
	let mut fixture = Fixture::new(2);
	fixture.start_job();

	fixture.signal(libc::SIGTERM);

	// It is still there, finishing what it started.
	std::thread::sleep(Duration::from_millis(300));
	assert!(fixture.daemon.alive(), "the daemon died instead of draining");
	assert_eq!(fixture.state_of_job().as_deref(), Some("running"));

	let code = fixture.daemon.wait(Duration::from_secs(30));
	assert_eq!(code, Some(0), "a drain is a clean exit: {}", fixture.daemon_log());

	// The job that was in flight finished, and the file is where its bytes belong.
	assert_eq!(fixture.state_of_job().as_deref(), Some("done"), "{}", fixture.journal());
	assert!(fixture.photos.join("slow.webp").exists(), "the output is there");
	assert!(!fixture.photos.join("slow.png").exists(), "the source is consumed");
	assert!(
		!fixture.state.join("endpoint").exists(),
		"no path looks like a live daemon"
	);
}

#[test]
fn a_second_signal_cancels_what_the_first_one_waited_for() {
	let mut fixture = Fixture::new(30);
	fixture.start_job();

	fixture.signal(libc::SIGTERM);
	std::thread::sleep(Duration::from_millis(200));
	fixture.signal(libc::SIGTERM);

	let code = fixture.daemon.wait(Duration::from_secs(30));
	assert_eq!(code, Some(0), "{}", fixture.daemon_log());

	// A tool with thirty seconds left was not waited for.
	assert_eq!(
		fixture.state_of_job().as_deref(),
		Some("cancelled"),
		"{}",
		fixture.journal()
	);
	assert!(fixture.photos.join("slow.png").exists(), "the source goes back");
	assert!(
		!fixture.photos.join("slow.webp").exists(),
		"a cancelled job writes nothing"
	);
}
