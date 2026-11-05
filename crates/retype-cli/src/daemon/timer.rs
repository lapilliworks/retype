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
				start_job(&pool, &job, None, false);
			}

			thread::sleep(TICK);
		}
	});

	(stop, handle)
}

/// The parked jobs whose time has come, which is also how a deadline that passed while
/// the daemon was not running is honoured.
fn due(shared: &Shared, pool: &Pool) -> Vec<Job> {
	let now = now_ms();
	let running = pool.ids();

	let Ok(jobs) = shared.jobs.lock() else {
		return Vec::new();
	};

	jobs.iter()
		.filter(|job| job.state == JobState::Pending)
		.filter(|job| !job.parked.as_ref().is_some_and(|parked| parked.reason == "ask"))
		.filter(|job| !running.contains(&job.id))
		.filter(|job| {
			job.parked
				.as_ref()
				.and_then(|parked| parked.deadline)
				.is_some_and(|deadline| deadline <= now)
		})
		.cloned()
		.collect()
}

/// Put a parked job back in the queue, either as it stands or with the intent a repair
/// produced, `docs/deferral.md` §8.
///
/// The source is consumed when the job commits if it came from a rename, and left
/// alone if it came from a client, which is the one thing the parked record has to
/// remember.
pub fn start_job(pool: &Pool, job: &Job, intent: Option<&str>, force: bool) -> bool {
	work(job, intent, force).is_some_and(|work| pool.submit(work))
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
