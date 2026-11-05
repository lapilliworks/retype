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
			JobState::Pending if job.parked.as_ref().is_some_and(|parked| parked.reason == "ask") => {
				require_confirmation(shared, &job);
			}
			_ => {}
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
