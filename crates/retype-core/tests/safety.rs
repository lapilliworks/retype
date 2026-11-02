//! Real filesystem regressions for exclusive staging, ownership and recovery bytes.

use std::fs;

use retype_core::convert::StagedOutput;
use retype_core::safety::{Fingerprint, Snapshot, remove_owned};

#[test]
fn a_late_destination_cannot_be_clobbered_by_publication() {
	let dir = tempfile::tempdir().unwrap();
	let output = dir.path().join("photo.webp");
	let staged = StagedOutput::new(&output).unwrap();
	fs::write(staged.temp(), b"completed conversion").unwrap();
	fs::write(&output, b"a file created while conversion ran").unwrap();
	assert!(staged.commit().is_err());
	assert_eq!(fs::read(output).unwrap(), b"a file created while conversion ran");
}

#[test]
fn disposal_claims_and_checks_a_replacement_before_unlinking() {
	let dir = tempfile::tempdir().unwrap();
	let input = dir.path().join("photo.png");
	fs::write(&input, b"original").unwrap();
	let original = Fingerprint::of(&input).unwrap();
	fs::rename(&input, dir.path().join("original.png")).unwrap();
	fs::write(&input, b"replacement").unwrap();
	assert!(remove_owned(&input, &original).is_err());
	assert_eq!(fs::read(input).unwrap(), b"replacement");
	assert_eq!(fs::read(dir.path().join("original.png")).unwrap(), b"original");
}

#[test]
fn disposal_detects_an_in_place_edit_even_with_the_same_length() {
	let dir = tempfile::tempdir().unwrap();
	let input = dir.path().join("photo.png");
	fs::write(&input, b"original").unwrap();
	let original = Fingerprint::of(&input).unwrap();
	fs::write(&input, b"modified").unwrap();
	assert!(remove_owned(&input, &original).is_err());
	assert_eq!(fs::read(input).unwrap(), b"modified");
}

#[test]
fn a_snapshot_does_not_change_with_a_live_source_edit() {
	let dir = tempfile::tempdir().unwrap();
	let input = dir.path().join("photo.png");
	fs::write(&input, b"original").unwrap();
	let snapshot = Snapshot::open(&input).unwrap();
	fs::write(&input, b"modified").unwrap();
	assert_eq!(fs::read(&snapshot.path).unwrap(), b"original");
	assert!(!snapshot.original.matches(&input).unwrap());
}

#[test]
fn snapshot_creation_never_overwrites_an_existing_copy() {
	let dir = tempfile::tempdir().unwrap();
	let input = dir.path().join("photo.png");
	let output = dir.path().join("copy.png");
	fs::write(&input, b"original").unwrap();
	fs::write(&output, b"existing copy").unwrap();
	assert!(retype_core::safety::copy_snapshot(&input, &output).is_err());
	assert_eq!(fs::read(input).unwrap(), b"original");
	assert_eq!(fs::read(output).unwrap(), b"existing copy");
}

#[cfg(unix)]
#[test]
fn staged_symlink_is_rejected_without_touching_its_referent() {
	let dir = tempfile::tempdir().unwrap();
	let input = dir.path().join("photo.png");
	let output = dir.path().join("photo.webp");
	fs::write(&input, b"original").unwrap();
	let staged = StagedOutput::new(&output).unwrap();
	std::os::unix::fs::symlink(&input, staged.temp()).unwrap();
	assert!(staged.commit().is_err());
	assert_eq!(fs::read(input).unwrap(), b"original");
	assert!(!output.exists());
}

