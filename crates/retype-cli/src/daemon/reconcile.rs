//! Startup reconciliation, `docs/dev/architecture/state.md` §6.
//!
//! A daemon that dies leaves the journal holding jobs that were in flight.
//! Queued jobs do not yet have a content fingerprint, so they require manual review
//! rather than automatic replay against a possibly replaced source. Running jobs may
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

	for job in jobs {
		match job.state {
			JobState::Queued | JobState::Running => interrupted(shared, &job),
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
			JobState::Cancelled
				if job
					.outputs
					.iter()
					.any(|output| output.state == retype_core::job::OutputState::Done) =>
			{
				finish_cancelled(shared, &job);
			}
			_ => {}
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

fn finish_cancelled(shared: &Shared, job: &Job) {
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
	match super::cancel::finish_retention(&store, job) {
		Ok(()) => clear_retention_warning(shared, job),
		Err(error) => eprintln!(
			"retype: undo record for cancelled job {} remains protected: {error}",
			job.id
		),
	}
	drop(lease);
}

fn clear_retention_warning(shared: &Shared, job: &Job) {
	let is_warning = job
		.error
		.as_ref()
		.is_some_and(|error| error.message.starts_with("undo retention was not started:"));
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

fn interrupted(shared: &Shared, job: &Job) {
	let mut failed = job.clone();
	let store = retype_core::undo::Store::open(&shared.state_dir);
	match store.load(&job.id) {
		Ok(Some(record)) => {
			failed.outputs = record
				.outputs
				.iter()
				.map(|planned| {
					let committed = record.commits.iter().find(|commit| commit.output.path == planned.path);
					if let Some(commit) = committed {
						if matches!(commit.fingerprint.matches(&planned.path), Ok(true)) {
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
		JobState::Queued => format!(
			"the daemon stopped while this job was queued; review {} and resubmit the recorded intent, \
			 because the queued input cannot yet be verified for automatic replay",
			job.source.path.display()
		),
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
