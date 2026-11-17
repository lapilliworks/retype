//! Reconnect budgets and explicit-stop intent use real disposable daemons.
#![cfg(unix)]
use std::fs;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Fixture {
	dir: tempfile::TempDir,
	state: std::path::PathBuf,
	follower: Child,
}
impl Fixture {
	fn new() -> Self {
		let dir = tempfile::tempdir().unwrap();
		let state = dir.path().join("state");
		let photos = dir.path().join("photos");
		fs::create_dir(&photos).unwrap();
		let config = dir.path().join("config.toml");
		fs::write(&config, "version = 'retype-config-1'\n[notify]\nenabled = false\n").unwrap();
		let follower = Command::new(env!("CARGO_BIN_EXE_retype"))
			.arg("watch")
			.arg(photos)
			.args(["--json", "--state-dir"])
			.arg(&state)
			.env("RETYPE_CONFIG", config)
			.env("RETYPE_TEST_SESSION", "terminal")
			.env_remove("RETYPE_NO_SPAWN")
			.stdin(Stdio::null())
			.stdout(fs::File::create(dir.path().join("events")).unwrap())
			.stderr(fs::File::create(dir.path().join("errors")).unwrap())
			.spawn()
			.unwrap();
		Self { dir, state, follower }
	}
	fn ready(&mut self, previous: Option<u32>) -> u32 {
		let deadline = Instant::now() + Duration::from_secs(12);
		loop {
			if let Ok(mut client) = retype_cli::client::connect(&self.state) {
				let status = client.status().unwrap();
				if previous != Some(status.pid)
					&& status
						.watches
						.first()
						.is_some_and(|watch| watch.status.as_deref() == Some("ready"))
				{
					return status.pid;
				}
			}
			assert!(
				self.follower.try_wait().unwrap().is_none(),
				"follower stopped: {}\n{}",
				fs::read_to_string(self.dir.path().join("errors")).unwrap(),
				fs::read_to_string(self.dir.path().join("events")).unwrap()
			);
			assert!(Instant::now() < deadline, "daemon did not reconnect");
			std::thread::sleep(Duration::from_millis(50));
		}
	}
	fn stopped(&mut self) {
		let deadline = Instant::now() + Duration::from_secs(8);
		while self.follower.try_wait().unwrap().is_none() {
			assert!(Instant::now() < deadline, "follower did not stop");
			std::thread::sleep(Duration::from_millis(50));
		}
	}
}
impl Drop for Fixture {
	fn drop(&mut self) {
		let _ = self.follower.kill();
		let _ = self.follower.wait();
		if let Ok(mut client) = retype_cli::client::connect(&self.state) {
			let _ = client.shutdown(&retype_ipc::ShutdownRequest { now: true, force: true });
		}
		retype_cli::client::wait_for_stop(&self.state, Duration::from_secs(3));
	}
}

#[test]
fn three_unknown_crashes_stop_the_shared_restart_budget() {
	let mut fixture = Fixture::new();
	let mut pid = fixture.ready(None);
	for _ in 0..2 {
		assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
		pid = fixture.ready(Some(pid));
	}
	assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
	fixture.stopped();
	let lifecycle: serde_json::Value =
		serde_json::from_slice(&fs::read(fixture.state.join("lifecycle.json")).unwrap()).unwrap();
	assert_eq!(
		lifecycle["unknown"],
		3,
		"{}\n{}",
		fs::read_to_string(fixture.dir.path().join("errors")).unwrap(),
		fs::read_to_string(fixture.dir.path().join("events")).unwrap()
	);
	let events = fs::read_to_string(fixture.dir.path().join("events")).unwrap();
	assert!(events.contains("three crashes"), "{events}");
}

#[test]
fn a_clean_explicit_stop_does_not_respawn_the_daemon() {
	let mut fixture = Fixture::new();
	fixture.ready(None);
	retype_cli::client::connect(&fixture.state)
		.unwrap()
		.shutdown(&retype_ipc::ShutdownRequest { now: true, force: true })
		.unwrap();
	fixture.stopped();
	assert!(retype_cli::client::wait_for_stop(
		&fixture.state,
		Duration::from_secs(3)
	));
	let errors = fs::read_to_string(fixture.dir.path().join("errors")).unwrap();
	assert!(
		errors.contains("explicitly stopped")
			|| fs::read_to_string(fixture.dir.path().join("events"))
				.unwrap()
				.contains("explicitly stopped"),
		"{errors}\n{}",
		fs::read_to_string(fixture.dir.path().join("events")).unwrap()
	);
}
