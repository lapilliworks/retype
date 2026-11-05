//! Filesystem and runner regressions reproduced against the pre-fix data-safety baseline.
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
		let dir = match std::env::var_os("RETYPE_SAFETY_INTERRUPTED_ROOT") {
			Some(root) => tempfile::tempdir_in(root).unwrap(),
			None => tempfile::tempdir().unwrap(),
		};
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
		self.job(&id)
	}

	fn job(&self, id: &JobId) -> Job {
		self.shared
			.jobs
			.lock()
			.unwrap()
			.iter()
			.find(|job| &job.id == id)
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
fn an_unvalidated_custom_output_cannot_authorize_source_deletion() {
	let fixture = Fixture::new("printf 'not a WebP' > \"$2\"\n", "enabled = false");
	let job = fixture.convert("webp", &Cancellation::default());
	assert!(
		fixture.source().exists(),
		"invalid WebP was accepted and the original was deleted: {:?}",
		job.state
	);
	assert_eq!(fs::read(fixture.source()).unwrap(), ORIGINAL);
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
	assert_eq!(fs::read(&source).unwrap(), ORIGINAL);
	assert!(
		store.source(&id).unwrap().is_none(),
		"the failed save must not pretend to have a backup"
	);
}

#[test]
fn an_occupied_source_name_and_missing_undo_capacity_do_not_allow_replacing_the_original() {
	let fixture = Fixture::new("printf converted > \"$2\"\n", "enabled = true\nquota = '0kb'");
	fs::write(fixture.source(), b"unrelated occupant").unwrap();
	let job = fixture.convert("webp", &Cancellation::default());
	assert_eq!(job.state, JobState::Failed);
	assert_eq!(fs::read(fixture.path("photo.webp")).unwrap(), ORIGINAL);
	assert_eq!(fs::read(fixture.source()).unwrap(), b"unrelated occupant");
	assert!(
		Store::open(&fixture.shared.state_dir)
			.source(&job.id)
			.unwrap()
			.is_none()
	);
}

#[test]
fn cancellation_after_one_output_keeps_the_input_and_undo_for_committed_outputs() {
	let fixture = Fixture::new(
		"case \"$2\" in *.jpg) touch \"$(dirname \"$0\")/second-started\"; sleep 30;; esac\ncp \"$1\" \"$2\"\n",
		"enabled = true",
	);
	let cancellation = Cancellation::default();
	let other = cancellation.clone();
	let ready = fixture.path("second-started");
	let canceller = std::thread::spawn(move || {
		let deadline = Instant::now() + Duration::from_secs(10);
		while !ready.exists() {
			assert!(Instant::now() < deadline, "second output did not start");
			std::thread::sleep(Duration::from_millis(10));
		}
		other.cancel();
	});
	let job = fixture.convert("webp,jpg", &cancellation);
	canceller.join().unwrap();
	assert_eq!(job.state, JobState::Cancelled);
	assert_eq!(fs::read(fixture.source()).unwrap(), ORIGINAL);
	assert_eq!(fs::read(fixture.path("photo.webp")).unwrap(), ORIGINAL);
	assert!(!fixture.path("photo.jpg").exists());
	let store = Store::open(&fixture.shared.state_dir);
	assert_eq!(
		fs::read(store.source(&job.id).unwrap().expect("retained original")).unwrap(),
		ORIGINAL
	);
	store.restore(&job.id).unwrap();
	assert!(!fixture.path("photo.webp").exists(), "cancelled job remains undoable");
}

#[test]
fn undo_cannot_remove_a_running_jobs_snapshot_or_race_its_publication() {
	let fixture = Fixture::new(
		"case \"$2\" in *.jpg) touch \"$(dirname \"$0\")/second-started\"; sleep 30;; esac\ncp \"$1\" \"$2\"\n",
		"enabled = true",
	);
	let cancellation = Cancellation::default();
	let outcome = std::thread::scope(|scope| {
		let runner = scope.spawn(|| fixture.convert("webp,jpg", &cancellation));
		let deadline = Instant::now() + Duration::from_secs(10);
		while !fixture.path("second-started").exists() {
			if Instant::now() >= deadline {
				cancellation.cancel();
				panic!("second output did not start");
			}
			std::thread::sleep(Duration::from_millis(10));
		}
		let id = fixture.shared.jobs.lock().unwrap()[0].id.clone();
		let outcome = Store::open(&fixture.shared.state_dir).restore(&id);
		cancellation.cancel();
		let job = runner.join().unwrap();
		(outcome, job)
	});
	assert_eq!(outcome.0.unwrap_err().code(), retype_core::error::Code::IoLocked);
	assert_eq!(outcome.1.state, JobState::Cancelled);
	Store::open(&fixture.shared.state_dir).restore(&outcome.1.id).unwrap();
	assert_eq!(fs::read(fixture.source()).unwrap(), ORIGINAL);
	assert!(!fixture.path("photo.webp").exists());
}

#[test]
fn source_disposal_does_not_delete_a_file_that_replaced_the_input() {
	let fixture = Fixture::new(
		"cp \"$1\" \"$2\"\nsource=\"$(dirname \"$0\")/photo.png\"\nmv \"$source\" \"$source.retained\"\nprintf 'new user file' > \"$source\"\n",
		"enabled = false",
	);
	let job = fixture.convert("webp", &Cancellation::default());
	assert_eq!(job.state, JobState::Done);
	assert_eq!(
		fs::read(fixture.source()).unwrap(),
		b"new user file",
		"disposal deleted a replacement"
	);
}

#[test]
fn an_output_that_appears_while_converting_is_not_overwritten_without_force() {
	let fixture = Fixture::new(
		"cp \"$1\" \"$2\"\nprintf 'new user output' > \"$(dirname \"$0\")/photo.webp\"\n",
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
fn a_forced_output_hard_link_to_the_input_is_refused() {
	let fixture = Fixture::new("printf converted > \"$2\"\n", "enabled = false");
	fs::write(fixture.source(), ORIGINAL).unwrap();
	let output = fixture.path("alias.webp");
	fs::hard_link(fixture.source(), &output).unwrap();
	let request = immediate::Request {
		input: fixture.source(),
		intent: "webp".to_string(),
		output: Some(output.to_string_lossy().into_owned()),
		force: true,
	};
	assert!(immediate::prepare(&fixture.shared.registry(), &request).is_err());
	assert_eq!(fs::read(fixture.source()).unwrap(), ORIGINAL);
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
fn a_resumed_tool_cannot_modify_the_retained_original_through_its_input() {
	let fixture = Fixture::new("cp \"$1\" \"$2\"\n", "enabled = true");
	fs::create_dir(fixture.path("photo.jpg")).unwrap();
	let job = fixture.convert("webp,jpg", &Cancellation::default());
	assert_eq!(job.state, JobState::Partial);
	fs::remove_dir(fixture.path("photo.jpg")).unwrap();
	fs::write(
		fixture.path("converter.sh"),
		"printf corrupt > \"$1\"\ncp \"$1\" \"$2\"\n",
	)
	.unwrap();
	assert_eq!(
		resume::run(&fixture.shared, "audit", &job, false).unwrap(),
		JobState::Partial
	);
	let held = Store::open(&fixture.shared.state_dir)
		.source(&job.id)
		.unwrap()
		.expect("verified backup");
	assert_eq!(fs::read(held).unwrap(), ORIGINAL);
	assert_eq!(fs::read(fixture.source()).unwrap(), ORIGINAL);
	assert!(!fixture.path("photo.jpg").exists());
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
fn loss_of_the_journal_directory_prevents_publication_and_source_disposal() {
	let fixture = Fixture::new(
		"cp \"$1\" \"$2\"\nroot=$(dirname \"$0\")\nmv \"$root/state\" \"$root/state-moved\"\n",
		"enabled = false",
	);
	fixture.convert("webp", &Cancellation::default());
	assert_eq!(fs::read(fixture.source()).unwrap(), ORIGINAL);
	assert!(
		!fixture.path("photo.webp").exists(),
		"a missing durable checkpoint must prevent publication"
	);
}

const BREAK_SECOND_CHECKPOINT: &str = "\
root=$(dirname \"$0\")\n\
printf '%s\\n' \"$2\" >> \"$root/started\"\n\
cp \"$1\" \"$2\"\n\
case \"$2\" in *.jpg) mv \"$root/state\" \"$root/state-moved\";; esac\n";

fn assert_checkpoint_failure_preserved_work(fixture: &Fixture, job: &Job) {
	assert_eq!(job.state, JobState::Partial);
	assert!(job.error.as_ref().unwrap().message.contains("not durable"));
	assert_eq!(job.outputs.len(), 3);
	assert_eq!(job.outputs[0].state, retype_core::job::OutputState::Done);
	assert_eq!(job.outputs[1].state, retype_core::job::OutputState::Failed);
	assert_eq!(job.outputs[2].state, retype_core::job::OutputState::Skipped);
	assert!(job.outputs[2].error.as_ref().unwrap().message.contains("not attempted"));
	assert!(fixture.source().exists());
	assert_eq!(fs::read(fixture.path("photo.webp")).unwrap(), ORIGINAL);
	assert!(!fixture.path("photo.jpg").exists());
	assert!(!fixture.path("photo-2.webp").exists());
	let started = fs::read_to_string(fixture.path("started")).unwrap();
	assert_eq!(started.lines().count(), 2, "later output must not start");
}

#[test]
fn a_watched_journal_failure_stops_later_outputs_and_reports_non_durable_status() {
	let fixture = Fixture::new(BREAK_SECOND_CHECKPOINT, "enabled = false");
	let job = fixture.convert("webp,jpg,webp", &Cancellation::default());
	assert_checkpoint_failure_preserved_work(&fixture, &job);
}

#[test]
fn a_direct_journal_failure_stops_later_outputs_and_reports_non_durable_status() {
	let fixture = Fixture::new(BREAK_SECOND_CHECKPOINT, "enabled = false");
	fs::write(fixture.source(), ORIGINAL).unwrap();
	let id = JobId::now().unwrap();
	let request = immediate::Request {
		input: fixture.source(),
		intent: "webp,jpg,webp".to_string(),
		output: None,
		force: false,
	};
	immediate::run(&fixture.shared, &request, &id, &Cancellation::default());
	let job = fixture.job(&id);
	assert_checkpoint_failure_preserved_work(&fixture, &job);
}

#[test]
fn a_resume_checkpoint_failure_stops_later_outputs_and_keeps_recovery_data() {
	let initial = "case \"$2\" in *.jpg|*-2.webp) exit 1;; esac\ncp \"$1\" \"$2\"\n";
	let fixture = Fixture::new(initial, "enabled = true");
	let job = fixture.convert("webp,jpg,webp", &Cancellation::default());
	assert_eq!(job.state, JobState::Partial);
	let fail_ledger = "\
root=$(dirname \"$0\")\n\
printf '%s\\n' \"$2\" >> \"$root/started\"\n\
cp \"$1\" \"$2\"\n\
case \"$2\" in *.jpg) for meta in \"$root\"/state/undo/*/meta.toml; do mv \"$meta\" \"$meta.before\"; done;; esac\n";
	fs::write(fixture.path("converter.sh"), fail_ledger).unwrap();
	assert_eq!(
		resume::run(&fixture.shared, "test", &job, false).unwrap(),
		JobState::Partial
	);
	let reported = fixture.job(&job.id);
	assert_eq!(reported.state, JobState::Partial);
	assert_eq!(reported.outputs[0].state, retype_core::job::OutputState::Done);
	assert_eq!(reported.outputs[1].state, retype_core::job::OutputState::Failed);
	assert!(
		reported.outputs[1]
			.error
			.as_ref()
			.unwrap()
			.message
			.contains("checkpoint")
	);
	assert_eq!(reported.outputs[2].state, retype_core::job::OutputState::Skipped);
	assert!(fixture.source().exists());
	assert_eq!(fs::read(fixture.path("photo.webp")).unwrap(), ORIGINAL);
	assert!(!fixture.path("photo.jpg").exists());
	assert!(!fixture.path("photo-2.webp").exists());
	assert!(fixture.path(&format!("state/undo/{}/source.png", job.id)).is_file());
	assert!(
		fixture
			.path(&format!("state/undo/{}/meta.toml.before", job.id))
			.is_file()
	);
	let started = fs::read_to_string(fixture.path("started")).unwrap();
	assert_eq!(started.lines().count(), 1, "resume must not start another output");
}

#[test]
fn a_stopped_forced_resume_still_reports_previously_completed_outputs() {
	let initial = "case \"$2\" in *.jpg) exit 1;; esac\ncp \"$1\" \"$2\"\n";
	let fixture = Fixture::new(initial, "enabled = true");
	let job = fixture.convert("webp,jpg,webp", &Cancellation::default());
	assert_eq!(job.state, JobState::Partial);
	assert_eq!(job.outputs[2].state, retype_core::job::OutputState::Done);
	let fail_ledger = "\
root=$(dirname \"$0\")\n\
printf '%s\\n' \"$2\" >> \"$root/started\"\n\
cp \"$1\" \"$2\"\n\
for meta in \"$root\"/state/undo/*/meta.toml; do mv \"$meta\" \"$meta.before\"; done\n";
	fs::write(fixture.path("converter.sh"), fail_ledger).unwrap();
	assert_eq!(
		resume::run(&fixture.shared, "test", &job, true).unwrap(),
		JobState::Partial
	);
	let reported = fixture.job(&job.id);
	assert_eq!(reported.outputs[0].state, retype_core::job::OutputState::Failed);
	assert_eq!(reported.outputs[1].state, retype_core::job::OutputState::Skipped);
	assert_eq!(reported.outputs[2].state, retype_core::job::OutputState::Done);
	assert_eq!(fs::read(fixture.path("photo.webp")).unwrap(), ORIGINAL);
	assert_eq!(fs::read(fixture.path("photo-2.webp")).unwrap(), ORIGINAL);
	assert_eq!(fs::read(fixture.source()).unwrap(), ORIGINAL);
	assert_eq!(fs::read_to_string(fixture.path("started")).unwrap().lines().count(), 1);
}

#[test]
fn a_resume_journal_failure_reports_its_result_as_non_durable() {
	let initial = "case \"$2\" in *.jpg|*-2.webp) exit 1;; esac\ncp \"$1\" \"$2\"\n";
	let fixture = Fixture::new(initial, "enabled = true");
	let job = fixture.convert("webp,jpg,webp", &Cancellation::default());
	assert_eq!(job.state, JobState::Partial);
	fs::write(fixture.path("converter.sh"), BREAK_SECOND_CHECKPOINT).unwrap();
	assert!(resume::run(&fixture.shared, "test", &job, false).is_err());
	let reported = fixture.job(&job.id);
	assert_eq!(reported.state, JobState::Partial);
	assert!(reported.error.as_ref().unwrap().message.contains("not durable"));
	assert_eq!(fs::read(fixture.source()).unwrap(), ORIGINAL);
	assert_eq!(fs::read(fixture.path("photo.webp")).unwrap(), ORIGINAL);
	assert!(
		fixture
			.path(&format!("state-moved/undo/{}/source.png", job.id))
			.is_file()
	);
	assert!(!fixture.path("photo.jpg").exists());
	assert!(!fixture.path("photo-2.webp").exists());
}

#[test]
fn an_ordinary_output_failure_does_not_stop_independent_outputs() {
	let fixture = Fixture::new(
		"case \"$2\" in *.webp) exit 1;; esac\ncp \"$1\" \"$2\"\n",
		"enabled = false",
	);
	let job = fixture.convert("webp,jpg", &Cancellation::default());
	assert_eq!(job.state, JobState::Partial);
	assert!(fixture.source().exists());
	assert!(!fixture.path("photo.webp").exists());
	assert_eq!(fs::read(fixture.path("photo.jpg")).unwrap(), ORIGINAL);
}

#[test]
fn interrupted_job_worker() {
	let Some(root) = std::env::var_os("RETYPE_SAFETY_INTERRUPTED_ROOT") else {
		return;
	};
	let fixture = Fixture::new(
		"case \"$2\" in *.jpg) printf '%s' \"$$\" > \"$(dirname \"$0\")/second-started\"; sleep 30;; esac\ncp \"$1\" \"$2\"\n",
		"enabled = true",
	);
	fs::write(
		Path::new(&root).join("fixture-dir"),
		fixture.dir.path().to_str().unwrap(),
	)
	.unwrap();
	fixture.convert("webp,jpg", &Cancellation::default());
	panic!("the parent should terminate this worker while the second output is running");
}

#[test]
fn an_actual_interrupted_runner_recovers_its_first_output_instead_of_losing_resume_state() {
	struct Worker(std::process::Child);
	impl Drop for Worker {
		fn drop(&mut self) {
			let _ = self.0.kill();
			let _ = self.0.wait();
		}
	}
	let root = tempfile::tempdir().unwrap();
	let mut worker = Worker(
		std::process::Command::new(std::env::current_exe().unwrap())
			.args(["--exact", "interrupted_job_worker", "--nocapture"])
			.env("RETYPE_SAFETY_INTERRUPTED_ROOT", root.path())
			.spawn()
			.unwrap(),
	);
	let deadline = Instant::now() + Duration::from_secs(20);
	let (directory, tool_pid) = loop {
		if let Ok(directory) = fs::read_to_string(root.path().join("fixture-dir")) {
			let directory = PathBuf::from(directory);
			if let Ok(pid) = fs::read_to_string(directory.join("second-started")) {
				break (directory, pid.parse::<i32>().unwrap());
			}
		}
		assert!(
			worker.0.try_wait().unwrap().is_none(),
			"worker exited before the second output"
		);
		assert!(Instant::now() < deadline, "worker did not reach its second output");
		std::thread::sleep(Duration::from_millis(10));
	};
	worker.0.kill().unwrap();
	worker.0.wait().unwrap();
	/* The marker is the PID of this test's still-sleeping, private tool process group. */
	assert_eq!(
		unsafe { libc::kill(-tool_pid, libc::SIGKILL) },
		0,
		"test tool group did not terminate"
	);
	let first = directory.join("photo.webp");
	let identity = retype_core::job::FileIdentity::of(&first).unwrap();
	let server = Server::open(&directory.join("state")).unwrap();
	server.pool().stop();
	let copy = Fixture::new("cp \"$1\" \"$2\"\n", "enabled = true");
	server.shared().reload_registry(copy.shared.registry().as_ref().clone());
	let job = server.shared().jobs.lock().unwrap()[0].clone();
	assert_eq!(
		job.state,
		JobState::Partial,
		"the committed first output must survive reconciliation"
	);
	assert_eq!(
		resume::run(&server.shared(), "audit", &job, false).unwrap(),
		JobState::Done
	);
	assert!(identity.matches(&retype_core::job::FileIdentity::of(&first).unwrap()));
	assert_eq!(fs::read(directory.join("photo.jpg")).unwrap(), ORIGINAL);
	assert!(Store::open(&directory.join("state")).source(&job.id).unwrap().is_some());
}

#[test]
fn rejects_a_parent_that_exits_before_its_detached_writer_finishes() {
	let fixture = Fixture::new(
		"printf 'partial' > \"$2\"\n(sleep 1; printf 'completed' >> \"$2\") </dev/null >/dev/null 2>/dev/null &\nexit 0\n",
		"enabled = false",
	);
	let job = fixture.convert("webp", &Cancellation::default());
	assert_eq!(job.state, JobState::Failed);
	assert_eq!(fs::read(fixture.source()).unwrap(), ORIGINAL);
	assert!(
		!fixture.path("photo.webp").exists(),
		"parent exit must not commit an unfinished output"
	);
}
