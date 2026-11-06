//! Transaction-level regressions using real native archives, process crashes and storage failures.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use retype_cli::daemon::{Server, resume, watcher};
use retype_core::convert::Publication;
use retype_core::execute::{self, Cancellation};
use retype_core::job::{FileIdentity, Job, JobId, JobState, Output, OutputState, Source, Tier};
use retype_core::registry::Registry;
use retype_core::safety::{Snapshot, remove_owned};
use retype_core::undo::{Policy, Store, Subject};

const CONTENT: &[u8] = b"original archive entry";
const CRASH_ID: &str = "01J00000000000000000000000";
const CONFIG: &str = "version = 'retype-config-1'
[notify]
enabled = false
[formats.tar]
family = 'archive'
native = 'tar'
lossy = false
alpha = false
tier = 'exact'
[formats.zip]
family = 'archive'
native = 'zip'
lossy = false
alpha = false
tier = 'exact'
[formats.\"tar.gz\"]
family = 'archive'
native = 'tar.gz'
lossy = false
alpha = false
tier = 'exact'
";

fn archive(path: &Path) {
	let mut tar = tar::Builder::new(fs::File::create(path).unwrap());
	for name in ["first.txt", "second.txt"] {
		let mut header = tar::Header::new_gnu();
		header.set_size(CONTENT.len() as u64);
		header.set_mode(0o644);
		header.set_cksum();
		tar.append_data(&mut header, name, CONTENT).unwrap();
	}
	tar.finish().unwrap();
}

fn pax_archive(path: &Path) -> Vec<u8> {
	let data: Vec<u8> = (0..20).collect();
	let mut tar = tar::Builder::new(fs::File::create(path).unwrap());
	tar.append_pax_extensions([("size", &b"20"[..])]).unwrap();
	let mut header = tar::Header::new_gnu();
	header.set_entry_type(tar::EntryType::Regular);
	header.set_size(10);
	header.set_mode(0o644);
	header.set_path("entry.bin").unwrap();
	header.set_cksum();
	tar.append(&header, &data[..]).unwrap();
	tar.finish().unwrap();
	data
}

fn large_archive(path: &Path, entries: usize) {
	let mut tar = tar::Builder::new(fs::File::create(path).unwrap());
	let body = [b'x'; 100];

	for index in 0..entries {
		let mut header = tar::Header::new_gnu();
		header.set_size(body.len() as u64);
		header.set_mode(0o644);
		header.set_cksum();
		tar.append_data(&mut header, format!("entry-{index}.txt"), &body[..])
			.unwrap();
	}

	tar.finish().unwrap();
}

fn check_zip(path: &Path) {
	let mut zip = zip::ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
	assert_eq!(zip.len(), 2);
	for name in ["first.txt", "second.txt"] {
		let mut bytes = Vec::new();
		std::io::Read::read_to_end(&mut zip.by_name(name).unwrap(), &mut bytes).unwrap();
		assert_eq!(bytes, CONTENT);
	}
}

fn check_tar_gzip(path: &Path) {
	let decoder = flate2::read::MultiGzDecoder::new(fs::File::open(path).unwrap());
	let mut tar = tar::Archive::new(decoder);
	let mut count = 0;
	for entry in tar.entries().unwrap() {
		let mut bytes = Vec::new();
		std::io::Read::read_to_end(&mut entry.unwrap(), &mut bytes).unwrap();
		assert_eq!(bytes, CONTENT);
		count += 1;
	}
	assert_eq!(count, 2);
	std::io::copy(&mut tar.into_inner(), &mut std::io::sink()).unwrap();
}

fn plans(intent: &str) -> Vec<retype_core::plan::Plan> {
	let registry = Registry::load(CONFIG).unwrap();
	let parsed = retype_core::intent::parse(intent).unwrap();
	let resolved = retype_core::resolve::resolve(
		&parsed,
		retype_core::resolve::Source {
			format: "tar",
			is_directory: false,
		},
		&registry,
	)
	.unwrap();
	retype_core::plan::compile(&registry, &retype_core::tools::Machine, &resolved).unwrap()
}

fn subject(id: JobId, source: &Path, outputs: Vec<retype_core::undo::Output>) -> Subject {
	Subject {
		id,
		intent: "zip,tar.gz".to_string(),
		machine: "safety".to_string(),
		bytes: source.to_path_buf(),
		source_path: source.to_path_buf(),
		source_ext: "tar".to_string(),
		outputs,
		consume_source: true,
	}
}

struct Fixture {
	dir: tempfile::TempDir,
	server: Server,
}

impl Fixture {
	fn new() -> Self {
		let dir = tempfile::tempdir().unwrap();
		let state = dir.path().join("state");
		fs::create_dir(&state).unwrap();
		let server = Server::open(&state).unwrap();
		server.pool().stop();
		server.shared().reload_registry(Registry::load(CONFIG).unwrap());
		Self { dir, server }
	}

	fn path(&self, name: &str) -> PathBuf {
		self.dir.path().join(name)
	}
	fn store(&self) -> Store {
		Store::open(&self.path("state"))
	}

	fn convert(&self, intent: &str, id: &JobId) -> Job {
		let source = self.path("bundle.tar");
		let target = self.path(&format!("bundle.{intent}"));
		archive(&target);
		watcher::convert(
			&self.server.shared(),
			self.dir.path(),
			&target,
			&source,
			&target,
			id,
			&Cancellation::default(),
		);
		self.server
			.shared()
			.jobs
			.lock()
			.unwrap()
			.iter()
			.find(|job| job.id == *id)
			.unwrap()
			.clone()
	}
}

impl Drop for Fixture {
	fn drop(&mut self) {
		self.server.pool().stop();
	}
}

#[test]
fn a_watched_conversion_stops_and_reports_an_undo_storage_error() {
	let fixture = Fixture::new();
	fs::write(fixture.path("state/undo"), b"not a directory").unwrap();
	let job = fixture.convert("zip", &JobId::now().unwrap());
	assert_eq!(job.state, JobState::Failed);
	assert!(job.error.as_ref().unwrap().message.contains("undo"));
	assert!(job.outputs.is_empty());
	assert!(fixture.path("bundle.tar").is_file());
	assert!(!fixture.path("bundle.zip").exists());
	let mut tar = tar::Archive::new(fs::File::open(fixture.path("bundle.tar")).unwrap());
	assert_eq!(tar.entries().unwrap().count(), 2);
}

#[test]
fn an_occupied_source_name_does_not_make_a_temporary_input_a_disposed_original() {
	let fixture = Fixture::new();
	let source = fixture.path("bundle.tar");
	fs::write(&source, b"unrelated occupant").unwrap();
	let id = JobId::now().unwrap();
	let job = fixture.convert("zip,tar.gz", &id);

	assert_eq!(job.state, JobState::Done);
	assert_eq!(fs::read(&source).unwrap(), b"unrelated occupant");
	let original_location = fixture.path("bundle.zip,tar.gz");
	let mut original = tar::Archive::new(fs::File::open(original_location).unwrap());
	assert_eq!(original.entries().unwrap().count(), 2);
	check_zip(&fixture.path("bundle.zip"));
	check_tar_gzip(&fixture.path("bundle.tar.gz"));
	let record = fixture.store().load(&id).unwrap().unwrap();
	assert!(record.pinned, "ambiguous original location must keep recovery pinned");
	assert!(fixture.store().source(&id).unwrap().is_some());
}

#[test]
fn a_direct_conversion_stops_and_reports_an_undo_storage_error() {
	let fixture = Fixture::new();
	let source = fixture.path("bundle.tar");
	archive(&source);
	let original = fs::read(&source).unwrap();
	fs::write(fixture.path("state/undo"), b"not a directory").unwrap();
	let id = JobId::now().unwrap();
	let request = retype_cli::daemon::immediate::Request {
		input: source.clone(),
		intent: "zip".to_string(),
		output: None,
		force: false,
	};
	retype_cli::daemon::immediate::run(&fixture.server.shared(), &request, &id, &Cancellation::default());
	let job = fixture
		.server
		.shared()
		.jobs
		.lock()
		.unwrap()
		.iter()
		.find(|job| job.id == id)
		.unwrap()
		.clone();
	assert_eq!(job.state, JobState::Failed);
	assert!(job.error.as_ref().unwrap().message.contains("undo"));
	assert!(job.outputs.is_empty());
	assert_eq!(fs::read(source).unwrap(), original);
	assert!(!fixture.path("bundle.zip").exists());
}

#[test]
fn a_verified_native_conversion_disposes_only_after_all_entries_are_committed() {
	let fixture = Fixture::new();
	let id = JobId::now().unwrap();
	let job = fixture.convert("zip,tar.gz", &id);
	assert_eq!(job.state, JobState::Done);
	check_zip(&fixture.path("bundle.zip"));
	assert_eq!(
		fixture.path("bundle.tar").exists(),
		!retype_core::fsx::supports_durable_cleanup()
	);
	check_tar_gzip(&fixture.path("bundle.tar.gz"));
	let store = fixture.store();
	let record = store.load(&id).unwrap().unwrap();
	assert_eq!(record.commits.len(), 2);
	assert!(!record.pinned);
	assert!(store.source(&id).unwrap().is_some());
	if !retype_core::fsx::supports_durable_cleanup() {
		assert_eq!(
			fs::read(fixture.path("bundle.tar")).unwrap(),
			fs::read(store.source(&id).unwrap().unwrap()).unwrap()
		);
		let error = store.restore(&id).unwrap_err();
		assert!(matches!(error, retype_core::error::Error::Io(ref error)
			if error.kind() == std::io::ErrorKind::Unsupported));
		check_zip(&fixture.path("bundle.zip"));
		check_tar_gzip(&fixture.path("bundle.tar.gz"));
		assert!(store.source(&id).unwrap().is_some());
		return;
	}
	store.restore(&id).unwrap();
	assert!(fixture.path("bundle.tar").exists());
	assert!(!fixture.path("bundle.zip").exists());
	assert!(!fixture.path("bundle.tar.gz").exists());
}

#[test]
fn forcing_a_native_output_cannot_overwrite_a_hard_link_to_the_input() {
	let fixture = Fixture::new();
	let source = fixture.path("bundle.tar");
	archive(&source);
	let original = fs::read(&source).unwrap();
	let alias = fixture.path("alias.zip");
	fs::hard_link(&source, &alias).unwrap();
	let prepared = retype_cli::daemon::immediate::prepare(
		&Registry::load(CONFIG).unwrap(),
		&retype_cli::daemon::immediate::Request {
			input: source.clone(),
			intent: "zip".to_string(),
			output: Some(alias.to_string_lossy().into_owned()),
			force: true,
		},
	);
	assert!(prepared.is_err(), "force does not override input identity protection");
	assert_eq!(fs::read(source).unwrap(), original);
	assert_eq!(fs::read(alias).unwrap(), original);
}

#[test]
fn a_partial_native_conversion_retains_the_original_even_with_a_valid_first_output() {
	let fixture = Fixture::new();
	fs::create_dir(fixture.path("bundle.tar.gz")).unwrap();
	let id = JobId::now().unwrap();
	let job = fixture.convert("zip,tar.gz", &id);
	assert_eq!(job.state, JobState::Partial);
	check_zip(&fixture.path("bundle.zip"));
	assert!(fixture.path("bundle.tar").exists());
	assert!(fixture.store().load(&id).unwrap().unwrap().pinned);
}

#[test]
fn a_native_keep_source_group_prevents_disposal() {
	let fixture = Fixture::new();
	let job = fixture.convert("tar,zip", &JobId::now().unwrap());
	assert_eq!(job.state, JobState::Done);
	check_zip(&fixture.path("bundle.zip"));
	assert!(fixture.path("bundle.tar").exists());
	let restored = fixture.store().restore(&job.id);
	assert!(fixture.path("bundle.tar").exists());
	if !retype_core::fsx::supports_durable_cleanup() {
		let error = restored.unwrap_err();
		assert!(matches!(error, retype_core::error::Error::Io(ref error)
			if error.kind() == std::io::ErrorKind::Unsupported));
		check_zip(&fixture.path("bundle.zip"));
		return;
	}
	restored.unwrap();
	assert!(!fixture.path("bundle.zip").exists());
}

#[test]
fn failed_backup_creation_stops_a_native_conversion_without_replacing_existing_data() {
	let fixture = Fixture::new();
	let id = JobId::now().unwrap();
	let occupied = fixture.store().directory(&id).join("source.tar");
	fs::create_dir_all(&occupied).unwrap();
	fs::write(occupied.join("existing"), b"unrelated").unwrap();
	let job = fixture.convert("zip", &id);
	assert_eq!(job.state, JobState::Failed);
	assert!(job.error.as_ref().unwrap().message.contains("undo"));
	assert!(!fixture.path("bundle.zip").exists());
	assert!(fixture.path("bundle.tar").exists());
	assert_eq!(fs::read(occupied.join("existing")).unwrap(), b"unrelated");
	assert!(fixture.store().source(&id).unwrap().is_none());
}

#[test]
fn an_undo_policy_without_a_record_retains_the_native_source() {
	let fixture = Fixture::new();
	fixture
		.server
		.shared()
		.reload_registry(Registry::load(&format!("{CONFIG}\n[undo]\nenabled = false\n")).unwrap());
	let job = fixture.convert("zip", &JobId::now().unwrap());
	assert_eq!(job.state, JobState::Done);
	assert!(fixture.path("bundle.tar").exists());
	assert!(fixture.store().load(&job.id).unwrap().is_none());
}

#[test]
fn valid_native_output_does_not_authorize_removing_a_replacement_source() {
	let fixture = Fixture::new();
	let source = fixture.path("bundle.tar");
	archive(&source);
	let snapshot = Snapshot::open(&source).unwrap();
	let output = fixture.path("bundle.zip");
	let report = execute::run_recorded(
		&plans("zip")[0],
		&snapshot.path,
		&output,
		&Cancellation::default(),
		None,
		Publication::Create,
		&|_, _| {
			fs::rename(&source, fixture.path("moved-original.tar"))?;
			fs::write(&source, b"a new unrelated file at the old name")?;
			Ok(())
		},
	)
	.unwrap();
	assert!(report.validation.fingerprint().is_some());
	check_zip(&output);
	assert!(remove_owned(&source, &snapshot.original).is_err());
	assert_eq!(fs::read(&source).unwrap(), b"a new unrelated file at the old name");
	assert!(fixture.path("moved-original.tar").exists());
}

#[test]
fn undo_restore_failure_after_a_cross_directory_rename_does_not_delete_outputs() {
	let fixture = Fixture::new();
	let old_directory = fixture.path("old");
	let new_directory = fixture.path("new");
	fs::create_dir(&old_directory).unwrap();
	fs::create_dir(&new_directory).unwrap();
	let source = old_directory.join("bundle.tar");
	let output = new_directory.join("bundle.zip");
	archive(&source);
	fs::rename(&source, &output).unwrap();
	let id = JobId::now().unwrap();
	watcher::convert(
		&fixture.server.shared(),
		fixture.dir.path(),
		&output,
		&source,
		&output,
		&id,
		&Cancellation::default(),
	);
	check_zip(&output);
	fs::remove_dir(&old_directory).unwrap();
	assert!(fixture.store().restore(&id).is_err());
	check_zip(&output);
	assert!(fixture.store().source(&id).unwrap().is_some());
}

#[test]
fn a_published_output_that_changes_cannot_authorize_source_disposal() {
	let fixture = Fixture::new();
	let source = fixture.path("bundle.tar");
	archive(&source);
	let snapshot = Snapshot::open(&source).unwrap();
	let output = fixture.path("bundle.zip");
	let id = JobId::now().unwrap();
	let store = fixture.store();
	store
		.save(subject(
			id.clone(),
			&source,
			vec![retype_core::undo::Output {
				path: output.clone(),
				ext: "zip".to_string(),
			}],
		))
		.unwrap();
	execute::run_recorded(
		&plans("zip")[0],
		&snapshot.path,
		&output,
		&Cancellation::default(),
		None,
		Publication::Create,
		&|staged, report| {
			let mut produced = Output::pending(output.clone(), "zip", Tier::Exact);
			produced.state = OutputState::Done;
			produced.bytes = Some(report.bytes);
			store.prepare_checked_output(&id, produced, staged, report.validation.fingerprint())
		},
	)
	.unwrap();
	store.verify_disposal(&id).unwrap();
	fs::rename(&output, fixture.path("moved-output.zip")).unwrap();
	fs::write(&output, b"a replacement created after publication").unwrap();
	assert!(
		Cancellation::default()
			.commit(|| {
				store.verify_disposal(&id)?;
				remove_owned(&source, &snapshot.original)
			})
			.is_err()
	);
	assert!(source.exists());
	assert_eq!(fs::read(output).unwrap(), b"a replacement created after publication");
}

#[test]
fn a_replaced_staged_output_cannot_be_recorded_as_validated() {
	let fixture = Fixture::new();
	let source = fixture.path("bundle.tar");
	archive(&source);
	let output = fixture.path("bundle.zip");
	let id = JobId::now().unwrap();
	let store = fixture.store();
	store.save(subject(id.clone(), &source, Vec::new())).unwrap();

	let result = execute::run_recorded(
		&plans("zip")[0],
		&source,
		&output,
		&Cancellation::default(),
		None,
		Publication::Create,
		&|staged, report| {
			let validated = report.validation.fingerprint().expect("archive was validated");
			let moved = staged.with_extension("validated");
			fs::rename(staged, &moved)?;
			fs::copy(&moved, staged)?;
			let produced = Output::pending(output.clone(), "zip", Tier::Exact);
			store.prepare_checked_output(&id, produced, staged, Some(validated))
		},
	);

	assert!(result.is_err());
	assert!(!output.exists());
	assert!(source.exists());
	assert!(store.load(&id).unwrap().unwrap().commits.is_empty());
	assert!(store.source(&id).unwrap().is_some());
}

#[test]
fn a_staged_output_changed_after_its_checkpoint_is_not_published() {
	let fixture = Fixture::new();
	let source = fixture.path("bundle.tar");
	archive(&source);
	let output = fixture.path("bundle.zip");
	let id = JobId::now().unwrap();
	let store = fixture.store();
	store.save(subject(id.clone(), &source, Vec::new())).unwrap();

	let result = execute::run_recorded(
		&plans("zip")[0],
		&source,
		&output,
		&Cancellation::default(),
		None,
		Publication::Create,
		&|staged, report| {
			let produced = Output::pending(output.clone(), "zip", Tier::Exact);
			store.prepare_checked_output(&id, produced, staged, report.validation.fingerprint())?;
			fs::write(staged, b"changed after the checkpoint")?;
			Ok(())
		},
	);

	assert!(result.is_err());
	assert!(!output.exists());
	assert!(source.exists());
	assert_eq!(store.load(&id).unwrap().unwrap().commits.len(), 1);
	assert!(store.source(&id).unwrap().is_some());
}

#[test]
fn a_manifest_write_failure_prevents_output_publication() {
	let fixture = Fixture::new();
	let source = fixture.path("bundle.tar");
	archive(&source);
	let id = JobId::now().unwrap();
	let store = fixture.store();
	store.save(subject(id.clone(), &source, Vec::new())).unwrap();
	let meta = store.directory(&id).join("meta.toml");
	fs::rename(&meta, store.directory(&id).join("meta-before-failure.toml")).unwrap();
	fs::create_dir(&meta).unwrap();
	let output = fixture.path("bundle.zip");
	let result = execute::run_recorded(
		&plans("zip")[0],
		&source,
		&output,
		&Cancellation::default(),
		None,
		Publication::Create,
		&|staged, report| {
			let mut output = Output::pending(report.output.clone(), "zip", Tier::Exact);
			output.state = OutputState::Done;
			store.prepare_output(&id, output, staged)
		},
	);
	assert!(result.is_err());
	assert!(!output.exists());
	assert!(source.exists());
	assert!(store.directory(&id).join("source.tar").exists());
}

#[test]
fn cancellation_before_native_finalization_prevents_publication() {
	let fixture = Fixture::new();
	let source = fixture.path("bundle.tar");
	archive(&source);
	let output = fixture.path("bundle.zip");
	let cancellation = Cancellation::default();
	cancellation.cancel();
	let result = execute::run_recorded(
		&plans("zip")[0],
		&source,
		&output,
		&cancellation,
		None,
		Publication::Create,
		&|_, _| panic!("a cancelled job must not prepare a commit"),
	);
	assert!(result.is_err());
	assert!(!output.exists());
	assert!(source.exists());
}

#[test]
fn cancellation_racing_a_commit_is_ordered_before_any_later_disposal() {
	let fixture = Fixture::new();
	let source = fixture.path("bundle.tar");
	archive(&source);
	let output = fixture.path("bundle.zip");
	let cancellation = Cancellation::default();
	let token = cancellation.clone();
	let (started, wait_for_start) = std::sync::mpsc::channel();
	let (requested, wait_for_request) = std::sync::mpsc::channel();
	let (completed, wait_for_completion) = std::sync::mpsc::channel();
	let canceller = std::thread::spawn(move || {
		wait_for_start.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
		requested.send(()).unwrap();
		token.cancel();
		completed.send(()).unwrap();
	});
	execute::run_recorded(
		&plans("zip")[0],
		&source,
		&output,
		&cancellation,
		None,
		Publication::Create,
		&|_, _| {
			started.send(()).unwrap();
			wait_for_request
				.recv_timeout(std::time::Duration::from_secs(5))
				.unwrap();
			assert_eq!(
				wait_for_completion.recv_timeout(std::time::Duration::from_millis(100)),
				Err(std::sync::mpsc::RecvTimeoutError::Timeout)
			);
			Ok(())
		},
	)
	.unwrap();
	canceller.join().unwrap();
	assert!(cancellation.is_cancelled());
	check_zip(&output);
	let expected = retype_core::safety::Fingerprint::of(&source).unwrap();
	assert!(cancellation.commit(|| remove_owned(&source, &expected)).is_err());
	assert!(source.exists());
}

#[test]
fn competing_runners_cannot_evict_each_others_newly_reserved_snapshots() {
	let fixture = Fixture::new();
	let input = fixture.path("bundle.tar");
	archive(&input);
	let quota = fs::metadata(&input).unwrap().len();
	let barrier = Arc::new(Barrier::new(4));
	let mut workers = Vec::new();
	for _ in 0..4 {
		let state = fixture.path("state");
		let input = input.clone();
		let barrier = Arc::clone(&barrier);
		workers.push(std::thread::spawn(move || {
			barrier.wait();
			Store::open(&state)
				.save_with_policy(
					subject(JobId::now().unwrap(), &input, Vec::new()),
					&Policy {
						quota,
						..Policy::default()
					},
				)
				.unwrap()
				.is_some()
		}));
	}
	assert_eq!(
		workers
			.into_iter()
			.map(|worker| worker.join().unwrap())
			.filter(|saved| *saved)
			.count(),
		1
	);
	let records = fixture.store().list().unwrap();
	assert_eq!(records.len(), 1);
	assert!(records[0].pinned);
	assert!(fixture.store().source(&records[0].id).unwrap().is_some());
}

#[test]
fn an_active_job_cannot_be_undone_or_have_its_recovery_removed() {
	let fixture = Fixture::new();
	let source = fixture.path("bundle.tar");
	archive(&source);
	let id = JobId::now().unwrap();
	let store = fixture.store();
	store.save(subject(id.clone(), &source, Vec::new())).unwrap();
	let lease = store.lease(&id).unwrap();
	assert_eq!(
		store.restore(&id).unwrap_err().code(),
		retype_core::error::Code::IoLocked
	);
	assert_eq!(
		store.remove(&id).unwrap_err().code(),
		retype_core::error::Code::IoLocked
	);
	assert!(store.source(&id).unwrap().is_some());
	drop(lease);
	store.remove(&id).unwrap();
	assert!(source.exists());
	assert!(store.load(&id).unwrap().is_none());
}

#[test]
fn unfinished_recovery_records_survive_expiry() {
	let fixture = Fixture::new();
	let source = fixture.path("bundle.tar");
	archive(&source);
	let id = JobId::now().unwrap();
	fixture.store().save(subject(id.clone(), &source, Vec::new())).unwrap();
	std::thread::sleep(std::time::Duration::from_millis(2));
	assert!(fixture.store().sweep(std::time::Duration::ZERO).unwrap().is_empty());
	assert!(fixture.store().source(&id).unwrap().is_some());
}

#[test]
fn a_pax_size_override_is_complete_before_the_original_is_disposed() {
	let fixture = Fixture::new();
	let source = fixture.path("bundle.tar");
	let target = fixture.path("bundle.zip");
	let expected = pax_archive(&target);
	let id = JobId::now().unwrap();

	watcher::convert(
		&fixture.server.shared(),
		fixture.dir.path(),
		&target,
		&source,
		&target,
		&id,
		&Cancellation::default(),
	);

	let job = fixture
		.server
		.shared()
		.jobs
		.lock()
		.unwrap()
		.iter()
		.find(|job| job.id == id)
		.cloned()
		.unwrap();
	assert_eq!(job.state, JobState::Done);

	let mut zip = zip::ZipArchive::new(fs::File::open(&target).unwrap()).unwrap();
	let mut entry = zip.by_index(0).unwrap();
	let mut bytes = Vec::new();
	entry.read_to_end(&mut bytes).unwrap();
	assert_eq!(bytes, expected, "the PAX size override must be carried");

	if retype_core::fsx::supports_durable_cleanup() {
		assert!(!source.exists(), "a complete conversion may dispose of its source");
	} else {
		assert!(source.exists(), "unsupported durability retains the source");
	}
}

#[test]
fn a_changed_source_keeps_the_original_backup_pinned() {
	let fixture = Fixture::new();
	let source = fixture.path("bundle.tar");
	let target = fixture.path("bundle.zip");
	large_archive(&target, 5_000);
	let id = JobId::now().unwrap();
	let shared = fixture.server.shared();
	let root = fixture.dir.path().to_path_buf();
	let worker_source = source.clone();
	let worker_target = target.clone();
	let worker_id = id.clone();

	let worker = std::thread::spawn(move || {
		watcher::convert(
			&shared,
			&root,
			&worker_target,
			&worker_source,
			&worker_target,
			&worker_id,
			&Cancellation::default(),
		);
	});

	let meta = fixture.store().directory(&id).join("meta.toml");
	let deadline = Instant::now() + Duration::from_secs(10);
	while !meta.exists() {
		assert!(Instant::now() < deadline, "the recovery record did not appear");
		std::thread::sleep(Duration::from_millis(1));
	}

	fs::write(&source, b"changed during conversion").unwrap();
	worker.join().unwrap();

	let job = fixture
		.server
		.shared()
		.jobs
		.lock()
		.unwrap()
		.iter()
		.find(|job| job.id == id)
		.cloned()
		.unwrap();
	assert_eq!(job.state, JobState::Done);
	assert!(source.exists(), "the changed live source is retained");

	let record = fixture.store().load(&id).unwrap().unwrap();
	assert!(
		record.pinned,
		"the only original-bearing backup must stay pinned after a failed disposal"
	);
}

fn start_crash(root: &Path, phase: &str) {
	let status = Command::new(std::env::current_exe().unwrap())
		.args(["--exact", "crash_worker", "--nocapture"])
		.env("RETYPE_SAFETY_CRASH_ROOT", root)
		.env("RETYPE_SAFETY_CRASH_PHASE", phase)
		.status()
		.unwrap();
	assert_eq!(status.code(), Some(81), "crash worker did not reach {phase}");
}

#[test]
fn crash_worker() {
	let Some(root) = std::env::var_os("RETYPE_SAFETY_CRASH_ROOT") else {
		return;
	};
	let root = PathBuf::from(root);
	let phase = std::env::var("RETYPE_SAFETY_CRASH_PHASE").unwrap();
	let state = root.join("state");
	fs::create_dir_all(&state).unwrap();
	let source = root.join("bundle.tar");
	archive(&source);
	let snapshot = Snapshot::open(&source).unwrap();
	let id = JobId::parse(CRASH_ID).unwrap();
	let outputs: Vec<_> = ["zip", "tar.gz"]
		.iter()
		.map(|format| Output::pending(root.join(format!("bundle.{format}")), *format, Tier::Exact))
		.collect();
	let store = Store::open(&state);
	store
		.save(subject(
			id.clone(),
			&source,
			outputs
				.iter()
				.map(|output| retype_core::undo::Output {
					path: output.path.clone(),
					ext: output.format.clone(),
				})
				.collect(),
		))
		.unwrap();
	let mut job = Job::new(
		Source {
			path: source.clone(),
			format: "tar".to_string(),
			size: fs::metadata(&source).unwrap().len(),
			mtime_ns: 0,
		},
		Some("zip,tar.gz".to_string()),
		outputs,
		"safety",
	)
	.unwrap();
	job.id = id.clone();
	job.consume_source = true;
	job.state = JobState::Running;
	let (mut journal, _) = retype_core::journal::Journal::open(&state.join("jobs.jsonl")).unwrap();
	journal.append(&job).unwrap();
	for (index, plan) in plans("zip,tar.gz").iter().enumerate() {
		let report = execute::run_recorded(
			plan,
			&snapshot.path,
			&job.outputs[index].path,
			&Cancellation::default(),
			None,
			Publication::Create,
			&|staged, report| {
				let mut output = job.outputs[index].clone();
				output.state = OutputState::Done;
				output.bytes = Some(report.bytes);
				output.identity = FileIdentity::of(staged);
				store.prepare_checked_output(&id, output, staged, report.validation.fingerprint())?;
				if phase == "before_publish" {
					std::process::exit(81);
				}
				Ok(())
			},
		)
		.unwrap();
		assert!(report.validation.fingerprint().is_some());
		job.outputs[index].state = OutputState::Done;
		job.outputs[index].identity = FileIdentity::of(&report.output);
		job.outputs[index].bytes = Some(report.bytes);
		if phase == "after_publish" {
			std::process::exit(81);
		}
	}
	job.state = JobState::Done;
	journal.append(&job).unwrap();
	remove_owned(&source, &snapshot.original).unwrap();
	std::process::exit(81);
}

fn restart(root: &Path) -> Server {
	let server = Server::open(&root.join("state")).unwrap();
	server.pool().stop();
	server.shared().reload_registry(Registry::load(CONFIG).unwrap());
	server
}

#[test]
fn crash_before_publication_leaves_a_verified_source_and_resumable_intent() {
	let dir = tempfile::tempdir().unwrap();
	start_crash(dir.path(), "before_publish");
	assert!(dir.path().join("bundle.tar").exists());
	assert!(!dir.path().join("bundle.zip").exists());
	let server = restart(dir.path());
	let job = server.shared().jobs.lock().unwrap()[0].clone();
	assert_eq!(job.state, JobState::Failed);
	assert_eq!(
		resume::run(&server.shared(), "safety", &job, false).unwrap(),
		JobState::Done
	);
	check_zip(&dir.path().join("bundle.zip"));
	assert!(
		Store::open(&dir.path().join("state"))
			.source(&job.id)
			.unwrap()
			.is_some()
	);
}

#[test]
fn crash_after_publication_recovers_the_committed_identity_and_resumes_only_the_missing_output() {
	let dir = tempfile::tempdir().unwrap();
	start_crash(dir.path(), "after_publish");
	let output = dir.path().join("bundle.zip");
	let identity = FileIdentity::of(&output).unwrap();
	let server = restart(dir.path());
	let job = server.shared().jobs.lock().unwrap()[0].clone();
	assert_eq!(job.state, JobState::Partial);
	assert_eq!(job.outputs[0].state, OutputState::Done);
	assert_eq!(
		resume::run(&server.shared(), "safety", &job, false).unwrap(),
		JobState::Done
	);
	assert!(identity.matches(&FileIdentity::of(&output).unwrap()));
	check_zip(&output);
	check_tar_gzip(&dir.path().join("bundle.tar.gz"));
}

#[test]
#[cfg(unix)]
fn crash_after_source_disposal_still_leaves_complete_outputs_and_an_undoable_original() {
	let dir = tempfile::tempdir().unwrap();
	start_crash(dir.path(), "after_disposal");
	assert!(!dir.path().join("bundle.tar").exists());
	check_zip(&dir.path().join("bundle.zip"));
	check_tar_gzip(&dir.path().join("bundle.tar.gz"));
	let server = restart(dir.path());
	let job = server.shared().jobs.lock().unwrap()[0].clone();
	assert_eq!(job.state, JobState::Done);
	Store::open(&dir.path().join("state")).restore(&job.id).unwrap();
	assert!(dir.path().join("bundle.tar").exists());
	assert!(!dir.path().join("bundle.zip").exists());
}

#[test]
fn recovery_does_not_trust_a_substituted_output_at_a_committed_path() {
	let dir = tempfile::tempdir().unwrap();
	start_crash(dir.path(), "after_publish");
	let output = dir.path().join("bundle.zip");
	fs::rename(&output, dir.path().join("old.zip")).unwrap();
	fs::write(&output, b"unrelated replacement").unwrap();
	let server = restart(dir.path());
	let job = server.shared().jobs.lock().unwrap()[0].clone();
	assert_eq!(job.outputs[0].state, OutputState::Pending);
	assert_eq!(
		resume::run(&server.shared(), "safety", &job, false).unwrap(),
		JobState::Partial
	);
	assert_eq!(fs::read(output).unwrap(), b"unrelated replacement");
	assert!(
		Store::open(&dir.path().join("state"))
			.source(&job.id)
			.unwrap()
			.is_some()
	);
}
