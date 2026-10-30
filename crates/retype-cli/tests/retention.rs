//! Retention is a startup decision: a record past its age is gone before the
//! daemon answers its first request, so resume stops being offered at the same
//! moment undo does.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use retype_core::job::JobId;
use retype_core::undo::{Record, Store, Subject};

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

struct Daemon(Child);

impl Drop for Daemon {
	fn drop(&mut self) {
		let _ = self.0.kill();
		let _ = self.0.wait();
	}
}

fn config(dir: &Path, retention: &str) -> PathBuf {
	let config = dir.join("retype.toml");
	fs::write(
		&config,
		format!("version = 'retype-config-1'\n\n[undo]\nretention = '{retention}'\n"),
	)
	.unwrap();
	config
}

/// Write a record and then age it, which is the one thing a test cannot wait for.
fn record(store: &Store, dir: &Path, index: usize, age_hours: i64) -> JobId {
	let bytes = dir.join(format!("photo{index}.heic"));
	fs::write(&bytes, b"heic bytes").unwrap();

	let id = JobId::parse(&format!("01J{:023}", index)).unwrap();
	let mut saved: Record = store
		.save(Subject {
			id: id.clone(),
			intent: "jpg".to_string(),
			machine: "machine".to_string(),
			bytes: bytes.clone(),
			source_path: bytes,
			source_ext: "heic".to_string(),
			outputs: Vec::new(),
		})
		.unwrap();

	saved.created = retype_core::job::now_ms() - age_hours * 60 * 60 * 1_000;
	fs::write(
		store.directory(&saved.id).join("meta.toml"),
		toml::to_string(&saved).unwrap(),
	)
	.unwrap();

	id
}

#[test]
fn the_daemon_drops_records_past_retention_and_keeps_the_rest() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	fs::create_dir(&state).unwrap();
	let config = config(dir.path(), "1h");

	let store = Store::open(&state);
	let old = record(&store, dir.path(), 1, 2);
	let recent = record(&store, dir.path(), 2, 0);

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
	while store.directory(&old).exists() {
		assert!(Instant::now() < deadline, "the daemon did not sweep");
		std::thread::sleep(Duration::from_millis(50));
	}

	assert!(!store.directory(&old).exists(), "a record past retention is dropped");
	assert!(
		store.load(&recent).unwrap().is_some(),
		"a record inside retention is kept"
	);
}
