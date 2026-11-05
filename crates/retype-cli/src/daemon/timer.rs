//! Expiry for parked jobs, `docs/deferral.md` §7.
//!
//! One thread, one clock. A parked job carries a deadline in its journal record, so a
//! daemon that restarts picks the timer up from the record rather than from memory,
//! and a deadline that passed while nothing was running is simply due on the first
//! tick.
//!
//! Expiry is unconditional: no notification, client or terminal is needed for a
//! `pending` job to run. What it is not is silent, because the record says it was
//! parked and why.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use retype_core::job::{Job, JobState, now_ms};

use crate::daemon::immediate;
use crate::daemon::queue::{Kind, Pool, Work};
use crate::daemon::watcher::Shared;

/// How often the clock is read. A parked job waits an hour by default, so a second is
/// precision nobody asked for.
const TICK: Duration = Duration::from_secs(1);

/// Start the timer, which stops when the flag is raised.
pub fn start(shared: Arc<Shared>, pool: Arc<Pool>) -> (Arc<AtomicBool>, JoinHandle<()>) {
	let stop = Arc::new(AtomicBool::new(false));
	let worker_stop = Arc::clone(&stop);

	let handle = thread::spawn(move || {
		while !worker_stop.load(Ordering::Relaxed) {
			for job in due(&shared, &pool) {
				if let Err(error) = start_due(&shared, &pool, &job) {
					eprintln!("retype: parked job {} could not be queued: {error}", job.id);
				}
			}

			thread::sleep(TICK);
		}
	});

	(stop, handle)
}

/// The parked jobs whose time has come, which is also how a deadline that passed while
/// the daemon was not running is honoured.
fn due(shared: &Shared, pool: &Pool) -> Vec<Job> {
	let running = pool.ids();

	let Ok(jobs) = shared.jobs.lock() else {
		return Vec::new();
	};

	jobs.iter()
		.filter(|job| is_due(job))
		.filter(|job| !running.contains(&job.id))
		.cloned()
		.collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
	Queued,
	Full,
	Stale,
}

/// Start an ordinary timed deferral only if the current record is still due.
pub fn start_due(shared: &Shared, pool: &Pool, expected: &Job) -> retype_core::error::Result<Admission> {
	admit(shared, pool, expected, Decision::Deadline)
}

/// Start a parked job after a person confirms it.
pub fn start_confirmed(
	shared: &Shared,
	pool: &Pool,
	expected: &Job,
	intent: Option<&str>,
	force: bool,
) -> retype_core::error::Result<Admission> {
	admit(shared, pool, expected, Decision::Confirmed { intent, force })
}

#[derive(Clone, Copy)]
enum Decision<'a> {
	Deadline,
	Confirmed { intent: Option<&'a str>, force: bool },
}

fn admit(
	shared: &Shared,
	pool: &Pool,
	expected: &Job,
	decision: Decision<'_>,
) -> retype_core::error::Result<Admission> {
	let _guard = shared.lock_parked_admission()?;
	let current = shared
		.jobs
		.lock()
		.map_err(|_| retype_core::error::Error::Io(std::io::Error::other("job list lock poisoned")))?
		.iter()
		.find(|job| job.id == expected.id)
		.cloned();
	let Some(current) = current.filter(|job| job == expected) else {
		return Ok(Admission::Stale);
	};
	let (intent, force) = match decision {
		Decision::Deadline if is_due(&current) => (None, false),
		Decision::Confirmed { intent, force }
			if matches!(current.state, JobState::Pending | JobState::NeedsAttention) =>
		{
			(intent, force)
		}
		_ => return Ok(Admission::Stale),
	};
	if current
		.parked
		.as_ref()
		.and_then(|parked| parked.input_fingerprint.as_ref())
		.is_none()
	{
		return missing_fingerprint(shared, current, decision);
	}
	let work = work(&current, intent, force)
		.ok_or_else(|| retype_core::error::Error::Io(std::io::Error::other("parked job has no intent to queue")))?;
	let mut queued = current;
	queued.set_state(JobState::Queued);
	if let Some(intent) = intent {
		queued.intent = Some(intent.to_string());
	}
	queued.error = None;
	if let Some(parked) = &mut queued.parked {
		parked.deadline = None;
		parked.force = force;
	}
	match pool.submit_after(work, || super::watcher::publish_checked(shared, &queued))? {
		true => Ok(Admission::Queued),
		false => Ok(Admission::Full),
	}
}

fn missing_fingerprint(shared: &Shared, mut job: Job, decision: Decision<'_>) -> retype_core::error::Result<Admission> {
	let message = "the parked input has no recorded fingerprint; cancel this job, review the file and resubmit it";
	match decision {
		Decision::Confirmed { .. } => Err(retype_core::error::Error::Diagnostic(
			retype_core::error::Diagnostic::global(retype_core::error::Code::JobResumeUnavailable, message),
		)),
		Decision::Deadline => {
			job.set_state(JobState::NeedsAttention);
			job.error = Some(retype_core::job::JobError::new(
				retype_core::error::Code::JobResumeUnavailable.to_string(),
				message,
			));
			if let Some(parked) = &mut job.parked {
				parked.deadline = None;
			}
			super::watcher::publish_checked(shared, &job)?;
			Ok(Admission::Stale)
		}
	}
}

fn is_due(job: &Job) -> bool {
	job.state == JobState::Pending
		&& job.parked.as_ref().is_some_and(|parked| {
			parked.reason != "ask" && parked.deadline.is_some_and(|deadline| deadline <= now_ms())
		})
}

pub fn work(job: &Job, intent: Option<&str>, force: bool) -> Option<Work> {
	let intent = intent.map(str::to_string).or_else(|| job.intent.clone())?;

	let consume = job.parked.as_ref().is_none_or(|parked| parked.consume);

	let kind = match consume {
		true => Kind::Deferred {
			root: job.source.path.parent().unwrap_or(Path::new("")).to_path_buf(),
			source: job.source.path.clone(),
			intent,
			force,
		},
		false => Kind::Immediate(immediate::Request {
			input: job.source.path.clone(),
			intent,
			output: match job.outputs.as_slice() {
				[single] => Some(single.path.to_string_lossy().into_owned()),
				_ => None,
			},
			force,
		}),
	};

	Some(Work {
		id: job.id.clone(),
		kind,
	})
}
