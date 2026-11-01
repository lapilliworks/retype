//! The event ring a client replays from, `docs/dev/architecture/ipc.md` §7.
//!
//! A conversion is submitted, and the ring is read the way a client reads it: ask
//! for what followed the last sequence seen.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

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
fn a_conversion_shows_up_in_the_event_ring_and_replays_from_a_sequence() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let config = copy_config(dir.path());

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

	let endpoint = state.join("endpoint");
	let deadline = Instant::now() + Duration::from_secs(20);
	while !endpoint.exists() {
		assert!(Instant::now() < deadline, "the daemon did not start");
		std::thread::sleep(Duration::from_millis(50));
	}

	let source = dir.path().join("photo.png");
	fs::write(&source, b"pixels").unwrap();

	let converted = retype()
		.args(["convert"])
		.arg(&source)
		.arg("webp")
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.output()
		.unwrap();
	assert!(
		converted.status.success(),
		"{}",
		String::from_utf8_lossy(&converted.stderr)
	);

	let mut client = retype_cli::client::connect(&state).unwrap();

	let replayed = client.call("events", json!({"since": 0})).unwrap();
	let events = replayed["events"].as_array().expect("an array of events");
	assert!(!events.is_empty(), "the conversion was published: {replayed}");

	let sequences: Vec<u64> = events
		.iter()
		.map(|event| event["seq"].as_u64().expect("a sequence"))
		.collect();
	assert!(
		sequences.windows(2).all(|pair| pair[0] < pair[1]),
		"sequences increase: {sequences:?}"
	);

	let newest = replayed["seq"].as_u64().unwrap();
	assert_eq!(Some(&newest), sequences.last());

	// The last event describes the job that was submitted, which is what a client
	// uses to notice a job it did not ask about.
	let last = events.last().unwrap();
	assert_eq!(last["event"], "job");
	assert_eq!(last["job"]["state"], "done");

	// Asking from where the client now stands is empty rather than a replay.
	let caught_up = client.call("events", json!({"since": newest})).unwrap();
	assert_eq!(caught_up["events"], Value::Array(Vec::new()));
	assert_eq!(caught_up["seq"], newest);
}
