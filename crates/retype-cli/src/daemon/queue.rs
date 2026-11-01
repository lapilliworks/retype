//! The runner pool, `docs/dev/architecture/ipc.md` §6 and `docs/watch.md` §8.
//!
//! Watchers submit work; runners take one job each, run it, and come back for the
//! next. Nobody holds a batch: a runner that finishes the last queued job waits
//! idle, so a burst of renames is worked through at the pool's width rather than
//! converted in whatever order the events arrived.
//!
//! The queue is bounded on purpose. A build system writing through a watched tree
//! can ask for more conversions than any machine should accept, so automatic
//! submissions above the cap are dropped and counted rather than piling up: the
//! number is reported so a notification can say conversion paused instead of
//! pretending a file is on its way.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use retype_core::job::JobId;

use crate::daemon::watcher::{Shared, convert, convert_deferred};
use retype_core::execute::Cancellation;

/// How many automatic jobs may wait. Fixed rather than derived from the machine,
/// because a different number of cores must not change how much a directory may
/// ask for.
pub const CAPACITY: usize = 256;

/// How many conversions run at once: enough to keep a machine busy without
/// turning a burst of renames into a burst of encoders.
pub fn runners() -> usize {
	std::thread::available_parallelism()
		.map(|count| count.get())
		.unwrap_or(2)
		.clamp(1, 4)
}

/// Wait this long for work before checking whether to stop.
const IDLE: Duration = Duration::from_millis(200);

/// One conversion, identified before it starts, `docs/dev/architecture/state.md` §2: a deferred job
/// is addressable immediately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Work {
	pub id: JobId,
	pub kind: Kind,
}

/// The two things a runner can be asked to do. They share the journal, the pool
/// and the cancellation path, and differ in where the output name comes from: a
/// rename names it, a submission is told it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
	/// A rename a watcher saw, `docs/watch.md`.
	Rename {
		root: PathBuf,
		/// Where the bytes are now. Usually the target, because a rename put them
		/// there; after a rename chain that overtook an earlier job, at the name
		/// that job was writing.
		bytes: PathBuf,
		/// The name the bytes belong under while the conversion runs.
		source: PathBuf,
		target: PathBuf,
	},
	/// A conversion a client asked for, `docs/dev/architecture/ipc.md` §6.
	Immediate(crate::daemon::immediate::Request),
	/// A parked rename whose time has come, `docs/deferral.md` §7. The bytes are
	/// already under the source name, so nothing is relabelled, and the source is
	/// consumed on commit.
	Deferred {
		root: PathBuf,
		source: PathBuf,
		intent: String,
		/// Overwrite an output that appeared while the job was parked.
		force: bool,
	},
}

/// What a cancel request found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cancelled {
	/// Removed from the queue before it started.
	Queued,
	/// Running, and asked to stop.
	Running,
	/// Neither: it finished, or it was never here.
	Unknown,
}

#[derive(Default)]
struct Inner {
	/// `retype convert` and jobs a person resolves. They skip the automatic
	/// admission order because the command is the request, `docs/cli.md` §2.
	immediate: VecDeque<Work>,
	waiting: VecDeque<Work>,
	closed: bool,
}

impl Inner {
	fn len(&self) -> usize {
		self.immediate.len() + self.waiting.len()
	}

	fn take_next(&mut self) -> Option<Work> {
		self.immediate.pop_front().or_else(|| self.waiting.pop_front())
	}
}

/// The queue itself: a bounded deque with runners waiting on it. Runners take one
/// job each and come back, so a runner never holds work it is not executing.
#[derive(Default)]
pub struct Queue {
	inner: Mutex<Inner>,
	ready: Condvar,
	dropped: AtomicU64,
}

impl Queue {
	/// Offer work to the queue. `false` means it was full and the job was dropped,
	/// which `docs/watch.md` §8 asks for rather than absorbing churn.
	pub fn submit(&self, work: Work) -> bool {
		let Ok(mut inner) = self.inner.lock() else {
			return false;
		};

		if inner.closed || inner.len() >= CAPACITY {
			self.dropped.fetch_add(1, Ordering::Relaxed);
			return false;
		}

		match &work.kind {
			Kind::Immediate(_) => inner.immediate.push_back(work),
			_ => inner.waiting.push_back(work),
		}

		self.ready.notify_one();
		true
	}

	/// Wait up to `idle` for a job.
	fn take(&self, idle: Duration) -> Take {
		let deadline = Instant::now() + idle;
		let Ok(mut inner) = self.inner.lock() else {
			return Take::Closed;
		};

		loop {
			if let Some(work) = inner.take_next() {
				return Take::Work(work);
			}

			if inner.closed {
				return Take::Closed;
			}

			let left = deadline.saturating_duration_since(Instant::now());
			if left.is_zero() {
				return Take::Idle;
			}

			match self.ready.wait_timeout(inner, left) {
				Ok((guard, _)) => inner = guard,
				Err(_) => return Take::Closed,
			}
		}
	}

	/// Remove a job that has not started. `None` when it is not waiting.
	pub fn take_back(&self, id: &JobId) -> Option<Work> {
		let mut inner = self.inner.lock().ok()?;

		if let Some(index) = inner.immediate.iter().position(|work| work.id == *id) {
			return inner.immediate.remove(index);
		}

		let index = inner.waiting.iter().position(|work| work.id == *id)?;
		inner.waiting.remove(index)
	}

	/// Everything still waiting, taken out in one step so a stop cannot race a runner.
	pub fn drain(&self) -> Vec<Work> {
		match self.inner.lock() {
			Ok(mut inner) => {
				let mut work: Vec<Work> = inner.immediate.drain(..).collect();
				work.extend(inner.waiting.drain(..));
				work
			}
			Err(_) => Vec::new(),
		}
	}

	/// The ids waiting to start, for prefix resolution.
	pub fn ids(&self) -> Vec<JobId> {
		self.inner
			.lock()
			.map(|inner| {
				inner
					.immediate
					.iter()
					.chain(inner.waiting.iter())
					.map(|work| work.id.clone())
					.collect()
			})
			.unwrap_or_default()
	}

	fn close(&self) {
		if let Ok(mut inner) = self.inner.lock() {
			inner.closed = true;
		}
		self.ready.notify_all();
	}

	/// How many automatic jobs were refused because the queue was full.
	pub fn dropped(&self) -> u64 {
		self.dropped.load(Ordering::Relaxed)
	}
}

/// The cancellations of jobs that are running, so a client can reach a job that
/// has left the queue.
#[derive(Default)]
pub struct Inflight {
	tokens: Mutex<BTreeMap<JobId, Cancellation>>,
}

impl Inflight {
	fn begin(&self, id: &JobId) -> Cancellation {
		let token = Cancellation::default();

		if let Ok(mut tokens) = self.tokens.lock() {
			tokens.insert(id.clone(), token.clone());
		}

		token
	}

	fn end(&self, id: &JobId) {
		if let Ok(mut tokens) = self.tokens.lock() {
			tokens.remove(id);
		}
	}

	fn ids(&self) -> Vec<JobId> {
		self.tokens
			.lock()
			.map(|tokens| tokens.keys().cloned().collect())
			.unwrap_or_default()
	}
}

pub struct Pool {
	queue: Arc<Queue>,
	inflight: Arc<Inflight>,
	workers: Mutex<Vec<JoinHandle<()>>>,
}

impl Pool {
	/// Start `size` runners, each of which takes one job at a time.
	pub fn start(shared: Arc<Shared>, size: usize) -> Arc<Self> {
		let queue = Arc::new(Queue::default());
		let inflight = Arc::new(Inflight::default());
		let mut workers = Vec::with_capacity(size);

		for _ in 0..size {
			let queue = Arc::clone(&queue);
			let inflight = Arc::clone(&inflight);
			let shared = Arc::clone(&shared);

			workers.push(thread::spawn(move || serve(&shared, &queue, &inflight)));
		}

		Arc::new(Self {
			queue,
			inflight,
			workers: Mutex::new(workers),
		})
	}

	pub fn submit(&self, work: Work) -> bool {
		self.queue.submit(work)
	}

	pub fn dropped(&self) -> u64 {
		self.queue.dropped()
	}

	/// Stop everything, waiting and running alike, which is what `daemon stop --now`
	/// means. The work the waiting jobs stood for is returned so the
	/// caller can record each one as cancelled.
	pub fn stop_all(&self) -> (Vec<JobId>, Vec<Work>) {
		let waiting = self.queue.drain();

		let running = match self.inflight.tokens.lock() {
			Ok(tokens) => tokens
				.iter()
				.map(|(id, token)| {
					token.cancel();
					id.clone()
				})
				.collect(),
			Err(_) => Vec::new(),
		};

		(running, waiting)
	}

	/// Wait until nothing is running, so a stop that cancelled something has the
	/// records written before the process ends.
	pub fn wait_until_idle(&self, timeout: std::time::Duration) {
		let deadline = std::time::Instant::now() + timeout;

		while !self.idle() && std::time::Instant::now() < deadline {
			thread::sleep(std::time::Duration::from_millis(20));
		}
	}

	/// Whether nothing is running, which is what a draining stop is waiting for.
	pub fn idle(&self) -> bool {
		self.inflight.ids().is_empty()
	}

	/// Stop a job because a later rename replaced it, `docs/dev/architecture/state.md` §5.3.
	pub fn supersede(&self, id: &JobId) -> (Cancelled, Option<Work>) {
		if let Some(work) = self.queue.take_back(id) {
			return (Cancelled::Queued, Some(work));
		}

		match self.inflight.tokens.lock() {
			Ok(tokens) => match tokens.get(id) {
				Some(token) => {
					token.supersede();
					(Cancelled::Running, None)
				}
				None => (Cancelled::Unknown, None),
			},
			Err(_) => (Cancelled::Unknown, None),
		}
	}

	/// Stop a job, whether it is waiting or running. The work a queued job stood
	/// for is returned so the caller can record it as cancelled.
	pub fn cancel(&self, id: &JobId) -> (Cancelled, Option<Work>) {
		if let Some(work) = self.queue.take_back(id) {
			return (Cancelled::Queued, Some(work));
		}

		match self.inflight.tokens.lock() {
			Ok(tokens) => match tokens.get(id) {
				Some(token) => {
					token.cancel();
					(Cancelled::Running, None)
				}
				None => (Cancelled::Unknown, None),
			},
			Err(_) => (Cancelled::Unknown, None),
		}
	}

	/// Every job that is waiting or running.
	pub fn ids(&self) -> Vec<JobId> {
		let mut ids = self.queue.ids();
		ids.extend(self.inflight.ids());
		ids
	}

	/// Stop taking work and wait for the runners to finish what they took.
	pub fn stop(&self) {
		self.queue.close();

		let workers = self
			.workers
			.lock()
			.map(|mut workers| std::mem::take(&mut *workers))
			.unwrap_or_default();

		for handle in workers {
			let _ = handle.join();
		}
	}
}

/// What a runner found when it asked for work.
enum Take {
	Work(Work),
	/// Nothing waiting, and the queue is still open: wait again.
	Idle,
	/// The queue is closed and empty, so this runner is done.
	Closed,
}

fn serve(shared: &Shared, queue: &Queue, inflight: &Inflight) {
	// One job per turn: the loop dequeues only after the previous job is done, so
	// a runner never holds work it is not executing.
	loop {
		match queue.take(IDLE) {
			Take::Work(work) => {
				let cancellation = inflight.begin(&work.id);

				match &work.kind {
					Kind::Rename {
						root,
						bytes,
						source,
						target,
					} => {
						convert(shared, root, bytes, source, target, &work.id, &cancellation);
					}
					Kind::Immediate(request) => {
						crate::daemon::immediate::run(shared, request, &work.id, &cancellation);
					}
					Kind::Deferred {
						root,
						source,
						intent,
						force,
					} => {
						convert_deferred(shared, root, source, intent, *force, &work.id, &cancellation);
					}
				}

				inflight.end(&work.id);
			}
			Take::Idle => continue,
			Take::Closed => break,
		}
	}
}

/// The queue's own report: how many automatic jobs were dropped.
#[derive(Default)]
pub struct Dropped(AtomicU64);

impl Dropped {
	pub fn note(&self) {
		self.0.fetch_add(1, Ordering::Relaxed);
	}

	pub fn count(&self) -> u64 {
		self.0.load(Ordering::Relaxed)
	}
}

#[cfg(test)]
mod tests {
	use std::fs;
	use std::sync::atomic::AtomicU64;

	use super::*;
	use crate::daemon::watcher::Recent;
	use retype_core::journal::Journal;
	use retype_core::registry::Registry;

	/// A job id, distinct per call, for tests that submit several.
	fn id(index: usize) -> JobId {
		JobId::parse(&format!("01J{:023}", index)).expect("a valid id")
	}

	fn shared(state: &std::path::Path) -> Arc<Shared> {
		let registry = Registry::load(
			"\
version = 'retype-config-1'

[backends]
prefer = ['copy']

[tools.copy]
command = 'true'
layout = ['{input}', '{ops}', '{output}']

[formats.png]
family = 'image'
lossy = false
alpha = true
tier = 'exact'

[formats.webp]
family = 'image'
lossy = true
alpha = true
tier = 'converted'
",
		)
		.expect("the fixture loads");

		let (journal, replay) = Journal::open(&state.join("jobs.jsonl")).expect("journal");

		Arc::new(Shared {
			registry: std::sync::RwLock::new(std::sync::Arc::new(registry)),
			journal: std::sync::Mutex::new(journal),
			jobs: std::sync::Mutex::new(replay.jobs),
			recent: Recent::default(),
			dropped: AtomicU64::new(0),
			skipped: AtomicU64::new(0),
			placeholders: AtomicU64::new(0),
			primed: AtomicU64::new(0),
			events: std::sync::Mutex::new(crate::daemon::events::Events::default()),
			policies: Default::default(),
			batches: Default::default(),
			notifier: std::sync::Mutex::new(None),
			machine: "test".to_string(),
			state_dir: state.to_path_buf(),
		})
	}

	#[test]
	fn every_submitted_job_is_run_once() {
		let dir = tempfile::tempdir().unwrap();
		let shared = shared(dir.path());
		let photos = dir.path().join("photos");
		fs::create_dir(&photos).unwrap();

		let pool = Pool::start(Arc::clone(&shared), 2);

		for index in 0..8 {
			let source = photos.join(format!("a{index}.png"));
			let target = photos.join(format!("a{index}.webp"));
			fs::write(&source, b"pixels").unwrap();

			assert!(pool.submit(Work {
				id: id(index),
				kind: Kind::Rename {
					root: photos.clone(),
					bytes: target.clone(),
					source,
					target,
				},
			}));
		}

		// The fixture's command is `true`, so each job fails; what matters here is
		// that each was taken exactly once, which the job list records.
		let deadline = std::time::Instant::now() + Duration::from_secs(10);
		while shared.jobs.lock().map(|jobs| jobs.len()).unwrap_or_default() < 8 {
			assert!(std::time::Instant::now() < deadline, "jobs did not run");
			thread::sleep(Duration::from_millis(20));
		}

		pool.stop();

		assert_eq!(shared.jobs.lock().unwrap().len(), 8);
	}

	#[test]
	fn a_runner_waits_for_the_next_job_instead_of_exiting() {
		let dir = tempfile::tempdir().unwrap();
		let shared = shared(dir.path());
		let photos = dir.path().join("photos");
		fs::create_dir(&photos).unwrap();

		let pool = Pool::start(Arc::clone(&shared), 1);

		// Longer than the idle wait, so a runner that gave up when the queue was
		// empty would have gone by now.
		thread::sleep(IDLE * 3);

		let source = photos.join("late.png");
		let target = photos.join("late.webp");
		fs::write(&source, b"pixels").unwrap();
		assert!(pool.submit(Work {
			id: id(0),
			kind: Kind::Rename {
				root: photos.clone(),
				bytes: target.clone(),
				source,
				target,
			},
		}));

		let deadline = std::time::Instant::now() + Duration::from_secs(10);
		while shared.jobs.lock().map(|jobs| jobs.len()).unwrap_or_default() < 1 {
			assert!(std::time::Instant::now() < deadline, "the runner had gone");
			thread::sleep(Duration::from_millis(20));
		}

		pool.stop();
	}

	#[test]
	fn an_immediate_job_goes_ahead_of_waiting_renames() {
		let queue = Queue::default();
		let photos = PathBuf::from("/photos");
		let rename = |id| Work {
			id,
			kind: Kind::Rename {
				root: photos.clone(),
				bytes: photos.join("a.webp"),
				source: photos.join("a.png"),
				target: photos.join("a.webp"),
			},
		};

		assert!(queue.submit(rename(id(1))));
		assert!(queue.submit(Work {
			id: id(2),
			kind: Kind::Immediate(crate::daemon::immediate::Request {
				input: photos.join("b.png"),
				intent: "webp".to_string(),
				output: None,
				force: false,
			}),
		}));
		assert!(queue.submit(rename(id(3))));

		let first = match queue.take(Duration::ZERO) {
			Take::Work(work) => work.id,
			_ => panic!("the queue was empty"),
		};
		let second = match queue.take(Duration::ZERO) {
			Take::Work(work) => work.id,
			_ => panic!("the queue was empty"),
		};
		let third = match queue.take(Duration::ZERO) {
			Take::Work(work) => work.id,
			_ => panic!("the queue was empty"),
		};

		assert_eq!(first, id(2), "the submission skips the waiting renames");
		assert_eq!(second, id(1), "the automatic jobs keep their own order");
		assert_eq!(third, id(3));
	}

	#[test]
	fn a_full_queue_drops_and_says_so() {
		let dir = tempfile::tempdir().unwrap();
		let shared = shared(dir.path());

		// No runners, so nothing is taken and the queue fills at its cap.
		let pool = Pool::start(shared, 0);
		let photos = dir.path().to_path_buf();
		let accepted = (0..CAPACITY + 5)
			.filter(|index| {
				pool.submit(Work {
					id: id(*index),
					kind: Kind::Rename {
						root: photos.clone(),
						bytes: photos.join(format!("{index}.webp")),
						source: photos.join(format!("{index}.png")),
						target: photos.join(format!("{index}.webp")),
					},
				})
			})
			.count();

		pool.stop();

		assert_eq!(accepted, CAPACITY, "the cap is the queue, not the machine");
	}
}
