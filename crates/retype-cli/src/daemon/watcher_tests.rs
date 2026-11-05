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

fn admission_fixture() -> (tempfile::TempDir, Arc<Shared>, Arc<Pool>, PathBuf) {
	let dir = tempfile::tempdir().unwrap();
	let photos = dir.path().join("photos");
	std::fs::create_dir(&photos).unwrap();
	let server = crate::daemon::Server::open(&dir.path().join("state")).unwrap();
	server.pool().stop();
	let shared = server.shared();
	shared.reload_registry(Registry::builtin().unwrap());
	let pool = Pool::start(Arc::clone(&shared), 0);
	(dir, shared, pool, photos)
}

fn decide_archive(shared: &Shared, pool: &Pool, photos: &Path) {
	decide(
		shared,
		pool,
		photos,
		retype_core::correlate::RenameFact {
			source: photos.join("bundle.tar"),
			target: photos.join("bundle.zip"),
		},
		None,
	);
}

#[test]
fn a_watched_rename_is_restored_and_journalled_before_it_can_run() {
	let (dir, shared, pool, photos) = admission_fixture();
	std::fs::write(photos.join("bundle.zip"), b"original bytes").unwrap();
	decide_archive(&shared, &pool, &photos);

	assert_eq!(std::fs::read(photos.join("bundle.tar")).unwrap(), b"original bytes");
	assert!(!photos.join("bundle.zip").exists());
	let (_, replay) = Journal::open(&dir.path().join("state/jobs.jsonl")).unwrap();
	assert_eq!(replay.jobs.len(), 1);
	let job = &replay.jobs[0];
	assert_eq!(job.state, JobState::Queued);
	assert_eq!(job.source.path, photos.join("bundle.tar"));
	assert_eq!(job.intent.as_deref(), Some("zip"));
	assert_eq!(job.outputs[0].path, photos.join("bundle.zip"));
	assert!(pool.ids().contains(&job.id));
	pool.stop();
}

#[test]
fn a_watched_rename_restores_the_exact_source_case() {
	let (_dir, shared, pool, photos) = admission_fixture();
	std::fs::write(photos.join("bundle.zip"), b"original bytes").unwrap();
	decide(
		&shared,
		&pool,
		&photos,
		retype_core::correlate::RenameFact {
			source: photos.join("bundle.TAR"),
			target: photos.join("bundle.zip"),
		},
		None,
	);

	assert_eq!(std::fs::read(photos.join("bundle.TAR")).unwrap(), b"original bytes");
	let names: Vec<_> = std::fs::read_dir(&photos)
		.unwrap()
		.map(|entry| entry.unwrap().file_name())
		.collect();
	assert!(names.contains(&std::ffi::OsString::from("bundle.TAR")));
	assert!(!names.contains(&std::ffi::OsString::from("bundle.tar")));
	assert_eq!(shared.jobs.lock().unwrap()[0].source.path, photos.join("bundle.TAR"));
	pool.stop();
}

#[test]
fn an_accepted_watched_rename_survives_restart_for_manual_review() {
	let (dir, shared, pool, photos) = admission_fixture();
	std::fs::write(photos.join("bundle.zip"), b"original bytes").unwrap();
	decide_archive(&shared, &pool, &photos);
	let id = pool.ids().pop().unwrap();
	pool.stop();

	let restarted = crate::daemon::Server::open(&dir.path().join("state")).unwrap();
	let replay = restarted.shared();
	let jobs = replay.jobs.lock().unwrap();
	let interrupted = jobs.iter().find(|job| job.id == id).unwrap();
	assert_eq!(interrupted.state, JobState::Failed);
	assert_eq!(
		interrupted.error.as_ref().unwrap().code,
		Code::JobInterrupted.to_string()
	);
	assert!(interrupted.error.as_ref().unwrap().message.contains("resubmit"));
	assert_eq!(std::fs::read(photos.join("bundle.tar")).unwrap(), b"original bytes");
	assert!(!photos.join("bundle.zip").exists());
	drop(jobs);
	restarted.pool().stop();
}

#[test]
fn a_full_queue_restores_a_watched_source_without_a_replayable_job() {
	let (dir, shared, pool, photos) = admission_fixture();
	std::fs::write(photos.join("bundle.zip"), b"original bytes").unwrap();
	for _ in 0..crate::daemon::queue::CAPACITY {
		assert!(pool.submit(crate::daemon::queue::Work {
			id: JobId::now().unwrap(),
			kind: crate::daemon::queue::Kind::Immediate(crate::daemon::immediate::Request {
				input: photos.join("dummy.tar"),
				intent: "zip".to_string(),
				output: None,
				force: false,
			}),
		}));
	}

	decide_archive(&shared, &pool, &photos);

	assert_eq!(std::fs::read(photos.join("bundle.tar")).unwrap(), b"original bytes");
	assert!(!photos.join("bundle.zip").exists());
	let (_, replay) = Journal::open(&dir.path().join("state/jobs.jsonl")).unwrap();
	assert!(replay.jobs.is_empty(), "a rejected rename must not replay");
	pool.stop();
}

#[test]
fn a_journal_failure_restores_a_watched_source_without_starting_work() {
	let (_dir, shared, pool, photos) = admission_fixture();
	std::fs::write(photos.join("bundle.zip"), b"original bytes").unwrap();
	let poisoned = Arc::clone(&shared);
	assert!(
		std::thread::spawn(move || {
			let _journal = poisoned.journal.lock().unwrap();
			panic!("inject a journal failure");
		})
		.join()
		.is_err()
	);

	decide_archive(&shared, &pool, &photos);

	assert!(pool.ids().is_empty());
	assert!(shared.jobs.lock().unwrap().is_empty());
	assert_eq!(std::fs::read(photos.join("bundle.tar")).unwrap(), b"original bytes");
	assert!(!photos.join("bundle.zip").exists());
	pool.stop();
}
