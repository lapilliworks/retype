//! Cancellation through the daemon: a running job can be stopped, and the file is
//! left saying what it contains.
//!
//! The tool sleeps, because the point is to catch a job mid-flight without needing
//! an encoder that takes a minute.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

fn slow_config(dir: &Path) -> PathBuf {
	let (command, layout) = match cfg!(windows) {
		true => (
			"cmd",
			"layout = ['/c', 'ping -n 30 127.0.0.1', '{input}', '{ops}', '{output}']",
		),
		false => ("sh", "layout = ['-c', 'sleep 30', '{input}', '{ops}', '{output}']"),
	};

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
fn a_running_job_can_be_cancelled() {
	let dir = tempfile::tempdir().unwrap();
	let photos = dir.path().join("photos");
	fs::create_dir(&photos).unwrap();
	let state = dir.path().join("state");
	let config = slow_config(dir.path());

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

	let source = photos.join("slow.png");
	fs::write(&source, b"pixels").unwrap();
	fs::rename(&source, photos.join("slow.webp")).unwrap();

	// The job appears while it runs, which is what makes its id reachable.
	let id = loop {
		let listing = retype()
			.args(["jobs", "list", "--state", "running", "--json"])
			.arg("--state-dir")
			.arg(&state)
			.output()
			.unwrap();

		let stdout = String::from_utf8_lossy(&listing.stdout);
		if let Some(id) = stdout.split("\"id\":\"").nth(1).and_then(|rest| rest.split('"').next()) {
			break id.to_string();
		}

		assert!(Instant::now() < deadline, "no running job appeared");
		std::thread::sleep(Duration::from_millis(100));
	};

	let cancelled = retype()
		.args(["jobs", "cancel", &id])
		.arg("--state-dir")
		.arg(&state)
		.output()
		.unwrap();
	assert!(
		cancelled.status.success(),
		"{}",
		String::from_utf8_lossy(&cancelled.stderr)
	);

	// The job finishes as cancelled rather than as a failed conversion.
	let deadline = Instant::now() + Duration::from_secs(20);
	loop {
		let journal = fs::read_to_string(state.join("jobs.jsonl")).unwrap_or_default();
		if journal.contains("\"state\":\"cancelled\"") {
			break;
		}

		assert!(Instant::now() < deadline, "the job was not cancelled: {journal}");
		std::thread::sleep(Duration::from_millis(100));
	}

	assert!(!photos.join("slow.webp").exists(), "a cancelled job writes nothing");
	assert_eq!(
		fs::read(&source).unwrap(),
		b"pixels",
		"the file goes back to the name that matches its bytes"
	);
}
