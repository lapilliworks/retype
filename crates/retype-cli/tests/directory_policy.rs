//! A directory's `.retype.toml`, `docs/config.md` §12: it may narrow what happens
//! beside it, and it may take itself out of use.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

fn config(dir: &Path, above: &str) -> PathBuf {
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

# The copy tool carries every operation it is asked for, so a clamped parameter still
# compiles without an encoder being installed.
[ops.quality.argv.copy]
args = []

[deferral]
above = '{above}'
above_without_undo = '{above}'
pause = '1h'
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

struct Fixture {
	dir: tempfile::TempDir,
	photos: PathBuf,
	state: PathBuf,
	config: PathBuf,
	_daemon: Daemon,
}

impl Fixture {
	fn new(above: &str, policy: &str) -> Self {
		let dir = tempfile::tempdir().unwrap();
		let photos = dir.path().join("photos");
		fs::create_dir(&photos).unwrap();

		if !policy.is_empty() {
			fs::write(
				photos.join(".retype.toml"),
				format!("version = 'retype-config-1'\n{policy}"),
			)
			.unwrap();
		}

		let state = dir.path().join("state");
		let config = config(dir.path(), above);

		let watched = retype()
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

		// The registration is refused for a paused directory, and the fixture still needs
		// a daemon to prove nothing happens there.
		assert!(
			watched.status.success() || String::from_utf8_lossy(&watched.stderr).contains("PAUSED_BY_POLICY"),
			"{}",
			String::from_utf8_lossy(&watched.stderr)
		);

		Self {
			dir,
			photos,
			state,
			config,
			_daemon: daemon,
		}
	}

	fn rename(&self, from: &str, to: &str, bytes: &[u8]) {
		let source = self.photos.join(from);
		let mut file = fs::File::create(&source).unwrap();
		file.write_all(bytes).unwrap();
		drop(file);
		fs::rename(&source, self.photos.join(to)).unwrap();
	}

	fn records(&self) -> Vec<serde_json::Value> {
		let contents = fs::read_to_string(self.state.join("jobs.jsonl")).unwrap_or_default();

		contents
			.lines()
			.filter_map(|line| serde_json::from_str(line).ok())
			.collect()
	}

	/// Wait for any record satisfying `this`, or answer `None` after a short while.
	fn wait_for(&self, this: impl Fn(&serde_json::Value) -> bool) -> Option<serde_json::Value> {
		let deadline = Instant::now() + Duration::from_secs(5);

		loop {
			if let Some(record) = self.records().into_iter().find(|record| this(record)) {
				return Some(record);
			}

			if Instant::now() >= deadline {
				return None;
			}

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
}

#[test]
fn a_denied_operation_parks_with_the_file_s_name_intact() {
	let fixture = Fixture::new("1s", "deny = ['quality']\n");
	fixture.rename("a.png", "a.webp+quality80", b"pixels");

	let parked = fixture
		.wait_for(|record| record["state"] == "needs-attention")
		.unwrap_or_else(|| panic!("a parked job: {:?}", fixture.records()));

	assert_eq!(parked["parked"]["reason"], "inapplicable", "{parked}");
	assert!(
		parked["error"]["message"]
			.as_str()
			.unwrap_or_default()
			.contains("refused in this directory"),
		"{parked}"
	);
	assert!(fixture.photos.join("a.png").exists(), "the name is restored");
	assert!(!fixture.photos.join("a.webp").exists());
}

#[test]
fn a_limited_parameter_is_clamped_and_the_name_says_so() {
	let fixture = Fixture::new("1s", "limits = { quality = 85 }\n");
	fixture.rename("b.png", "b.webp+quality95", b"pixels");

	let done = fixture
		.wait_for(|record| record["state"] == "done")
		.unwrap_or_else(|| panic!("a finished job: {:?}", fixture.records()));

	assert_eq!(done["intent"], "webp+quality85", "{done}");
	assert!(
		fixture.photos.join("b-quality85.webp").exists(),
		"the derived name is the truth"
	);
	assert!(!fixture.photos.join("b-quality95.webp").exists());
}

#[test]
fn a_pattern_keeps_a_file_out_of_the_directory_s_business() {
	let fixture = Fixture::new("1s", "patterns = ['*.heic']\n");
	fixture.rename("c.png", "c.webp", b"pixels");

	// The file sits under the name the rename gave it and nothing else happens: the
	// directory said this pattern is not its business.
	assert!(
		fixture.wait_for(|record| record["id"].is_string()).is_none(),
		"no job was created: {:?}",
		fixture.records()
	);
}

#[test]
fn a_paused_directory_is_not_watched_and_is_refused_by_watch() {
	let fixture = Fixture::new("1s", "paused = true\n");
	fixture.rename("d.png", "d.webp", b"pixels");

	assert!(
		fixture.wait_for(|record| record["id"].is_string()).is_none(),
		"no job was created: {:?}",
		fixture.records()
	);

	let refused = fixture
		.command()
		.args(["watch", fixture.photos.to_str().unwrap()])
		.output()
		.unwrap();

	assert_eq!(refused.status.code(), Some(3), "a diagnosis");
	assert!(
		String::from_utf8_lossy(&refused.stderr).contains("E_WATCH_PAUSED_BY_POLICY"),
		"{}",
		String::from_utf8_lossy(&refused.stderr)
	);
	let _ = &fixture.dir;
}

#[test]
fn confirm_false_suppresses_the_cost_confirmation_in_that_directory() {
	// Eight megabytes is about two seconds by the image constant, which the policy
	// threshold of one second would park.
	let fixture = Fixture::new("1s", "confirm = false\n");

	let source = fixture.photos.join("e.png");
	let mut file = fs::File::create(&source).unwrap();
	file.write_all(&vec![b'p'; 8 * 1024 * 1024]).unwrap();
	drop(file);

	fs::rename(&source, fixture.photos.join("e.webp")).unwrap();

	let done = fixture
		.wait_for(|record| record["state"] == "done")
		.expect("the job ran without being parked");

	assert_eq!(done["state"], "done", "{done}");
	assert!(fixture.photos.join("e.webp").exists());
}

/// A directory may ask for more settled time than the daemon's window gives,
/// `docs/watch.md` §7.
///
/// The tool here is a copy, so a job finishes as soon as it starts: what the test
/// measures is when the decision happens, not how long the work takes.
#[test]
fn debounce_ms_holds_a_rename_until_the_file_stops_changing() {
	let fixture = Fixture::new("1h", "debounce_ms = 1500\n");
	let source = fixture.photos.join("f.png");
	fs::write(&source, b"pixels").unwrap();
	fs::rename(&source, fixture.photos.join("f.webp")).unwrap();

	// Nothing decides on the daemon's own four hundred milliseconds.
	std::thread::sleep(Duration::from_millis(700));
	assert!(fixture.records().is_empty(), "{:?}", fixture.records());

	// The second rename arrives inside the hold and is the one that counts.
	fs::rename(fixture.photos.join("f.webp"), fixture.photos.join("f.webp+q75")).unwrap();

	let done = fixture
		.wait_for(|record| record["state"] == "done")
		.expect("a settled job");

	// One job, asked for what the last name asked for, and named after it.
	assert_eq!(done["intent"], "webp+quality75", "{done:?}");
	assert_eq!(done["source"]["path"], source.to_str().unwrap(), "{done:?}");
	assert!(fixture.photos.join("f-quality75.webp").exists());
	assert_eq!(
		fixture.records().len(),
		2,
		"one job, one settled record: {:?}",
		fixture.records()
	);
}
