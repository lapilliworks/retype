/* Test-only named crash checkpoints. No release binary reads these controls. */

use std::cell::RefCell;
use std::fs::{self, File};
use std::io::{self, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::convert::StagedOutput;
use crate::error::Result;
use crate::job::{Job, JobId, JobState, Output, OutputState, Source, SourceDisposition, Tier};
use crate::journal::Journal;
use crate::safety::Fingerprint;
use crate::undo::{Store, Subject};

const ID: &str = "01J00000000000000000000001";
const OWNER: &[u8] = b"retype-durability-fixture-1\n";
const ORIGINAL: &[u8] = b"synthetic recoverable original";
const OUTPUT: &[u8] = b"synthetic published output";
const UNRELATED: &[u8] = b"unrelated file must not change";
const ROOT_ENV: &str = "RETYPE_DURABILITY_ROOT";
const ACTION_ENV: &str = "RETYPE_DURABILITY_ACTION";
const WORKER: &str = "durability_tests::recovery_worker";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Operation {
	Publish,
	Dispose,
	Undo,
}

#[derive(Clone, Copy, Debug)]
struct Case {
	operation: Operation,
	checkpoint: &'static str,
}

macro_rules! recovery_cases {
	($($name:ident: $operation:ident, $checkpoint:literal;)*) => {
		const CASES: &[Case] = &[$(Case { operation: Operation::$operation, checkpoint: $checkpoint }),*];
		$(
			#[test]
			fn $name() {
				let case = Case { operation: Operation::$operation, checkpoint: $checkpoint };
				assert_error_recovery(case);
				assert_process_crash_recovery(case);
			}
		)*
	};
}

recovery_cases! {
	publish_intent: Publish, "publication.intent-durable";
	publish_prepared: Publish, "publication.prepared";
	publish_renamed: Publish, "publication.renamed";
	publish_durable: Publish, "publication.durable";
	dispose_claimed: Dispose, "cleanup.claimed";
	dispose_claim_durable: Dispose, "cleanup.claim-durable";
	dispose_unlinked: Dispose, "cleanup.unlinked";
	dispose_cleanup_complete: Dispose, "cleanup.complete";
	dispose_recorded: Dispose, "disposition.recorded";
	dispose_complete: Dispose, "disposition.complete";
	undo_started: Undo, "undo.started";
	undo_intent: Undo, "undo.intent-durable";
	undo_publication_prepared: Undo, "publication.prepared";
	undo_publication_renamed: Undo, "publication.renamed";
	undo_publication_durable: Undo, "publication.durable";
	undo_source_durable: Undo, "undo.source-durable";
	undo_claimed: Undo, "cleanup.claimed";
	undo_claim_durable: Undo, "cleanup.claim-durable";
	undo_unlinked: Undo, "cleanup.unlinked";
	undo_cleanup_complete: Undo, "cleanup.complete";
	undo_before_forget: Undo, "undo.before-forget";
	undo_complete: Undo, "undo.complete";
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
	operation: Operation,
	checkpoint: String,
}

impl Request {
	fn case(&self) -> Case {
		*CASES
			.iter()
			.find(|case| case.operation == self.operation && case.checkpoint == self.checkpoint)
			.expect("unknown durability operation/checkpoint")
	}
}

#[derive(Clone)]
enum Injection {
	None,
	Fail,
	Stop,
	Cancel(crate::execute::Cancellation),
}

#[derive(Clone)]
struct Fault {
	root: PathBuf,
	case: Case,
	injection: Injection,
}

thread_local! {
	static FAULT: RefCell<Option<Fault>> = const { RefCell::new(None) };
}

struct Scope {
	previous: Option<Fault>,
	_not_send: PhantomData<Rc<()>>,
}

impl Scope {
	fn enter(root: &Path, case: Case, injection: Injection) -> Self {
		assert_eq!(fs::read(root.join("owner")).unwrap(), OWNER);
		let fault = Fault {
			root: root.canonicalize().unwrap(),
			case,
			injection,
		};
		Self {
			previous: FAULT.with(|slot| slot.replace(Some(fault))),
			_not_send: PhantomData,
		}
	}
}

impl Drop for Scope {
	fn drop(&mut self) {
		FAULT.with(|slot| {
			slot.replace(self.previous.take());
		});
	}
}

pub(crate) fn publication_checkpoint(name: &str, target: &Path) -> io::Result<()> {
	/* Manifests use the same publisher; stop only at the selected original or output publication. */
	let is_selected = FAULT.with(|slot| {
		let state = slot.borrow();
		let Some(fault) = state.as_ref() else { return false };
		let selected = match fault.case.operation {
			Operation::Publish => fault.root.join("photos/generated.jpg"),
			Operation::Undo => fault.root.join("photos/original.heic"),
			Operation::Dispose => return false,
		};
		target == selected
	});
	if is_selected { checkpoint(name) } else { Ok(()) }
}

pub(crate) fn checkpoint(name: &str) -> io::Result<()> {
	let fault = FAULT.with(|slot| slot.borrow().clone());
	let Some(fault) = fault else { return Ok(()) };
	if fault.case.checkpoint != name {
		return Ok(());
	}
	match fault.injection {
		Injection::None => Ok(()),
		Injection::Fail => Err(io::Error::other(format!("injected durability failure at {name}"))),
		Injection::Cancel(cancellation) => {
			cancellation.request_cancel();
			Ok(())
		}
		Injection::Stop => {
			/* Do not flush this marker: doing so could harden the operation being interrupted. */
			fs::write(fault.root.join("checkpoint"), name)?;
			println!("RETYPE_CHECKPOINT {name}");
			io::stdout().flush()?;
			let deadline = Instant::now() + Duration::from_secs(120);
			while Instant::now() < deadline {
				std::thread::sleep(Duration::from_millis(20));
			}
			Err(io::Error::new(
				io::ErrorKind::TimedOut,
				"checkpoint was not interrupted within 120s",
			))
		}
	}
}

struct Fixture {
	root: PathBuf,
	case: Case,
}

impl Fixture {
	fn prepare(root: &Path, case: Case) -> Self {
		fs::create_dir(root).expect("durability fixture must use a new directory");
		let root = root.canonicalize().unwrap();
		fs::write(root.join("owner"), OWNER).unwrap();
		let fixture = Self { root, case };
		let _scope = Scope::enter(&fixture.root, case, Injection::None);
		crate::fsx::create_dir_all_durable(&fixture.root.join("photos")).unwrap();
		fs::write(fixture.source(), ORIGINAL).unwrap();
		fs::write(fixture.sentinel(), UNRELATED).unwrap();
		crate::fsx::sync_file(&fixture.source()).unwrap();
		crate::fsx::sync_file(&fixture.sentinel()).unwrap();
		crate::fsx::sync_directory(&fixture.root.join("photos")).unwrap();
		fixture
			.store()
			.save(Subject {
				id: fixture.id(),
				intent: "jpg".to_string(),
				machine: "durability-fixture".to_string(),
				bytes: fixture.source(),
				source_path: fixture.source(),
				source_ext: "heic".to_string(),
				outputs: vec![crate::undo::Output {
					path: fixture.output(),
					ext: "jpg".to_string(),
				}],
				consume_source: true,
			})
			.unwrap();
		if case.operation != Operation::Publish {
			fixture.publish().unwrap();
		}
		if case.operation == Operation::Undo {
			fixture.dispose().unwrap();
		}
		let request = Request {
			operation: case.operation,
			checkpoint: case.checkpoint.to_string(),
		};
		fs::write(fixture.root.join("request.json"), serde_json::to_vec(&request).unwrap()).unwrap();
		crate::fsx::sync_file(&fixture.root.join("request.json")).unwrap();
		crate::fsx::sync_file(&fixture.root.join("owner")).unwrap();
		crate::fsx::sync_directory(&fixture.root).unwrap();
		crate::fsx::sync_directory(fixture.root.parent().unwrap()).unwrap();
		fixture
	}

	fn load(root: &Path) -> Self {
		assert_eq!(
			fs::read(root.join("owner")).unwrap(),
			OWNER,
			"not an owned synthetic fixture"
		);
		let request: Request = serde_json::from_slice(&fs::read(root.join("request.json")).unwrap()).unwrap();
		let fixture = Self {
			root: root.canonicalize().unwrap(),
			case: request.case(),
		};
		if let Some(record) = fixture.store().load(&fixture.id()).unwrap() {
			assert_eq!(record.id, fixture.id());
			assert_eq!(record.source_path, fixture.source());
			assert_eq!(record.source_ext, "heic");
			assert_eq!(record.source_size, ORIGINAL.len() as u64);
			assert_eq!(
				record.outputs,
				[crate::undo::Output {
					path: fixture.output(),
					ext: "jpg".to_string()
				}]
			);
			assert!(
				record
					.commits
					.iter()
					.all(|commit| commit.output.path == fixture.output())
			);
		}
		fixture.verify();
		fixture
	}

	fn id(&self) -> JobId {
		JobId::parse(ID).unwrap()
	}
	fn store(&self) -> Store {
		Store::open(&self.root.join("state"))
	}
	fn source(&self) -> PathBuf {
		self.root.join("photos/original.heic")
	}
	fn output(&self) -> PathBuf {
		self.root.join("photos/generated.jpg")
	}
	fn sentinel(&self) -> PathBuf {
		self.root.join("photos/unrelated.txt")
	}

	fn produced(&self) -> Output {
		let mut output = Output::pending(self.output(), "jpg", Tier::Converted);
		output.state = OutputState::Done;
		output.bytes = Some(OUTPUT.len() as u64);
		output.identity = crate::job::FileIdentity::of(&self.output());
		output
	}

	fn publish(&self) -> Result<()> {
		let staged = StagedOutput::new(&self.output())?;
		fs::write(staged.temp(), OUTPUT)?;
		let checked = Fingerprint::of(staged.temp())?;
		self.store()
			.prepare_checked_output(&self.id(), self.produced(), staged.temp(), Some(&checked))?;
		staged.commit()?;
		let source = Source {
			path: self.source(),
			format: "heic".to_string(),
			size: ORIGINAL.len() as u64,
			mtime_ns: 0,
			fingerprint: Some(Fingerprint::of(&self.source())?),
		};
		let mut job = Job::new(
			source,
			Some("jpg".to_string()),
			vec![self.produced()],
			"durability-fixture",
		)?;
		job.id = self.id();
		job.consume_source = true;
		job.source_disposition = Some(SourceDisposition::Pending);
		job.set_state(JobState::Done);
		let (mut journal, _) = Journal::open(&self.root.join("state/jobs.jsonl"))?;
		journal.append(&job)?;
		Ok(())
	}

	fn dispose(&self) -> Result<()> {
		let store = self.store();
		let _lease = store.lease(&self.id())?;
		let record = store.load(&self.id())?.unwrap();
		let _guard = store.guard_disposal(&self.id(), &[self.produced()])?;
		let (mut journal, replay) = Journal::open(&self.root.join("state/jobs.jsonl"))?;
		let mut job = replay.jobs.into_iter().find(|job| job.id == self.id()).unwrap();
		assert_eq!(job.state, JobState::Done);
		assert_eq!(job.source_disposition, Some(SourceDisposition::Pending));
		crate::safety::remove_owned(&self.source(), record.source_fingerprint.as_ref().unwrap())?;
		job.source_disposition = Some(SourceDisposition::Removed);
		journal.append(&job)?;
		checkpoint("disposition.recorded")?;
		store.finish(&self.id())?;
		checkpoint("disposition.complete")?;
		Ok(())
	}

	fn run(&self) -> Result<()> {
		match self.case.operation {
			Operation::Publish => self.publish(),
			Operation::Dispose => self.dispose(),
			Operation::Undo => self.store().restore(&self.id()).map(|_| ()),
		}
	}

	fn verify(&self) {
		assert_eq!(fs::read(self.sentinel()).unwrap(), UNRELATED);
		let source_exists = self.source().try_exists().unwrap();
		if source_exists {
			assert_eq!(fs::read(self.source()).unwrap(), ORIGINAL);
		}
		if self.output().try_exists().unwrap() {
			assert_eq!(fs::read(self.output()).unwrap(), OUTPUT);
		}
		if let Some(record) = self.store().load(&self.id()).unwrap() {
			let backup = self
				.store()
				.source(&self.id())
				.unwrap()
				.expect("verified recovery bytes");
			assert_eq!(fs::read(backup).unwrap(), ORIGINAL);
			if record.undo_progress.is_some() {
				assert!(record.pinned);
			}
		} else {
			assert!(source_exists, "neither the original nor recovery exists");
			assert!(
				!self.output().exists(),
				"cleanup record disappeared before output removal"
			);
		}
	}

	fn verify_interruption(&self) {
		self.verify();
		let record = self.store().load(&self.id()).unwrap();
		if self.case.operation == Operation::Undo && self.case.checkpoint == "undo.complete" {
			assert!(record.is_none(), "completed undo retained its recovery record");
			return;
		}
		let record = record.expect("interrupted operation lost its recovery record");
		if self.case.operation == Operation::Dispose && self.case.checkpoint == "disposition.complete" {
			assert!(!record.pinned && record.retention_started.is_some());
		} else {
			assert!(record.pinned, "unfinished recovery was released");
		}
		if self.case.operation == Operation::Undo {
			assert!(record.undo_progress.is_some(), "undo intent was not retained");
		}
	}

	fn recover(&self) {
		let _scope = Scope::enter(&self.root, self.case, Injection::None);
		self.verify();
		if self.store().load(&self.id()).unwrap().is_some() {
			self.store().restore(&self.id()).unwrap();
		}
		self.verify();
		assert_eq!(fs::read(self.source()).unwrap(), ORIGINAL);
		assert!(!self.output().exists());
		assert!(self.store().load(&self.id()).unwrap().is_none());
	}
}

struct Worker(Child);

impl Worker {
	fn interrupt(&mut self) {
		self.0.kill().unwrap();
		let deadline = Instant::now() + Duration::from_secs(3);
		loop {
			if let Some(status) = self.0.try_wait().unwrap() {
				assert!(!status.success(), "interrupted worker reported success");
				return;
			}
			assert!(Instant::now() < deadline, "interrupted worker did not exit");
			std::thread::sleep(Duration::from_millis(20));
		}
	}
}

impl Drop for Worker {
	fn drop(&mut self) {
		if self.0.try_wait().ok().flatten().is_some() {
			return;
		}
		if let Err(error) = self.0.kill() {
			eprintln!("cannot stop durability worker: {error}");
		}
		let deadline = Instant::now() + Duration::from_secs(3);
		while Instant::now() < deadline {
			if self.0.try_wait().ok().flatten().is_some() {
				return;
			}
			std::thread::sleep(Duration::from_millis(20));
		}
		eprintln!("durability worker did not exit after termination");
	}
}

fn interrupt(fixture: &Fixture) {
	let log = File::create(fixture.root.join("worker.log")).unwrap();
	let child = Command::new(std::env::current_exe().unwrap())
		.args(["--exact", WORKER, "--nocapture"])
		.env(ROOT_ENV, &fixture.root)
		.env(ACTION_ENV, "run")
		.stdin(Stdio::null())
		.stdout(Stdio::from(log.try_clone().unwrap()))
		.stderr(Stdio::from(log))
		.spawn()
		.unwrap();
	let mut worker = Worker(child);
	let deadline = Instant::now() + Duration::from_secs(20);
	loop {
		if fs::read(fixture.root.join("checkpoint")).is_ok_and(|bytes| bytes == fixture.case.checkpoint.as_bytes()) {
			break;
		}
		let status = worker.0.try_wait().unwrap();
		assert!(
			status.is_none() && Instant::now() < deadline,
			"worker did not reach {:?}: {status:?}; {}",
			fixture.case,
			fs::read_to_string(fixture.root.join("worker.log")).unwrap()
		);
		std::thread::sleep(Duration::from_millis(20));
	}
	#[cfg(windows)]
	assert_claim_is_guarded(fixture);
	worker.interrupt();
}

#[cfg(windows)]
fn assert_claim_is_guarded(fixture: &Fixture) {
	if !matches!(fixture.case.checkpoint, "cleanup.claimed" | "cleanup.claim-durable") {
		return;
	}
	let name = match fixture.case.operation {
		Operation::Dispose => "original.heic",
		Operation::Undo => "generated.jpg",
		Operation::Publish => unreachable!(),
	};
	let claimed = fs::read_dir(fixture.root.join("photos"))
		.unwrap()
		.map(|entry| entry.unwrap().path().join(name))
		.find(|path| path.is_file())
		.expect("claimed synthetic file");
	let error = fs::OpenOptions::new().write(true).open(&claimed).unwrap_err();
	assert_eq!(
		error.raw_os_error(),
		Some(32),
		"claimed file admitted a competing writer: {error}"
	);
	assert!(crate::fsx::rename_noreplace(&claimed, &fixture.root.join("stolen")).is_err());
}

fn assert_error_recovery(case: Case) {
	let parent = tempfile::tempdir().unwrap();
	let fixture = Fixture::prepare(&parent.path().join("fixture"), case);
	let scope = Scope::enter(&fixture.root, case, Injection::Fail);
	let error = fixture.run().unwrap_err();
	assert!(
		error.to_string().contains("injected durability failure"),
		"{case:?}: {error}"
	);
	drop(scope);
	fixture.verify_interruption();
	fixture.recover();
}

fn assert_process_crash_recovery(case: Case) {
	let parent = tempfile::tempdir().unwrap();
	let fixture = Fixture::prepare(&parent.path().join("fixture"), case);
	interrupt(&fixture);
	fixture.verify_interruption();
	fixture.recover();
	fixture.recover();
}

#[test]
fn same_content_replacements_do_not_authorize_cleanup() {
	for operation in [Operation::Dispose, Operation::Undo] {
		let case = Case {
			operation,
			checkpoint: "cleanup.claimed",
		};
		let parent = tempfile::tempdir().unwrap();
		let fixture = Fixture::prepare(&parent.path().join("fixture"), case);
		let selected = if operation == Operation::Dispose {
			fixture.source()
		} else {
			fixture.output()
		};
		let expected = if operation == Operation::Dispose {
			ORIGINAL
		} else {
			OUTPUT
		};
		let saved = fixture.root.join("previous-file");
		crate::fsx::rename_noreplace(&selected, &saved).unwrap();
		fs::copy(&saved, &selected).unwrap();
		let _scope = Scope::enter(&fixture.root, case, Injection::None);
		assert!(fixture.run().is_err(), "replacement authorized cleanup: {operation:?}");
		assert_eq!(fs::read(&selected).unwrap(), expected);
		assert_eq!(fs::read(&saved).unwrap(), expected);
		assert!(fixture.store().load(&fixture.id()).unwrap().unwrap().pinned);
		fixture.verify();
	}
}

#[test]
fn cancellation_at_recovery_boundaries_preserves_bytes_and_can_be_retried() {
	for checkpoint in [
		"undo.started",
		"undo.source-durable",
		"cleanup.claimed",
		"cleanup.claim-durable",
	] {
		let case = Case {
			operation: Operation::Undo,
			checkpoint,
		};
		let parent = tempfile::tempdir().unwrap();
		let fixture = Fixture::prepare(&parent.path().join("fixture"), case);
		let cancellation = crate::execute::Cancellation::default();
		let scope = Scope::enter(&fixture.root, case, Injection::Cancel(cancellation.clone()));
		assert!(
			fixture
				.store()
				.restore_cancellable(&fixture.id(), &cancellation)
				.is_err()
		);
		assert!(cancellation.is_cancelled(), "checkpoint was not reached: {checkpoint}");
		drop(scope);
		fixture.verify_interruption();
		assert!(
			fixture.output().exists(),
			"cancelled recovery deleted its output: {checkpoint}"
		);
		fixture.recover();
	}
}

#[cfg(windows)]
#[test]
fn recovery_and_output_junctions_refuse_source_disposal() {
	for relative in ["state/undo", "photos"] {
		let parent = tempfile::tempdir().unwrap();
		let fixture = Fixture::prepare(
			&parent.path().join("fixture"),
			Case {
				operation: Operation::Dispose,
				checkpoint: "cleanup.claimed",
			},
		);
		let alias = fixture.root.join(relative);
		let target = fixture.root.join("relocated");
		crate::fsx::rename_noreplace(&alias, &target).unwrap();
		let linked = Command::new("cmd.exe")
			.args(["/d", "/c", "mklink", "/J"])
			.arg(&alias)
			.arg(&target)
			.output()
			.unwrap();
		assert!(linked.status.success(), "junction fixture failed: {linked:?}");
		assert!(fixture.run().is_err(), "junction authorized disposal: {relative}");
		assert_eq!(fs::read(fixture.source()).unwrap(), ORIGINAL);
		assert_eq!(fs::read(fixture.output()).unwrap(), OUTPUT);
		assert!(fixture.store().load(&fixture.id()).unwrap().unwrap().pinned);
		fixture.verify();
		fs::remove_dir(&alias).unwrap();
		crate::fsx::rename_noreplace(&target, &alias).unwrap();
	}
}

#[cfg(windows)]
#[test]
fn read_only_outputs_refuse_cleanup_without_losing_recovery() {
	let case = Case {
		operation: Operation::Undo,
		checkpoint: "cleanup.claimed",
	};
	let parent = tempfile::tempdir().unwrap();
	let fixture = Fixture::prepare(&parent.path().join("fixture"), case);
	let _scope = Scope::enter(&fixture.root, case, Injection::None);
	let permissions = fs::metadata(fixture.output()).unwrap().permissions();
	let mut protected = permissions.clone();
	protected.set_readonly(true);
	fs::set_permissions(fixture.output(), protected).unwrap();
	let failed = fixture.run();
	fs::set_permissions(fixture.output(), permissions).unwrap();
	assert!(failed.is_err(), "read-only output authorized cleanup");
	fixture.verify_interruption();
	assert_eq!(fs::read(fixture.output()).unwrap(), OUTPUT);
	fixture.recover();
}

#[cfg(windows)]
#[test]
fn denied_delete_permission_preserves_recovery_and_allows_retry() {
	let case = Case {
		operation: Operation::Undo,
		checkpoint: "cleanup.claimed",
	};
	let parent = tempfile::tempdir().unwrap();
	let fixture = Fixture::prepare(&parent.path().join("fixture"), case);
	let _scope = Scope::enter(&fixture.root, case, Injection::None);
	let user = format!(
		"{}\\{}",
		std::env::var("USERDOMAIN").unwrap(),
		std::env::var("USERNAME").unwrap()
	);
	let deny = Command::new("icacls.exe")
		.arg(fixture.output())
		.args(["/deny", &format!("{user}:(D)")])
		.output()
		.unwrap();
	assert!(deny.status.success(), "cannot set synthetic deny-delete ACL: {deny:?}");
	let failed = fixture.run();
	let restore = Command::new("icacls.exe")
		.arg(fixture.output())
		.args(["/remove:d", &user])
		.output()
		.unwrap();
	assert!(restore.status.success(), "cannot restore synthetic ACL: {restore:?}");
	assert_eq!(failed.unwrap_err().code(), crate::error::Code::IoPermission);
	fixture.verify_interruption();
	assert_eq!(fs::read(fixture.output()).unwrap(), OUTPUT);
	fixture.recover();
}

#[cfg(windows)]
fn actual_disk_exhaustion(root: &Path) {
	use std::os::windows::ffi::OsStrExt;
	use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
	let path: Vec<_> = root
		.parent()
		.unwrap()
		.as_os_str()
		.encode_wide()
		.chain(Some(0))
		.collect();
	let mut capacity = 0;
	assert_ne!(
		unsafe { GetDiskFreeSpaceExW(path.as_ptr(), std::ptr::null_mut(), &mut capacity, std::ptr::null_mut()) },
		0
	);
	assert!(
		capacity <= 128 * 1024 * 1024,
		"space tests require a disposable volume of at most 128 MiB"
	);
	let case = Case {
		operation: Operation::Undo,
		checkpoint: "undo.started",
	};
	let fixture = Fixture::prepare(root, case);
	let _scope = Scope::enter(&fixture.root, case, Injection::None);
	{
		let pin = Scope::enter(&fixture.root, case, Injection::Fail);
		assert!(
			fixture
				.run()
				.unwrap_err()
				.to_string()
				.contains("injected durability failure")
		);
		drop(pin);
	}
	let filler = fixture.root.join("synthetic-space-filler");
	let mut file = File::create(&filler).unwrap();
	let block = [0x5au8; 4096];
	let mut written = 0u64;
	loop {
		match file.write_all(&block) {
			Ok(()) => {
				written += block.len() as u64;
				assert!(written <= capacity);
			}
			Err(error) => {
				assert_eq!(crate::error::code_for_io(&error), crate::error::Code::IoSpace);
				break;
			}
		}
	}
	drop(file);
	let error = fixture.run().unwrap_err();
	assert_eq!(error.code(), crate::error::Code::IoSpace, "{error}");
	fixture.verify();
	assert!(fixture.output().exists());
	assert!(fixture.store().load(&fixture.id()).unwrap().unwrap().pinned);
	fs::remove_file(filler).unwrap();
	fixture.recover();
	println!("Actual NTFS disk exhaustion preserved recovery and exact-byte retry; filled {written} bytes");
}

#[test]
fn recovery_worker() {
	let Ok(action) = std::env::var(ACTION_ENV) else { return };
	let root = PathBuf::from(std::env::var_os(ROOT_ENV).expect("private fixture root required"));
	assert!(root.is_absolute(), "durability fixture root must be absolute");
	#[cfg(windows)]
	if action == "space" {
		actual_disk_exhaustion(&root);
		return;
	}
	if action == "prepare" {
		let request = Request {
			operation: serde_json::from_value(serde_json::Value::String(
				std::env::var("RETYPE_DURABILITY_OPERATION").unwrap(),
			))
			.unwrap(),
			checkpoint: std::env::var("RETYPE_DURABILITY_CHECKPOINT").unwrap(),
		};
		Fixture::prepare(&root, request.case());
		println!("Synthetic fixture prepared: {}", root.display());
		return;
	}
	let fixture = Fixture::load(&root);
	match action.as_str() {
		"run" => {
			let _scope = Scope::enter(&fixture.root, fixture.case, Injection::Stop);
			fixture.run().unwrap();
			panic!("requested checkpoint was not reached");
		}
		"inspect" => {
			fixture.verify_interruption();
			println!("Pre-recovery inspection passed: {:?}", fixture.case);
		}
		"recover" => {
			fixture.recover();
			println!("Recovery inspection passed: {:?}", fixture.case);
		}
		_ => panic!("unknown durability worker action"),
	}
}

#[test]
fn crash_checkpoints_are_confined_to_their_thread_and_publication() {
	let parent = tempfile::tempdir().unwrap();
	let case = Case {
		operation: Operation::Publish,
		checkpoint: "publication.prepared",
	};
	let fixture = Fixture::prepare(&parent.path().join("fixture"), case);
	let _scope = Scope::enter(&fixture.root, fixture.case, Injection::Fail);
	let outside = parent.path().join("outside");
	publication_checkpoint(case.checkpoint, &outside).unwrap();
	assert!(publication_checkpoint(case.checkpoint, &fixture.output()).is_err());
	let output = fixture.output();
	std::thread::spawn(move || publication_checkpoint(case.checkpoint, &output).unwrap())
		.join()
		.unwrap();
	assert_eq!(fs::read(fixture.source()).unwrap(), ORIGINAL);
}

#[test]
fn durable_directory_creation_checks_occupied_names() {
	let root = tempfile::tempdir().unwrap();
	let nested = root.path().join("state/undo/job");
	crate::fsx::create_dir_all_durable(&nested).unwrap();
	crate::fsx::create_dir_all_durable(&nested).unwrap();
	let file = root.path().join("occupied");
	fs::write(&file, UNRELATED).unwrap();
	assert!(crate::fsx::create_dir_all_durable(&file).is_err());
	assert_eq!(fs::read(file).unwrap(), UNRELATED);
}
