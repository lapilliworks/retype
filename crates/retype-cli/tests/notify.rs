//! One notification per batch.
//!
//! The `notify-send` here is a script that records what it was asked to say and answers
//! with the action it was told to answer with, so the delivery path is proven without a
//! desktop session, and so is the Undo button.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

/// A `notify-send` that writes its arguments to `$RECORD` and, when `$ANSWER` says so,
/// answers with an action as the real one does.
fn fake_notifier(dir: &Path) -> PathBuf {
	let bin = dir.join("bin");
	fs::create_dir(&bin).unwrap();

	let path = bin.join("notify-send");
	fs::write(
		&path,
		"#!/bin/sh\n\
		 printf '%s\\n' \"$@\" >> \"$RECORD\"\n\
		 if [ \"$ANSWER\" = undo ]; then printf 'undo\\n'; fi\n\
		 exit 0\n",
	)
	.unwrap();
	fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();

	bin
}

fn config(dir: &Path) -> PathBuf {
	let config = dir.join("retype.toml");
	fs::write(
		&config,
		"\
version = 'retype-config-1'

[backends]
prefer = ['copy']

[tools.copy]
command = 'cp'
layout = ['{input}', '{ops}', '{output}']

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
",
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

struct Fixture {
	_dir: tempfile::TempDir,
	photos: PathBuf,
	state: PathBuf,
	record: PathBuf,
	_daemon: Daemon,
}

impl Fixture {
	fn new(answer: &str) -> Self {
		let dir = tempfile::tempdir().unwrap();
		let photos = dir.path().join("photos");
		fs::create_dir(&photos).unwrap();
		let state = dir.path().join("state");
		let record = dir.path().join("notified");
		let bin = fake_notifier(dir.path());
		let config = config(dir.path());

		let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());

		retype()
			.args(["watch", photos.to_str().unwrap()])
			.arg("--state-dir")
			.arg(&state)
			.output()
			.unwrap();

		let daemon = Daemon(
			retype()
				.args(["daemon", "--detached"])
				.arg("--state-dir")
				.arg(&state)
				.env("RETYPE_CONFIG", &config)
				.env("PATH", path)
				.env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/tmp/retype-test-bus")
				.env("RECORD", &record)
				.env("ANSWER", answer)
				.stdout(Stdio::null())
				.stderr(Stdio::null())
				.spawn()
				.unwrap(),
		);

		let endpoint = state.join("endpoint");
		let deadline = Instant::now() + Duration::from_secs(20);
		while !endpoint.exists() {
			assert!(Instant::now() < deadline, "the daemon did not start");
			std::thread::sleep(Duration::from_millis(50));
		}

		Self {
			_dir: dir,
			photos,
			state,
			record,
			_daemon: daemon,
		}
	}

	fn rename(&self, from: &str, to: &str) {
		let source = self.photos.join(from);
		fs::write(&source, b"pixels").unwrap();
		fs::rename(&source, self.photos.join(to)).unwrap();
	}

	fn notified(&self) -> String {
		fs::read_to_string(&self.record).unwrap_or_default()
	}

	/// Wait for the notifier to be called at least once.
	fn wait_for_notification(&self) -> String {
		let deadline = Instant::now() + Duration::from_secs(20);

		loop {
			let recorded = self.notified();
			if !recorded.is_empty() {
				return recorded;
			}
			assert!(Instant::now() < deadline, "nothing was said");
			std::thread::sleep(Duration::from_millis(50));
		}
	}
}

#[test]
fn a_batch_that_converted_a_file_is_one_notification_with_an_undo_button() {
	let fixture = Fixture::new("");
	fixture.rename("a.png", "a.webp");

	let recorded = fixture.wait_for_notification();

	assert!(recorded.contains("--app-name"), "{recorded}");
	assert!(recorded.contains("--wait"), "{recorded}");
	assert!(recorded.contains("undo=Undo"), "{recorded}");
	assert!(recorded.contains("1 converted"), "{recorded}");
	assert!(fixture.photos.join("a.webp").exists(), "the conversion stands");
}

#[test]
fn the_undo_button_puts_the_source_back() {
	let fixture = Fixture::new("undo");
	fixture.rename("b.png", "b.webp");

	let recorded = fixture.wait_for_notification();
	assert!(recorded.contains("undo=Undo"), "{recorded}");

	// The action was answered, so the daemon undid the batch: the source is back and the
	// output is gone.
	let deadline = Instant::now() + Duration::from_secs(20);
	while !fixture.photos.join("b.png").exists() {
		assert!(
			Instant::now() < deadline,
			"the undo did not happen: {:?}",
			fs::read_to_string(fixture.state.join("jobs.jsonl"))
		);
		std::thread::sleep(Duration::from_millis(50));
	}

	assert!(!fixture.photos.join("b.webp").exists(), "the output is removed");
}

#[test]
fn nothing_is_said_when_the_platform_has_no_notifier() {
	// No `notify-send` on PATH and no session: the daemon falls back to its own log, and
	// the job still happens.
	let dir = tempfile::tempdir().unwrap();
	let photos = dir.path().join("photos");
	fs::create_dir(&photos).unwrap();
	let state = dir.path().join("state");
	let config = config(dir.path());
	let record = dir.path().join("notified");

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
			.env_remove("DBUS_SESSION_BUS_ADDRESS")
			.env_remove("DISPLAY")
			.env_remove("WAYLAND_DISPLAY")
			.env("RECORD", &record)
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.spawn()
			.unwrap(),
	);

	let endpoint = state.join("endpoint");
	let deadline = Instant::now() + Duration::from_secs(20);
	while !endpoint.exists() {
		assert!(Instant::now() < deadline, "the daemon did not start");
		std::thread::sleep(Duration::from_millis(50));
	}

	let source = photos.join("c.png");
	fs::write(&source, b"pixels").unwrap();
	fs::rename(&source, photos.join("c.webp")).unwrap();

	let output = photos.join("c.webp");
	let deadline = Instant::now() + Duration::from_secs(20);
	while !output.exists() {
		assert!(Instant::now() < deadline, "the conversion did not happen");
		std::thread::sleep(Duration::from_millis(50));
	}

	assert!(!record.exists(), "no notifier was run");
}
