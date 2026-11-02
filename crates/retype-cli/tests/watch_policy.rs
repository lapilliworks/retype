//! What the daemon considers at all, `docs/watch.md` §4 and §6: the built-in list,
//! hidden names, the user's globs and the size ceiling.
//!
//! Every test here is about a rename that does *not* become a job, so the absence of a
//! record is the evidence. Each fixture also proves it can convert at all, by renaming
//! one file the policy allows, so a test cannot pass because nothing ever works.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

mod support;

/// The name a path is recorded under: the journal holds canonical paths, and a
/// source that was renamed to its target no longer exists to canonicalise.
fn recorded(path: &Path) -> PathBuf {
	let Some(name) = path.file_name() else {
		return path.to_path_buf();
	};

	match path.parent().and_then(|parent| parent.canonicalize().ok()) {
		Some(parent) => parent.join(name),
		None => path.to_path_buf(),
	}
}

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

fn config(dir: &Path, watch: &str) -> PathBuf {
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

[formats.jpg]
family = 'image'
lossy = true
tier = 'converted'

[formats.webp]
family = 'image'
lossy = true
alpha = true
tier = 'converted'

[deferral]
above = '1h'
above_without_undo = '1h'
pause = '1h'

[watch]
{watch}
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
	_daemon: Daemon,
}

impl Fixture {
	fn new(watch: &str, directories: &[&str]) -> Self {
		let dir = tempfile::tempdir().unwrap();
		let photos = dir.path().join("photos");
		fs::create_dir(&photos).unwrap();

		for directory in directories {
			fs::create_dir_all(photos.join(directory)).unwrap();
		}
		let state = dir.path().join("state");
		let config = config(dir.path(), watch);

		retype()
			.args(["watch", photos.to_str().unwrap()])
			.arg("--state-dir")
			.arg(&state)
			.output()
			.unwrap();

		// The daemon's own words, kept where a failing test can show them.
		let log = fs::File::create(dir.path().join("daemon.err")).unwrap();

		let daemon = Daemon(
			retype()
				.args(["daemon", "--detached"])
				.arg("--state-dir")
				.arg(&state)
				.env("RETYPE_CONFIG", &config)
				.stdout(Stdio::null())
				.stderr(Stdio::from(log))
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
			_daemon: daemon,
		}
	}

	/// Write `bytes` at `photos/<relative>` and rename it beside itself.
	fn rename(&self, relative: &str, to: &str, bytes: &[u8]) -> PathBuf {
		let source = self.photos.join(relative);
		if let Some(parent) = source.parent() {
			fs::create_dir_all(parent).unwrap();
		}
		let before = support::observed(&self.state);
		fs::write(&source, bytes).unwrap();
		support::observe(&self.state, before);

		let target = source.with_file_name(to);
		fs::rename(&source, &target).unwrap();
		target
	}

	fn records(&self) -> Vec<serde_json::Value> {
		let contents = fs::read_to_string(self.state.join("jobs.jsonl")).unwrap_or_default();

		contents
			.lines()
			.filter_map(|line| serde_json::from_str(line).ok())
			.collect()
	}

	/// Wait until a job for `path` is recorded, which is how "this one was considered"
	/// is proved.
	fn wait_for(&self, path: &Path) {
		let deadline = Instant::now() + Duration::from_secs(20);

		loop {
			if self.considered(path) {
				return;
			}

			assert!(
				Instant::now() < deadline,
				"no settled job for {}: {:?}\ndaemon: {}",
				path.display(),
				self.records(),
				fs::read_to_string(self.dir.path().join("daemon.err")).unwrap_or_default()
			);
			std::thread::sleep(Duration::from_millis(50));
		}
	}

	/// Whether any settled record names a source at this path. A job that is still
	/// `running` has not done anything yet, so it does not count.
	fn considered(&self, path: &Path) -> bool {
		let wanted = recorded(path);

		self.records().into_iter().any(|record| {
			record["source"]["path"]
				.as_str()
				.is_some_and(|source| Path::new(source) == wanted)
				&& record["state"] != "running"
				&& record["state"] != "queued"
		})
	}
}

const PIXELS: &[u8] = b"pixels";

#[test]
fn a_rename_inside_a_built_in_directory_is_not_a_job() {
	let fixture = Fixture::new("", &["sub", ".git"]);

	fixture.rename(".git/a.png", "a.webp", PIXELS);

	// And the same daemon does convert an ordinary file, so the absence above is about
	// the path rather than about the daemon.
	fixture.rename("sub/b.png", "b.webp", PIXELS);
	fixture.wait_for(&fixture.photos.join("sub/b.png"));

	assert!(
		!fixture.considered(&fixture.photos.join(".git/a.png")),
		"{:?}",
		fixture.records()
	);
}

#[test]
fn a_hidden_directory_is_skipped_unless_asked_for() {
	let skipped = Fixture::new("", &[".cache"]);
	skipped.rename(".cache/a.png", "a.webp", PIXELS);
	skipped.rename("b.png", "b.webp", PIXELS);
	skipped.wait_for(&skipped.photos.join("b.png"));

	assert!(
		!skipped.considered(&skipped.photos.join(".cache/a.png")),
		"{:?}",
		skipped.records()
	);

	let asked = Fixture::new("watch_hidden = true", &[".cache"]);
	asked.rename(".cache/a.png", "a.webp", PIXELS);
	asked.wait_for(&asked.photos.join(".cache/a.png"));
}

#[test]
fn a_user_glob_adds_to_the_list() {
	let fixture = Fixture::new("ignore = ['*.png']", &[]);

	fixture.rename("a.png", "a.webp", PIXELS);

	// A file that matches no glob is still considered, which is what makes the
	// absence above about the pattern rather than about the daemon.
	fixture.rename("c.jpg", "c.webp", PIXELS);
	fixture.wait_for(&fixture.photos.join("c.jpg"));

	assert!(
		!fixture.considered(&fixture.photos.join("a.png")),
		"{:?}",
		fixture.records()
	);
}

#[test]
fn a_source_over_the_ceiling_is_passed_over_and_counted() {
	let fixture = Fixture::new("max_source_size = '1kb'", &[]);

	let big = fixture.rename("big.png", "big.webp", &vec![b'p'; 4096]);
	fixture.rename("small.png", "small.webp", PIXELS);

	fixture.wait_for(&fixture.photos.join("small.png"));

	assert!(
		!fixture.records().iter().any(|record| record["source"]["path"]
			.as_str()
			.is_some_and(|path| path.contains("big"))),
		"{:?}",
		fixture.records()
	);

	// The oversized file is where the rename put it and still holds its bytes; the
	// small one became its output.
	assert!(big.exists(), "the oversized file is left alone");
	assert_eq!(fs::read(&big).unwrap().len(), 4096, "and its bytes are untouched");
	assert!(
		fixture.photos.join("small.webp").exists(),
		"the small one was converted"
	);
	assert!(
		fixture.photos.join("small.png").exists(),
		"an unvalidated copy backend retains its source"
	);
}
