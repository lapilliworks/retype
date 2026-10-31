//! Deferral through the daemon, `docs/deferral.md`: a rename that is parked, the file
//! its name is restored to, and the ways it ends.
//!
//! The thresholds are seconds, so a job has to be worth more than one to be parked: the
//! file below is large enough that the image constant puts it over.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

/// A copying tool and a deferral policy that parks anything an estimate can measure.
fn config(dir: &Path, pause: &str, repair: &str) -> PathBuf {
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

[deferral]
above = '1s'
above_without_undo = '1s'
pause = '{pause}'
repair = '{repair}'
"
		),
	)
	.unwrap();

	config
}

/// Eight megabytes, which the image constant puts around two seconds.
fn big_file(path: &Path) {
	let mut file = fs::File::create(path).unwrap();
	file.write_all(&vec![b'p'; 8 * 1024 * 1024]).unwrap();
}

struct Daemon(Child);

impl Drop for Daemon {
	fn drop(&mut self) {
		let _ = self.0.kill();
		let _ = self.0.wait();
	}
}

struct Fixture {
	dir: tempfile::TempDir,
	photos: PathBuf,
	state: PathBuf,
	config: PathBuf,
	_daemon: Daemon,
}

impl Fixture {
	fn new(pause: &str) -> Self {
		Self::with(pause, "ask")
	}

	fn with(pause: &str, repair: &str) -> Self {
		let dir = tempfile::tempdir().unwrap();
		let photos = dir.path().join("photos");
		fs::create_dir(&photos).unwrap();
		let state = dir.path().join("state");
		let config = config(dir.path(), pause, repair);

		retype()
			.args(["watch", photos.to_str().unwrap()])
			.arg("--state-dir")
			.arg(&state)
			.output()
			.unwrap();

		let daemon = Daemon(
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

		Self {
			dir,
			photos,
			state,
			config,
			_daemon: daemon,
		}
	}

	/// Rename a file into an intent and answer with the job the daemon recorded.
	fn rename(&self, from: &str, to: &str) -> serde_json::Value {
		let source = self.photos.join(from);
		big_file(&source);
		fs::rename(&source, self.photos.join(to)).unwrap();

		self.wait_for(|record| record["state"] != "running")
	}

	fn records(&self) -> Vec<serde_json::Value> {
		let contents = fs::read_to_string(self.state.join("jobs.jsonl")).unwrap_or_default();

		contents
			.lines()
			.filter_map(|line| serde_json::from_str(line).ok())
			.collect()
	}

	/// The newest record at or after `from` that satisfies `done`.
	fn wait_for(&self, done: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
		let deadline = Instant::now() + Duration::from_secs(20);

		loop {
			if let Some(record) = self.records().into_iter().rev().find(|record| done(record)) {
				return record;
			}

			assert!(Instant::now() < deadline, "no record appeared: {:?}", self.records());
			std::thread::sleep(Duration::from_millis(50));
		}
	}

	fn command(&self) -> Command {
		let mut command = retype();
		command
			.arg("--state-dir")
			.arg(&self.state)
			.env("RETYPE_CONFIG", &self.config);
		command
	}

	fn id(record: &serde_json::Value) -> String {
		record["id"].as_str().unwrap_or_default().to_string()
	}
}

#[test]
fn an_expensive_rename_is_parked_under_its_own_name_and_runs_when_the_timer_fires() {
	let fixture = Fixture::new("1s");
	let parked = fixture.rename("a.png", "a.webp");

	assert_eq!(parked["state"], "pending", "{parked}");
	assert_eq!(parked["parked"]["reason"], "cost", "{parked}");
	assert!(parked["parked"]["deadline"].is_i64(), "{parked}");

	// The name is back, because a path may not claim a format its bytes do not hold.
	assert!(fixture.photos.join("a.png").exists(), "the source name is restored");
	assert!(!fixture.photos.join("a.webp").exists());

	// And the timer runs it without anyone asking.
	let done = fixture.wait_for(|record| record["state"] == "done" && Fixture::id(record) == Fixture::id(&parked));

	assert_eq!(done["state"], "done");
	assert!(fixture.photos.join("a.webp").exists(), "the output is there");
	assert!(!fixture.photos.join("a.png").exists(), "the source is consumed");
}

#[test]
fn ask_parks_a_job_whatever_the_estimate_says_and_now_runs_it() {
	let fixture = Fixture::new("1h");
	let small = fixture.photos.join("b.png");
	fs::write(&small, b"pixels").unwrap();
	fs::rename(&small, fixture.photos.join("b.webp+ask")).unwrap();

	let parked = fixture.wait_for(|record| record["parked"]["reason"] == "ask");
	assert_eq!(parked["state"], "pending", "{parked}");

	let ran = fixture
		.command()
		.args(["jobs", "resolve", &Fixture::id(&parked), "--now"])
		.output()
		.unwrap();
	assert!(ran.status.success(), "{}", String::from_utf8_lossy(&ran.stderr));

	let done = fixture.wait_for(|record| record["state"] == "done" && Fixture::id(record) == Fixture::id(&parked));
	assert_eq!(done["state"], "done");
	assert!(fixture.photos.join("b.webp").exists());
}

#[test]
fn an_intent_that_cannot_be_carried_waits_with_a_string_to_fix() {
	let fixture = Fixture::new("1h");
	let small = fixture.photos.join("c.png");
	fs::write(&small, b"pixels").unwrap();
	fs::rename(&small, fixture.photos.join("c.webp+zzz")).unwrap();

	let parked = fixture.wait_for(|record| record["state"] == "needs-attention");

	assert_eq!(parked["parked"]["reason"], "intent", "{parked}");
	assert_eq!(parked["parked"]["requested"], "webp+zzz", "{parked}");
	assert_eq!(parked["parked"]["corrected"], "webp", "{parked}");
	assert!(parked["error"]["code"].is_string(), "{parked}");

	// `--now` cannot honour a request that does not run.
	let refused = fixture
		.command()
		.args(["jobs", "resolve", &Fixture::id(&parked), "--now"])
		.output()
		.unwrap();
	assert_eq!(refused.status.code(), Some(7), "a state conflict");

	// `--convert` runs what can be carried.
	let ran = fixture
		.command()
		.args(["jobs", "resolve", &Fixture::id(&parked), "--convert"])
		.output()
		.unwrap();
	assert!(ran.status.success(), "{}", String::from_utf8_lossy(&ran.stderr));

	fixture.wait_for(|record| record["state"] == "done" && Fixture::id(record) == Fixture::id(&parked));
	assert!(fixture.photos.join("c.webp").exists());
}

#[test]
fn a_parked_job_can_be_refused_before_its_time_comes() {
	let fixture = Fixture::new("1h");
	let parked = fixture.rename("d.png", "d.webp");
	assert_eq!(parked["state"], "pending", "{parked}");

	let cancelled = fixture
		.command()
		.args(["jobs", "cancel", &Fixture::id(&parked)])
		.output()
		.unwrap();
	assert!(
		cancelled.status.success(),
		"{}",
		String::from_utf8_lossy(&cancelled.stderr)
	);

	let last = fixture.wait_for(|record| Fixture::id(record) == Fixture::id(&parked) && record["state"] == "cancelled");
	assert_eq!(last["state"], "cancelled");
	assert!(fixture.photos.join("d.png").exists(), "nothing was converted");
	assert!(!fixture.photos.join("d.webp").exists());
	let _ = fixture.dir;
}

#[test]
fn repair_converts_without_the_token_it_could_not_carry_and_says_what_it_dropped() {
	let fixture = Fixture::with("1h", "convert");
	let small = fixture.photos.join("e.png");
	fs::write(&small, b"pixels").unwrap();
	fs::rename(&small, fixture.photos.join("e.webp+zzz")).unwrap();

	let done = fixture.wait_for(|record| record["state"] == "done");

	assert_eq!(done["parked"]["reason"], "intent", "{done}");
	assert_eq!(done["parked"]["requested"], "webp+zzz", "{done}");
	assert_eq!(done["parked"]["corrected"], "webp", "{done}");
	assert_eq!(done["parked"]["dropped"], serde_json::json!(["zzz"]), "{done}");
	assert!(fixture.photos.join("e.webp").exists(), "the result is there");
	assert!(!fixture.photos.join("e.png").exists(), "the source is consumed");
}

#[test]
fn the_interactive_walk_refuses_a_pipe_rather_than_reading_a_script() {
	let dir = tempfile::tempdir().unwrap();

	let ran = retype()
		.args(["jobs", "--interactive"])
		.arg("--state-dir")
		.arg(dir.path().join("state"))
		.stdin(Stdio::null())
		.output()
		.unwrap();

	assert_eq!(ran.status.code(), Some(2), "usage is exit 2");
	assert!(
		String::from_utf8_lossy(&ran.stderr).contains("needs a terminal"),
		"{}",
		String::from_utf8_lossy(&ran.stderr)
	);
}

#[test]
fn a_file_that_appeared_during_the_pause_is_not_overwritten() {
	let fixture = Fixture::new("1h");
	let small = fixture.photos.join("d.png");
	fs::write(&small, b"pixels").unwrap();
	fs::rename(&small, fixture.photos.join("d.webp+ask")).unwrap();

	let parked = fixture.wait_for(|record| record["parked"]["reason"] == "ask");

	// The person put something at the name the job would write while it waited.
	let theirs = fixture.photos.join("d.webp");
	fs::write(&theirs, b"mine").unwrap();

	let ran = fixture
		.command()
		.args(["jobs", "resolve", &Fixture::id(&parked), "--now"])
		.output()
		.unwrap();
	assert!(ran.status.success(), "{}", String::from_utf8_lossy(&ran.stderr));

	let ended = fixture.wait_for(|record| {
		Fixture::id(record) == Fixture::id(&parked) && record["state"] != "running" && record["state"] != "pending"
	});

	assert_eq!(ended["state"], "failed", "{ended}");
	assert_eq!(ended["outputs"][0]["error"]["code"], "E_IO_EXISTS", "{ended}");

	// Their file is untouched, and the source is still where they can find it.
	assert_eq!(fs::read(&theirs).unwrap(), b"mine");
	assert!(fixture.photos.join("d.png").exists(), "the source goes back");
}

#[test]
fn force_replaces_a_file_that_appeared_during_the_pause() {
	let fixture = Fixture::new("1h");
	let small = fixture.photos.join("e.png");
	fs::write(&small, b"pixels").unwrap();
	fs::rename(&small, fixture.photos.join("e.webp+ask")).unwrap();

	let parked = fixture.wait_for(|record| record["parked"]["reason"] == "ask");
	let theirs = fixture.photos.join("e.webp");
	fs::write(&theirs, b"mine").unwrap();

	let ran = fixture
		.command()
		.args(["jobs", "resolve", &Fixture::id(&parked), "--now", "--force"])
		.output()
		.unwrap();
	assert!(ran.status.success(), "{}", String::from_utf8_lossy(&ran.stderr));

	let done = fixture.wait_for(|record| record["state"] == "done" && Fixture::id(record) == Fixture::id(&parked));
	assert_eq!(done["state"], "done");
	assert_eq!(fs::read(&theirs).unwrap(), b"pixels", "the output replaced it");
	assert!(!fixture.photos.join("e.png").exists(), "the source is consumed");
}
