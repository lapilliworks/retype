//! Slow tool preparation must leave the daemon's control path usable.
#![cfg(unix)]

use std::fs;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn a_slow_probe_returns_a_durable_handle_and_can_be_cancelled() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let config = dir.path().join("config.toml");
	let marker = dir.path().join("probe-started");
	let source = dir.path().join("photo.png");
	fs::write(&source, b"original").unwrap();
	let probe = serde_json::to_string(&format!(": > '{}'; exec sleep 30", marker.display())).unwrap();
	fs::write(&config, format!("version = 'retype-config-1'\n[notify]\nenabled = false\n[backends]\nprefer = ['slow']\n[tools.slow]\ncommand = 'sh'\nprobe = ['-c', {probe}]\nlayout = ['{{input}}', '{{ops}}', '{{output}}']\n[formats.png]\nfamily = 'image'\nlossy = false\nalpha = true\ntier = 'converted'\n[formats.webp]\nfamily = 'image'\nlossy = true\nalpha = true\ntier = 'converted'\n")).unwrap();
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
	let deadline = Instant::now() + Duration::from_secs(10);
	while retype_cli::client::connect(&state).is_err() {
		assert!(Instant::now() < deadline, "daemon did not start");
		std::thread::sleep(Duration::from_millis(20));
	}
	let started = Instant::now();
	let submitted = retype_cli::client::connect(&state)
		.unwrap()
		.jobs_submit(&retype_ipc::JobSubmit {
			input: source.clone(),
			intent: "webp".into(),
			output: None,
			force: false,
			mode: retype_ipc::SubmitMode::Immediate,
		})
		.unwrap();
	assert!(
		started.elapsed() < Duration::from_secs(2),
		"submission waited for the probe"
	);
	let journal = fs::read_to_string(state.join("jobs.jsonl")).unwrap();
	assert!(
		journal.contains(submitted.id.as_str()),
		"accepted work has no durable handle"
	);
	while !marker.exists() {
		assert!(Instant::now() < deadline, "worker did not start the probe");
		std::thread::sleep(Duration::from_millis(20));
	}
	let started = Instant::now();
	retype_cli::client::connect(&state).unwrap().status().unwrap();
	retype_cli::client::connect(&state)
		.unwrap()
		.jobs_cancel(submitted.id.as_str())
		.unwrap();
	assert!(
		started.elapsed() < Duration::from_secs(2),
		"control requests waited for the probe"
	);
	loop {
		let job = retype_cli::client::connect(&state)
			.unwrap()
			.jobs_get(submitted.id.as_str())
			.unwrap();
		if job.state == retype_core::job::JobState::Cancelled {
			break;
		}
		assert!(
			Instant::now() < deadline,
			"preparation did not settle after cancellation: {:?}",
			job.state
		);
		std::thread::sleep(Duration::from_millis(20));
	}
	assert!(
		started.elapsed() < Duration::from_secs(2),
		"cancellation waited for the probe deadline"
	);
	assert_eq!(fs::read(source).unwrap(), b"original");
	assert!(!dir.path().join("photo.webp").exists());

	use std::os::unix::ffi::OsStrExt;
	let fifo = dir.path().join("pipe.png");
	let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
	assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
	let started = Instant::now();
	let error = retype_cli::client::connect(&state)
		.unwrap()
		.jobs_submit(&retype_ipc::JobSubmit {
			input: fifo,
			intent: "webp".into(),
			output: None,
			force: false,
			mode: retype_ipc::SubmitMode::Immediate,
		})
		.unwrap_err();
	assert!(started.elapsed() < Duration::from_secs(2));
	let retype_ipc::ClientError::Rpc(error) = error else {
		panic!("non-regular input was not diagnosed");
	};
	assert_eq!(error.code, retype_core::error::Code::UsageArgs);
	retype_cli::client::connect(&state).unwrap().status().unwrap();
}
