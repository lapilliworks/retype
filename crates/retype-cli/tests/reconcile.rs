//! Startup reconciliation, `docs/dev/architecture/state.md` §6.
//!
//! Queued inputs without content fingerprints require manual review; running jobs
//! may have committed outputs and are declared interrupted for explicit recovery.

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
	let metadata = fs::metadata(source).unwrap();
	let mtime_ns = metadata
		.modified()
		.unwrap()
		.duration_since(std::time::UNIX_EPOCH)
		.unwrap()
		.as_nanos() as i64;
	Job {
		id: id.clone(),
		created: now_ms(),
		updated: now_ms(),
		source: Source {
			path: source.to_path_buf(),
			format: "png".to_string(),
			size: metadata.len(),
			mtime_ns,
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
		source_disposition: None,
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
fn a_restart_reports_queued_and_running_jobs_as_interrupted() {
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
	for (id, name) in [(&queued, "queued"), (&running, "running")] {
		loop {
			let record = last_record(&journal, id);
			if record["state"] == "failed" {
				assert_eq!(record["error"]["code"], "E_JOB_INTERRUPTED", "{record}");
				break;
			}
			assert!(
				Instant::now() < deadline,
				"the job was not marked interrupted: {record}"
			);
			std::thread::sleep(Duration::from_millis(50));
		}
		assert!(!dir.path().join(format!("{name}.webp")).exists());
		assert_eq!(fs::read(dir.path().join(format!("{name}.png"))).unwrap(), b"pixels");
	}
}

#[test]
fn an_old_timed_ask_is_restored_as_a_confirmation_without_a_deadline() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	fs::create_dir_all(&state).unwrap();
	let config = copy_config(dir.path());
	let id = JobId::now().unwrap();
	let source = dir.path().join("asked.png");
	let output = dir.path().join("asked.webp");
	fs::write(&source, b"pixels").unwrap();
	let mut job = record(&id, &source, &output, JobState::Pending);
	job.intent = Some("webp+ask".to_string());
	job.parked = Some(retype_core::job::Parked {
		reason: "ask".to_string(),
		deadline: Some(now_ms() - 1_000),
		batch: None,
		estimate: None,
		requested: Some("webp+ask".to_string()),
		corrected: None,
		dropped: Vec::new(),
		consume: true,
		force: false,
		input_fingerprint: None,
	});
	let journal = state.join("jobs.jsonl");
	fs::write(&journal, format!("{}\n", job.to_line().unwrap())).unwrap();

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
	loop {
		let record = last_record(&journal, &id);
		if record["state"] == "needs-attention" {
			assert!(record["parked"]["deadline"].is_null());
			break;
		}
		assert!(Instant::now() < deadline, "the ask was not restored: {record}");
		std::thread::sleep(Duration::from_millis(50));
	}
	std::thread::sleep(Duration::from_millis(1_200));
	assert!(!output.exists(), "a previously timed ask must not execute");
	assert_eq!(fs::read(source).unwrap(), b"pixels");
}

#[test]
fn a_queued_job_does_not_run_against_a_replacement_source() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	fs::create_dir_all(&state).unwrap();
	let config = copy_config(dir.path());

	let id = JobId::parse(&format!("01J{:023}", 4)).unwrap();
	let source = dir.path().join("replaced.png");
	let output = dir.path().join("replaced.webp");
	fs::write(&source, b"pixels").unwrap();

	let job = record(&id, &source, &output, JobState::Queued);
	let journal = state.join("jobs.jsonl");
	fs::write(&journal, format!("{}\n", job.to_line().unwrap())).unwrap();

	// The replacement has the same size and timestamp as the queued input.
	let modified = fs::metadata(&source).unwrap().modified().unwrap();
	fs::write(&source, b"change").unwrap();
	fs::OpenOptions::new()
		.write(true)
		.open(&source)
		.unwrap()
		.set_times(fs::FileTimes::new().set_modified(modified))
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
	loop {
		let record = last_record(&journal, &id);
		if record["state"] == "failed" {
			assert_eq!(record["error"]["code"], "E_JOB_INTERRUPTED", "{record}");
			break;
		}
		assert!(
			Instant::now() < deadline,
			"the changed source was not refused: {record}"
		);
		std::thread::sleep(Duration::from_millis(50));
	}

	assert!(!output.exists(), "nothing was written from the replacement");
	assert_eq!(fs::read(&source).unwrap(), b"change");
}

#[test]
fn an_unverified_queued_submission_remains_available_for_review() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	fs::create_dir_all(&state).unwrap();
	let config = copy_config(dir.path());

	let id = JobId::parse(&format!("01J{:023}", 3)).unwrap();
	let source = dir.path().join("submitted.png");
	let output = dir.path().join("submitted.webp");
	fs::write(&source, b"pixels").unwrap();

	let mut job = record(&id, &source, &output, JobState::Queued);
	job.consume_source = false;

	let journal = state.join("jobs.jsonl");
	fs::write(&journal, format!("{}\n", job.to_line().unwrap())).unwrap();

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
	loop {
		let record = last_record(&journal, &id);
		if record["state"] == "failed" {
			assert_eq!(record["error"]["code"], "E_JOB_INTERRUPTED");
			assert!(record["error"]["message"].as_str().unwrap().contains("resubmit"));
			break;
		}

		assert!(
			Instant::now() < deadline,
			"the queued submission was not reported: {record}"
		);
		std::thread::sleep(Duration::from_millis(50));
	}

	assert!(!output.exists());
	assert_eq!(fs::read(source).unwrap(), b"pixels");
}
