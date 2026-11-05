//! Installed watcher status is distinct from a saved registration.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Fixture {
	_dir: tempfile::TempDir,
	state: PathBuf,
	photos: PathBuf,
	config: PathBuf,
	daemon: Option<Child>,
}

impl Fixture {
	fn new() -> Self {
		let dir = tempfile::tempdir().unwrap();
		let state = dir.path().join("state");
		let photos = dir.path().join("photos");
		fs::create_dir(&photos).unwrap();
		let config = dir.path().join("retype.toml");
		fs::write(&config, "version = 'retype-config-1'\n[notify]\nenabled = false\n").unwrap();
		Self {
			_dir: dir,
			state,
			photos,
			config,
			daemon: None,
		}
	}

	fn start(&mut self) {
		let registration = Command::new(env!("CARGO_BIN_EXE_retype"))
			.arg("watch")
			.arg(&self.photos)
			.arg("--state-dir")
			.arg(&self.state)
			.env("RETYPE_CONFIG", &self.config)
			.output()
			.unwrap();
		assert!(
			registration.status.success(),
			"{}",
			String::from_utf8_lossy(&registration.stderr)
		);

		self.daemon = Some(
			Command::new(env!("CARGO_BIN_EXE_retype"))
				.args(["daemon", "--detached"])
				.arg("--state-dir")
				.arg(&self.state)
				.env("RETYPE_CONFIG", &self.config)
				.stdout(Stdio::null())
				.stderr(Stdio::null())
				.spawn()
				.unwrap(),
		);
		let deadline = Instant::now() + Duration::from_secs(12);
		while retype_cli::client::connect(&self.state).is_err() {
			assert!(Instant::now() < deadline, "the daemon did not start");
			std::thread::sleep(Duration::from_millis(25));
		}
	}

	fn status(&self) -> retype_ipc::Status {
		let mut client = retype_cli::client::connect(&self.state).unwrap();
		client.status().unwrap()
	}

	fn wait_for(&self, expected: &str) -> retype_ipc::Watch {
		let deadline = Instant::now() + Duration::from_secs(12);
		loop {
			let status = self.status();
			let watch = status.watches.into_iter().next().expect("registration is present");
			if watch.status.as_deref() == Some(expected) {
				return watch;
			}
			assert!(Instant::now() < deadline, "watch did not become {expected}: {watch:?}");
			std::thread::sleep(Duration::from_millis(25));
		}
	}
}

impl Drop for Fixture {
	fn drop(&mut self) {
		if let Some(mut daemon) = self.daemon.take() {
			let _ = daemon.kill();
			let _ = daemon.wait();
		}
	}
}

fn archive(path: &Path) {
	let file = fs::File::create(path).unwrap();
	let mut builder = tar::Builder::new(file);
	let contents = b"watch readiness";
	let mut header = tar::Header::new_gnu();
	header.set_size(contents.len() as u64);
	header.set_mode(0o644);
	header.set_cksum();
	builder.append_data(&mut header, "message.txt", &contents[..]).unwrap();
	builder.finish().unwrap();
}

#[test]
fn a_ready_watcher_has_primed_a_preexisting_file() {
	let mut fixture = Fixture::new();
	let source = fixture.photos.join("message.tar");
	let output = fixture.photos.join("message.zip");
	archive(&source);
	fixture.start();
	let watch = fixture.wait_for("ready");
	assert!(watch.available);
	assert!(watch.problem.is_none());
	assert!(fixture.status().primed >= 1);

	fs::rename(&source, &output).unwrap();
	let deadline = Instant::now() + Duration::from_secs(15);
	loop {
		let jobs = fs::read_to_string(fixture.state.join("jobs.jsonl")).unwrap_or_default();
		if jobs.contains("\"state\":\"done\"") {
			break;
		}
		assert!(Instant::now() < deadline, "the rename did not finish: {jobs}");
		std::thread::sleep(Duration::from_millis(50));
	}
	let bytes = fs::read(&output).unwrap();
	assert!(bytes.starts_with(b"PK\x03\x04"));
}

#[test]
fn a_paused_registration_reports_when_its_worker_has_stopped() {
	let mut fixture = Fixture::new();
	fixture.start();
	fixture.wait_for("ready");
	{
		let mut client = retype_cli::client::connect(&fixture.state).unwrap();
		client
			.watch_add(&retype_ipc::WatchAdd {
				path: fixture.photos.clone(),
				recursive: Some(true),
				paused: Some(true),
			})
			.unwrap();
	}

	let watch = fixture.wait_for("paused");
	assert!(watch.paused);
}

#[test]
fn a_malformed_registration_file_is_reported_and_monitoring_recovers() {
	let mut fixture = Fixture::new();
	fixture.start();
	fixture.wait_for("ready");
	let registry = fixture.state.join("watches.toml");
	let original = fs::read(&registry).unwrap();
	let mut file = fs::File::create(&registry).unwrap();
	file.write_all(b"not a valid registration file").unwrap();
	drop(file);

	let deadline = Instant::now() + Duration::from_secs(12);
	loop {
		if let Some(error) = fixture.status().watch_error {
			assert!(error.contains("cannot read watch registrations"), "{error}");
			break;
		}
		assert!(Instant::now() < deadline, "the registration error was not reported");
		std::thread::sleep(Duration::from_millis(25));
	}
	std::thread::sleep(Duration::from_millis(1200));
	assert!(fixture.status().watch_error.is_some());
	fs::write(registry, original).unwrap();
	fixture.wait_for("ready");
}

#[cfg(unix)]
#[test]
fn an_incomplete_initial_scan_is_an_error_not_readiness() {
	use std::os::unix::fs::PermissionsExt;

	struct RestorePermissions(PathBuf, fs::Permissions);
	impl Drop for RestorePermissions {
		fn drop(&mut self) {
			let _ = fs::set_permissions(&self.0, self.1.clone());
		}
	}

	let mut fixture = Fixture::new();
	let blocked = fixture.photos.join("blocked");
	fs::create_dir(&blocked).unwrap();
	let original = fs::metadata(&blocked).unwrap().permissions();
	let _restore = RestorePermissions(blocked.clone(), original);
	fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
	if fs::read_dir(&blocked).is_ok() {
		eprintln!("SKIPPED: this user can read a directory with no permissions");
		return;
	}
	archive(&fixture.photos.join("message.tar"));
	fixture.start();
	let watch = fixture.wait_for("error");
	let problem = watch.problem.unwrap();
	assert!(
		problem.contains("initial scan incomplete") || problem.contains("cannot install watcher"),
		"{problem}"
	);

	let source = fixture.photos.join("message.tar");
	let target = fixture.photos.join("message.zip");
	let original = fs::read(&source).unwrap();
	fs::rename(source, &target).unwrap();
	std::thread::sleep(Duration::from_secs(2));
	assert_eq!(fs::read(target).unwrap(), original);
	assert!(
		fs::read_to_string(fixture.state.join("jobs.jsonl"))
			.unwrap_or_default()
			.is_empty()
	);
}

#[cfg(unix)]
#[test]
fn replacing_a_watched_directory_restarts_and_reprimes_it() {
	let mut fixture = Fixture::new();
	archive(&fixture.photos.join("old.tar"));
	fixture.start();
	fixture.wait_for("ready");
	let before = fixture.status().primed;
	let old = fixture._dir.path().join("old-photos");
	fs::rename(&fixture.photos, &old).unwrap();
	fs::create_dir(&fixture.photos).unwrap();
	archive(&fixture.photos.join("new.tar"));

	let deadline = Instant::now() + Duration::from_secs(12);
	loop {
		let status = fixture.status();
		let ready = status.watches.first().and_then(|watch| watch.status.as_deref()) == Some("ready");
		if ready && status.primed > before {
			break;
		}
		assert!(Instant::now() < deadline, "the replacement was not primed: {status:?}");
		std::thread::sleep(Duration::from_millis(25));
	}
	fs::rename(fixture.photos.join("new.tar"), fixture.photos.join("new.zip")).unwrap();
	let deadline = Instant::now() + Duration::from_secs(15);
	loop {
		let jobs = fs::read_to_string(fixture.state.join("jobs.jsonl")).unwrap_or_default();
		if jobs.contains("new.tar") && jobs.contains("\"state\":\"done\"") {
			break;
		}
		assert!(
			Instant::now() < deadline,
			"the replacement directory was not watched: {jobs}"
		);
		std::thread::sleep(Duration::from_millis(50));
	}
}
