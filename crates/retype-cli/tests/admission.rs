//! Content capture and slow tool preparation must leave the daemon's control path usable.
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

#[test]
fn content_acceptance_precedes_probes_and_rejects_later_same_size_edits() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let config = dir.path().join("config.toml");
	let marker = dir.path().join("probe-started");
	let release = dir.path().join("release-probe");
	let source = dir.path().join("photo.png");
	fs::write(&source, b"original").unwrap();
	let modified = fs::metadata(&source).unwrap().modified().unwrap();
	let expected = retype_core::safety::Fingerprint::of(&source).unwrap();
	let probe = serde_json::to_string(&format!(
		": > '{}'; while [ ! -e '{}' ]; do sleep 0.01; done; printf '1.0.0\\n'",
		marker.display(),
		release.display(),
	))
	.unwrap();
	fs::write(&config, format!("version = 'retype-config-1'\n[notify]\nenabled = false\n[backends]\nprefer = ['slow']\n[tools.slow]\ncommand = 'sh'\nprobe = ['-c', {probe}]\nlayout = ['{{input}}', '{{ops}}', '{{output}}']\n[formats.png]\nfamily = 'image'\nlossy = false\nalpha = true\ntier = 'converted'\n[formats.webp]\nfamily = 'image'\nlossy = true\nalpha = true\ntier = 'converted'\n")).unwrap();
	let context = retype_client::Context::new(&state)
		.unwrap()
		.with_daemon(retype_client::Daemon::at(env!("CARGO_BIN_EXE_retype")))
		.with_config(Some(config));
	struct Stop(std::process::Child);
	impl Drop for Stop {
		fn drop(&mut self) {
			let _ = self.0.kill();
			let _ = self.0.wait();
		}
	}
	let daemon = Command::new(env!("CARGO_BIN_EXE_retype"))
		.args(["daemon", "--detached", "--state-dir"])
		.arg(&state)
		.env("RETYPE_CONFIG", context.config.as_ref().unwrap())
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.spawn()
		.unwrap();
	let _stop = Stop(daemon);
	let deadline = Instant::now() + Duration::from_secs(10);
	while retype_client::connect(&context).is_err() {
		assert!(Instant::now() < deadline, "fixture daemon did not become ready");
		std::thread::sleep(Duration::from_millis(20));
	}
	let accepted = retype_client::jobs::submit(
		&context,
		&retype_ipc::JobSubmit {
			input: source.clone(),
			intent: "webp".into(),
			output: None,
			force: false,
			mode: retype_ipc::SubmitMode::Immediate,
		},
	)
	.unwrap();
	let captured = retype_client::connect(&context)
		.unwrap()
		.jobs_get(accepted.id.as_str())
		.unwrap();
	assert_eq!(captured.source.fingerprint, Some(expected));
	let deadline = Instant::now() + Duration::from_secs(5);
	while !marker.exists() {
		assert!(Instant::now() < deadline, "worker did not start the probe");
		std::thread::sleep(Duration::from_millis(10));
	}
	retype_client::connect(&context).unwrap().status().unwrap();
	fs::write(&source, b"modified").unwrap();
	fs::File::options()
		.write(true)
		.open(&source)
		.unwrap()
		.set_times(fs::FileTimes::new().set_modified(modified))
		.unwrap();
	fs::write(release, b"ready").unwrap();
	let settled = retype_client::jobs::wait(&context, accepted.id.as_str(), |_| {}).unwrap();
	assert_eq!(settled.state, retype_core::job::JobState::Failed, "{settled:?}");
	assert_eq!(settled.error.unwrap().code, "E_JOB_INTERRUPTED");
	assert_eq!(fs::read(source).unwrap(), b"modified");
	assert!(!dir.path().join("photo.webp").exists());
}

#[test]
fn losing_content_acceptance_keeps_the_received_handle_for_inspection() {
	use retype_ipc::{Response, read_frame, write_frame};
	use std::io::BufReader;
	use std::os::unix::net::UnixListener;

	let dir = tempfile::tempdir().unwrap();
	let socket = dir.path().join("daemon.sock");
	let listener = UnixListener::bind(&socket).unwrap();
	fs::write(dir.path().join("endpoint"), socket.to_str().unwrap()).unwrap();
	let server = std::thread::spawn(move || {
		let mut received = serde_json::Value::Null;
		for connection in 0..2 {
			let (stream, _) = listener.accept().unwrap();
			stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
			let mut stream = BufReader::new(stream);
			let hello = read_frame(&mut stream).unwrap().unwrap();
			write_frame(
				stream.get_mut(),
				&Response::ok(
					hello["id"].as_u64().unwrap(),
					serde_json::json!({
						"ipc": retype_ipc::IPC_VERSION, "pid": 1, "machine": "test", "uptime_ms": 0,
						"features": ["jobs.submit.id"], "capabilities": {}
					}),
				)
				.unwrap(),
			)
			.unwrap();
			let request = read_frame(&mut stream).unwrap().unwrap();
			if connection == 0 {
				assert_eq!(request["method"], "jobs.submit");
				received = request["params"]["job_id"].clone();
				write_frame(
					stream.get_mut(),
					&Response::ok(
						request["id"].as_u64().unwrap(),
						serde_json::json!({"id": received, "state": "queued"}),
					)
					.unwrap(),
				)
				.unwrap();
			} else {
				assert_eq!(request["method"], "jobs.get");
			}
		}
		received
	});
	let context = retype_client::Context::new(dir.path()).unwrap().with_spawn(false);
	let error = retype_client::jobs::submit(
		&context,
		&retype_ipc::JobSubmit {
			input: dir.path().join("photo.png"),
			intent: "webp".into(),
			output: None,
			force: false,
			mode: retype_ipc::SubmitMode::Immediate,
		},
	)
	.unwrap_err();
	let retype_client::Error::Client(retype_ipc::ClientError::Rpc(error)) = error else {
		panic!("content acceptance failure lost the structured request handle");
	};
	let detail = error.detail.unwrap();
	assert_eq!(detail["id"], server.join().unwrap());
	assert_eq!(detail["acceptance"], "unknown");
	assert!(error.message.contains("before retrying"));
}
