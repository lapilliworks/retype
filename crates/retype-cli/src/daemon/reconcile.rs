//! Startup reconciliation, `docs/dev/architecture/state.md` §6.
//!
//! A daemon that dies leaves the journal holding jobs that were in flight. Two
//! things can be true of them, and they are different:
//!
//! - `queued`: nothing ran, so nothing was written. The job is put back in the queue
//!   with the request the journal recorded
//! - `running`: a conversion was interrupted. Some of its outputs may already be
//!   committed, so re-running it blindly would either overwrite them or fail on them.
//!   It is marked `E_INTERRUPTED`, which is exactly the state `retype jobs resume`
//!   exists for: the outputs that carry their recorded identity are skipped and the
//!   rest are run again
//!
//! A parked job is not reconciled here: it keeps its deadline and its timer is
//! re-armed, `docs/deferral.md` §7.

use std::path::Path;

use retype_core::error::Code;
use retype_core::job::{Job, JobError, JobState};

use crate::daemon::immediate;
use crate::daemon::queue::{Kind, Pool, Work};
use crate::daemon::watcher::{self, Shared};

/// Put the journal's unfinished jobs back where they belong.
pub fn run(shared: &Shared, pool: &Pool) {
	let jobs: Vec<Job> = match shared.jobs.lock() {
		Ok(jobs) => jobs.clone(),
		Err(_) => return,
	};

	for job in jobs {
		match job.state {
			JobState::Queued => requeue(shared, pool, &job),
			JobState::Running => interrupted(shared, &job),
			_ => {}
		}
	}
}

fn requeue(shared: &Shared, pool: &Pool, job: &Job) {
	// A job with no intent was not a conversion, so there is nothing to run again.
	let Some(intent) = job.intent.clone() else {
		interrupted(shared, job);
		return;
	};

	let request = immediate::Request {
		input: job.source.path.clone(),
		intent,
		// The recorded path, not a fresh derivation: `-o` was part of the request and
		// is not in the journal, so the output is the only place it survives. A
		// rename-derived job has no `-o`.
		output: match (job.consume_source, job.outputs.as_slice()) {
			(false, [single]) => Some(single.path.to_string_lossy().into_owned()),
			_ => None,
		},
		force: false,
	};

	// The plan is made again rather than trusted. A source that went away or a tool
	// that was uninstalled between the two runs is then the failure it would have
	// been, instead of a job that dies unexplained.
	if immediate::prepare(&shared.registry(), &request).is_err() {
		interrupted(shared, job);
		return;
	}

	// A job that consumes its source came from a rename or a repair, not from
	// `retype convert`. Requeueing it as an immediate job would leave the source
	// behind after the conversion committed.
	let kind = match job.consume_source {
		true => Kind::Deferred {
			root: job.source.path.parent().unwrap_or(Path::new("")).to_path_buf(),
			source: request.input.clone(),
			intent: request.intent.clone(),
			force: false,
		},
		false => Kind::Immediate(request),
	};

	let work = Work {
		id: job.id.clone(),
		kind,
	};

	if !pool.submit(work) {
		interrupted(shared, job);
	}
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
	failed.fail(JobError {
		code: Code::JobInterrupted.to_string(),
		message: "the daemon stopped before this job finished; verified recovery bytes are required to resume"
			.to_string(),
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
