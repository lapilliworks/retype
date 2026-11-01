//! `retype jobs resume`: a partial job finishes, skipping the output it already
//! produced, and takes its source bytes from the undo record because the rename
//! consumed the source name.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

mod support;

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
fn a_partial_job_resumes_and_skips_what_it_already_produced() {
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
			.stderr(Stdio::null())
			.spawn()
			.unwrap(),
	);

	let deadline = Instant::now() + Duration::from_secs(20);
	while !state.join("endpoint").exists() {
		assert!(Instant::now() < deadline, "the daemon did not start");
		std::thread::sleep(Duration::from_millis(50));
	}

	// One output is blocked by a directory in its place, so the job ends partial.
	let blocked = photos.join("photo-flipv.png");
	fs::create_dir(&blocked).unwrap();

	let source = photos.join("photo.png");
	let before = support::observed(&state);
	fs::write(&source, b"pixels").unwrap();
	support::observe(&state, before);
	fs::rename(&source, photos.join("photo.webp+fliph,png+flipv")).unwrap();

	let journal_path = state.join("jobs.jsonl");
	let deadline = Instant::now() + Duration::from_secs(20);
	let journal = loop {
		let recorded = fs::read_to_string(&journal_path).unwrap_or_default();
		if recorded.contains("\"state\":\"partial\"") {
			break recorded;
		}

		assert!(Instant::now() < deadline, "no partial job: {recorded}");
		std::thread::sleep(Duration::from_millis(100));
	};

	let produced = photos.join("photo-fliph.webp");
	assert_eq!(fs::read(&produced).unwrap(), b"pixels", "the first output was written");

	let id = journal
		.rsplit("\"id\":\"")
		.next()
		.and_then(|rest| rest.split('"').next())
		.expect("a job id")
		.to_string();

	// Clear the obstruction; resume should now finish, and should not rewrite the
	// output that is already what the job produced.
	fs::remove_dir(&blocked).unwrap();
	let before = fs::metadata(&produced).unwrap().modified().unwrap();

	let resumed = retype()
		.args(["jobs", "resume", &id])
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

	assert_eq!(fs::read(&blocked).unwrap(), b"pixels", "the missing output is written");

	// Skipping an output needs a file identity, which the stable Windows metadata
	// API does not expose, so there a resume runs it again.
	#[cfg(unix)]
	assert_eq!(
		fs::metadata(&produced).unwrap().modified().unwrap(),
		before,
		"the output that was already correct was not rewritten"
	);

	#[cfg(not(unix))]
	let _ = before;
	assert!(
		!state
			.join("undo")
			.read_dir()
			.map(|mut entries| entries.next().is_some())
			.unwrap_or(false),
		"a finished job needs no undo record"
	);
}
