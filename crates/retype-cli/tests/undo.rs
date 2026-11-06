//! `retype undo`: a rename in a watched directory converts, and undo puts the file
//! back exactly as it was.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

mod support;

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

fn copy_config(dir: &Path) -> PathBuf {
	let (command, layout) = match cfg!(windows) {
		true => ("cmd", "layout = ['/c', 'copy', '{input}', '{ops}', '{output}']"),
		false => ("cp", "layout = ['{input}', '{ops}', '{output}']"),
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
prefer = ['copy']

[tools.copy]
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

fn archive_bytes() -> Vec<u8> {
	let mut archive = tar::Builder::new(Vec::new());
	let mut header = tar::Header::new_gnu();
	header.set_size(7);
	header.set_mode(0o644);
	header.set_cksum();
	archive.append_data(&mut header, "file.txt", &b"payload"[..]).unwrap();
	archive.into_inner().unwrap()
}

fn assert_daemon_reachable(state: &Path) {
	let deadline = Instant::now() + Duration::from_secs(3);
	loop {
		if let Ok(mut client) = retype_cli::client::connect(state) {
			if client.status().is_ok() {
				return;
			}
		}
		assert!(Instant::now() < deadline, "the daemon stopped after refusing undo");
		std::thread::sleep(Duration::from_millis(25));
	}
}

fn assert_undo_refused_without_stopping_daemon(undo: &std::process::Output, state: &Path) {
	assert!(!undo.status.success());
	let stderr = String::from_utf8_lossy(&undo.stderr);
	assert!(stderr.contains("directory durability"), "{stderr}");
	assert_daemon_reachable(state);
}

#[test]
fn a_watched_native_archive_conversion_restores_the_original_on_undo() {
	let dir = tempfile::tempdir().unwrap();
	let photos = dir.path().join("photos");
	fs::create_dir(&photos).unwrap();
	let state = dir.path().join("state");
	let config = copy_config(dir.path());

	let registered = retype()
		.args(["watch", photos.to_str().unwrap()])
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.output()
		.unwrap();
	assert!(registered.status.success());
	let _daemon = Daemon(
		retype()
			.args(["daemon", "--detached"])
			.arg("--state-dir")
			.arg(&state)
			.env("RETYPE_CONFIG", &config)
			.stdout(Stdio::null())
			.stderr(Stdio::from(fs::File::create(dir.path().join("daemon.log")).unwrap()))
			.spawn()
			.unwrap(),
	);
	let deadline = Instant::now() + Duration::from_secs(20);
	while !state.join("endpoint").exists() {
		assert!(Instant::now() < deadline, "the daemon did not start");
		std::thread::sleep(Duration::from_millis(50));
	}

	let original = archive_bytes();
	let source = photos.join("bundle.tar");
	let target = photos.join("bundle.zip");
	let before = support::observed(&state);
	fs::write(&source, &original).unwrap();
	support::observe(&state, before);
	fs::rename(&source, &target).unwrap();

	let store = retype_core::undo::Store::open(&state);
	let deadline = Instant::now() + Duration::from_secs(20);
	let journal = state.join("jobs.jsonl");
	loop {
		let records = fs::read_to_string(&journal).unwrap_or_default();
		if records.contains("\"state\":\"done\"") && store.list().unwrap().iter().any(|record| !record.pinned) {
			assert!(records.contains("\"state\":\"queued\""), "{records}");
			break;
		}
		assert!(Instant::now() < deadline, "the archive was not converted: {records}");
		std::thread::sleep(Duration::from_millis(50));
	}

	let mut zip = zip::ZipArchive::new(fs::File::open(&target).unwrap()).unwrap();
	let mut content = String::new();
	std::io::Read::read_to_string(&mut zip.by_name("file.txt").unwrap(), &mut content).unwrap();
	assert_eq!(content, "payload");
	drop(zip);
	if retype_core::fsx::supports_durable_cleanup() {
		assert!(
			!source.exists(),
			"a validated transform should retain the source only in undo"
		);
	} else {
		assert_eq!(fs::read(&source).unwrap(), original);
	}

	let record = store.list().unwrap().into_iter().find(|record| !record.pinned).unwrap();
	let output = fs::read(&target).unwrap();
	let fingerprint = retype_core::safety::Fingerprint::of(&target).unwrap();
	fs::write(&target, b"changed output").unwrap();
	let mut client = retype_cli::client::connect(&state).unwrap();
	let refusal = client.undo(&retype_ipc::UndoRequest {
		id: Some(record.id.as_str().to_string()),
		last: false,
	});
	match refusal {
		Err(retype_ipc::ClientError::Rpc(error)) => assert_eq!(error.code, retype_core::error::Code::IoExists),
		other => panic!("expected a changed-output refusal, got {other:?}"),
	}
	drop(client);
	assert_daemon_reachable(&state);
	assert_eq!(fs::read(&target).unwrap(), b"changed output");
	if retype_core::fsx::supports_durable_cleanup() {
		assert!(!source.exists());
	} else {
		assert_eq!(fs::read(&source).unwrap(), original);
	}
	assert_eq!(fs::read(store.source(&record.id).unwrap().unwrap()).unwrap(), original);
	assert!(store.load(&record.id).unwrap().is_some());
	fs::write(&target, output).unwrap();
	assert!(fingerprint.matches(&target).unwrap());

	let undo = retype()
		.args(["--no-spawn", "undo", "--last"])
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.output()
		.unwrap();
	if !retype_core::fsx::supports_durable_cleanup() {
		assert_undo_refused_without_stopping_daemon(&undo, &state);
		assert_eq!(fs::read(&source).unwrap(), original);
		assert!(target.exists());
		assert!(!store.list().unwrap().is_empty());
		return;
	}
	assert!(undo.status.success(), "{}", String::from_utf8_lossy(&undo.stderr));
	assert_eq!(fs::read(&source).unwrap(), original);
	assert!(!target.exists());
}

#[test]
fn undo_puts_the_source_back_and_removes_the_output() {
	let dir = tempfile::tempdir().unwrap();
	let photos = dir.path().join("photos");
	fs::create_dir(&photos).unwrap();
	let state = dir.path().join("state");
	let config = copy_config(dir.path());

	retype()
		.args(["watch", photos.to_str().unwrap()])
		.arg("--state-dir")
		.arg(&state)
		.output()
		.unwrap();

	let _daemon = Daemon(
		retype()
			.args(["daemon", "--detached"])
			.arg("--state-dir")
			.arg(&state)
			.env("RETYPE_CONFIG", &config)
			.stdout(Stdio::null())
			.stderr(Stdio::from(fs::File::create(dir.path().join("daemon.log")).unwrap()))
			.spawn()
			.unwrap(),
	);

	let deadline = Instant::now() + Duration::from_secs(20);
	while !state.join("endpoint").exists() {
		assert!(Instant::now() < deadline, "the daemon did not start");
		std::thread::sleep(Duration::from_millis(50));
	}

	let source = photos.join("photo.png");
	let before = support::observed(&state);
	fs::write(&source, b"pixels").unwrap();
	support::observe(&state, before);
	fs::rename(&source, photos.join("photo.webp")).unwrap();

	// The rename itself already puts the bytes at the new name, so the conversion
	// is done when the journal says so, not when the file has content.
	let output = photos.join("photo.webp");
	let journal = state.join("jobs.jsonl");
	let deadline = Instant::now() + Duration::from_secs(20);
	loop {
		let recorded = fs::read_to_string(&journal).unwrap_or_default();
		if recorded.contains("\"state\":\"done\"") {
			break;
		}

		assert!(Instant::now() < deadline, "the rename was not converted: {recorded}");
		std::thread::sleep(Duration::from_millis(100));
	}

	assert_eq!(fs::read(&output).unwrap(), b"pixels");

	// The record is written before the conversion, so it is already there.
	let undo = retype()
		.args(["undo", "--last"])
		.arg("--state-dir")
		.arg(&state)
		.output()
		.unwrap();

	if !retype_core::fsx::supports_durable_cleanup() {
		assert_undo_refused_without_stopping_daemon(&undo, &state);
		assert_eq!(fs::read(&source).unwrap(), b"pixels");
		assert!(output.exists());
		assert!(!retype_core::undo::Store::open(&state).list().unwrap().is_empty());
		return;
	}
	assert!(undo.status.success(), "{}", String::from_utf8_lossy(&undo.stderr));
	assert!(String::from_utf8_lossy(&undo.stdout).contains("photo.png"));

	assert_eq!(fs::read(&source).unwrap(), b"pixels", "the source is back");
	assert!(!output.exists(), "the output is gone");
	assert!(
		!state
			.join("undo")
			.read_dir()
			.map(|mut entries| entries.next().is_some())
			.unwrap_or(false),
		"the record is deleted"
	);
}

#[test]
fn undo_with_nothing_to_do_says_so() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");

	let output = retype()
		.args(["undo", "--last"])
		.arg("--state-dir")
		.arg(&state)
		.output()
		.unwrap();

	assert_eq!(output.status.code(), Some(7));
	assert!(String::from_utf8_lossy(&output.stderr).contains("E_JOB_STATE"));
}

#[test]
fn undo_without_a_target_is_a_usage_error() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");

	let output = retype().args(["undo", "--state-dir"]).arg(&state).output().unwrap();

	assert_eq!(output.status.code(), Some(2));
	assert!(String::from_utf8_lossy(&output.stderr).contains("E_USAGE_ARGS"));
	assert!(String::from_utf8_lossy(&output.stderr).contains("--last"));
}
