//! Deterministic publication races, independent of filesystem flush timing.

use super::*;

fn pending() -> Job {
	serde_json::from_value(serde_json::json!({
		"v": retype_core::job::JOURNAL_VERSION,
		"kind": "job",
		"id": JobId::now().unwrap(),
		"created": now_ms(), "updated": now_ms(),
		"source": { "path": "photo.png", "format": "png", "size": 6, "mtime_ns": 0 },
		"intent": "webp", "outputs": [], "state": "pending",
		"backend": null, "machine": "test", "error": null,
	}))
	.unwrap()
}

#[test]
fn a_visible_journal_record_cannot_race_a_not_found_from_the_job_list() {
	let dir = tempfile::tempdir().unwrap();
	let server = crate::daemon::Server::open(dir.path()).unwrap();
	server.pool().stop();
	let shared = server.shared();
	let job = pending();
	let (visible, observed) = std::sync::mpsc::channel();
	let (release, proceed) = std::sync::mpsc::channel();

	std::thread::scope(|scope| {
		let publishing = Arc::clone(&shared);
		let snapshot = job.clone();
		let publisher = scope.spawn(move || {
			record_job(&publishing, &snapshot, || {
				persist(&publishing, &snapshot)?;
				visible.send(()).unwrap();
				proceed.recv_timeout(Duration::from_secs(10)).unwrap();
				Ok(())
			})
		});
		observed.recv_timeout(Duration::from_secs(10)).unwrap();
		let recorded = std::fs::read_to_string(dir.path().join("jobs.jsonl")).unwrap();
		let blocks_lookup = matches!(shared.jobs.try_lock(), Err(std::sync::TryLockError::WouldBlock));
		release.send(()).unwrap();
		publisher.join().unwrap().unwrap();
		assert!(recorded.contains(job.id.as_str()), "the pending job is already on disk");
		assert!(
			blocks_lookup,
			"IPC lookup must wait until the journaled job is registered"
		);
	});
	assert!(shared.jobs.lock().unwrap().iter().any(|entry| entry.id == job.id));
}

#[test]
fn a_failed_checkpoint_does_not_publish_a_job() {
	let dir = tempfile::tempdir().unwrap();
	let server = crate::daemon::Server::open(dir.path()).unwrap();
	server.pool().stop();
	let shared = server.shared();
	let outcome = record_job(&shared, &pending(), || {
		Err(retype_core::error::Error::Io(std::io::Error::other("storage failure")))
	});
	assert!(outcome.is_err());
	assert!(shared.jobs.lock().unwrap().is_empty());
}
