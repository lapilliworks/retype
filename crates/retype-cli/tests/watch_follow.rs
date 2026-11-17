//! The default watch command establishes active watching and detaches independently.
#![cfg(unix)]

use std::fs;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn fresh_watch_starts_waits_for_readiness_and_interrupt_only_detaches() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let photos = dir.path().join("photos");
	fs::create_dir(&photos).unwrap();
	let config = dir.path().join("config.toml");
	fs::write(&config, "version = 'retype-config-1'\n[notify]\nenabled = false\n").unwrap();
	let log = dir.path().join("watch.jsonl");
	let mut watch = Command::new(env!("CARGO_BIN_EXE_retype"))
		.arg("watch")
		.arg(&photos)
		.args(["--json", "--state-dir"])
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.env_remove("RETYPE_NO_SPAWN")
		.stdin(Stdio::null())
		.stdout(fs::File::create(&log).unwrap())
		.stderr(Stdio::null())
		.process_group(0)
		.spawn()
		.unwrap();
	struct Cleanup<'a> {
		child: &'a mut std::process::Child,
		state: &'a std::path::Path,
	}
	impl Drop for Cleanup<'_> {
		fn drop(&mut self) {
			let _ = self.child.kill();
			let _ = self.child.wait();
			if let Ok(mut client) = retype_cli::client::connect(self.state) {
				let _ = client.shutdown(&retype_ipc::ShutdownRequest { force: true, now: true });
			}
			retype_cli::client::wait_for_stop(self.state, Duration::from_secs(3));
		}
	}
	let cleanup = Cleanup {
		child: &mut watch,
		state: &state,
	};
	let deadline = Instant::now() + Duration::from_secs(15);
	loop {
		let text = fs::read_to_string(&log).unwrap();
		if text
			.lines()
			.filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
			.any(|value| value["status"] == "ready")
		{
			break;
		}
		assert!(
			cleanup.child.try_wait().unwrap().is_none(),
			"watch exited before readiness: {text}"
		);
		assert!(Instant::now() < deadline, "watch never reported readiness: {text}");
		std::thread::sleep(Duration::from_millis(25));
	}
	assert_eq!(unsafe { libc::kill(cleanup.child.id() as i32, libc::SIGINT) }, 0);
	while cleanup.child.try_wait().unwrap().is_none() {
		assert!(Instant::now() < deadline, "interrupt did not detach");
		std::thread::sleep(Duration::from_millis(25));
	}
	let status = retype_cli::client::connect(&state).unwrap().status().unwrap();
	assert_eq!(status.watches.len(), 1);
	assert_eq!(status.watches[0].status.as_deref(), Some("ready"));
}

#[test]
fn watch_honours_spawn_suppression_and_registration_is_explicit() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let run = |register| {
		let mut command = Command::new(env!("CARGO_BIN_EXE_retype"));
		command
			.arg("watch")
			.arg(dir.path())
			.args(["--no-spawn", "--state-dir"])
			.arg(&state);
		if register {
			command.arg("--register");
		}
		command.output().unwrap()
	};
	assert_eq!(run(false).status.code(), Some(6));
	assert!(run(true).status.success());
	assert!(!state.join("endpoint").exists());
	assert!(state.join("watches.toml").exists());
}
