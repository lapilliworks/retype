//! Process interruption at undo checkpoints, not power-loss durability evidence.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::*;

const ID: &str = "01J00000000000000000000000";
const WORKER: &str = "undo::recovery_tests::interrupted_undo_worker";
const ROOT: &str = "RETYPE_UNDO_CRASH_ROOT";
const PHASE: &str = "RETYPE_UNDO_CRASH_PHASE";
const ORIGINAL: &[u8] = b"synthetic original";
const OUTPUT: &[u8] = b"synthetic output";

struct Fixture {
	root: tempfile::TempDir,
	store: Store,
	record: Record,
	output: PathBuf,
}

impl Fixture {
	fn new(consume_source: bool) -> Self {
		let root = tempfile::tempdir().unwrap();
		let photos = root.path().join("photos");
		fs::create_dir(&photos).unwrap();
		let source = photos.join("photo.heic");
		let output = photos.join("photo.jpg");
		fs::write(&source, ORIGINAL).unwrap();
		fs::write(&output, OUTPUT).unwrap();
		let store = Store::open(&root.path().join("state"));
		let record = store
			.save(Subject {
				id: JobId::parse(ID).unwrap(),
				intent: "jpg".to_string(),
				machine: "test".to_string(),
				bytes: source.clone(),
				source_path: source.clone(),
				source_ext: "heic".to_string(),
				outputs: vec![Output {
					path: output.clone(),
					ext: "jpg".to_string(),
				}],
				consume_source,
			})
			.unwrap();
		let produced = crate::job::Output::pending(output.clone(), "jpg", crate::job::Tier::Converted);
		store.prepare_output(&record.id, produced, &output).unwrap();
		store.finish(&record.id).unwrap();
		let mut finished = store.load(&record.id).unwrap().unwrap();
		finished.retention_started = Some(crate::job::now_ms() - 60_000);
		write_manifest(&store.directory(&record.id), &finished).unwrap();
		if consume_source {
			/* Keep the old file alive so its identity cannot be reused by a replacement. */
			fs::rename(&source, root.path().join("previous-original.heic")).unwrap();
		}
		Self {
			root,
			store,
			record,
			output,
		}
	}

	fn interrupt(&self, phase: &str) {
		interrupt_worker(self.root.path(), phase);
	}

	fn assert_recovery_is_pinned(&self) {
		let record = self.store.load(&self.record.id).unwrap().unwrap();
		assert!(record.pinned);
		assert!(record.undo_progress.is_some());
		assert!(self.store.source(&self.record.id).unwrap().is_some());
		assert!(self.store.finish(&self.record.id).is_err());
		assert!(self.store.pin(&self.record.id).is_err());
		assert!(self.store.sweep(Duration::ZERO).unwrap().is_empty());
	}

	fn assert_cleanup_outcome(&self, result: Result<Record>) {
		assert_eq!(fs::read(&self.record.source_path).unwrap(), ORIGINAL);
		if crate::fsx::supports_durable_cleanup() {
			result.unwrap();
			assert!(!self.output.exists());
			assert!(self.store.load(&self.record.id).unwrap().is_none());
		} else {
			assert!(result.unwrap_err().to_string().contains("directory durability"));
			assert_eq!(fs::read(&self.output).unwrap(), OUTPUT);
			self.assert_recovery_is_pinned();
		}
	}
}

struct Worker(Option<Child>);

impl Drop for Worker {
	fn drop(&mut self) {
		if let Some(mut child) = self.0.take() {
			let _ = child.kill();
			let _ = child.wait();
		}
	}
}

fn interrupt_worker(root: &Path, phase: &str) {
	let stderr = fs::File::create(root.join("worker.stderr")).unwrap();
	let mut worker = Worker(Some(
		Command::new(std::env::current_exe().unwrap())
			.args(["--exact", WORKER, "--nocapture"])
			.env(ROOT, root)
			.env(PHASE, phase)
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::from(stderr))
			.spawn()
			.unwrap(),
	));
	let deadline = Instant::now() + Duration::from_secs(20);
	loop {
		if fs::read(root.join("checkpoint")).is_ok_and(|bytes| bytes == phase.as_bytes()) {
			break;
		}
		if let Some(status) = worker.0.as_mut().unwrap().try_wait().unwrap() {
			worker.0.take();
			panic!(
				"undo worker exited before {phase}: {status}; {:?}",
				fs::read(root.join("worker.stderr"))
			);
		}
		assert!(Instant::now() < deadline, "undo worker did not reach {phase}");
		std::thread::sleep(Duration::from_millis(20));
	}
	let child = worker.0.as_mut().unwrap();
	child.kill().unwrap();
	assert!(!child.wait().unwrap().success());
	worker.0.take();
}

#[test]
fn interrupted_undo_worker() {
	let Some(root) = std::env::var_os(ROOT) else {
		return;
	};
	let root = PathBuf::from(root);
	let phase = std::env::var(PHASE).unwrap();
	let store = Store::open(&root.join("state"));
	let id = JobId::parse(ID).unwrap();
	if phase == "before-publication" {
		let _guard = store_lock(&store.root).unwrap();
		let _lease = store.lease(&id).unwrap();
		let mut record = store.load(&id).unwrap().unwrap();
		let policy = existing_source_policy(&record);
		store.begin_undo(&mut record, policy).unwrap();
		let held = store.source(&id).unwrap().unwrap();
		let (_staged, _) = store.prepare_restoration(&mut record, &held).unwrap();
		wait_for_interruption(&root, &phase).unwrap();
	} else if matches!(phase.as_str(), "after-publication" | "started") {
		store
			.restore_with_cleanup(&id, |_| wait_for_interruption(&root, &phase))
			.unwrap();
	} else {
		panic!("unknown undo checkpoint: {phase}");
	}
	panic!("parent did not interrupt the undo worker");
}

fn wait_for_interruption(root: &Path, phase: &str) -> Result<()> {
	fs::write(root.join("checkpoint"), phase.as_bytes())?;
	let deadline = Instant::now() + Duration::from_secs(30);
	while Instant::now() < deadline {
		std::thread::sleep(Duration::from_millis(20));
	}
	Err(std::io::Error::other("parent did not interrupt the undo worker").into())
}

#[test]
fn a_killed_undo_can_retry_before_and_after_restoration_publication() {
	for phase in ["before-publication", "after-publication"] {
		let fixture = Fixture::new(true);
		fixture.interrupt(phase);
		fixture.assert_recovery_is_pinned();
		let record = fixture.store.load(&fixture.record.id).unwrap().unwrap();
		let intent = record.undo_progress.unwrap().restore_intent.unwrap();
		if phase == "before-publication" {
			assert!(!fixture.record.source_path.exists());
		} else {
			assert!(intent.matches(&fixture.record.source_path).unwrap());
		}
		fixture.assert_cleanup_outcome(fixture.store.restore(&fixture.record.id));
		if !crate::fsx::supports_durable_cleanup() {
			fixture.assert_cleanup_outcome(fixture.store.restore(&fixture.record.id));
		}
	}
}

#[test]
fn a_same_content_replacement_of_the_restored_original_is_not_accepted() {
	let fixture = Fixture::new(true);
	fixture.interrupt("after-publication");
	let saved = fixture.root.path().join("restored-original.heic");
	fs::rename(&fixture.record.source_path, &saved).unwrap();
	fs::copy(&saved, &fixture.record.source_path).unwrap();

	let error = fixture.store.restore(&fixture.record.id).unwrap_err();
	assert_eq!(error.code(), Code::IoExists);
	assert_eq!(fs::read(&fixture.record.source_path).unwrap(), ORIGINAL);
	assert_eq!(fs::read(&saved).unwrap(), ORIGINAL);
	assert_eq!(fs::read(&fixture.output).unwrap(), OUTPUT);
	fixture.assert_recovery_is_pinned();
}

#[test]
fn an_occupied_source_after_the_restore_intent_is_not_overwritten() {
	let fixture = Fixture::new(true);
	fixture.interrupt("before-publication");
	fs::write(&fixture.record.source_path, b"unrelated occupant").unwrap();

	let error = fixture.store.restore(&fixture.record.id).unwrap_err();
	assert_eq!(error.code(), Code::IoExists);
	assert_eq!(fs::read(&fixture.record.source_path).unwrap(), b"unrelated occupant");
	assert_eq!(fs::read(&fixture.output).unwrap(), OUTPUT);
	fixture.assert_recovery_is_pinned();
}

#[test]
fn a_refused_undo_pins_its_expired_recovery_record() {
	let fixture = Fixture::new(true);
	let saved = fixture.root.path().join("prior-output.jpg");
	fs::rename(&fixture.output, &saved).unwrap();
	fs::copy(&saved, &fixture.output).unwrap();

	let error = fixture.store.restore(&fixture.record.id).unwrap_err();
	assert_eq!(error.code(), Code::IoExists);
	assert!(!fixture.record.source_path.exists());
	assert_eq!(fs::read(&fixture.output).unwrap(), OUTPUT);
	fixture.assert_recovery_is_pinned();
}

#[test]
fn a_same_content_replacement_of_an_output_is_not_removed() {
	let fixture = Fixture::new(true);
	fixture.interrupt("after-publication");
	let saved = fixture.root.path().join("prior-output.jpg");
	fs::rename(&fixture.output, &saved).unwrap();
	fs::copy(&saved, &fixture.output).unwrap();

	let error = fixture.store.restore(&fixture.record.id).unwrap_err();
	assert_eq!(error.code(), Code::IoExists);
	assert_eq!(fs::read(&fixture.record.source_path).unwrap(), ORIGINAL);
	assert_eq!(fs::read(&fixture.output).unwrap(), OUTPUT);
	assert_eq!(fs::read(&saved).unwrap(), OUTPUT);
	fixture.assert_recovery_is_pinned();
}

#[test]
fn changing_a_restored_original_during_cleanup_keeps_the_recovery_record() {
	let fixture = Fixture::new(true);
	let result = fixture.store.restore_with_cleanup(&fixture.record.id, |record| {
		fs::remove_file(&fixture.output)?;
		fs::write(&record.source_path, b"changed during cleanup")?;
		Ok(())
	});

	let error = result.unwrap_err();
	#[cfg(windows)]
	{
		assert_sharing_violation(&error);
		assert_eq!(fs::read(&fixture.record.source_path).unwrap(), ORIGINAL);
	}
	#[cfg(not(windows))]
	{
		assert!(error.to_string().contains("changed during undo"));
		assert_eq!(
			fs::read(&fixture.record.source_path).unwrap(),
			b"changed during cleanup"
		);
	}
	assert!(!fixture.output.exists());
	fixture.assert_recovery_is_pinned();
}

#[test]
fn replacing_the_restored_original_during_cleanup_keeps_the_recovery_record() {
	let fixture = Fixture::new(true);
	let saved = fixture.root.path().join("restored-original.heic");
	let result = fixture.store.restore_with_cleanup(&fixture.record.id, |record| {
		fs::remove_file(&fixture.output)?;
		crate::fsx::rename_noreplace(&record.source_path, &saved)?;
		fs::copy(&saved, &record.source_path)?;
		Ok(())
	});

	let error = result.unwrap_err();
	#[cfg(windows)]
	{
		assert_sharing_violation(&error);
		assert!(!saved.exists());
	}
	#[cfg(not(windows))]
	{
		assert!(error.to_string().contains("changed during undo"));
		assert_eq!(fs::read(&saved).unwrap(), ORIGINAL);
	}
	assert_eq!(fs::read(&fixture.record.source_path).unwrap(), ORIGINAL);
	assert!(!fixture.output.exists());
	fixture.assert_recovery_is_pinned();
}

#[test]
fn changing_a_kept_original_during_cleanup_keeps_the_recovery_record() {
	let fixture = Fixture::new(false);
	fs::write(&fixture.record.source_path, b"earlier user edits").unwrap();
	let result = fixture.store.restore_with_cleanup(&fixture.record.id, |record| {
		fs::remove_file(&fixture.output)?;
		fs::write(&record.source_path, b"later user edits")?;
		Ok(())
	});

	let error = result.unwrap_err();
	#[cfg(windows)]
	{
		assert_sharing_violation(&error);
		assert_eq!(fs::read(&fixture.record.source_path).unwrap(), b"earlier user edits");
	}
	#[cfg(not(windows))]
	{
		assert!(error.to_string().contains("changed during undo"));
		assert_eq!(fs::read(&fixture.record.source_path).unwrap(), b"later user edits");
	}
	assert!(!fixture.output.exists());
	fixture.assert_recovery_is_pinned();
}

#[cfg(windows)]
fn assert_sharing_violation(error: &Error) {
	use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;
	match error {
		Error::Io(error) => assert_eq!(error.raw_os_error(), Some(ERROR_SHARING_VIOLATION as i32)),
		other => panic!("expected a sharing violation, got {other}"),
	}
}

#[test]
#[cfg(windows)]
fn windows_undo_holds_source_and_backup_against_writes_and_renames() {
	let fixture = Fixture::new(true);
	let backup = fixture
		.store
		.directory(&fixture.record.id)
		.join(fixture.record.source_file());
	let moved_source = fixture.root.path().join("moved-source.heic");
	let moved_backup = fixture.root.path().join("moved-backup.heic");
	let result = fixture.store.restore_with_cleanup(&fixture.record.id, |record| {
		for path in [&record.source_path, &backup] {
			let error = fs::OpenOptions::new().write(true).open(path).unwrap_err();
			assert_sharing_violation(&error.into());
		}
		for (path, target) in [(&record.source_path, &moved_source), (&backup, &moved_backup)] {
			let error = crate::fsx::rename_noreplace(path, target).unwrap_err();
			assert_sharing_violation(&error.into());
		}
		Err(std::io::Error::other("stop after checking undo handles").into())
	});

	assert!(
		result
			.unwrap_err()
			.to_string()
			.contains("stop after checking undo handles")
	);
	assert_eq!(fs::read(&fixture.record.source_path).unwrap(), ORIGINAL);
	assert_eq!(fs::read(&backup).unwrap(), ORIGINAL);
	assert_eq!(fs::read(&fixture.output).unwrap(), OUTPUT);
	assert!(!moved_source.exists());
	assert!(!moved_backup.exists());
	fixture.assert_recovery_is_pinned();
	/* The handles are released after a failed undo. */
	crate::fsx::rename_noreplace(&fixture.record.source_path, &moved_source).unwrap();
	crate::fsx::rename_noreplace(&backup, &moved_backup).unwrap();
}

#[test]
#[cfg(windows)]
fn windows_disposal_guard_blocks_output_and_backup_mutation() {
	let fixture = Fixture::new(true);
	let backup = fixture
		.store
		.directory(&fixture.record.id)
		.join(fixture.record.source_file());
	let mut output = crate::job::Output::pending(fixture.output.clone(), "jpg", crate::job::Tier::Converted);
	output.state = crate::job::OutputState::Done;
	output.bytes = Some(OUTPUT.len() as u64);
	let checked = Fingerprint::of(&fixture.output).unwrap();
	fixture
		.store
		.prepare_checked_output(&fixture.record.id, output.clone(), &fixture.output, Some(&checked))
		.unwrap();

	let guard = fixture.store.guard_disposal(&fixture.record.id, &[output]).unwrap();
	for (path, target) in [
		(&fixture.output, fixture.root.path().join("moved-output.jpg")),
		(&backup, fixture.root.path().join("moved-backup.heic")),
	] {
		let error = fs::OpenOptions::new().write(true).open(path).unwrap_err();
		assert_sharing_violation(&error.into());
		let error = crate::fsx::rename_noreplace(path, &target).unwrap_err();
		assert_sharing_violation(&error.into());
		assert!(!target.exists());
	}
	assert!(!crate::fsx::supports_durable_cleanup());
	drop(guard);
	crate::fsx::rename_noreplace(&backup, &fixture.root.path().join("moved-backup.heic")).unwrap();
	crate::fsx::rename_noreplace(&fixture.output, &fixture.root.path().join("moved-output.jpg")).unwrap();
}

#[test]
#[cfg(unix)]
fn a_read_only_restored_original_can_be_retried() {
	use std::os::unix::fs::PermissionsExt;
	let fixture = Fixture::new(true);
	let mut record = fixture.store.load(&fixture.record.id).unwrap().unwrap();
	record.source_mode = 0o400;
	write_manifest(&fixture.store.directory(&record.id), &record).unwrap();
	let first = fixture.store.restore_with_cleanup(&record.id, |_| {
		Err(std::io::Error::other("injected output-removal failure").into())
	});
	assert!(first.is_err());
	assert_eq!(
		fs::metadata(&record.source_path).unwrap().permissions().mode() & 0o777,
		0o400
	);
	fixture.assert_cleanup_outcome(fixture.store.restore(&record.id));
	assert_eq!(
		fs::metadata(&record.source_path).unwrap().permissions().mode() & 0o777,
		0o400
	);
}

#[test]
#[cfg(unix)]
fn undo_handles_refuse_a_final_component_symlink() {
	use std::os::unix::fs::symlink;
	let root = tempfile::tempdir().unwrap();
	let target = root.path().join("target");
	let link = root.path().join("link");
	fs::write(&target, ORIGINAL).unwrap();
	symlink(&target, &link).unwrap();
	assert!(open_undo_backup(&link).is_err());
	assert!(open_undo_source(&link).is_err());
	assert_eq!(fs::read(target).unwrap(), ORIGINAL);
}

#[test]
fn a_killed_keep_undo_does_not_overwrite_later_original_edits() {
	let fixture = Fixture::new(false);
	fixture.interrupt("started");
	fs::write(&fixture.record.source_path, b"user edits").unwrap();
	let result = fixture.store.restore(&fixture.record.id);
	assert_eq!(fs::read(&fixture.record.source_path).unwrap(), b"user edits");
	if crate::fsx::supports_durable_cleanup() {
		result.unwrap();
		assert!(!fixture.output.exists());
	} else {
		assert!(result.unwrap_err().to_string().contains("directory durability"));
		assert_eq!(fs::read(&fixture.output).unwrap(), OUTPUT);
		fixture.assert_recovery_is_pinned();
	}
}
