//! Startup reconciliation, `docs/dev/architecture/state.md` §6.
//!
//! A daemon that dies leaves the journal holding jobs that were in flight.
//! Queued jobs keep their admitted input fingerprint. Restart reports whether the
//! input still matches, but requires explicit review rather than automatic replay. Running jobs may
//! have committed outputs; reconciliation verifies those against the recovery ledger
//! and marks the job interrupted for explicit recovery.
//!
//! A parked job is not reconciled here: it keeps its deadline and its timer is
//! re-armed, `docs/deferral.md` §7.

use retype_core::error::Code;
use retype_core::job::{Job, JobError, JobState};

use crate::daemon::watcher::{self, Shared};

/// Put the journal's unfinished jobs back where they belong.
pub fn run(shared: &Shared) {
	let jobs: Vec<Job> = match shared.jobs.lock() {
		Ok(jobs) => jobs.clone(),
		Err(_) => return,
	};

	run_jobs(shared, jobs);
}

pub(super) fn run_jobs(shared: &Shared, jobs: Vec<Job>) {
	run_cancellable(shared, jobs, &retype_core::execute::Cancellation::default());
}

pub(super) fn run_cancellable(shared: &Shared, jobs: Vec<Job>, cancellation: &retype_core::execute::Cancellation) {
	let startup_ids = jobs
		.iter()
		.map(|job| job.id.clone())
		.collect::<std::collections::BTreeSet<_>>();
	for job in jobs {
		if cancellation.is_cancelled() {
			break;
		}
		match job.state {
			JobState::Queued | JobState::Running => interrupted(shared, &job, cancellation),
			JobState::Done
				if matches!(
					&job.source_disposition,
					Some(retype_core::job::SourceDisposition::Pending)
				) =>
			{
				unresolved_disposition(shared, &job);
			}
			JobState::Pending if job.parked.as_ref().is_some_and(|parked| parked.reason == "ask") => {
				require_confirmation(shared, &job);
			}
			JobState::Cancelled => finish_cancelled(shared, &job, cancellation),
			_ => {}
		}
	}
	let store = retype_core::undo::Store::open(&shared.state_dir);
	if let Ok(records) = store.list() {
		for record in records {
			if cancellation.is_cancelled() {
				break;
			}
			if !startup_ids.contains(&record.id)
				|| record.source_fingerprint.is_some()
				|| record.reservation_fingerprint.is_none()
			{
				continue;
			}
			let result = store
				.lease(&record.id)
				.and_then(|lease| store.retain(lease, Some(record)))
				.and_then(|recovery| match recovery {
					retype_core::undo::Recovery::Retained(retained) => {
						retained.release_unpublished_cancellable(cancellation)
					}
					_ => Ok(()),
				});
			if let Err(error) = result {
				eprintln!("retype: incomplete reservation remains protected: {error}");
			}
		}
	}
}

fn unresolved_disposition(shared: &Shared, job: &Job) {
	let store = retype_core::undo::Store::open(&shared.state_dir);
	let has_record = match store.load(&job.id) {
		Ok(record) => record.is_some(),
		Err(error) => {
			eprintln!("retype: cannot inspect source disposition for {}: {error}", job.id);
			return;
		}
	};
	let _lease = if has_record {
		match store.lease(&job.id) {
			Ok(lease) => Some(lease),
			Err(error) => {
				eprintln!("retype: cannot reconcile source disposition for {}: {error}", job.id);
				return;
			}
		}
	} else {
		None
	};
	let mut unresolved = job.clone();
	unresolved.source_disposition = Some(retype_core::job::SourceDisposition::Unresolved {
		reason: "source disposition was interrupted; inspect the source and any recovery data".to_string(),
	});
	unresolved.updated = retype_core::job::now_ms();
	if let Err(error) = watcher::publish_checked(shared, &unresolved) {
		eprintln!(
			"retype: cannot record unresolved source disposition for {}: {error}",
			job.id
		);
	}
}

fn finish_cancelled(shared: &Shared, job: &Job, cancellation: &retype_core::execute::Cancellation) {
	let store = retype_core::undo::Store::open(&shared.state_dir);
	let record = match store.load(&job.id) {
		Ok(Some(record)) if record.pinned => record,
		Ok(_) => return,
		Err(error) => {
			eprintln!("retype: cannot inspect cancelled job {}: {error}", job.id);
			return;
		}
	};
	let lease = match store.lease(&record.id) {
		Ok(lease) => lease,
		Err(error) => {
			eprintln!("retype: cannot finish retention for cancelled job {}: {error}", job.id);
			return;
		}
	};
	let recovery = store.retain(lease, Some(record));
	match recovery.and_then(|recovery| super::cancel::finish_retention_cancellable(recovery, job, cancellation)) {
		Ok(()) => clear_retention_warning(shared, job),
		Err(error) => eprintln!(
			"retype: undo record for cancelled job {} remains protected: {error}",
			job.id
		),
	}
}

fn clear_retention_warning(shared: &Shared, job: &Job) {
	let is_warning = job.error.as_ref().is_some_and(|error| {
		error.message.starts_with("undo retention was not started:")
			|| error.message.starts_with("redundant recovery was not released:")
	});
	if is_warning {
		let mut corrected = job.clone();
		corrected.error = None;
		if let Err(error) = watcher::publish_checked(shared, &corrected) {
			eprintln!("retype: could not clear retention warning for job {}: {error}", job.id);
		}
	}
}

fn require_confirmation(shared: &Shared, job: &Job) {
	let mut waiting = job.clone();
	waiting.set_state(JobState::NeedsAttention);
	if let Some(parked) = &mut waiting.parked {
		parked.deadline = None;
	}
	watcher::publish(shared, &waiting);
}

fn queued_interruption(job: &Job, cancellation: &retype_core::execute::Cancellation) -> String {
	let expected = job
		.source
		.fingerprint
		.as_ref()
		.or_else(|| job.parked.as_ref().and_then(|parked| parked.input_fingerprint.as_ref()));
	let condition = match expected {
		Some(expected) => match expected.matches_cancellable(&job.source.path, cancellation) {
			Ok(true) => "the queued input still matches its admitted identity and bytes".to_string(),
			Ok(false) => "the queued input changed or was replaced".to_string(),
			Err(error) => format!("the queued input cannot be checked: {error}"),
		},
		None => match &job.admission {
			Some(stamp) => match stamp.verify(&job.source.path) {
				Ok(()) => "the admitted identity and metadata still match, but content preparation was interrupted"
					.to_string(),
				Err(error) if error.code() == Code::JobInterrupted => {
					"the queued input changed or was replaced".to_string()
				}
				Err(error) => format!("the queued input cannot be checked: {error}"),
			},
			None => "this legacy queued record has no input fingerprint".to_string(),
		},
	};
	format!(
		"the daemon stopped while this job was queued; {condition}; review {} and existing outputs \
		 and resubmit the recorded intent only when safe; nothing was automatically replayed",
		job.source.path.display(),
	)
}

fn interrupted(shared: &Shared, job: &Job, cancellation: &retype_core::execute::Cancellation) {
	let store = retype_core::undo::Store::open(&shared.state_dir);
	let _lease = match store.lease(&job.id) {
		Ok(lease) => lease,
		Err(error) => {
			eprintln!("retype: startup recovery for {} deferred: {error}", job.id);
			return;
		}
	};
	if !shared
		.jobs
		.lock()
		.is_ok_and(|jobs| jobs.iter().any(|current| current == job))
	{
		return;
	}
	let mut failed = job.clone();
	match store.load(&job.id) {
		Ok(Some(record)) => {
			failed.outputs = record
				.outputs
				.iter()
				.map(|planned| {
					let committed = record.commits.iter().find(|commit| commit.output.path == planned.path);
					if let Some(commit) = committed {
						if matches!(
							commit.fingerprint.matches_cancellable(&planned.path, cancellation),
							Ok(true)
						) {
							return commit.output.clone();
						}
					}
					retype_core::job::Output::pending(
						planned.path.clone(),
						&planned.ext,
						retype_core::job::Tier::Converted,
					)
				})
				.collect();
		}
		Ok(None) => {}
		Err(error) => eprintln!("retype: cannot read recovery ledger for {}: {error}", job.id),
	}
	let message = match job.state {
		JobState::Queued => queued_interruption(job, cancellation),
		_ => "the daemon stopped before this job finished; verified recovery bytes are required to resume".to_string(),
	};
	failed.fail(JobError {
		code: Code::JobInterrupted.to_string(),
		message,
	});
	if failed
		.outputs
		.iter()
		.any(|output| output.state == retype_core::job::OutputState::Done)
	{
		failed.state = JobState::Partial;
	}
	watcher::publish(shared, &failed);
}
