//! Real daemon interruption and restart on disposable, production-qualified paths.

use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use retype_core::job::{Job, JobId, JobState, SourceDisposition};
use retype_core::safety::Fingerprint;
use retype_core::test_checkpoints::{CHECKPOINT_ENV, CHECKPOINT_FILE, FIXTURE_PREFIX, MARKER, ROOT_ENV, TOKEN_ENV};
use retype_core::undo::Store;

#[path = "support/commands.rs"]
mod commands;
#[path = "support/journal.rs"]
mod journal;
#[path = "support/mod.rs"]
mod support;

const CONTENT: &[u8] = b"synthetic native archive entry";
const UNRELATED: &[u8] = b"unrelated file must remain untouched";
const START_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
	Direct,
	Watched,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
	OutputPublished,
	DispositionPending,
	SourceClaimed,
	SourceUnlinked,
	SourceRemoved,
	DispositionRecorded,
	UndoSourceReady,
	UndoOutputClaimed,
	UndoOutputUnlinked,
	UndoBeforeForget,
}

impl Phase {
	fn checkpoint(self) -> &'static str {
		match self {
			Self::OutputPublished => "publication.renamed:photo.zip",
			Self::DispositionPending => "disposition.pending:photo.tar",
			Self::SourceClaimed => "cleanup.claimed:photo.tar",
			Self::SourceUnlinked => "cleanup.unlinked:photo.tar",
			Self::SourceRemoved => "disposition.removed:photo.tar",
			Self::DispositionRecorded => "disposition.recorded:photo.tar",
			Self::UndoSourceReady => "undo.source-durable:photo.tar",
			Self::UndoOutputClaimed => "cleanup.claimed:photo.zip",
			Self::UndoOutputUnlinked => "cleanup.unlinked:photo.zip",
			Self::UndoBeforeForget => "undo.before-forget:photo.tar",
		}
	}

	fn during_undo(self) -> bool {
		matches!(
			self,
			Self::UndoSourceReady | Self::UndoOutputClaimed | Self::UndoOutputUnlinked | Self::UndoBeforeForget
		)
	}
}

macro_rules! crash_cases {
	($($name:ident: $phase:ident;)*) => {
		$(#[test] fn $name() { exercise_restart(Phase::$phase, Route::Direct); })*
	};
}

crash_cases! {
	interrupted_output_publication_is_reconciled: OutputPublished;
	interrupted_pending_disposition_is_unresolved: DispositionPending;
	interrupted_source_claim_preserves_the_original: SourceClaimed;
	interrupted_source_unlink_retains_recovery: SourceUnlinked;
	interrupted_removed_source_is_unresolved: SourceRemoved;
	interrupted_recorded_disposition_keeps_recovery_pinned: DispositionRecorded;
	interrupted_undo_after_source_restoration_is_retryable: UndoSourceReady;
	interrupted_undo_claim_keeps_both_original_and_output: UndoOutputClaimed;
	interrupted_undo_unlink_keeps_remaining_output: UndoOutputUnlinked;
	interrupted_undo_before_forgetting_record_is_retryable: UndoBeforeForget;
}

#[test]
fn a_watched_source_claim_survives_daemon_restart() {
	exercise_restart(Phase::SourceClaimed, Route::Watched);
}

#[test]
fn a_checkpoint_cannot_pause_a_conversion_outside_its_fixture() {
	let fixture = Fixture::new(Route::Direct);
	let outside = tempfile::tempdir().unwrap();
	let source = outside.path().join("photo.tar");
	fs::write(&source, &fixture.original).unwrap();
	let _daemon = fixture.daemon(Some(Phase::OutputPublished.checkpoint()));
	let mut command = fixture.command();
	command
		.arg("convert")
		.arg(&source)
		.args(["zip,tar.gz", "--json", "--no-spawn", "--state-dir"])
		.arg(fixture.state());
	let result = commands::bounded(&mut command, Duration::from_secs(20)).unwrap();
	assert!(
		!result.status.success()
			&& (result.stdout.contains("checkpoint is outside its fixture")
				|| result.stderr.contains("checkpoint is outside its fixture")),
		"outside checkpoint was not refused: stdout: {}; stderr: {}; daemon: {}",
		result.stdout,
		result.stderr,
		fixture.daemon_log()
	);
	assert_eq!(fs::read(source).unwrap(), fixture.original);
	assert_eq!(fs::read(fixture.source()).unwrap(), fixture.original);
	assert!(!fixture.root().join(CHECKPOINT_FILE).exists());
}

struct Fixture {
	directory: tempfile::TempDir,
	route: Route,
	token: String,
	original: Vec<u8>,
}

impl Fixture {
	fn new(route: Route) -> Self {
		let directory = tempfile::Builder::new().prefix(FIXTURE_PREFIX).tempdir().unwrap();
		let token = JobId::now().unwrap().as_str().to_string();
		fs::write(directory.path().join(MARKER), &token).unwrap();
		fs::write(
			directory.path().join("retype.toml"),
			"version = 'retype-config-1'\n[notify]\nenabled = false\n",
		)
		.unwrap();
		let mut archive = tar::Builder::new(Vec::new());
		let mut header = tar::Header::new_gnu();
		header.set_size(CONTENT.len() as u64);
		header.set_mode(0o644);
		header.set_cksum();
		archive.append_data(&mut header, "file.txt", CONTENT).unwrap();
		let original = archive.into_inner().unwrap();
		let files = if route == Route::Watched {
			let photos = directory.path().join("photos");
			fs::create_dir(&photos).unwrap();
			photos
		} else {
			directory.path().to_path_buf()
		};
		fs::write(files.join("photo.tar"), &original).unwrap();
		fs::write(directory.path().join("unrelated.txt"), UNRELATED).unwrap();
		Self {
			directory,
			route,
			token,
			original,
		}
	}

	fn root(&self) -> &Path {
		self.directory.path()
	}
	fn state(&self) -> PathBuf {
		self.root().join("state")
	}
	fn files(&self) -> PathBuf {
		if self.route == Route::Watched {
			self.root().join("photos")
		} else {
			self.root().to_path_buf()
		}
	}
	fn source(&self) -> PathBuf {
		self.files().join("photo.tar")
	}
	fn zip(&self) -> PathBuf {
		self.files().join("photo.zip")
	}
	fn gzip(&self) -> PathBuf {
		self.files().join("photo.tar.gz")
	}
	fn store(&self) -> Store {
		Store::open(&self.state())
	}

	fn daemon(&self, checkpoint: Option<&str>) -> Process {
		let log = OpenOptions::new()
			.create(true)
			.append(true)
			.open(self.root().join("daemon.log"))
			.unwrap();
		let mut command = Command::new(env!("CARGO_BIN_EXE_retype"));
		command
			.args(["daemon", "--detached", "--state-dir"])
			.arg(self.state())
			.env("RETYPE_CONFIG", self.root().join("retype.toml"))
			.env(ROOT_ENV, self.root())
			.env(TOKEN_ENV, &self.token)
			.env_remove(CHECKPOINT_ENV)
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::from(log));
		if let Some(checkpoint) = checkpoint {
			command.env(CHECKPOINT_ENV, checkpoint);
		}
		let daemon = Process(command.spawn().unwrap());
		let deadline = Instant::now() + START_TIMEOUT;
		while retype_cli::client::connect(&self.state()).is_err() {
			assert!(Instant::now() < deadline, "daemon did not start: {}", self.daemon_log());
			std::thread::sleep(Duration::from_millis(25));
		}
		daemon
	}

	fn command(&self) -> Command {
		let mut command = Command::new(env!("CARGO_BIN_EXE_retype"));
		command
			.env("RETYPE_CONFIG", self.root().join("retype.toml"))
			.env_remove(ROOT_ENV)
			.env_remove(TOKEN_ENV)
			.env_remove(CHECKPOINT_ENV);
		command
	}

	fn register_watch(&self) {
		let mut command = self.command();
		command
			.args(["watch", "--register"])
			.arg(self.files())
			.arg("--state-dir")
			.arg(self.state());
		let result = commands::bounded(&mut command, Duration::from_secs(20)).unwrap();
		assert!(
			result.status.success(),
			"watch registration failed: stdout: {}; stderr: {}",
			result.stdout,
			result.stderr
		);
	}

	fn conversion(&self) -> Command {
		let mut command = self.command();
		command
			.arg("convert")
			.arg(self.source())
			.args(["zip,tar.gz", "--json", "--no-spawn", "--state-dir"])
			.arg(self.state());
		command
	}

	fn undo(&self, id: &JobId) -> Command {
		let mut command = self.command();
		command
			.args(["undo", id.as_str(), "--no-spawn", "--state-dir"])
			.arg(self.state());
		command
	}

	fn daemon_log(&self) -> String {
		fs::read_to_string(self.root().join("daemon.log")).unwrap_or_default()
	}

	fn wait_for_checkpoint(
		&self,
		phase: Phase,
		daemon: &mut Process,
		mut client: Option<&mut Process>,
		deadline: Instant,
	) {
		loop {
			let daemon_status = daemon.0.try_wait().unwrap();
			let client_status = client.as_deref_mut().and_then(|client| client.0.try_wait().unwrap());
			assert!(
				daemon_status.is_none() && client_status.is_none() && Instant::now() < deadline,
				"checkpoint {:?} was not reached: daemon={daemon_status:?}; client={client_status:?}; {}",
				phase,
				self.daemon_log()
			);
			if fs::read(self.root().join(CHECKPOINT_FILE)).is_ok_and(|bytes| bytes == phase.checkpoint().as_bytes()) {
				return;
			}
			std::thread::sleep(Duration::from_millis(20));
		}
	}

	fn job_id(&self) -> JobId {
		let text = fs::read_to_string(self.state().join("jobs.jsonl")).unwrap();
		let jobs = journal::latest(&text);
		assert_eq!(jobs.len(), 1, "unexpected journal: {text}");
		JobId::parse(jobs[0]["id"].as_str().unwrap()).unwrap()
	}

	fn wait_for_reconciliation(&self, id: &JobId, phase: Phase) -> Job {
		let deadline = Instant::now() + START_TIMEOUT;
		loop {
			let job = retype_cli::client::connect(&self.state())
				.unwrap()
				.jobs_get(id.as_str())
				.unwrap();
			let ready = match phase {
				Phase::OutputPublished => job.state == JobState::Partial,
				Phase::DispositionPending | Phase::SourceClaimed | Phase::SourceUnlinked | Phase::SourceRemoved => {
					matches!(&job.source_disposition, Some(SourceDisposition::Unresolved { .. }))
				}
				_ => matches!(&job.source_disposition, Some(SourceDisposition::Removed)),
			};
			if ready {
				return job;
			}
			assert!(
				Instant::now() < deadline,
				"reconciliation did not finish: {job:?}; {}",
				self.daemon_log()
			);
			std::thread::sleep(Duration::from_millis(25));
		}
	}
}

struct Process(Child);

impl Process {
	fn stop(&mut self) -> std::io::Result<std::process::ExitStatus> {
		if let Some(status) = self.0.try_wait()? {
			return Ok(status);
		}
		if let Err(error) = self.0.kill() {
			return self.0.try_wait()?.ok_or(error);
		}
		let deadline = Instant::now() + Duration::from_secs(3);
		while Instant::now() < deadline {
			if let Some(status) = self.0.try_wait()? {
				return Ok(status);
			}
			std::thread::sleep(Duration::from_millis(20));
		}
		Err(std::io::Error::new(
			std::io::ErrorKind::TimedOut,
			"test process did not exit",
		))
	}

	fn interrupt(&mut self) {
		assert!(
			self.0.try_wait().unwrap().is_none(),
			"process exited before interruption"
		);
		assert!(!self.stop().unwrap().success(), "interrupted process reported success");
	}
}

impl Drop for Process {
	fn drop(&mut self) {
		if let Err(error) = self.stop() {
			if std::thread::panicking() {
				eprintln!("could not stop test process: {error}");
			} else {
				panic!("could not stop test process: {error}");
			}
		}
	}
}

fn spawn_client(mut command: Command) -> Process {
	Process(
		command
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.spawn()
			.unwrap(),
	)
}

fn verify_zip(path: &Path) {
	let mut archive = zip::ZipArchive::new(File::open(path).unwrap()).unwrap();
	let mut content = Vec::new();
	archive.by_name("file.txt").unwrap().read_to_end(&mut content).unwrap();
	assert_eq!(content, CONTENT);
}

fn verify_gzip(path: &Path) {
	let reader = flate2::read::MultiGzDecoder::new(File::open(path).unwrap());
	let mut archive = tar::Archive::new(reader);
	let mut entries = archive.entries().unwrap();
	let mut entry = entries.next().unwrap().unwrap();
	assert_eq!(entry.path().unwrap(), Path::new("file.txt"));
	let mut content = Vec::new();
	entry.read_to_end(&mut content).unwrap();
	assert_eq!(content, CONTENT);
	assert!(entries.next().is_none());
}

fn recovery_directories(fixture: &Fixture) -> Vec<PathBuf> {
	fs::read_dir(fixture.files())
		.unwrap()
		.map(|entry| entry.unwrap().path())
		.filter(|path| {
			path.file_name()
				.unwrap()
				.to_string_lossy()
				.starts_with(".retype-recovery.")
		})
		.collect()
}

fn orphan(
	fixture: &Fixture,
	phase: Phase,
	record: &retype_core::undo::Record,
) -> Option<(PathBuf, Option<Fingerprint>)> {
	let mut directories = recovery_directories(fixture);
	let claimed = matches!(phase, Phase::SourceClaimed | Phase::UndoOutputClaimed);
	let unlinked = matches!(phase, Phase::SourceUnlinked | Phase::UndoOutputUnlinked);
	if !claimed && !unlinked {
		assert!(
			directories.is_empty(),
			"unexpected recovery directories: {directories:?}"
		);
		return None;
	}
	assert_eq!(directories.len(), 1, "expected one orphaned recovery directory");
	let directory = directories.pop().unwrap();
	let contents: Vec<_> = fs::read_dir(&directory)
		.unwrap()
		.map(|entry| entry.unwrap().path())
		.collect();
	if unlinked {
		assert!(contents.is_empty());
		return Some((directory, None));
	}
	assert_eq!(contents.len(), 1);
	let actual = Fingerprint::of(&contents[0]).unwrap();
	let expected = if phase == Phase::SourceClaimed {
		assert_eq!(fs::read(&contents[0]).unwrap(), fixture.original);
		record.source_fingerprint.as_ref().unwrap()
	} else {
		verify_zip(&contents[0]);
		&record
			.commits
			.iter()
			.find(|commit| commit.output.format == "zip")
			.unwrap()
			.fingerprint
	};
	assert!(
		expected.same_file_and_content(&actual),
		"orphaned claim identity changed"
	);
	Some((directory, Some(actual)))
}

fn assert_orphan_preserved(fixture: &Fixture, orphan: &Option<(PathBuf, Option<Fingerprint>)>) {
	let directories = recovery_directories(fixture);
	let Some((directory, fingerprint)) = orphan else {
		assert!(
			directories.is_empty(),
			"recovery created an unreported orphan: {directories:?}"
		);
		return;
	};
	assert_eq!(directories.as_slice(), std::slice::from_ref(directory));
	let contents: Vec<_> = fs::read_dir(directory)
		.unwrap()
		.map(|entry| entry.unwrap().path())
		.collect();
	match fingerprint {
		Some(fingerprint) => {
			assert_eq!(contents.len(), 1);
			assert!(fingerprint.same_file_and_content(&Fingerprint::of(&contents[0]).unwrap()));
		}
		None => assert!(contents.is_empty()),
	}
}

struct Interrupted {
	fixture: Fixture,
	id: JobId,
	record: retype_core::undo::Record,
	orphan: Option<(PathBuf, Option<Fingerprint>)>,
}

fn interrupt_at(phase: Phase, route: Route) -> Interrupted {
	let fixture = Fixture::new(route);
	if route == Route::Watched {
		assert!(!phase.during_undo());
		fixture.register_watch();
	}
	let mut daemon = fixture.daemon(Some(phase.checkpoint()));
	if phase.during_undo() {
		let conversion = commands::bounded(&mut fixture.conversion(), Duration::from_secs(20)).unwrap();
		assert!(
			conversion.status.success(),
			"conversion failed: stdout: {}; stderr: {}",
			conversion.stdout,
			conversion.stderr
		);
		assert!(!fixture.source().exists());
		verify_zip(&fixture.zip());
		verify_gzip(&fixture.gzip());
	}
	if route == Route::Watched {
		support::ready(&fixture.state());
	}
	let deadline = Instant::now() + START_TIMEOUT;
	let mut client = if route == Route::Watched {
		fs::rename(fixture.source(), fixture.files().join("photo.zip,tar.gz")).unwrap();
		None
	} else if phase.during_undo() {
		Some(spawn_client(fixture.undo(&fixture.job_id())))
	} else {
		Some(spawn_client(fixture.conversion()))
	};
	fixture.wait_for_checkpoint(phase, &mut daemon, client.as_mut(), deadline);
	let id = fixture.job_id();
	daemon.interrupt();
	drop(client);
	let record = fixture.store().load(&id).unwrap().unwrap();
	assert!(record.pinned, "interrupted recovery must remain pinned");
	assert_eq!(
		fs::read(fixture.store().source(&id).unwrap().unwrap()).unwrap(),
		fixture.original
	);
	let orphan = orphan(&fixture, phase, &record);
	Interrupted {
		fixture,
		id,
		record,
		orphan,
	}
}

fn exercise_restart(phase: Phase, route: Route) {
	let Interrupted {
		fixture,
		id,
		record,
		orphan,
	} = interrupt_at(phase, route);
	let _daemon = fixture.daemon(None);
	if route == Route::Watched {
		support::ready(&fixture.state());
	}
	let job = fixture.wait_for_reconciliation(&id, phase);
	if phase != Phase::OutputPublished {
		assert_eq!(job.state, JobState::Done);
	}
	assert!(fixture.store().load(&id).unwrap().unwrap().pinned);
	if matches!(phase, Phase::OutputPublished | Phase::DispositionPending) || phase.during_undo() {
		assert_eq!(fs::read(fixture.source()).unwrap(), fixture.original);
	} else {
		assert!(!fixture.source().exists());
	}
	if phase == Phase::OutputPublished {
		assert_eq!(job.error.as_ref().unwrap().code, "E_JOB_INTERRUPTED");
		verify_zip(&fixture.zip());
		assert!(!fixture.gzip().exists());
	} else if phase == Phase::UndoBeforeForget {
		assert!(!fixture.zip().exists());
		assert!(!fixture.gzip().exists());
	} else {
		if !matches!(phase, Phase::UndoOutputClaimed | Phase::UndoOutputUnlinked) {
			verify_zip(&fixture.zip());
		}
		verify_gzip(&fixture.gzip());
	}
	if phase.during_undo() {
		assert!(record.undo_progress.is_some());
	}
	assert_eq!(fs::read(fixture.root().join("unrelated.txt")).unwrap(), UNRELATED);
	let undo = commands::bounded(&mut fixture.undo(&id), Duration::from_secs(20)).unwrap();
	assert!(
		undo.status.success(),
		"retrying undo failed: stdout: {}; stderr: {}; daemon: {}",
		undo.stdout,
		undo.stderr,
		fixture.daemon_log()
	);
	assert_eq!(fs::read(fixture.source()).unwrap(), fixture.original);
	assert!(!fixture.zip().exists());
	assert!(!fixture.gzip().exists());
	assert!(fixture.store().load(&id).unwrap().is_none());
	assert_eq!(fixture.job_id(), id);
	assert_eq!(fs::read(fixture.root().join("unrelated.txt")).unwrap(), UNRELATED);
	assert_orphan_preserved(&fixture, &orphan);
	let repeated = commands::bounded(&mut fixture.undo(&id), Duration::from_secs(20)).unwrap();
	assert!(!repeated.status.success(), "a completed undo must not execute twice");
	assert_eq!(fs::read(fixture.source()).unwrap(), fixture.original);
	assert_orphan_preserved(&fixture, &orphan);
}

#[test]
fn a_replacement_source_after_a_claim_is_not_overwritten() {
	let phase = Phase::SourceClaimed;
	let Interrupted {
		fixture, id, orphan, ..
	} = interrupt_at(phase, Route::Direct);
	fs::write(fixture.source(), b"unrelated replacement source").unwrap();
	let _daemon = fixture.daemon(None);
	fixture.wait_for_reconciliation(&id, phase);
	let undo = commands::bounded(&mut fixture.undo(&id), Duration::from_secs(20)).unwrap();
	assert!(
		!undo.status.success() && undo.stderr.contains("already exists"),
		"an occupied source must be refused: stdout: {}; stderr: {}",
		undo.stdout,
		undo.stderr
	);
	assert_eq!(fs::read(fixture.source()).unwrap(), b"unrelated replacement source");
	verify_zip(&fixture.zip());
	verify_gzip(&fixture.gzip());
	assert!(fixture.store().load(&id).unwrap().unwrap().pinned);
	assert_eq!(
		fs::read(fixture.store().source(&id).unwrap().unwrap()).unwrap(),
		fixture.original
	);
	assert_orphan_preserved(&fixture, &orphan);
}

#[test]
fn a_replacement_output_after_an_undo_claim_is_not_removed() {
	let phase = Phase::UndoOutputClaimed;
	let Interrupted {
		fixture, id, orphan, ..
	} = interrupt_at(phase, Route::Direct);
	fs::write(fixture.zip(), b"unrelated replacement output").unwrap();
	let _daemon = fixture.daemon(None);
	fixture.wait_for_reconciliation(&id, phase);
	let undo = commands::bounded(&mut fixture.undo(&id), Duration::from_secs(20)).unwrap();
	assert!(
		!undo.status.success() && undo.stderr.contains("already exists"),
		"a replacement output must be refused: stdout: {}; stderr: {}",
		undo.stdout,
		undo.stderr
	);
	assert_eq!(fs::read(fixture.source()).unwrap(), fixture.original);
	assert_eq!(fs::read(fixture.zip()).unwrap(), b"unrelated replacement output");
	verify_gzip(&fixture.gzip());
	assert!(fixture.store().load(&id).unwrap().unwrap().pinned);
	assert_orphan_preserved(&fixture, &orphan);
}
