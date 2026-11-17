//! The product's loop: a rename in a registered directory becomes a conversion.
//!
//! The daemon here uses a copy tool instead of a converter, so the test proves the
//! pipeline rather than the encoder.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[path = "support/journal.rs"]
mod journal;
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

[formats.jpg]
family = 'image'
lossy = true
alpha = false
tier = 'converted'
"
		),
	)
	.unwrap();

	config
}

struct Daemon {
	child: Child,
}

impl Drop for Daemon {
	fn drop(&mut self) {
		let _ = self.child.kill();
		let _ = self.child.wait();
	}
}

#[test]
fn a_rename_in_a_watched_directory_is_converted() {
	let dir = tempfile::tempdir().unwrap();
	let photos = dir.path().join("photos");
	fs::create_dir(&photos).unwrap();
	let source = photos.join("holiday.png");
	fs::write(&source, b"pixels").unwrap();
	let state = dir.path().join("state");
	let config = copy_config(dir.path());

	let watched = retype()
		.args(["watch", "--register", photos.to_str().unwrap()])
		.arg("--state-dir")
		.arg(&state)
		.output()
		.unwrap();
	assert!(watched.status.success(), "{}", String::from_utf8_lossy(&watched.stderr));

	let mut daemon = Daemon {
		child: retype()
			.args(["daemon", "--detached"])
			.arg("--state-dir")
			.arg(&state)
			.env("RETYPE_CONFIG", &config)
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.spawn()
			.unwrap(),
	};

	// The daemon publishes its endpoint when it is listening.
	let endpoint = state.join("endpoint");
	let deadline = Instant::now() + Duration::from_secs(20);
	while !endpoint.exists() {
		assert!(Instant::now() < deadline, "the daemon did not start");
		std::thread::sleep(Duration::from_millis(50));
	}

	support::ready(&state);
	fs::rename(&source, photos.join("holiday.webp")).unwrap();

	// The record is written after the bytes, so the wait covers both.
	let target = photos.join("holiday.webp");
	let journal_path = state.join("jobs.jsonl");
	let deadline = Instant::now() + Duration::from_secs(20);

	let journal = loop {
		let converted = fs::read(&target).ok().as_deref() == Some(b"pixels".as_slice());
		let recorded = fs::read_to_string(&journal_path).unwrap_or_default();

		let jobs = journal::latest(&recorded);
		if converted
			&& jobs.len() == 1
			&& jobs[0]["state"] == "done"
			&& jobs[0]["source_disposition"]["state"] == "retained"
		{
			assert_eq!(
				jobs[0]["source"]["path"],
				source.canonicalize().unwrap().to_str().unwrap()
			);
			break recorded;
		}

		assert!(Instant::now() < deadline, "the rename was not converted: {recorded}");
		std::thread::sleep(Duration::from_millis(100));
	};

	assert!(journal.contains("holiday.webp"), "{journal}");
	assert!(source.exists(), "an unvalidated copy backend retains its source");

	let _ = daemon.child.kill();
}

#[test]
fn one_rename_with_two_outputs_converts_both() {
	// A target name may name several outputs. The job reads its source once,
	// so the second output must still find bytes to convert; preparing the source
	// per output is what made this fail.
	let dir = tempfile::tempdir().unwrap();
	let photos = dir.path().join("photos");
	fs::create_dir(&photos).unwrap();
	let source = photos.join("holiday.png");
	fs::write(&source, b"pixels").unwrap();
	let state = dir.path().join("state");
	let config = copy_config(dir.path());

	let watched = retype()
		.args(["watch", "--register", photos.to_str().unwrap()])
		.arg("--state-dir")
		.arg(&state)
		.output()
		.unwrap();
	assert!(watched.status.success(), "{}", String::from_utf8_lossy(&watched.stderr));

	let _daemon = Daemon {
		child: retype()
			.args(["daemon", "--detached"])
			.arg("--state-dir")
			.arg(&state)
			.env("RETYPE_CONFIG", &config)
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.spawn()
			.unwrap(),
	};

	let endpoint = state.join("endpoint");
	let deadline = Instant::now() + Duration::from_secs(20);
	while !endpoint.exists() {
		assert!(Instant::now() < deadline, "the daemon did not start");
		std::thread::sleep(Duration::from_millis(50));
	}

	support::ready(&state);
	fs::rename(&source, photos.join("holiday.webp,jpg")).unwrap();

	let first = photos.join("holiday.webp");
	let second = photos.join("holiday.jpg");
	let journal_path = state.join("jobs.jsonl");
	let deadline = Instant::now() + Duration::from_secs(20);

	let journal = loop {
		let both = fs::read(&first).is_ok() && fs::read(&second).is_ok();
		let recorded = fs::read_to_string(&journal_path).unwrap_or_default();

		let jobs = journal::latest(&recorded);
		if both && jobs.len() == 1 && jobs[0]["state"] == "done" && jobs[0]["source_disposition"]["state"] == "retained"
		{
			assert_eq!(
				jobs[0]["source"]["path"],
				source.canonicalize().unwrap().to_str().unwrap()
			);
			assert_eq!(jobs[0]["outputs"].as_array().unwrap().len(), 2);
			break recorded;
		}

		assert!(Instant::now() < deadline, "the rename was not converted: {recorded}");
		std::thread::sleep(Duration::from_millis(100));
	};

	assert_eq!(fs::read(&first).unwrap(), b"pixels");
	assert_eq!(fs::read(&second).unwrap(), b"pixels");
	assert_eq!(journal::latest(&journal)[0]["state"], "done", "{journal}");
	assert!(source.exists(), "an unvalidated copy backend retains its source");
}
