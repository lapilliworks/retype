//! Adversarial safety checks for the review. Failures demonstrate missing safeguards.
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use retype_cli::daemon::{Server, immediate, resume, watcher};
use retype_core::convert::{StagedOutput, restore_source_name};
use retype_core::execute::{self, Cancellation};
use retype_core::job::{Job, JobId, JobState};
use retype_core::registry::Registry;
use retype_core::undo::{Policy, Store};

const ORIGINAL: &[u8] = b"\x89PNG original bytes that must survive";

struct Fixture {
	dir: tempfile::TempDir,
	server: Server,
	shared: Arc<watcher::Shared>,
}

impl Fixture {
	fn new(script: &str, undo: &str) -> Self {
		let dir = tempfile::tempdir().unwrap();
		let script_path = dir.path().join("converter.sh");
		fs::write(&script_path, script).unwrap();
		let state = dir.path().join("state");
		fs::create_dir(&state).unwrap();
		let server = Server::open(&state).unwrap();
		server.pool().stop();
		let shared = server.shared();
		let registry = Registry::load(&format!(
			"version = 'retype-config-1'\n\
			[notify]\nenabled = false\n\
			[undo]\n{undo}\n\
			[backends]\nprefer = ['audit']\n\
			[tools.audit]\ncommand = 'sh'\n\
			layout = ['{}', '{{input}}', '{{ops}}', '{{output}}']\n\
			[formats.png]\nfamily = 'image'\nlossy = false\nalpha = true\ntier = 'exact'\n\
			[formats.webp]\nfamily = 'image'\nlossy = true\nalpha = true\ntier = 'converted'\n\
			[formats.jpg]\nfamily = 'image'\nlossy = true\nalpha = false\ntier = 'converted'\n",
			script_path.display(),
		))
		.unwrap();
		shared.reload_registry(registry);
		Self { dir, server, shared }
	}

	fn path(&self, name: &str) -> PathBuf {
		self.dir.path().join(name)
	}

	fn source(&self) -> PathBuf {
		self.path("photo.png")
	}

	fn convert(&self, intent: &str, cancellation: &Cancellation) -> Job {
		let source = self.source();
		let target = self.path(&format!("photo.{intent}"));
		fs::write(&target, ORIGINAL).unwrap();
		let id = JobId::now().unwrap();
		watcher::convert(
			&self.shared,
			self.dir.path(),
			&target,
			&source,
			&target,
			&id,
			cancellation,
		);
		self.shared
			.jobs
			.lock()
			.unwrap()
			.iter()
			.find(|job| job.id == id)
			.unwrap()
			.clone()
	}

	fn plan(&self, input: &Path, output: &Path) -> retype_core::plan::Plan {
		immediate::prepare(
			&self.shared.registry(),
			&immediate::Request {
				input: input.to_path_buf(),
				intent: "webp".to_string(),
				output: Some(output.to_string_lossy().into_owned()),
				force: false,
			},
		)
		.unwrap()
		.plans
		.remove(0)
	}
}

impl Drop for Fixture {
	fn drop(&mut self) {
		self.server.pool().stop();
	}
}

#[test]
fn rejects_an_empty_output_before_deleting_the_source() {
	let fixture = Fixture::new(": > \"$2\"\n", "enabled = false");
	let job = fixture.convert("webp", &Cancellation::default());
	assert!(
		fixture.source().exists(),
		"zero-byte output was accepted and the original was deleted: {:?}",
		job.state
	);
	assert_ne!(job.state, JobState::Done, "zero-byte output was accepted");
}

#[test]
fn rejects_invalid_format_bytes_before_deleting_the_source() {
	let fixture = Fixture::new("printf 'not a WebP' > \"$2\"\n", "enabled = false");
	let job = fixture.convert("webp", &Cancellation::default());
	assert!(
		fixture.source().exists(),
		"invalid WebP was accepted and the original was deleted: {:?}",
		job.state
	);
	assert_ne!(job.state, JobState::Done, "invalid WebP was accepted");
}

#[test]
fn does_not_accept_a_stale_staged_file_from_an_earlier_run() {
	let fixture = Fixture::new("exit 0\n", "enabled = false");
	let input = fixture.source();
	let output = fixture.path("out.webp");
	fs::write(&input, ORIGINAL).unwrap();
	fs::write(fixture.path(".retype-tmp.out.webp"), b"old partial output").unwrap();
	let plan = fixture.plan(&input, &output);
	assert!(
		execute::run(&plan, &input, &output).is_err(),
		"no-op committed stale data"
	);
}

#[test]
fn keeps_the_original_when_one_requested_output_fails() {
	let fixture = Fixture::new(
		"case \"$2\" in *.jpg) exit 1;; esac\ncp \"$1\" \"$2\"\n",
		"enabled = false",
	);
	let job = fixture.convert("webp,jpg", &Cancellation::default());
	assert_eq!(job.state, JobState::Partial);
	assert!(fixture.source().exists(), "partial job deleted its source");
}

#[test]
fn honors_an_explicit_keep_source_group() {
	let fixture = Fixture::new("cp \"$1\" \"$2\"\n", "enabled = false");
	let job = fixture.convert("png,webp", &Cancellation::default());
	assert_eq!(job.state, JobState::Done);
	assert!(fixture.source().exists(), "keep-source group was ignored");
}

#[test]
fn keeps_the_source_when_undo_save_fails() {
	let fixture = Fixture::new("cp \"$1\" \"$2\"\n", "enabled = true");
	let source = fixture.source();
	let target = fixture.path("photo.webp");
	fs::write(&target, ORIGINAL).unwrap();
	let id = JobId::now().unwrap();
	let store = Store::open(&fixture.shared.state_dir);
	fs::create_dir_all(store.directory(&id).join("source.png")).unwrap();
	watcher::convert(
		&fixture.shared,
		fixture.dir.path(),
		&target,
		&source,
		&target,
		&id,
		&Cancellation::default(),
	);
	assert!(source.exists(), "undo failed but original was permanently removed");
	assert!(
		store.source(&id).unwrap().is_some(),
		"no recoverable undo record exists"
	);
}

#[test]
fn cancellation_after_a_linked_output_keeps_the_only_original_copy() {
	let fixture = Fixture::new(
		"printf converted > \"$2\"\n(sleep 0.3; touch \"$2.ready\"; sleep 0.3) &\nexit 0\n",
		"enabled = true",
	);
	fs::write(fixture.source(), b"unrelated occupant").unwrap();
	let cancellation = Cancellation::default();
	let other = cancellation.clone();
	let ready = fixture.path(".retype-tmp.photo.webp.ready");
	let canceller = std::thread::spawn(move || {
		let deadline = Instant::now() + Duration::from_secs(10);
		while !ready.exists() {
			assert!(Instant::now() < deadline, "stderr helper did not start");
			std::thread::sleep(Duration::from_millis(10));
		}
		other.cancel();
	});
	let job = fixture.convert("webp", &cancellation);
	canceller.join().unwrap();
	assert_eq!(job.state, JobState::Cancelled);
	assert_eq!(fs::read(fixture.source()).unwrap(), b"unrelated occupant");
	assert_eq!(fs::read(fixture.path("photo.webp")).unwrap(), b"converted");
	assert!(!fixture.path(".retype-in.photo.png").exists());
	let store = Store::open(&fixture.shared.state_dir);
	assert!(
		store.source(&job.id).unwrap().is_some(),
		"cancel removed the only original backup"
	);
}

#[test]
fn source_disposal_does_not_delete_a_file_that_replaced_the_input() {
	let fixture = Fixture::new(
		"cp \"$1\" \"$2\"\nmv \"$1\" \"$1.retained\"\nprintf 'new user file' > \"$1\"\n",
		"enabled = false",
	);
	let job = fixture.convert("webp", &Cancellation::default());
	assert_eq!(job.state, JobState::Done);
	assert!(
		fixture.source().exists(),
		"disposal deleted a replacement file it never converted"
	);
}

#[test]
fn an_output_that_appears_while_converting_is_not_overwritten_without_force() {
	let fixture = Fixture::new(
		"cp \"$1\" \"$2\"\ndirectory=${2%/*}\nname=${2##*/}\nprintf 'new user output' > \"$directory/${name#.retype-tmp.}\"\n",
		"enabled = false",
	);
	let job = fixture.convert("webp", &Cancellation::default());
	assert_ne!(job.state, JobState::Done, "commit clobbered a newly occupied output");
	assert_eq!(fs::read(fixture.path("photo.webp")).unwrap(), b"new user output");
}

#[test]
fn output_paths_through_a_directory_symlink_cannot_overwrite_the_input() {
	let fixture = Fixture::new("printf 'converted' > \"$2\"\n", "enabled = false");
	fs::write(fixture.source(), ORIGINAL).unwrap();
	std::os::unix::fs::symlink(fixture.dir.path(), fixture.path("alias")).unwrap();
	let request = immediate::Request {
		input: fixture.source(),
		intent: "webp".to_string(),
		output: Some(fixture.path("alias/photo.png").to_string_lossy().into_owned()),
		force: true,
	};
	let prepared = immediate::prepare(&fixture.shared.registry(), &request);
	if prepared.is_ok() {
		immediate::run(
			&fixture.shared,
			&request,
			&JobId::now().unwrap(),
			&Cancellation::default(),
		);
	}
	assert_eq!(
		fs::read(fixture.source()).unwrap(),
		ORIGINAL,
		"aliased output overwrote the original input"
	);
	assert!(
		prepared.is_err(),
		"lexically distinct output points to the source itself"
	);
}

#[test]
fn a_stale_temporary_input_is_not_truncated_through_a_hard_link() {
	let dir = tempfile::tempdir().unwrap();
	let target = dir.path().join("photo.webp");
	let occupied = dir.path().join("photo.png");
	let stale = dir.path().join(".retype-in.photo.png");
	fs::write(&target, ORIGINAL).unwrap();
	fs::write(&occupied, b"occupied").unwrap();
	fs::hard_link(&target, &stale).unwrap();
	let prepared = retype_core::convert::prepare_input(&target, "photo", "png");
	assert_eq!(
		fs::read(&target).unwrap(),
		ORIGINAL,
		"copy fallback truncated its own source inode"
	);
	drop(prepared);
}

#[test]
fn a_dangling_symlink_is_an_occupied_restore_destination() {
	let dir = tempfile::tempdir().unwrap();
	let target = dir.path().join("photo.webp");
	let restored = dir.path().join("photo.png");
	fs::write(&target, ORIGINAL).unwrap();
	std::os::unix::fs::symlink("missing", &restored).unwrap();
	assert!(
		restore_source_name(&target, "photo", "png").is_err(),
		"restore clobbered a symlink"
	);
	assert!(fs::symlink_metadata(restored).unwrap().file_type().is_symlink());
}

#[test]
fn output_staging_does_not_share_a_path_between_jobs() {
	let dir = tempfile::tempdir().unwrap();
	let output = dir.path().join("photo.webp");
	let first = StagedOutput::new(&output).unwrap();
	let second = StagedOutput::new(&output).unwrap();
	assert_ne!(
		first.temp(),
		second.temp(),
		"concurrent jobs share staged output ownership"
	);
}

#[test]
fn quota_eviction_does_not_remove_an_active_jobs_backup() {
	let fixture = Fixture::new("cp \"$1\" \"$2\"\n", "enabled = true");
	let source = fixture.source();
	fs::write(&source, ORIGINAL).unwrap();
	let id = JobId::now().unwrap();
	let store = Store::open(&fixture.shared.state_dir);
	store
		.save(retype_core::undo::Subject {
			id: id.clone(),
			intent: "webp".to_string(),
			machine: "audit".to_string(),
			bytes: source.clone(),
			source_path: source.clone(),
			source_ext: "png".to_string(),
			outputs: Vec::new(),
			consume_source: true,
		})
		.unwrap();
	let mut active = retype_core::job::Job::new(
		retype_core::job::Source {
			path: source.clone(),
			format: "png".to_string(),
			size: ORIGINAL.len() as u64,
			mtime_ns: 0,
		},
		Some("webp".to_string()),
		Vec::new(),
		"audit",
	)
	.unwrap();
	active.id = id.clone();
	active.state = JobState::Running;
	fixture.shared.jobs.lock().unwrap().push(active);
	let policy = Policy {
		quota: ORIGINAL.len() as u64,
		..Policy::default()
	};
	store.make_room(&source, &policy).unwrap();
	assert!(
		store.source(&id).unwrap().is_some(),
		"quota discarded a running job's backup"
	);
}

#[test]
fn finishing_a_partial_resume_keeps_the_original_backup_for_undo() {
	let fixture = Fixture::new("cp \"$1\" \"$2\"\n", "enabled = true");
	fs::create_dir(fixture.path("photo.jpg")).unwrap();
	let job = fixture.convert("webp,jpg", &Cancellation::default());
	assert_eq!(job.state, JobState::Partial);
	let store = Store::open(&fixture.shared.state_dir);
	assert!(store.source(&job.id).unwrap().is_some());
	fs::remove_dir(fixture.path("photo.jpg")).unwrap();
	assert_eq!(
		resume::run(&fixture.shared, "audit", &job, false).unwrap(),
		JobState::Done
	);
	assert!(
		store.source(&job.id).unwrap().is_some(),
		"successful resume deleted the original backup"
	);
}

#[test]
fn resume_reads_the_recorded_original_not_a_new_file_at_the_source_name() {
	let fixture = Fixture::new("cp \"$1\" \"$2\"\n", "enabled = true");
	fs::create_dir(fixture.path("photo.jpg")).unwrap();
	let job = fixture.convert("webp,jpg", &Cancellation::default());
	assert_eq!(job.state, JobState::Partial);
	fs::write(fixture.source(), b"a new unrelated source file").unwrap();
	fs::remove_dir(fixture.path("photo.jpg")).unwrap();
	assert_eq!(
		resume::run(&fixture.shared, "audit", &job, false).unwrap(),
		JobState::Done
	);
	assert_eq!(
		fs::read(fixture.path("photo.jpg")).unwrap(),
		ORIGINAL,
		"resume converted the unrelated replacement instead of the recorded original"
	);
}

#[test]
fn undo_does_not_remove_a_blocking_file_the_partial_job_never_produced() {
	let fixture = Fixture::new("cp \"$1\" \"$2\"\n", "enabled = true");
	fs::write(fixture.path("photo.jpg"), b"unrelated blocked output").unwrap();
	let job = fixture.convert("webp,jpg", &Cancellation::default());
	assert_eq!(job.state, JobState::Partial);
	Store::open(&fixture.shared.state_dir).restore(&job.id).unwrap();
	assert!(
		fixture.path("photo.jpg").exists(),
		"undo deleted an unrelated collision file"
	);
}

#[test]
fn waits_for_a_detached_writer_before_committing_and_deleting_the_source() {
	let fixture = Fixture::new(
		"printf 'partial' > \"$2\"\n(sleep 1; printf 'completed' >> \"$2\") </dev/null >/dev/null 2>/dev/null &\nexit 0\n",
		"enabled = false",
	);
	let job = fixture.convert("webp", &Cancellation::default());
	std::thread::sleep(Duration::from_millis(1200));
	assert_eq!(job.state, JobState::Done);
	assert_eq!(
		fs::read(fixture.path("photo.webp")).unwrap(),
		b"partialcompleted",
		"parent exited, but helper had not finished when output was committed"
	);
}
