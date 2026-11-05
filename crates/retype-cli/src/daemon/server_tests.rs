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
