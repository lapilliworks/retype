//! A rename chain that spans two batches, `docs/dev/architecture/state.md` §5.3: the second rename
//! overtakes the job the first one started, and the chain becomes one conversion.
//!
//! Unix only, because the tool is a shell script that sleeps.
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

mod support;

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

/// A tool slow enough that the second rename arrives while the first job runs.
fn slow_config(dir: &Path) -> PathBuf {
	let script = dir.join("slow.sh");
	fs::write(&script, "#!/bin/sh\nsleep 4\ncp \"$1\" \"$2\"\n").unwrap();

	{
		use std::os::unix::fs::PermissionsExt;
		let mut permissions = fs::metadata(&script).unwrap().permissions();
		permissions.set_mode(0o755);
		fs::set_permissions(&script, permissions).unwrap();
	}

	let command = script.to_str().unwrap();
	let layout = "layout = ['{input}', '{ops}', '{output}']";

	let config = dir.join("retype.toml");
	fs::write(
		&config,
		format!(
			"\
version = 'retype-config-1'

[backends]
prefer = ['slow']

[tools.slow]
command = '{command}'
{layout}

[formats.png]
family = 'image'
lossy = false
alpha = true
tier = 'exact'

[formats.jpg]
family = 'image'
lossy = true
alpha = false
tier = 'converted'

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
fn a_second_rename_replaces_the_job_the_first_one_started() {
	let dir = tempfile::tempdir().unwrap();
	let photos = dir.path().to_path_buf();
	let state = dir.path().join("state");
	let config = slow_config(&photos);

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
			.stderr(Stdio::null())
			.spawn()
			.unwrap(),
	);

	let deadline = Instant::now() + Duration::from_secs(20);
	while !state.join("endpoint").exists() {
		assert!(Instant::now() < deadline, "the daemon did not start");
		std::thread::sleep(Duration::from_millis(50));
	}

	// The supervisor picks registrations up within a second, so the watcher has to
	// exist before a fact is worth producing.
	std::thread::sleep(Duration::from_millis(1500));

	let source = photos.join("photo.png");
	let before = support::observed(&state);
	fs::write(&source, b"pixels").unwrap();
	support::observe(&state, before);
	fs::rename(&source, photos.join("photo.webp")).unwrap();

	// The first job is running and has not finished: the tool sleeps longer than
	// this. While it runs the file shows its source name again, so that is the name
	// a person renames, `docs/dev/architecture/state.md` §5.4.
	let journal_path = state.join("jobs.jsonl");
	let deadline = Instant::now() + Duration::from_secs(20);
	while !fs::read_to_string(&journal_path).unwrap_or_default().contains("\"state\":\"running\"")
		|| photos.join("photo.webp").exists()
	{
		assert!(Instant::now() < deadline, "the job never started");
		std::thread::sleep(Duration::from_millis(50));
	}

	let before = support::observed(&state);
	fs::rename(photos.join("photo.png"), photos.join("photo.jpg")).unwrap();
	support::observe(&state, before);

	let deadline = Instant::now() + Duration::from_secs(30);
	let journal = loop {
		let recorded = fs::read_to_string(&journal_path).unwrap_or_default();

		if recorded.contains("\"state\":\"done\"") && recorded.contains("\"state\":\"superseded\"") {
			break recorded;
		}

		assert!(Instant::now() < deadline, "no supersession: {recorded}");
		std::thread::sleep(Duration::from_millis(100));
	};

	// One gesture, one output: the intermediate name is gone and the chain ends at
	// jpg, which is what the user asked for in the end.
	assert!(photos.join("photo.jpg").exists(), "{journal}");
	assert!(!photos.join("photo.webp").exists(), "the intermediate name is consumed");
	assert!(!photos.join("photo.png").exists(), "the source name is consumed");

	// The job that was overtaken says so, and did not produce anything.
	let superseded = journal
		.lines()
		.find(|line| line.contains("\"state\":\"superseded\""))
		.expect("a superseded record");
	assert!(superseded.contains("\"skipped\""), "{superseded}");
}
