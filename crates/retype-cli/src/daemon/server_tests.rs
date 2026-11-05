use std::fs;
use std::path::Path;
use std::sync::Arc;

use super::*;
use crate::daemon::immediate;
use crate::daemon::queue::{CAPACITY, Kind, Pool, Work};
use retype_core::job::JobId;
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
