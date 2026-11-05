use std::fs;
use std::path::Path;
use std::sync::Arc;

use super::*;
use crate::daemon::immediate;
use crate::daemon::queue::{CAPACITY, Kind, Pool, Work};
use retype_core::job::{JobId, Output, Source, Tier};
use retype_core::registry::Registry;

fn server(state: &Path) -> Server {
	let mut server = Server::open(state).unwrap();
	server.pool.stop();
	server.shared.reload_registry(Registry::builtin().unwrap());
	server.pool = Pool::start(Arc::clone(&server.shared), 0);
	server
}

fn submission(input: &Path) -> Request {
	Request::new(1, "jobs.submit")
		.with_params(json!({"input": input, "intent": "zip"}))
		.unwrap()
}

fn partial_job(server: &Server, directory: &Path) -> Job {
	let id = JobId::now().unwrap();
	let source = directory.join("source.tar");
	let output = directory.join("done.zip");
	let staged = directory.join("staged.zip");
	let missing = directory.join("missing.zip");
	fs::write(&source, b"original bytes").unwrap();
	fs::write(&staged, b"output").unwrap();
	let store = retype_core::undo::Store::open(&server.shared.state_dir);
	store
		.save(retype_core::undo::Subject {
			id: id.clone(),
			intent: "zip,zip".to_string(),
			machine: "test".to_string(),
			bytes: source.clone(),
			source_path: source.clone(),
			source_ext: "tar".to_string(),
			outputs: vec![
				retype_core::undo::Output {
					path: output.clone(),
					ext: "zip".to_string(),
				},
				retype_core::undo::Output {
					path: missing.clone(),
					ext: "zip".to_string(),
				},
			],
			consume_source: true,
		})
		.unwrap();
	let mut done = Output::pending(output.clone(), "zip", Tier::Converted);
	done.state = OutputState::Done;
	done.bytes = Some(6);
	done.identity = retype_core::job::FileIdentity::of(&staged);
	store.prepare_output(&id, done.clone(), &staged).unwrap();
	fs::rename(&staged, &output).unwrap();
	let mut failed = Output::pending(missing, "zip", Tier::Converted);
	failed.state = OutputState::Failed;
	let now = retype_core::job::now_ms();
	let job = Job {
		id,
		created: now,
		updated: now,
		source: Source {
			path: source,
			format: "tar".to_string(),
			size: 14,
			mtime_ns: 0,
		},
		intent: Some("zip,zip".to_string()),
		outputs: vec![done, failed],
		state: JobState::Partial,
		backend: None,
		engine: None,
		machine: "test".to_string(),
		error: None,
		parked: None,
		consume_source: true,
	};
	crate::daemon::watcher::publish_checked(&server.shared, &job).unwrap();
	job
}

fn cancel_request(id: &JobId) -> Request {
	Request::new(1, "jobs.cancel").with_params(json!({"id": id})).unwrap()
}

fn parked_job(server: &Server, directory: &Path, reason: &str, state: JobState) -> Job {
	let source = directory.join(format!("{}.tar", JobId::now().unwrap()));
	fs::write(&source, b"input bytes").unwrap();
	let now = retype_core::job::now_ms();
	let job = Job {
		id: JobId::now().unwrap(),
		created: now,
		updated: now,
		source: Source {
			path: source.clone(),
			format: "tar".to_string(),
			size: 11,
			mtime_ns: 0,
		},
		intent: Some("zip".to_string()),
		outputs: vec![Output::pending(source.with_extension("zip"), "zip", Tier::Converted)],
		state,
		backend: None,
		engine: None,
		machine: "test".to_string(),
		error: None,
		parked: Some(retype_core::job::Parked {
			reason: reason.to_string(),
			deadline: Some(now - 1),
			batch: None,
			estimate: None,
			requested: Some("zip".to_string()),
			corrected: None,
			dropped: Vec::new(),
			consume: false,
			force: false,
			input_fingerprint: Some(retype_core::safety::Fingerprint::of(&source).unwrap()),
		}),
		consume_source: false,
	};
	crate::daemon::watcher::publish_checked(&server.shared, &job).unwrap();
	job
}

#[test]
fn cancelling_a_partial_job_preserves_outputs_and_starts_retention() {
	let dir = tempfile::tempdir().unwrap();
	let server = server(&dir.path().join("state"));
	let job = partial_job(&server, dir.path());
	let started = retype_core::job::now_ms();

	let response = server.cancel(&cancel_request(&job.id)).unwrap();
	assert!(response.ok, "{response:?}");
	assert_eq!(response.result.unwrap()["warning"], serde_json::Value::Null);
	let cancelled = server.find(job.id.as_str()).unwrap();
	assert_eq!(cancelled.state, JobState::Cancelled);
	assert_eq!(cancelled.outputs, job.outputs);
	assert_eq!(fs::read(&job.source.path).unwrap(), b"original bytes");
	assert_eq!(fs::read(&job.outputs[0].path).unwrap(), b"output");
	let record = retype_core::undo::Store::open(&server.shared.state_dir)
		.load(&job.id)
		.unwrap()
		.unwrap();
	assert!(!record.pinned);
	assert!(record.retention_started.is_some_and(|time| time >= started));
	server.pool.stop();
}

#[test]
fn cancelling_a_partial_job_reports_changed_outputs_and_keeps_undo_pinned() {
	let dir = tempfile::tempdir().unwrap();
	let server = server(&dir.path().join("state"));
	let job = partial_job(&server, dir.path());
	fs::write(&job.outputs[0].path, b"tamper").unwrap();

	let response = server.cancel(&cancel_request(&job.id)).unwrap();
	assert!(response.ok, "{response:?}");
	let warning = response.result.unwrap()["warning"].as_str().unwrap().to_string();
	assert!(warning.contains("changed after publication"), "{warning}");
	let cancelled = server.find(job.id.as_str()).unwrap();
	assert_eq!(cancelled.state, JobState::Cancelled);
	assert!(cancelled.error.unwrap().message.contains(&warning));
	assert_eq!(fs::read(&job.outputs[0].path).unwrap(), b"tamper");
	let record = retype_core::undo::Store::open(&server.shared.state_dir)
		.load(&job.id)
		.unwrap()
		.unwrap();
	assert!(record.pinned);
	assert!(record.retention_started.is_none());
	server.pool.stop();
}

#[test]
fn cancelling_a_partial_job_protects_the_backup_if_the_original_was_replaced() {
	let dir = tempfile::tempdir().unwrap();
	let server = server(&dir.path().join("state"));
	let job = partial_job(&server, dir.path());
	fs::rename(&job.source.path, dir.path().join("moved-original.tar")).unwrap();
	fs::write(&job.source.path, b"unrelated").unwrap();

	let response = server.cancel(&cancel_request(&job.id)).unwrap();
	assert!(response.ok, "{response:?}");
	let warning = response.result.unwrap()["warning"].as_str().unwrap().to_string();
	assert!(warning.contains("no longer holds the original"), "{warning}");
	assert_eq!(fs::read(&job.source.path).unwrap(), b"unrelated");
	assert_eq!(fs::read(&job.outputs[0].path).unwrap(), b"output");
	assert!(
		retype_core::undo::Store::open(&server.shared.state_dir)
			.load(&job.id)
			.unwrap()
			.unwrap()
			.pinned
	);
	server.pool.stop();
}

#[test]
fn cancelling_without_an_undo_record_reports_the_limit_and_keeps_files() {
	let dir = tempfile::tempdir().unwrap();
	let server = server(&dir.path().join("state"));
	let job = partial_job(&server, dir.path());
	let store = retype_core::undo::Store::open(&server.shared.state_dir);
	store.remove(&job.id).unwrap();

	let response = server.cancel(&cancel_request(&job.id)).unwrap();
	assert!(response.ok, "{response:?}");
	let warning = response.result.unwrap()["warning"].as_str().unwrap().to_string();
	assert!(warning.contains("no undo record"), "{warning}");
	assert_eq!(server.find(job.id.as_str()).unwrap().state, JobState::Cancelled);
	assert_eq!(fs::read(&job.source.path).unwrap(), b"original bytes");
	assert_eq!(fs::read(&job.outputs[0].path).unwrap(), b"output");
	server.pool.stop();
}

#[test]
fn cancellation_retention_is_finished_after_a_restart() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let server = server(&state);
	let mut job = partial_job(&server, dir.path());
	job.set_state(JobState::Cancelled);
	job.updated = retype_core::job::now_ms() - 60 * 24 * 60 * 60 * 1_000;
	for _ in 0..12 {
		crate::daemon::watcher::publish_checked(&server.shared, &job).unwrap();
	}
	let store = retype_core::undo::Store::open(&state);
	assert!(store.load(&job.id).unwrap().unwrap().pinned);
	server.pool.stop();
	drop(server);

	let restarted = Server::open(&state).unwrap();
	let record = store.load(&job.id).unwrap().unwrap();
	assert!(!record.pinned);
	assert!(record.retention_started.is_some());
	assert_eq!(restarted.find(job.id.as_str()).unwrap().state, JobState::Cancelled);
	restarted.pool.stop();
}

#[test]
fn a_recovered_output_clears_the_cancellation_warning_on_reconciliation() {
	let dir = tempfile::tempdir().unwrap();
	let server = server(&dir.path().join("state"));
	let job = partial_job(&server, dir.path());
	fs::write(&job.outputs[0].path, b"tamper").unwrap();
	assert!(server.cancel(&cancel_request(&job.id)).unwrap().ok);
	assert!(server.find(job.id.as_str()).unwrap().error.is_some());
	fs::write(&job.outputs[0].path, b"output").unwrap();

	crate::daemon::reconcile::run(&server.shared);

	let record = retype_core::undo::Store::open(&server.shared.state_dir)
		.load(&job.id)
		.unwrap()
		.unwrap();
	assert!(!record.pinned);
	assert!(server.find(job.id.as_str()).unwrap().error.is_none());
	server.pool.stop();
}

#[test]
fn a_partial_cancellation_is_not_claimed_if_the_journal_fails() {
	let dir = tempfile::tempdir().unwrap();
	let server = server(&dir.path().join("state"));
	let job = partial_job(&server, dir.path());
	let shared = Arc::clone(&server.shared);
	assert!(
		std::thread::spawn(move || {
			let _journal = shared.journal.lock().unwrap();
			panic!("inject a journal failure");
		})
		.join()
		.is_err()
	);

	let response = server.cancel(&cancel_request(&job.id)).unwrap();
	assert!(!response.ok);
	assert!(
		response
			.error
			.unwrap()
			.message
			.contains("could not be durably recorded")
	);
	assert_eq!(server.find(job.id.as_str()).unwrap().state, JobState::Partial);
	assert_eq!(fs::read(&job.outputs[0].path).unwrap(), b"output");
	assert!(
		retype_core::undo::Store::open(&server.shared.state_dir)
			.load(&job.id)
			.unwrap()
			.unwrap()
			.pinned
	);
	server.pool.stop();
}

#[test]
fn cancelling_a_parked_job_prevents_a_stale_timer_from_queueing_it() {
	use crate::daemon::timer::{self, Admission};

	let dir = tempfile::tempdir().unwrap();
	let server = server(&dir.path().join("state"));
	let job = parked_job(&server, dir.path(), "cost", JobState::Pending);
	assert!(server.cancel(&cancel_request(&job.id)).unwrap().ok);
	assert_eq!(
		timer::start_due(&server.shared, &server.pool, &job).unwrap(),
		Admission::Stale
	);
	assert!(!server.pool.ids().contains(&job.id));
	assert_eq!(server.find(job.id.as_str()).unwrap().state, JobState::Cancelled);
	assert_eq!(fs::read(&job.source.path).unwrap(), b"input bytes");
	server.pool.stop();
}

#[test]
fn superseding_a_parked_job_prevents_a_stale_timer_from_queueing_it() {
	use crate::daemon::timer::{self, Admission};

	let dir = tempfile::tempdir().unwrap();
	let server = server(&dir.path().join("state"));
	let job = parked_job(&server, dir.path(), "cost", JobState::Pending);
	let mut second = job.clone();
	second.id = JobId::now().unwrap();
	crate::daemon::watcher::publish_checked(&server.shared, &second).unwrap();
	let guard = server.shared.lock_parked_admission().unwrap();
	crate::daemon::park::superseded(&server.shared, &job.source.path, &guard).unwrap();
	drop(guard);
	for job in [job, second] {
		assert_eq!(
			timer::start_due(&server.shared, &server.pool, &job).unwrap(),
			Admission::Stale
		);
		assert!(!server.pool.ids().contains(&job.id));
		assert_eq!(server.find(job.id.as_str()).unwrap().state, JobState::Superseded);
	}
	server.pool.stop();
}

#[test]
fn a_legacy_parked_job_without_a_fingerprint_requires_review() {
	use crate::daemon::timer::{self, Admission};

	let dir = tempfile::tempdir().unwrap();
	let server = server(&dir.path().join("state"));
	let mut job = parked_job(&server, dir.path(), "cost", JobState::Pending);
	job.parked.as_mut().unwrap().input_fingerprint = None;
	crate::daemon::watcher::publish_checked(&server.shared, &job).unwrap();

	assert_eq!(
		timer::start_due(&server.shared, &server.pool, &job).unwrap(),
		Admission::Stale
	);
	let waiting = server.find(job.id.as_str()).unwrap();
	assert_eq!(waiting.state, JobState::NeedsAttention);
	assert!(waiting.parked.as_ref().unwrap().deadline.is_none());
	assert!(waiting.error.as_ref().unwrap().message.contains("resubmit"));
	assert!(timer::start_confirmed(&server.shared, &server.pool, &waiting, None, false).is_err());
	assert!(!server.pool.ids().contains(&job.id));
	server.pool.stop();
}

#[test]
fn a_parked_input_replaced_after_queueing_is_not_converted() {
	use crate::daemon::timer::{self, Admission};

	let dir = tempfile::tempdir().unwrap();
	let server = server(&dir.path().join("state"));
	let job = parked_job(&server, dir.path(), "ask", JobState::NeedsAttention);
	assert_eq!(
		timer::start_confirmed(&server.shared, &server.pool, &job, None, false).unwrap(),
		Admission::Queued
	);
	let modified = fs::metadata(&job.source.path).unwrap().modified().unwrap();
	fs::write(&job.source.path, b"other bytes").unwrap();
	fs::OpenOptions::new()
		.write(true)
		.open(&job.source.path)
		.unwrap()
		.set_times(fs::FileTimes::new().set_modified(modified))
		.unwrap();
	let work = timer::work(&job, None, false).unwrap();
	let Kind::Immediate(request) = work.kind else {
		panic!("expected a direct request")
	};
	crate::daemon::immediate::run(
		&server.shared,
		&request,
		&job.id,
		&retype_core::execute::Cancellation::default(),
	);
	let failed = server.find(job.id.as_str()).unwrap();
	assert_eq!(failed.state, JobState::Failed);
	assert_eq!(failed.error.unwrap().code, Code::JobInterrupted.to_string());
	assert!(!job.outputs[0].path.exists());
	assert_eq!(fs::read(&job.source.path).unwrap(), b"other bytes");
	server.pool.stop();
}

#[test]
fn a_parked_input_replaced_with_identical_bytes_is_not_converted() {
	use crate::daemon::timer::{self, Admission};

	let dir = tempfile::tempdir().unwrap();
	let server = server(&dir.path().join("state"));
	let job = parked_job(&server, dir.path(), "ask", JobState::NeedsAttention);
	assert_eq!(
		timer::start_confirmed(&server.shared, &server.pool, &job, None, false).unwrap(),
		Admission::Queued
	);
	fs::rename(&job.source.path, dir.path().join("original.tar")).unwrap();
	fs::write(&job.source.path, b"input bytes").unwrap();
	let work = timer::work(&job, None, false).unwrap();
	let Kind::Immediate(request) = work.kind else {
		panic!("expected a direct request")
	};
	crate::daemon::immediate::run(
		&server.shared,
		&request,
		&job.id,
		&retype_core::execute::Cancellation::default(),
	);
	let failed = server.find(job.id.as_str()).unwrap();
	assert_eq!(failed.state, JobState::Failed);
	assert_eq!(failed.error.unwrap().code, Code::JobInterrupted.to_string());
	assert!(!job.outputs[0].path.exists());
	assert_eq!(fs::read(&job.source.path).unwrap(), b"input bytes");
	assert_eq!(fs::read(dir.path().join("original.tar")).unwrap(), b"input bytes");
	server.pool.stop();
}

#[test]
fn a_timed_job_is_journalled_before_queueing_and_cancellation_keeps_its_request() {
	use crate::daemon::timer::{self, Admission};

	let dir = tempfile::tempdir().unwrap();
	let server = server(&dir.path().join("state"));
	let job = parked_job(&server, dir.path(), "cost", JobState::Pending);
	assert_eq!(
		timer::start_due(&server.shared, &server.pool, &job).unwrap(),
		Admission::Queued
	);
	let (_, replay) = Journal::open(&server.shared.state_dir.join("jobs.jsonl")).unwrap();
	let queued = replay.jobs.iter().find(|queued| queued.id == job.id).unwrap();
	assert_eq!(queued.state, JobState::Queued);
	assert!(queued.parked.as_ref().unwrap().deadline.is_none());
	assert!(server.cancel(&cancel_request(&job.id)).unwrap().ok);
	let cancelled = server.find(job.id.as_str()).unwrap();
	assert_eq!(cancelled.state, JobState::Cancelled);
	assert_eq!(cancelled.source, job.source);
	assert_eq!(cancelled.intent, job.intent);
	assert_eq!(cancelled.outputs[0].path, job.outputs[0].path);
	assert_eq!(cancelled.outputs[0].format, job.outputs[0].format);
	assert_eq!(cancelled.outputs[0].state, OutputState::Skipped);
	assert_eq!(
		cancelled.outputs[0].error.as_ref().unwrap().code,
		Code::JobCancelled.to_string()
	);
	assert!(!server.pool.ids().contains(&job.id));
	server.pool.stop();
}

#[test]
fn a_legacy_timed_ask_cannot_start_without_confirmation() {
	use crate::daemon::timer::{self, Admission};

	let dir = tempfile::tempdir().unwrap();
	let server = server(&dir.path().join("state"));
	let job = parked_job(&server, dir.path(), "ask", JobState::Pending);
	assert_eq!(
		timer::start_due(&server.shared, &server.pool, &job).unwrap(),
		Admission::Stale
	);
	assert!(server.pool.ids().is_empty());
	assert_eq!(
		timer::start_confirmed(&server.shared, &server.pool, &job, None, false).unwrap(),
		Admission::Queued
	);
	assert_eq!(server.find(job.id.as_str()).unwrap().state, JobState::Queued);
	server.pool.stop();
}

#[test]
fn a_confirmed_correction_is_recorded_before_queueing() {
	use crate::daemon::timer::{self, Admission};

	let dir = tempfile::tempdir().unwrap();
	let server = server(&dir.path().join("state"));
	let job = parked_job(&server, dir.path(), "intent", JobState::NeedsAttention);
	assert_eq!(
		timer::start_confirmed(&server.shared, &server.pool, &job, Some("zip+now"), true).unwrap(),
		Admission::Queued
	);
	let (_, replay) = Journal::open(&server.shared.state_dir.join("jobs.jsonl")).unwrap();
	let queued = replay.jobs.iter().find(|queued| queued.id == job.id).unwrap();
	assert_eq!(queued.state, JobState::Queued);
	assert_eq!(queued.intent.as_deref(), Some("zip+now"));
	assert_eq!(queued.parked.as_ref().unwrap().requested.as_deref(), Some("zip"));
	assert!(queued.parked.as_ref().unwrap().force);
	server.pool.stop();
}

#[test]
fn competing_confirmations_cannot_queue_the_same_job_twice() {
	use crate::daemon::timer::{self, Admission};
	use std::sync::Barrier;

	let dir = tempfile::tempdir().unwrap();
	let server = server(&dir.path().join("state"));
	let job = parked_job(&server, dir.path(), "ask", JobState::NeedsAttention);
	let barrier = Arc::new(Barrier::new(3));
	let threads: Vec<_> = (0..2)
		.map(|_| {
			let shared = Arc::clone(&server.shared);
			let pool = Arc::clone(&server.pool);
			let barrier = Arc::clone(&barrier);
			let job = job.clone();
			std::thread::spawn(move || {
				barrier.wait();
				timer::start_confirmed(&shared, &pool, &job, None, false).unwrap()
			})
		})
		.collect();
	barrier.wait();
	let outcomes: Vec<_> = threads.into_iter().map(|thread| thread.join().unwrap()).collect();
	assert_eq!(
		outcomes.iter().filter(|outcome| **outcome == Admission::Queued).count(),
		1
	);
	assert_eq!(
		outcomes.iter().filter(|outcome| **outcome == Admission::Stale).count(),
		1
	);
	assert_eq!(server.pool.ids().iter().filter(|id| **id == job.id).count(), 1);
	server.pool.stop();
}

#[test]
fn timer_and_cancellation_have_one_serialized_outcome() {
	use crate::daemon::timer::{self, Admission};
	use std::sync::Barrier;

	let dir = tempfile::tempdir().unwrap();
	let server = Arc::new(server(&dir.path().join("state")));
	for _ in 0..12 {
		let job = parked_job(&server, dir.path(), "cost", JobState::Pending);
		let barrier = Arc::new(Barrier::new(3));
		let timer_thread = {
			let shared = Arc::clone(&server.shared);
			let pool = Arc::clone(&server.pool);
			let barrier = Arc::clone(&barrier);
			let job = job.clone();
			std::thread::spawn(move || {
				barrier.wait();
				timer::start_due(&shared, &pool, &job).unwrap()
			})
		};
		let cancel_thread = {
			let server = Arc::clone(&server);
			let barrier = Arc::clone(&barrier);
			let id = job.id.clone();
			std::thread::spawn(move || {
				barrier.wait();
				server.cancel(&cancel_request(&id)).unwrap()
			})
		};
		barrier.wait();
		let admission = timer_thread.join().unwrap();
		assert!(matches!(admission, Admission::Queued | Admission::Stale));
		let response = cancel_thread.join().unwrap();
		assert!(response.ok, "{response:?}");
		assert!(!server.pool.ids().contains(&job.id));
		assert_eq!(server.find(job.id.as_str()).unwrap().state, JobState::Cancelled);
	}
	server.pool.stop();
}

#[test]
fn parked_admission_stops_when_its_journal_checkpoint_fails() {
	let dir = tempfile::tempdir().unwrap();
	let server = server(&dir.path().join("state"));
	let job = parked_job(&server, dir.path(), "cost", JobState::Pending);
	let shared = Arc::clone(&server.shared);
	assert!(
		std::thread::spawn(move || {
			let _journal = shared.journal.lock().unwrap();
			panic!("inject a journal failure");
		})
		.join()
		.is_err()
	);

	assert!(crate::daemon::timer::start_due(&server.shared, &server.pool, &job).is_err());
	assert!(!server.pool.ids().contains(&job.id));
	assert_eq!(server.find(job.id.as_str()).unwrap().state, JobState::Pending);
	assert_eq!(fs::read(&job.source.path).unwrap(), b"input bytes");
	server.pool.stop();
}

#[test]
fn a_parked_cancellation_is_not_claimed_if_its_journal_fails() {
	let dir = tempfile::tempdir().unwrap();
	let server = server(&dir.path().join("state"));
	let job = parked_job(&server, dir.path(), "cost", JobState::Pending);
	let shared = Arc::clone(&server.shared);
	assert!(
		std::thread::spawn(move || {
			let _journal = shared.journal.lock().unwrap();
			panic!("inject a journal failure");
		})
		.join()
		.is_err()
	);

	let response = server.cancel(&cancel_request(&job.id)).unwrap();
	assert!(!response.ok);
	assert!(response.error.unwrap().message.contains("may still run"));
	assert_eq!(server.find(job.id.as_str()).unwrap().state, JobState::Pending);
	server.pool.stop();
}

#[test]
fn a_queued_cancellation_reports_journal_failure_and_cannot_replay() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let server = server(&state);
	let job = parked_job(&server, dir.path(), "cost", JobState::Pending);
	assert_eq!(
		crate::daemon::timer::start_due(&server.shared, &server.pool, &job).unwrap(),
		crate::daemon::timer::Admission::Queued
	);
	let shared = Arc::clone(&server.shared);
	assert!(
		std::thread::spawn(move || {
			let _journal = shared.journal.lock().unwrap();
			panic!("inject a journal failure");
		})
		.join()
		.is_err()
	);

	let response = server.cancel(&cancel_request(&job.id)).unwrap();
	assert!(!response.ok);
	assert!(response.error.unwrap().message.contains("removed from the queue"));
	assert!(!server.pool.ids().contains(&job.id));
	server.pool.stop();
	drop(server);
	let restarted = Server::open(&state).unwrap();
	let recovered = restarted.find(job.id.as_str()).unwrap();
	assert_eq!(recovered.state, JobState::Failed);
	assert_eq!(recovered.error.unwrap().code, Code::JobInterrupted.to_string());
	assert!(!restarted.pool.ids().contains(&job.id));
	assert_eq!(fs::read(&job.source.path).unwrap(), b"input bytes");
	restarted.pool.stop();
}

#[test]
fn stopping_the_daemon_prevents_later_timer_admission() {
	use crate::daemon::timer::{self, Admission};

	let dir = tempfile::tempdir().unwrap();
	let mut server = server(&dir.path().join("state"));
	let job = parked_job(&server, dir.path(), "cost", JobState::Pending);
	server.stop(false);
	assert_eq!(
		timer::start_due(&server.shared, &server.pool, &job).unwrap(),
		Admission::Full
	);
	assert_eq!(server.find(job.id.as_str()).unwrap().state, JobState::Pending);
	assert!(!server.pool.ids().contains(&job.id));
	server.pool.stop();
}

#[test]
fn a_full_queue_leaves_a_timed_job_parked() {
	use crate::daemon::timer::{self, Admission};

	let dir = tempfile::tempdir().unwrap();
	let server = server(&dir.path().join("state"));
	let job = parked_job(&server, dir.path(), "cost", JobState::Pending);
	for _ in 0..CAPACITY {
		assert!(server.pool.submit(Work {
			id: JobId::now().unwrap(),
			kind: Kind::Immediate(immediate::Request {
				input: job.source.path.clone(),
				intent: "zip".to_string(),
				output: None,
				force: false,
			}),
		}));
	}
	assert_eq!(
		timer::start_due(&server.shared, &server.pool, &job).unwrap(),
		Admission::Full
	);
	assert_eq!(server.find(job.id.as_str()).unwrap().state, JobState::Pending);
	assert!(server.cancel(&cancel_request(&job.id)).unwrap().ok);
	assert!(!server.pool.ids().contains(&job.id));
	server.pool.stop();
}

#[test]
fn a_disconnected_client_is_not_a_daemon_failure() {
	let disconnected = crate::error::Error::Frame(retype_ipc::FrameError::Io("broken pipe".to_string()));
	assert!(connection_result(Err(disconnected)).is_ok());

	let internal = crate::error::Error::Io(std::io::Error::other("internal failure"));
	assert!(connection_result(Err(internal)).is_err());
}

#[test]
fn a_cli_submission_is_journalled_before_it_waits_in_the_queue() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let input = dir.path().join("input.tar");
	fs::write(&input, b"input bytes").unwrap();
	let server = server(&state);

	let response = server.submit(&submission(&input)).unwrap();
	assert!(response.ok);
	let result = response.result.unwrap();
	let id = JobId::parse(result["id"].as_str().unwrap()).unwrap();
	assert!(server.pool.ids().contains(&id));
	let (_, replay) = Journal::open(&state.join("jobs.jsonl")).unwrap();
	assert_eq!(replay.jobs.len(), 1);
	assert_eq!(replay.jobs[0].id, id);
	assert_eq!(replay.jobs[0].state, JobState::Queued);
	assert_eq!(replay.jobs[0].source.path, input);
	assert_ne!(replay.jobs[0].source.mtime_ns, 0);
	server.pool.stop();
}

#[test]
fn an_explicit_cli_ask_is_journalled_without_entering_the_queue() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let input = dir.path().join("input.tar");
	fs::write(&input, b"input bytes").unwrap();
	let server = server(&state);

	for intent in ["zip+ask", "zip+"] {
		let request = Request::new(1, "jobs.submit")
			.with_params(json!({"input": input, "intent": intent}))
			.unwrap();
		let response = server.submit(&request).unwrap();
		assert!(response.ok, "{response:?}");
		assert_eq!(response.result.unwrap()["state"], "needs-attention");
	}

	assert!(server.pool.ids().is_empty());
	let (_, replay) = Journal::open(&state.join("jobs.jsonl")).unwrap();
	assert_eq!(replay.jobs.len(), 2);
	for job in replay.jobs {
		assert_eq!(job.state, JobState::NeedsAttention);
		let parked = job.parked.unwrap();
		assert_eq!(parked.reason, "ask");
		assert_eq!(parked.deadline, None);
	}
	assert!(!dir.path().join("input.zip").exists());
	server.pool.stop();
}

#[test]
fn a_full_queue_does_not_leave_a_replayable_cli_job() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let input = dir.path().join("input.tar");
	fs::write(&input, b"input bytes").unwrap();
	let server = server(&state);

	for _ in 0..CAPACITY {
		assert!(server.pool.submit(Work {
			id: JobId::now().unwrap(),
			kind: Kind::Immediate(immediate::Request {
				input: input.clone(),
				intent: "zip".to_string(),
				output: None,
				force: false,
			}),
		}));
	}

	let response = server.submit(&submission(&input)).unwrap();
	let error = response.error.unwrap();
	assert_eq!(error.code, Code::JobState);
	assert!(error.message.contains("nothing was submitted"));
	let (_, replay) = Journal::open(&state.join("jobs.jsonl")).unwrap();
	assert!(replay.jobs.is_empty(), "a rejected job must not replay");
	assert_eq!(fs::read(&input).unwrap(), b"input bytes");
	server.pool.stop();
}

#[test]
fn a_journal_failure_does_not_enqueue_a_cli_job() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let input = dir.path().join("input.tar");
	fs::write(&input, b"input bytes").unwrap();
	let server = server(&state);
	let shared = Arc::clone(&server.shared);

	let failed = std::thread::spawn(move || {
		let _journal = shared.journal.lock().unwrap();
		panic!("inject a journal failure");
	})
	.join();
	assert!(failed.is_err());

	let response = server.submit(&submission(&input)).unwrap();
	let error = response.error.unwrap();
	assert_eq!(error.code, Code::IoFailed);
	assert!(error.message.contains("did not start"));
	assert!(error.message.contains("check the job before retrying"));
	assert!(server.pool.ids().is_empty());
	assert!(!dir.path().join("input.zip").exists());
	assert_eq!(fs::read(&input).unwrap(), b"input bytes");
	server.pool.stop();
}
