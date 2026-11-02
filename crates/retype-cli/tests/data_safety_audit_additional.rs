//! Archive, staging and undo regressions reproduced against the pre-fix baseline.
#![cfg(unix)]

use std::fs;
use std::path::Path;

use retype_core::archive;
use retype_core::convert::StagedOutput;
use retype_core::execute::{self, Cancellation};
use retype_core::job::JobId;
use retype_core::registry::{Native, Registry};
use retype_core::undo::{Output, Store, Subject};

const ORIGINAL: &[u8] = b"original bytes";

fn subject(id: JobId, source: &Path, output: &Path) -> Subject {
	Subject {
		id,
		intent: "webp".to_string(),
		machine: "audit".to_string(),
		bytes: source.to_path_buf(),
		source_path: source.to_path_buf(),
		source_ext: "png".to_string(),
		outputs: vec![Output {
			path: output.to_path_buf(),
			ext: "webp".to_string(),
		}],
		consume_source: true,
	}
}

fn plan(registry: &Registry) -> retype_core::plan::Plan {
	let parsed = retype_core::intent::parse("webp").unwrap();
	let resolved = retype_core::resolve::resolve(
		&parsed,
		retype_core::resolve::Source {
			format: "png",
			is_directory: false,
		},
		registry,
	)
	.unwrap();
	retype_core::plan::compile(registry, &retype_core::tools::Machine, &resolved)
		.unwrap()
		.remove(0)
}

fn shell_registry(script: &str) -> Registry {
	Registry::load(&format!(
		"version = 'retype-config-1'\n\
		[backends]\nprefer = ['audit']\n\
		[tools.audit]\ncommand = 'sh'\nlayout = ['-c', '{script}', 'audit', '{{input}}', '{{ops}}', '{{output}}']\n\
		[formats.webp]\nfamily = 'image'\nlossy = true\nalpha = true\ntier = 'converted'\n",
	))
	.unwrap()
}

#[test]
fn staged_output_symlinks_cannot_truncate_the_original_or_its_backup() {
	let dir = tempfile::tempdir().unwrap();
	let input = dir.path().join("photo.png");
	let output = dir.path().join("photo.webp");
	fs::write(&input, ORIGINAL).unwrap();
	let store = Store::open(dir.path());
	let id = JobId::now().unwrap();
	store.save(subject(id.clone(), &input, &output)).unwrap();
	let staged = dir.path().join(".retype-tmp.photo.webp");
	std::os::unix::fs::symlink(&input, &staged).unwrap();
	let registry = shell_registry(": > \"$2\"");
	let result = execute::run(&plan(&registry), &input, &output);
	assert_eq!(
		fs::read(&input).unwrap(),
		ORIGINAL,
		"staging followed a symlink into the original"
	);
	assert_eq!(fs::read(store.source(&id).unwrap().unwrap()).unwrap(), ORIGINAL);
	assert!(result.is_err());
}

#[test]
fn a_failed_competing_stage_cannot_delete_another_jobs_output() {
	let dir = tempfile::tempdir().unwrap();
	let output = dir.path().join("photo.webp");
	let first = StagedOutput::new(&output).unwrap();
	fs::write(first.temp(), b"first job's completed output").unwrap();
	{
		let _second = StagedOutput::new(&output).unwrap();
	}
	assert!(
		first.temp().exists(),
		"second job's drop removed the first job's staged file"
	);
	first.commit().unwrap();
}

#[test]
fn undo_backup_is_a_snapshot_not_mutable_through_the_live_source() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("photo.png");
	let output = dir.path().join("photo.webp");
	fs::write(&source, ORIGINAL).unwrap();
	let store = Store::open(dir.path());
	let id = JobId::now().unwrap();
	store.save(subject(id.clone(), &source, &output)).unwrap();
	fs::write(&source, b"a concurrent edit").unwrap();
	assert_eq!(
		fs::read(store.source(&id).unwrap().unwrap()).unwrap(),
		ORIGINAL,
		"live source mutation changed the purported original backup"
	);
}

#[test]
fn undo_does_not_delete_a_new_file_at_an_old_output_path() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("photo.png");
	let output = dir.path().join("photo.webp");
	fs::write(&source, ORIGINAL).unwrap();
	let store = Store::open(dir.path());
	let id = JobId::now().unwrap();
	store.save(subject(id.clone(), &source, &output)).unwrap();
	fs::rename(&source, dir.path().join("source-moved-for-test.png")).unwrap();
	fs::write(&output, b"an unrelated later file").unwrap();
	store.restore(&id).unwrap();
	assert!(output.exists(), "undo deleted an unrelated replacement output");
}

#[test]
fn failed_child_keeps_the_original_and_does_not_commit_partial_bytes() {
	let dir = tempfile::tempdir().unwrap();
	let input = dir.path().join("photo.png");
	let output = dir.path().join("photo.webp");
	fs::write(&input, ORIGINAL).unwrap();
	let registry = shell_registry("printf partial > \"$2\"; exit 1");
	assert!(execute::run(&plan(&registry), &input, &output).is_err());
	assert_eq!(fs::read(input).unwrap(), ORIGINAL);
	assert!(!output.exists());
	assert!(!dir.path().join(".retype-tmp.photo.webp").exists());
}

#[test]
fn native_archive_finalization_produces_a_readable_complete_archive() {
	let dir = tempfile::tempdir().unwrap();
	let input = dir.path().join("archive.tar");
	let output = dir.path().join("archive.zip");
	let mut tar = tar::Builder::new(Vec::new());
	let mut header = tar::Header::new_gnu();
	header.set_size(ORIGINAL.len() as u64);
	header.set_mode(0o644);
	header.set_cksum();
	tar.append_data(&mut header, "photo.png", ORIGINAL).unwrap();
	fs::write(&input, tar.into_inner().unwrap()).unwrap();
	archive::repack(
		None,
		Native::Tar,
		Native::Zip,
		&input,
		&output,
		&Cancellation::default(),
	)
	.unwrap();
	let mut zip = zip::ZipArchive::new(fs::File::open(output).unwrap()).unwrap();
	assert_eq!(zip.len(), 1);
	let mut bytes = Vec::new();
	std::io::Read::read_to_end(&mut zip.by_index(0).unwrap(), &mut bytes).unwrap();
	assert_eq!(bytes, ORIGINAL);
	assert!(input.exists());
}

#[test]
fn carries_every_member_of_a_valid_multi_member_gzip_tar() {
	let dir = tempfile::tempdir().unwrap();
	let input = dir.path().join("archive.tar.gz");
	let output = dir.path().join("archive.zip");
	let mut tar = tar::Builder::new(Vec::new());
	for name in ["first.png", "second.png"] {
		let mut header = tar::Header::new_gnu();
		header.set_size(ORIGINAL.len() as u64);
		header.set_mode(0o644);
		header.set_cksum();
		tar.append_data(&mut header, name, ORIGINAL).unwrap();
	}
	let bytes = tar.into_inner().unwrap();
	let mut compressed = Vec::new();
	/* The first member ends after one complete entry. Both members together are the full tar. */
	for part in [&bytes[..1024], &bytes[1024..]] {
		let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
		std::io::Write::write_all(&mut gzip, part).unwrap();
		compressed.extend(gzip.finish().unwrap());
	}
	let mut decoded = Vec::new();
	std::io::Read::read_to_end(
		&mut flate2::read::MultiGzDecoder::new(compressed.as_slice()),
		&mut decoded,
	)
	.unwrap();
	assert_eq!(decoded, bytes, "test source is a valid complete multi-member gzip tar");
	fs::write(&input, &compressed).unwrap();
	archive::repack(
		None,
		Native::TarGz,
		Native::Zip,
		&input,
		&output,
		&Cancellation::default(),
	)
	.unwrap();
	let zip = zip::ZipArchive::new(fs::File::open(output).unwrap()).unwrap();
	assert_eq!(
		zip.len(),
		2,
		"repack reported success after silently dropping the second entry"
	);
}

#[test]
fn rejects_a_tar_gzip_source_with_an_invalid_gzip_checksum() {
	let dir = tempfile::tempdir().unwrap();
	let input = dir.path().join("archive.tar.gz");
	let output = dir.path().join("archive.zip");
	let mut tar = tar::Builder::new(Vec::new());
	let mut header = tar::Header::new_gnu();
	header.set_size(ORIGINAL.len() as u64);
	header.set_mode(0o644);
	header.set_cksum();
	tar.append_data(&mut header, "photo.png", ORIGINAL).unwrap();
	let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
	std::io::Write::write_all(&mut gzip, &tar.into_inner().unwrap()).unwrap();
	let mut compressed = gzip.finish().unwrap();
	let checksum = compressed.len() - 8;
	compressed[checksum] ^= 0xff;
	fs::write(&input, &compressed).unwrap();
	assert!(
		archive::repack(
			None,
			Native::TarGz,
			Native::Zip,
			&input,
			&output,
			&Cancellation::default()
		)
		.is_err(),
		"tar iteration ended without verifying the gzip trailer"
	);
	assert!(!output.exists());
}
