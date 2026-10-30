//! The product's loop: a rename in a registered directory becomes a conversion.
//!
//! The daemon here uses a copy tool instead of a converter, so the test proves the
//! pipeline rather than the encoder.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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
	let state = dir.path().join("state");
	let config = copy_config(dir.path());

	let watched = retype()
		.args(["watch", photos.to_str().unwrap()])
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

	let source = photos.join("holiday.png");
	fs::write(&source, b"pixels").unwrap();
	fs::rename(&source, photos.join("holiday.webp")).unwrap();

	// The record is written after the bytes, so the wait covers both.
	let target = photos.join("holiday.webp");
	let journal_path = state.join("jobs.jsonl");
	let deadline = Instant::now() + Duration::from_secs(20);

	let journal = loop {
		let converted = fs::read(&target).ok().as_deref() == Some(b"pixels".as_slice());
		let recorded = fs::read_to_string(&journal_path).unwrap_or_default();

		if converted && recorded.contains("\"state\":\"done\"") {
			break recorded;
		}

		assert!(Instant::now() < deadline, "the rename was not converted: {recorded}");
		std::thread::sleep(Duration::from_millis(100));
	};

	assert!(journal.contains("holiday.webp"), "{journal}");
	assert!(!source.exists(), "the source name does not come back");

	let _ = daemon.child.kill();
}
