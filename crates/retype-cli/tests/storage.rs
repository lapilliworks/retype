//! Undo admission keeps recovery while later work waits for storage.
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn crossing_the_goal_parks_the_next_job_and_configuration_reload_releases_it() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let config = dir.path().join("config.toml");
	fs::write(
		&config,
		"version = 'retype-config-1'\n[notify]\nenabled = false\n[undo]\nquota = '1kb'\n",
	)
	.unwrap();
	let mut daemon = Command::new(env!("CARGO_BIN_EXE_retype"))
		.args(["daemon", "--detached", "--state-dir"])
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.spawn()
		.unwrap();
	struct Stop<'a>(&'a mut std::process::Child);
	impl Drop for Stop<'_> {
		fn drop(&mut self) {
			let _ = self.0.kill();
			let _ = self.0.wait();
		}
	}
	let _stop = Stop(&mut daemon);
	let deadline = Instant::now() + Duration::from_secs(15);
	while retype_cli::client::connect(&state).is_err() {
		assert!(Instant::now() < deadline, "daemon did not start");
		std::thread::sleep(Duration::from_millis(20));
	}
	let convert = |source: &Path| {
		Command::new(env!("CARGO_BIN_EXE_retype"))
			.arg("convert")
			.arg(source)
			.args(["zip", "--json", "--no-spawn", "--state-dir"])
			.arg(&state)
			.env("RETYPE_CONFIG", &config)
			.output()
			.unwrap()
	};
	for name in ["first", "second"] {
		let file = fs::File::create(dir.path().join(format!("{name}.tar"))).unwrap();
		let mut tar = tar::Builder::new(file);
		let mut header = tar::Header::new_gnu();
		header.set_size(7);
		header.set_mode(0o600);
		header.set_mtime(0);
		header.set_cksum();
		tar.append_data(&mut header, "entry.txt", &b"payload"[..]).unwrap();
		tar.finish().unwrap();
	}
	let first = convert(&dir.path().join("first.tar"));
	assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stdout));
	let second = convert(&dir.path().join("second.tar"));
	assert_eq!(second.status.code(), Some(7));
	let response: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
	assert_eq!(response["error"]["code"], "E_UNDO_LIMIT");
	assert_eq!(response["error"]["detail"]["reason"], "storage");
	let id = response["error"]["detail"]["id"].as_str().unwrap();
	assert!(!dir.path().join("second.zip").exists());
	assert!(dir.path().join("second.tar").exists());
	let mut client = retype_cli::client::connect(&state).unwrap();
	let usage = client.status().unwrap().undo.unwrap();
	assert!(usage.overage > 0);
	assert!(usage.retained > usage.goal);
	let waiting = client.jobs_get(id).unwrap();
	assert_eq!(waiting.parked.unwrap().reason, "storage");
	assert_eq!(client.undo_list().unwrap().records.len(), 1);
	drop(client);
	fs::write(
		&config,
		"version = 'retype-config-1'\n[notify]\nenabled = false\n[undo]\nquota = '1mb'\n",
	)
	.unwrap();
	retype_cli::client::connect(&state).unwrap().config_reload().unwrap();
	loop {
		let job = retype_cli::client::connect(&state).unwrap().jobs_get(id).unwrap();
		if job.state == retype_core::job::JobState::Done {
			break;
		}
		assert!(
			Instant::now() < deadline,
			"storage job did not finish: {:?} {:?}",
			job.state,
			job.error
		);
		std::thread::sleep(Duration::from_millis(25));
	}
	assert!(dir.path().join("second.zip").exists());
	assert_eq!(
		retype_cli::client::connect(&state)
			.unwrap()
			.undo_list()
			.unwrap()
			.records
			.len(),
		2
	);
}
