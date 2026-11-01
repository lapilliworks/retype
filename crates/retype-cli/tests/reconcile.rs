//! Startup reconciliation, `docs/dev/architecture/state.md` §6.
//!
//! A `queued` job never ran, so it goes back in the queue; a `running` job may have
//! committed some of its outputs, so it is declared interrupted and `jobs resume` is
//! the path that finishes it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use retype_core::job::{Job, JobId, JobState, Output, Source, Tier, now_ms};

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

/// A journal record for a job that never finished, written by hand because that is
/// the state a killed daemon leaves behind.
fn record(id: &JobId, source: &Path, output: &Path, state: JobState) -> Job {
	Job {
		id: id.clone(),
		created: now_ms(),
		updated: now_ms(),
		source: Source {
			path: source.to_path_buf(),
			format: "png".to_string(),
			size: 6,
			mtime_ns: 0,
		},
		intent: Some("webp".to_string()),
		outputs: vec![Output::pending(output.to_path_buf(), "webp", Tier::Converted)],
		state,
		backend: None,
		engine: None,
		machine: "test".to_string(),
		error: None,
		parked: None,
		consume_source: true,
	}
}

/// The last record the journal holds for one id.
fn last_record(journal: &Path, id: &JobId) -> serde_json::Value {
	let contents = fs::read_to_string(journal).unwrap_or_default();

	contents
		.lines()
		.filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
		.rfind(|record| record["id"] == id.as_str())
		.unwrap_or_else(|| panic!("no record for {id} in: {contents}"))
}

#[test]
fn a_restart_requeues_the_queued_and_declares_the_running() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	fs::create_dir_all(&state).unwrap();
	let config = copy_config(dir.path());

	let queued = JobId::parse(&format!("01J{:023}", 1)).unwrap();
	let running = JobId::parse(&format!("01J{:023}", 2)).unwrap();

	for name in ["queued", "running"] {
		fs::write(dir.path().join(format!("{name}.png")), b"pixels").unwrap();
	}

	let journal = state.join("jobs.jsonl");
	let mut lines = String::new();

	for (id, name, job_state) in [
		(&queued, "queued", JobState::Queued),
		(&running, "running", JobState::Running),
	] {
		let job = record(
			id,
			&dir.path().join(format!("{name}.png")),
			&dir.path().join(format!("{name}.webp")),
			job_state,
		);

		lines.push_str(&job.to_line().unwrap());
		lines.push('\n');
	}

	fs::write(&journal, lines).unwrap();

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
	while !dir.path().join("queued.webp").exists() {
		assert!(Instant::now() < deadline, "the queued job did not run");
		std::thread::sleep(Duration::from_millis(50));
	}

	// And it is recorded as finished rather than merely written.
	loop {
		if last_record(&journal, &queued)["state"] == "done" {
			break;
		}
		assert!(Instant::now() < deadline, "the queued job has no terminal record");
		std::thread::sleep(Duration::from_millis(50));
	}

	let declared = last_record(&journal, &running);
	assert_eq!(declared["state"], "failed", "{declared}");
	assert_eq!(declared["error"]["code"], "E_JOB_INTERRUPTED", "{declared}");
	assert!(
		!dir.path().join("running.webp").exists(),
		"an interrupted job is not silently re-run"
	);
}
