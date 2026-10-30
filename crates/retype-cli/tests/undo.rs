//! `retype undo`: a rename in a watched directory converts, and undo puts the file
//! back exactly as it was.

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

struct Daemon(Child);

impl Drop for Daemon {
	fn drop(&mut self) {
		let _ = self.0.kill();
		let _ = self.0.wait();
	}
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
	fs::write(&source, b"pixels").unwrap();
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
