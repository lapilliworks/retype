//! Cancellation keeps completed outputs for undo and releases only redundant no-output recovery.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Daemon {
	child: Child,
	state: PathBuf,
}

impl Drop for Daemon {
	fn drop(&mut self) {
		if let Ok(mut client) = retype_cli::client::connect(&self.state) {
			let _ = client.shutdown(&retype_ipc::ShutdownRequest { now: true, force: true });
		}
		let _ = self.child.kill();
		let _ = self.child.wait();
	}
}

fn configuration(dir: &Path) -> PathBuf {
	let marker = dir.join("second-started");
	let tool = dir.join("split-tool.sh");
	fs::write(
		&tool,
		format!(
			"#!/bin/sh\ncase \"$2\" in\n  *.webp) cp \"$1\" \"$2\" ;;\n  *) touch '{}'; sleep 30; cp \"$1\" \"$2\" ;;\nesac\n",
			marker.display()
		),
	)
	.unwrap();
	fs::set_permissions(&tool, fs::Permissions::from_mode(0o700)).unwrap();
	let config = dir.join("retype.toml");
	fs::write(
		&config,
		format!(
			"version = 'retype-config-1'\n\
[notify]\nenabled = false\n\
[backends]\nprefer = ['split']\n\
[tools.split]\ncommand = '{}'\nlayout = ['{{input}}', '{{ops}}', '{{output}}']\n\
[formats.png]\nfamily = 'image'\nlossy = false\nalpha = true\ntier = 'exact'\n\
[formats.webp]\nfamily = 'image'\nlossy = true\nalpha = true\ntier = 'converted'\n\
[ops.fliph.argv.split]\nargs = []\n\
[ops.flipv.argv.split]\nargs = []\n",
			tool.display()
		),
	)
	.unwrap();
	config
}

fn start_daemon(state: &Path, config: &Path) -> Daemon {
	let daemon = Daemon {
		child: Command::new(env!("CARGO_BIN_EXE_retype"))
			.args(["daemon", "--detached"])
			.arg("--state-dir")
			.arg(state)
			.env("RETYPE_CONFIG", config)
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.spawn()
			.unwrap(),
		state: state.to_path_buf(),
	};
	let deadline = Instant::now() + Duration::from_secs(10);
	while retype_cli::client::connect(state).is_err() {
		assert!(Instant::now() < deadline, "the daemon did not start");
		std::thread::sleep(Duration::from_millis(25));
	}
	daemon
}

fn cancel_and_wait(state: &Path, id: &retype_core::job::JobId) -> retype_core::job::Job {
	let cancelled = {
		let mut client = retype_cli::client::connect(state).unwrap();
		client.jobs_cancel(id.as_str()).unwrap()
	};
	assert_eq!(cancelled.state, "cancelling");
	let deadline = Instant::now() + Duration::from_secs(10);
	loop {
		let job = {
			let mut client = retype_cli::client::connect(state).unwrap();
			client.jobs_get(id.as_str()).unwrap()
		};
		if job.state == retype_core::job::JobState::Cancelled {
			return job;
		}
		assert!(
			Instant::now() < deadline,
			"cancellation did not finish: {:?}",
			job.state
		);
		std::thread::sleep(Duration::from_millis(25));
	}
}

fn wait_for_marker(marker: &Path, deadline: Instant) {
	while !marker.exists() {
		assert!(Instant::now() < deadline, "the tool did not start");
		std::thread::sleep(Duration::from_millis(25));
	}
}

fn wait_for_watcher(state: &Path, deadline: Instant) {
	loop {
		let status = {
			let mut client = retype_cli::client::connect(state).unwrap();
			client.status().unwrap()
		};
		if status.watches.first().and_then(|watch| watch.status.as_deref()) == Some("ready") {
			return;
		}
		assert!(
			Instant::now() < deadline,
			"the watcher did not become ready: {status:?}"
		);
		std::thread::sleep(Duration::from_millis(25));
	}
}

fn assert_retained(state: &Path, id: &retype_core::job::JobId) {
	let record = retype_core::undo::Store::open(state).load(id).unwrap().unwrap();
	assert!(!record.pinned);
	assert!(record.retention_started.is_some());
}

#[test]
fn cancelling_a_running_job_starts_retention_for_its_completed_output() {
	let dir = tempfile::tempdir().unwrap();
	let config = configuration(dir.path());
	let state = dir.path().join("state");
	let source = dir.path().join("photo.png");
	let produced = dir.path().join("photo-fliph.webp");
	let pending = dir.path().join("photo-flipv.png");
	fs::write(&source, b"pixels").unwrap();
	let _daemon = start_daemon(&state, &config);
	let deadline = Instant::now() + Duration::from_secs(10);
	let submitted = {
		let mut client = retype_cli::client::connect(&state).unwrap();
		client
			.jobs_submit(&retype_ipc::JobSubmit {
				input: source.clone(),
				intent: "webp+fliph,png+flipv".to_string(),
				output: None,
				force: false,
				mode: retype_ipc::SubmitMode::Immediate,
			})
			.unwrap()
	};
	wait_for_marker(&dir.path().join("second-started"), deadline);
	assert_eq!(fs::read(&produced).unwrap(), b"pixels");
	let job = cancel_and_wait(&state, &submitted.id);
	assert!(job.error.is_none(), "{:?}", job.error);
	assert_eq!(fs::read(source).unwrap(), b"pixels");
	assert_eq!(fs::read(produced).unwrap(), b"pixels");
	assert!(!pending.exists());
	assert_retained(&state, &submitted.id);
}

#[test]
fn cancelling_a_running_watched_job_keeps_its_completed_output() {
	let dir = tempfile::tempdir().unwrap();
	let config = configuration(dir.path());
	let state = dir.path().join("state");
	let photos = dir.path().join("photos");
	fs::create_dir(&photos).unwrap();
	let source = photos.join("photo.png");
	let produced = photos.join("photo-fliph.webp");
	let pending = photos.join("photo-flipv.png");
	fs::write(&source, b"pixels").unwrap();
	let registered = Command::new(env!("CARGO_BIN_EXE_retype"))
		.args(["watch", "--register"])
		.arg(&photos)
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.output()
		.unwrap();
	assert!(registered.status.success());
	let _daemon = start_daemon(&state, &config);
	let deadline = Instant::now() + Duration::from_secs(10);
	wait_for_watcher(&state, deadline);
	fs::rename(&source, photos.join("photo.webp+fliph,png+flipv")).unwrap();
	wait_for_marker(&dir.path().join("second-started"), deadline);
	let id = {
		let mut client = retype_cli::client::connect(&state).unwrap();
		client
			.jobs_list()
			.unwrap()
			.jobs
			.into_iter()
			.find(|job| job.source == source)
			.unwrap()
			.id
	};
	let job = cancel_and_wait(&state, &id);
	assert!(job.error.is_none(), "{:?}", job.error);
	assert_eq!(fs::read(source).unwrap(), b"pixels");
	assert_eq!(fs::read(produced).unwrap(), b"pixels");
	assert!(!pending.exists());
	assert_retained(&state, &id);
}

#[test]
fn cancelling_a_running_cli_job_without_outputs_releases_its_redundant_copy() {
	let dir = tempfile::tempdir().unwrap();
	let config = configuration(dir.path());
	let state = dir.path().join("state");
	let source = dir.path().join("photo.png");
	let original = b"original pixels";
	fs::write(&source, original).unwrap();
	let _daemon = start_daemon(&state, &config);
	let submitted = {
		let mut client = retype_cli::client::connect(&state).unwrap();
		client
			.jobs_submit(&retype_ipc::JobSubmit {
				input: source.clone(),
				intent: "png+flipv".to_string(),
				output: None,
				force: false,
				mode: retype_ipc::SubmitMode::Immediate,
			})
			.unwrap()
	};
	wait_for_marker(
		&dir.path().join("second-started"),
		Instant::now() + Duration::from_secs(10),
	);
	assert!(
		retype_core::undo::Store::open(&state)
			.load(&submitted.id)
			.unwrap()
			.is_some()
	);
	let job = cancel_and_wait(&state, &submitted.id);
	assert!(job.error.is_none(), "{:?}", job.error);
	assert_eq!(fs::read(source).unwrap(), original);
	assert!(!dir.path().join("photo-flipv.png").exists());
	assert!(
		retype_core::undo::Store::open(&state)
			.load(&submitted.id)
			.unwrap()
			.is_none()
	);
}

#[test]
fn cancelling_a_running_watched_job_without_outputs_keeps_history_but_not_redundant_recovery() {
	let dir = tempfile::tempdir().unwrap();
	let config = configuration(dir.path());
	let state = dir.path().join("state");
	let photos = dir.path().join("photos");
	fs::create_dir(&photos).unwrap();
	let source = photos.join("photo.png");
	let original = b"original pixels";
	fs::write(&source, original).unwrap();
	let registered = Command::new(env!("CARGO_BIN_EXE_retype"))
		.args(["watch", "--register"])
		.arg(&photos)
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.output()
		.unwrap();
	assert!(registered.status.success(), "{registered:?}");
	let _daemon = start_daemon(&state, &config);
	let deadline = Instant::now() + Duration::from_secs(10);
	wait_for_watcher(&state, deadline);
	fs::rename(&source, photos.join("photo.png+flipv")).unwrap();
	wait_for_marker(&dir.path().join("second-started"), deadline);
	let id = {
		let mut client = retype_cli::client::connect(&state).unwrap();
		client
			.jobs_list()
			.unwrap()
			.jobs
			.into_iter()
			.find(|job| job.source == source)
			.unwrap()
			.id
	};
	let job = cancel_and_wait(&state, &id);
	assert!(job.error.is_none(), "{:?}", job.error);
	assert_eq!(job.source.path, source);
	assert_eq!(fs::read(&source).unwrap(), original);
	assert!(!photos.join("photo-flipv.png").exists());
	assert!(retype_core::undo::Store::open(&state).load(&id).unwrap().is_none());
}
