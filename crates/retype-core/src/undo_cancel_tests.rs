//! Redundant recovery cleanup preserves the original and never removes an output.

use super::*;

const ORIGINAL: &[u8] = b"synthetic original";

struct Fixture {
	_directory: tempfile::TempDir,
	store: Store,
	record: Record,
	output: PathBuf,
}

impl Fixture {
	fn new() -> Self {
		let directory = tempfile::tempdir().unwrap();
		let source = directory.path().join("photo.png");
		let output = directory.path().join("photo.jpg");
		fs::write(&source, ORIGINAL).unwrap();
		let store = Store::open(&directory.path().join("state"));
		let record = store
			.save(Subject {
				id: JobId::now().unwrap(),
				intent: "jpg".to_string(),
				machine: "test".to_string(),
				bytes: source.clone(),
				source_path: source,
				source_ext: "png".to_string(),
				outputs: vec![Output {
					path: output.clone(),
					ext: "jpg".to_string(),
				}],
				consume_source: true,
			})
			.unwrap();
		Self {
			_directory: directory,
			store,
			record,
			output,
		}
	}

	fn release(&self) -> Result<()> {
		let lease = self.store.lease(&self.record.id)?;
		let recovery = self.store.retain(lease, Some(self.record.clone()))?;
		let Recovery::Retained(recovery) = recovery else {
			unreachable!()
		};
		recovery.release_unpublished()
	}

	fn assert_recovery_remains(&self) {
		assert!(self.store.load(&self.record.id).unwrap().unwrap().pinned);
		assert!(self.store.source(&self.record.id).unwrap().is_some());
	}
}

#[test]
fn an_unpublished_copy_is_released_only_after_the_original_verifies() {
	let fixture = Fixture::new();
	let result = fixture.release();
	assert_eq!(fs::read(&fixture.record.source_path).unwrap(), ORIGINAL);
	if crate::fsx::supports_durable_cleanup() {
		result.unwrap();
		assert!(fixture.store.load(&fixture.record.id).unwrap().is_none());
	} else {
		assert!(result.unwrap_err().to_string().contains("directory durability"));
		fixture.assert_recovery_remains();
	}
}

#[test]
fn a_missing_original_is_restored_before_redundant_recovery_is_released() {
	let fixture = Fixture::new();
	fs::rename(
		&fixture.record.source_path,
		fixture._directory.path().join("held-original.png"),
	)
	.unwrap();
	let result = fixture.release();
	if crate::fsx::supports_durable_cleanup() {
		result.unwrap();
		assert_eq!(fs::read(&fixture.record.source_path).unwrap(), ORIGINAL);
		assert!(fixture.store.load(&fixture.record.id).unwrap().is_none());
	} else {
		assert!(result.is_err());
		assert!(!fixture.record.source_path.exists());
		fixture.assert_recovery_remains();
	}
}

#[test]
fn a_changed_original_keeps_recovery_and_is_not_overwritten() {
	let fixture = Fixture::new();
	fs::write(&fixture.record.source_path, b"later edits").unwrap();
	assert!(fixture.release().is_err());
	assert_eq!(fs::read(&fixture.record.source_path).unwrap(), b"later edits");
	fixture.assert_recovery_remains();
}

#[test]
fn an_identical_byte_replacement_keeps_recovery_and_is_not_overwritten() {
	let fixture = Fixture::new();
	fs::rename(
		&fixture.record.source_path,
		fixture._directory.path().join("held-original.png"),
	)
	.unwrap();
	fs::write(&fixture.record.source_path, ORIGINAL).unwrap();
	assert!(fixture.release().is_err());
	assert_eq!(fs::read(&fixture.record.source_path).unwrap(), ORIGINAL);
	fixture.assert_recovery_remains();
}

#[test]
fn a_publication_intent_keeps_recovery_even_when_no_output_is_reported() {
	let fixture = Fixture::new();
	fs::write(&fixture.output, b"published output").unwrap();
	let output = crate::job::Output::pending(fixture.output.clone(), "jpg", crate::job::Tier::Converted);
	fixture
		.store
		.prepare_output(&fixture.record.id, output, &fixture.output)
		.unwrap();
	assert!(fixture.release().is_err());
	assert_eq!(fs::read(&fixture.output).unwrap(), b"published output");
	fixture.assert_recovery_remains();
	assert!(
		fixture
			.store
			.load(&fixture.record.id)
			.unwrap()
			.unwrap()
			.undo_progress
			.is_none()
	);
}

#[test]
fn a_manifest_from_another_job_cannot_authorize_recovery_cleanup() {
	let fixture = Fixture::new();
	let mut record = fixture.store.load(&fixture.record.id).unwrap().unwrap();
	record.id = JobId::now().unwrap();
	write_manifest(&fixture.store.directory(&fixture.record.id), &record).unwrap();
	assert!(fixture.release().is_err());
	assert_eq!(fs::read(&fixture.record.source_path).unwrap(), ORIGINAL);
	assert!(fixture.store.directory(&fixture.record.id).exists());
}

#[test]
fn legacy_recovery_without_attribute_evidence_is_not_automatically_released() {
	let fixture = Fixture::new();
	let mut record = fixture.store.load(&fixture.record.id).unwrap().unwrap();
	record.source_attributes_sha256 = None;
	write_manifest(&fixture.store.directory(&record.id), &record).unwrap();
	assert!(fixture.release().is_err());
	assert_eq!(fs::read(&fixture.record.source_path).unwrap(), ORIGINAL);
	fixture.assert_recovery_remains();
}

#[test]
fn a_corrupt_recovery_copy_is_not_discarded_as_verified() {
	let fixture = Fixture::new();
	let backup = fixture
		.store
		.directory(&fixture.record.id)
		.join(fixture.record.source_file());
	fs::write(&backup, b"corrupt backup").unwrap();
	assert!(fixture.release().is_err());
	assert_eq!(fs::read(&fixture.record.source_path).unwrap(), ORIGINAL);
	assert!(backup.exists());
	assert!(fixture.store.load(&fixture.record.id).unwrap().unwrap().pinned);
}

#[cfg(unix)]
#[test]
fn changed_permissions_are_not_reset_by_cancellation_cleanup() {
	use std::os::unix::fs::PermissionsExt;
	let fixture = Fixture::new();
	fs::set_permissions(&fixture.record.source_path, fs::Permissions::from_mode(0o400)).unwrap();
	assert!(fixture.release().is_err());
	assert_eq!(
		fs::metadata(&fixture.record.source_path).unwrap().permissions().mode() & 0o777,
		0o400
	);
	fixture.assert_recovery_remains();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn changed_original_attributes_keep_the_verified_recovery_copy() {
	let fixture = Fixture::new();
	crate::xattr::write_intent(&fixture.record.source_path, "later-edit").unwrap();
	assert!(fixture.release().is_err());
	assert_eq!(
		crate::xattr::read_intent(&fixture.record.source_path)
			.unwrap()
			.as_deref(),
		Some("later-edit")
	);
	fixture.assert_recovery_remains();
}
