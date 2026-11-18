//! `retype jobs resume`: a partial job finishes, skipping the output it already
//! produced, and takes its source bytes from the undo record because the rename
//! consumed the source name.

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

fn wait_for_partial(state: &Path, id: &str) {
	let deadline = Instant::now() + Duration::from_secs(10);
	loop {
		let job = retype_cli::client::connect(state).unwrap().jobs_get(id).unwrap();
		if job.state == retype_core::job::JobState::Partial {
			return;
		}
		assert!(
			Instant::now() < deadline,
			"partial checkpoint was not published: {job:?}"
		);
		std::thread::sleep(Duration::from_millis(20));
	}
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

# Copy carries the two flag-shaped operations, which need no argument.
[ops.fliph.argv.copy]
args = []

[ops.flipv.argv.copy]
args = []
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
fn cancelling_a_settled_partial_job_keeps_its_successful_output_and_undo() {
	let dir = tempfile::tempdir().unwrap();
	let photos = dir.path().join("photos");
	fs::create_dir(&photos).unwrap();
	let source = photos.join("photo.png");
	fs::write(&source, b"pixels").unwrap();
	let state = dir.path().join("state");
	let config = copy_config(dir.path());
	retype()
		.args(["watch", "--register", photos.to_str().unwrap()])
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

	support::ready(&state);
	let blocked = photos.join("photo-flipv.png");
	fs::create_dir(&blocked).unwrap();
	fs::rename(&source, photos.join("photo.webp+fliph,png+flipv")).unwrap();
	let journal_path = state.join("jobs.jsonl");
	let deadline = Instant::now() + Duration::from_secs(20);
	let partial = loop {
		let contents = fs::read_to_string(&journal_path).unwrap_or_default();
		let jobs = journal::latest(&contents);
		if jobs.len() == 1 && jobs[0]["state"] == "partial" {
			assert_eq!(
				jobs[0]["source"]["path"],
				source.canonicalize().unwrap().to_str().unwrap()
			);
			break jobs.into_iter().next().unwrap();
		}
		assert!(Instant::now() < deadline, "no partial job: {contents}");
		std::thread::sleep(Duration::from_millis(100));
	};
	let id = partial["id"].as_str().unwrap();
	wait_for_partial(&state, id);
	let produced = photos.join("photo-fliph.webp");

	let cancelled = retype()
		.args(["jobs", "cancel", id, "--json"])
		.arg("--state-dir")
		.arg(&state)
		.output()
		.unwrap();
	assert!(
		cancelled.status.success(),
		"{}",
		String::from_utf8_lossy(&cancelled.stderr)
	);
	let answer: serde_json::Value = serde_json::from_slice(&cancelled.stdout).unwrap();
	assert_eq!(answer["state"], "cancelled");
	assert!(answer.get("warning").is_none(), "{answer}");
	assert_eq!(fs::read(&source).unwrap(), b"pixels");
	assert_eq!(fs::read(&produced).unwrap(), b"pixels");
	assert!(blocked.is_dir());
	let record = retype_core::undo::Store::open(&state)
		.load(&retype_core::job::JobId::parse(id).unwrap())
		.unwrap()
		.unwrap();
	assert!(!record.pinned);
	assert!(record.retention_started.is_some());

	let undone = retype()
		.args(["undo", id])
		.arg("--state-dir")
		.arg(&state)
		.output()
		.unwrap();
	if retype_core::fsx::supports_durable_cleanup() {
		assert!(undone.status.success(), "{}", String::from_utf8_lossy(&undone.stderr));
		assert!(!produced.exists());
	} else {
		assert!(!undone.status.success());
		assert!(produced.exists());
	}
	assert_eq!(fs::read(source).unwrap(), b"pixels");
	assert!(blocked.is_dir(), "undo does not remove a path the job did not produce");
}

#[test]
fn a_partial_job_resumes_and_skips_what_it_already_produced() {
	let dir = tempfile::tempdir().unwrap();
	let photos = dir.path().join("photos");
	fs::create_dir(&photos).unwrap();
	let source = photos.join("photo.png");
	fs::write(&source, b"pixels").unwrap();
	let state = dir.path().join("state");
	let config = copy_config(dir.path());

	retype()
		.args(["watch", "--register", photos.to_str().unwrap()])
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

	support::ready(&state);
	// One output is blocked by a directory in its place, so the job ends partial.
	let blocked = photos.join("photo-flipv.png");
	fs::create_dir(&blocked).unwrap();

	fs::rename(&source, photos.join("photo.webp+fliph,png+flipv")).unwrap();

	let journal_path = state.join("jobs.jsonl");
	let deadline = Instant::now() + Duration::from_secs(20);
	let partial = loop {
		let contents = fs::read_to_string(&journal_path).unwrap_or_default();
		let jobs = journal::latest(&contents);
		if jobs.len() == 1 && jobs[0]["state"] == "partial" {
			assert_eq!(
				jobs[0]["source"]["path"],
				source.canonicalize().unwrap().to_str().unwrap()
			);
			break jobs.into_iter().next().unwrap();
		}

		assert!(Instant::now() < deadline, "no partial job: {contents}");
		std::thread::sleep(Duration::from_millis(100));
	};

	let produced = photos.join("photo-fliph.webp");
	assert_eq!(fs::read(&produced).unwrap(), b"pixels", "the first output was written");

	let id = partial["id"].as_str().unwrap();
	wait_for_partial(&state, id);

	let still_blocked = retype()
		.args(["jobs", "resume", id, "--json"])
		.arg("--state-dir")
		.arg(&state)
		.output()
		.unwrap();
	assert!(!still_blocked.status.success(), "an incomplete resume is not success");
	let answer: serde_json::Value = serde_json::from_slice(&still_blocked.stdout).unwrap();
	assert_eq!(answer["ok"], false);
	assert_eq!(answer["error"]["code"], "E_IO_EXISTS");
	assert_eq!(answer["error"]["detail"]["resume_state"], "partial");
	assert_eq!(answer["error"]["detail"]["job"]["state"], "partial");
	assert!(blocked.is_dir());
	assert_eq!(fs::read(&produced).unwrap(), b"pixels");

	// Clear the obstruction; resume should now finish, and should not rewrite the
	// output that is already what the job produced.
	fs::remove_dir(&blocked).unwrap();
	let before = retype_core::job::FileIdentity::of(&produced).unwrap();

	let resumed = retype()
		.args(["jobs", "resume", id])
		.arg("--state-dir")
		.arg(&state)
		.output()
		.unwrap();
	assert!(
		resumed.status.success(),
		"{}\n{}",
		String::from_utf8_lossy(&resumed.stderr),
		String::from_utf8_lossy(&resumed.stdout)
	);
	assert!(String::from_utf8_lossy(&resumed.stdout).contains("done"));
	assert!(
		String::from_utf8_lossy(&resumed.stderr).contains("original retained"),
		"{}",
		String::from_utf8_lossy(&resumed.stderr)
	);

	assert_eq!(fs::read(&blocked).unwrap(), b"pixels", "the missing output is written");

	assert!(
		before.matches(&retype_core::job::FileIdentity::of(&produced).unwrap()),
		"the output that was already correct was not replaced"
	);
	assert!(
		state
			.join("undo")
			.read_dir()
			.map(|mut entries| entries.next().is_some())
			.unwrap_or(false),
		"a successful resume keeps its original and undo record"
	);
	let undone = retype()
		.args(["undo", "--last"])
		.arg("--state-dir")
		.arg(&state)
		.output()
		.unwrap();
	if !retype_core::fsx::supports_durable_cleanup() {
		assert!(!undone.status.success());
		assert_eq!(fs::read(&source).unwrap(), b"pixels");
		assert!(produced.exists());
		assert!(blocked.exists());
		return;
	}
	assert!(undone.status.success(), "{}", String::from_utf8_lossy(&undone.stderr));
	assert_eq!(fs::read(&source).unwrap(), b"pixels");
	assert!(!produced.exists());
	assert!(!blocked.exists());
}
