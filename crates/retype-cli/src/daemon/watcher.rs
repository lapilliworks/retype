//! Watching the registered directories and converting what a rename asks for,
//! `docs/watch.md` and `docs/dev/measurement/events.md`.
//!
//! One thread per root, each collecting a window of events and handing the batch
//! to the correlator. A batch that produces no rename fact produces no work, which
//! is the common case: an editor's save, a sync client's write, a copy that never
//! finished.
//!
//! Conversions are submitted to the runner pool, so a slow job does not hold the
//! event thread. The queue is what makes them cancellable and concurrent.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use retype_core::correlate::Correlator;
use retype_core::error::Code;
use retype_core::execute::Cancellation;
use retype_core::job::{Job, JobError, JobId, JobState, Output, OutputState, Source, now_ms};
use retype_core::journal::Journal;
use retype_core::plan::{self, Plan};
use retype_core::registry::Registry;
use retype_core::resolve::{self, Resolved, Source as Origin};
use retype_core::{execute, intent, print, watches};
use retype_watch::Window;

use crate::daemon::queue::Pool;
use crate::error::Result;

/// What the daemon, its watchers and its runners share.
pub struct Shared {
	/// The merged configuration, replaced whole on a successful reload so a
	/// reader never sees a half-updated document.
	pub(crate) registry: std::sync::RwLock<Arc<Registry>>,
	pub journal: Mutex<Journal>,
	/// The jobs this process has run, for `jobs.list`.
	pub jobs: Mutex<Vec<Job>>,
	/// What this process has converted, so its own writes are not mistaken for a
	/// rename request.
	pub recent: Recent,
	/// Automatic jobs refused because the queue was full, `docs/watch.md` §8.
	pub dropped: AtomicU64,
	/// Sources passed over because they were too large to convert unasked,
	/// `docs/watch.md` §6.
	pub skipped: AtomicU64,
	/// Files passed over because their contents are not on this machine: a provider's
	/// placeholder, `docs/watch.md` §5.
	pub placeholders: AtomicU64,
	/// Identities recorded by the startup walk, `docs/watch.md` §3.
	pub primed: AtomicU64,
	/// Event paths the watcher has looked at, `docs/watch.md` §3. It answers "is the
	/// watcher seeing anything", and a caller can wait for it to grow before it
	/// renames a file it just wrote, since on macOS a create and a rename inside one
	/// window are reported as a create and carry no identity to correlate.
	pub observed: AtomicU64,
	/// Paths this process moved itself and has not yet told the correlator about,
	/// `docs/watch.md` §7. A job restores a renamed file to its source name before
	/// converting it, and a platform that does not report its own process's renames
	/// would leave the identity under the name the file no longer has.
	///
	/// The sighting is taken when the move happens, not when the watcher next looks:
	/// the file may be renamed again before then, and asking the filesystem later
	/// would find nothing to record.
	pub own_renames: Mutex<Vec<retype_core::correlate::Sighting>>,
	/// What clients replay from, `docs/dev/architecture/ipc.md` §7.
	pub events: Mutex<crate::daemon::events::Events>,
	/// Each directory's `.retype.toml`, read once per change rather than once per file,
	/// `docs/config.md` §12.
	pub policies: retype_core::policy::Cache,
	/// The jobs of each rename batch, so one notification answers for all of them,
	///.
	pub batches: crate::daemon::batches::Batches,
	/// How this machine is told what happened, or nothing when the user turned it off.
	pub(crate) notifier: Mutex<Option<crate::notify::Channel>>,
	/// This install's identity, recorded with every job and undo record.
	pub machine: String,
	/// Where the state lives, which is where undo records go.
	pub state_dir: PathBuf,
}

/// A conversion consumes one path and produces another. The filesystem reports
/// both, and a naive reader sees them as the *opposite* rename: `a.png` became
/// `a.webp` because the user asked, and then the watcher is told `a.webp` became
/// `a.png` because our own output appeared while our input name went away. The
/// pair is recorded so the inverse can be recognised for what it is.
#[derive(Default)]
pub struct Recent {
	pairs: Mutex<Vec<(PathBuf, PathBuf, Instant)>>,
}

/// How long a conversion's own footprints are recognised. Long enough to cover a
/// slow filesystem, short enough that a user's real rename is never ignored.
const ECHO_WINDOW: Duration = Duration::from_secs(5);

impl Recent {
	pub fn note(&self, consumed: &Path, produced: &Path) {
		if let Ok(mut pairs) = self.pairs.lock() {
			pairs.retain(|(_, _, at)| at.elapsed() < ECHO_WINDOW);
			pairs.push((consumed.to_path_buf(), produced.to_path_buf(), Instant::now()));
		}
	}

	/// Whether this fact is the echo of our own work rather than a request.
	fn is_echo(&self, source: &Path, target: &Path) -> bool {
		let Ok(pairs) = self.pairs.lock() else {
			return false;
		};

		pairs
			.iter()
			.any(|(consumed, produced, at)| *consumed == target && *produced == source && at.elapsed() < ECHO_WINDOW)
	}
}

impl Shared {
	/// Note a path this process moved itself, for the watcher to record.
	pub fn note_own_rename(&self, path: &Path) {
		let Some(sighting) = retype_core::correlate::Sighting::of(path) else {
			return;
		};

		if let Ok(mut moved) = self.own_renames.lock() {
			moved.push(sighting);
		}
	}

	/// Take the paths this process moved since the last look.
	fn take_own_renames(&self) -> Vec<retype_core::correlate::Sighting> {
		match self.own_renames.lock() {
			Ok(mut moved) => std::mem::take(&mut *moved),
			Err(_) => Vec::new(),
		}
	}

	/// A snapshot of the configuration. The `Arc` keeps the registry alive even
	/// if a reload replaces it while a job is using it.
	pub fn registry(&self) -> Arc<Registry> {
		self.registry
			.read()
			.map(|registry| Arc::clone(&registry))
			.unwrap_or_else(|_| Arc::new(Registry::builtin().expect("the built-in table loads")))
	}

	pub fn reload_registry(&self, registry: Registry) {
		let policy = registry.notify();
		if let Ok(mut current) = self.registry.write() {
			*current = Arc::new(registry);
		}
		if let Ok(mut notifier) = self.notifier.lock() {
			*notifier = match policy.enabled {
				true => Some(crate::notify::Channel::detect(policy.actions)),
				false => None,
			};
		}
	}
}

pub struct Watcher {
	stop: Arc<AtomicBool>,
	threads: Vec<JoinHandle<()>>,
}

impl Watcher {
	/// Stop the threads and wait for them. A batch in progress finishes first, so
	/// a shutdown never leaves a half-converted file.
	pub fn stop(self) {
		self.stop.store(true, Ordering::Relaxed);

		for handle in self.threads {
			let _ = handle.join();
		}
	}
}

/// Watch the registered roots, and keep watching the registration file: a root
/// added while the daemon runs is picked up within a second, and a root removed
/// stops being watched. That is what makes `retype watch` answer while a daemon is
/// already running, rather than only after a restart.
pub fn start(shared: Arc<Shared>, pool: Arc<Pool>, state_dir: &Path) -> Result<Option<Watcher>> {
	let stop = Arc::new(AtomicBool::new(false));
	let (ready, ready_rx) = std::sync::mpsc::channel();
	let supervisor = {
		let shared = Arc::clone(&shared);
		let stop = Arc::clone(&stop);
		let state_dir = state_dir.to_path_buf();

		thread::spawn(move || supervise(&shared, &pool, &state_dir, &stop, ready))
	};

	// A daemon a client can reach has to be watching already. macOS reports only
	// what happens after its event stream starts, so a rename made between the
	// endpoint appearing and the first registration round is lost rather than late.
	// The wait is bounded because a registration on a stalled mount must not hold
	// the daemon before it has even installed its signal handlers.
	let _ = ready_rx.recv_timeout(Duration::from_secs(2));

	Ok(Some(Watcher {
		stop,
		threads: vec![supervisor],
	}))
}

/// How often the registration file is re-read.
const RELOAD: Duration = Duration::from_secs(1);

fn supervise(
	shared: &Arc<Shared>,
	pool: &Arc<Pool>,
	state_dir: &Path,
	stop: &AtomicBool,
	ready: std::sync::mpsc::Sender<()>,
) {
	// One worker per root, keyed by the canonical path the registration holds.
	let mut workers: BTreeMap<PathBuf, (Arc<AtomicBool>, JoinHandle<()>)> = BTreeMap::new();
	// A missing path is retried on a slow clock so a deleted directory does not
	// fill the daemon log every second, `docs/watch.md` §1.
	let mut retry: BTreeMap<PathBuf, Instant> = BTreeMap::new();
	let mut config_stamp = config_stamp();

	while !stop.load(Ordering::Relaxed) {
		reload_config(shared, &mut config_stamp);

		let wanted = match watches::load(state_dir) {
			Ok(registrations) => registrations,
			Err(error) => {
				eprintln!("retype: cannot read watch registrations: {error}");
				// Nothing can be watched, but the daemon still has to become reachable
				// and report the error.
				let _ = ready.send(());
				thread::sleep(RELOAD);
				continue;
			}
		};

		// Stop what is no longer registered, paused, or whose path vanished. A
		// paused registration is state, and a policy-paused directory is handled
		// when a fact arrives, `docs/watch.md` §8.
		let keep: BTreeSet<PathBuf> = wanted
			.iter()
			.filter(|registration| !registration.paused && registration.path.exists())
			.map(|registration| registration.path.clone())
			.collect();

		let stale: Vec<PathBuf> = workers.keys().filter(|root| !keep.contains(*root)).cloned().collect();

		for root in stale {
			if let Some((worker_stop, handle)) = workers.remove(&root) {
				worker_stop.store(true, Ordering::Relaxed);
				let _ = handle.join();
			}
		}

		for registration in wanted {
			if registration.paused || workers.contains_key(&registration.path) {
				continue;
			}

			if retry.get(&registration.path).is_some_and(|at| *at > Instant::now()) {
				continue;
			}

			match spawn_worker(
				Arc::clone(shared),
				Arc::clone(pool),
				&registration.path,
				registration.recursive,
			) {
				Ok((worker_stop, handle)) => {
					retry.remove(&registration.path);
					workers.insert(registration.path.clone(), (worker_stop, handle));
				}
				Err(error) => {
					eprintln!("retype: cannot watch {}: {error}", registration.path.display());
					retry.insert(registration.path.clone(), Instant::now() + Duration::from_secs(30));
				}
			}
		}

		// Everything registered by now has a live watcher, so the daemon may say it
		// is ready. Later rounds have nobody waiting, and the send is ignored.
		let _ = ready.send(());

		thread::sleep(RELOAD);
	}

	for (_, (worker_stop, handle)) in workers {
		worker_stop.store(true, Ordering::Relaxed);
		let _ = handle.join();
	}
}

/// The user configuration's modification stamp, so an edit takes effect without
/// restarting the daemon, `docs/dev/architecture/ipc.md` §3.
fn config_stamp() -> Option<std::time::SystemTime> {
	crate::config::user_path().and_then(|path| std::fs::metadata(path).and_then(|meta| meta.modified()).ok())
}

/// Reload the last good configuration. A broken edit keeps what is running and
/// says so, rather than taking the daemon down over a typo.
fn reload_config(shared: &Arc<Shared>, stamp: &mut Option<std::time::SystemTime>) {
	let now = config_stamp();
	if now == *stamp {
		return;
	}
	*stamp = now;

	match crate::config::load() {
		Ok(registry) => {
			crate::config::report_warnings(&registry);
			shared.reload_registry(registry);
			crate::verbose::note("configuration reloaded");
		}
		Err(error) => eprintln!("retype: configuration reload failed; keeping the last good one: {error}"),
	}
}

fn spawn_worker(
	shared: Arc<Shared>,
	pool: Arc<Pool>,
	root: &Path,
	recursive: bool,
) -> std::result::Result<(Arc<AtomicBool>, JoinHandle<()>), retype_watch::notify::Error> {
	let (watcher, receiver) = retype_watch::watch_mode(root, recursive)?;
	let stop = Arc::new(AtomicBool::new(false));
	let window = Window::default();
	let watched_root = root.to_path_buf();
	let worker_stop = Arc::clone(&stop);

	let handle = thread::spawn(move || {
		// The watcher lives as long as the thread, which is what keeps the events
		// arriving.
		let _watcher = watcher;
		crate::verbose::detail(format!("watching {}", watched_root.display()));
		let mut correlator = Correlator::new(256);

		// Prime before the first batch is decided: a file that existed before the
		// daemon started has no identity otherwise, and on macOS identity is the
		// only correlation mechanism, `docs/watch.md` §3 and `docs/dev/measurement/events.md` §3.
		let registry = shared.registry();
		let primed = retype_watch::prime(
			&mut correlator,
			&watched_root,
			recursive,
			|path| !retype_core::convert::is_temp_file(path) && registry.watch().considers(&watched_root, path),
			|| worker_stop.load(Ordering::Relaxed),
		);
		shared.primed.fetch_add(primed as u64, Ordering::Relaxed);
		crate::verbose::note(format!("primed {primed} file(s) under {}", watched_root.display()));
		// Renames waiting out a directory's own debounce, by directory.
		let mut held: std::collections::BTreeMap<PathBuf, Held> = std::collections::BTreeMap::new();

		while !worker_stop.load(Ordering::Relaxed) {
			// A directory that asked for a longer debounce is answered when its own clock
			// says so, before this pass waits for anything else.
			release(&shared, &pool, &watched_root, &mut held);

			let batch = retype_watch::collect(&receiver, less_than(&held, window));

			if batch.is_empty() {
				continue;
			}

			if crate::verbose::level() >= 2 {
				// The daemon's own state is most of the traffic and none of the
				// question, so the line names everything else, whole.
				let mut paths: Vec<String> = batch
					.events
					.iter()
					.flat_map(|event| event.paths.iter())
					.filter(|path| !path.starts_with(&shared.state_dir))
					.map(|path| path.display().to_string())
					.collect();
				paths.dedup();

				crate::verbose::detail(format!(
					"batch: {} event(s), {} outside state: {}",
					batch.events.len(),
					paths.len(),
					paths.join(", ")
				));
			}

			let observed: u64 = batch.events.iter().map(|event| event.paths.len() as u64).sum();

			// One batch per window, so the jobs parked in one tick can be answered
			// together, `docs/deferral.md` §10.
			let parked_batch = retype_core::job::JobId::now().ok().map(|id| id.as_str().to_string());

			// Our own renames are carried into the batch: a platform may not report a
			// rename the process performed, and the file may have moved again by the
			// time the batch is decided.
			let own = shared.take_own_renames();
			let facts = retype_watch::feed_with(&mut correlator, &batch, &own);

			for fact in &facts {
				crate::verbose::detail(format!(
					"rename {} -> {}",
					fact.source.display(),
					fact.target.display()
				));
			}

			// After `feed`, so a caller that sees the count has had its file recorded:
			// the sighting is what a later rename correlates against.
			shared.observed.fetch_add(observed, Ordering::Relaxed);

			for fact in facts {
				// A directory may ask for more settled time than the window gives, which is
				// what `debounce_ms` is for: an application that saves by renaming twice
				// should be converted once, under the name it finally chose.
				let directory = fact.target.parent().unwrap_or(&watched_root);

				let debounce = match shared.policies.policy(&watched_root, directory).as_ref() {
					Ok(policy) => policy.debounce_ms.unwrap_or_default(),
					Err(problem) => {
						eprintln!("retype: {problem}");
						continue;
					}
				};

				let wait = std::time::Duration::from_millis(debounce);

				if wait > window.first_wait {
					held.entry(directory.to_path_buf()).or_default().hold(fact, wait);
					continue;
				}

				decide(&shared, &pool, &watched_root, fact, parked_batch.as_deref());
			}
		}

		crate::verbose::detail(format!("stopped watching {}", watched_root.display()));
	});

	Ok((stop, handle))
}

/// One rename, decided: hold it, park it, or run it.
///
/// The same path serves a fact the window has just produced and a fact a directory's
/// own debounce has just released, so a policy cannot be honoured in one place and
/// forgotten in the other.
fn decide(
	shared: &Shared,
	pool: &Pool,
	watched_root: &Path,
	fact: retype_core::correlate::RenameFact,
	parked_batch: Option<&str>,
) {
	if retype_core::convert::is_temp_file(&fact.source) || retype_core::convert::is_temp_file(&fact.target) {
		return;
	}

	if shared.recent.is_echo(&fact.source, &fact.target) {
		return;
	}

	// What the daemon considers at all comes before a directory's own
	// policy: a `.git` directory cannot be opted back in by the file inside
	// it, `docs/watch.md` §4.
	// Both names a rename has, because they describe the same bytes: a glob
	// that names the source is about the file being converted.
	if !shared.registry().watch().considers(watched_root, &fact.target)
		|| !shared.registry().watch().considers(watched_root, &fact.source)
	{
		crate::verbose::detail(format!("{} is not considered", fact.target.display()));
		return;
	}

	// A placeholder looks like a file and behaves like a download, so it is passed over
	// rather than read, `docs/watch.md` §5.
	if retype_core::synced::dataless(&fact.target) {
		shared.placeholders.fetch_add(1, Ordering::Relaxed);
		crate::verbose::detail(format!("{} is a placeholder, not a copy", fact.target.display()));
		return;
	}

	// A rename is a fast gesture and a forty gigabyte video is not, so an
	// oversized source is passed over rather than converted, `docs/watch.md`
	// §6.
	let size = std::fs::metadata(&fact.target).map(|meta| meta.len()).unwrap_or(0);

	if !shared.registry().watch().admits(size) {
		shared.skipped.fetch_add(1, Ordering::Relaxed);
		eprintln!(
			"retype: {} is {} and the automatic limit is {}; `retype convert` converts it",
			fact.target.display(),
			retype_core::print::size(size),
			retype_core::print::size(shared.registry().watch().max_source_size),
		);
		return;
	}

	// The directory has a say before anything else happens: a paused
	// directory converts nothing, and `patterns` decides which files are
	// considered at all, `docs/config.md` §12.
	let directory = fact.target.parent().unwrap_or(watched_root);
	let cached = shared.policies.policy(watched_root, directory);

	let policy = match cached.as_ref() {
		Ok(policy) => policy,
		Err(problem) => {
			eprintln!("retype: {problem}");
			return;
		}
	};

	if policy.paused {
		crate::verbose::detail(format!("{} is paused by its policy", directory.display()));
		return;
	}

	let name = fact
		.source
		.file_name()
		.and_then(|name| name.to_str())
		.unwrap_or_default();

	let relative = fact.source.strip_prefix(watched_root).unwrap_or(&fact.source);

	if !policy.allows(name, relative) {
		return;
	}

	let Ok(id) = retype_core::job::JobId::now() else {
		return;
	};

	// A rename chain that spans two batches: the file being renamed is
	// what an earlier job is writing, so the earlier job is spent and the
	// chain becomes one conversion, `docs/dev/architecture/state.md` §5.3.
	// The rename just put the bytes at the target, so that is where they
	// are. What the chain changes is the *source* name: after two renames
	// the file is not what its intermediate name claimed, it is what the
	// job that was overtaken was reading, `docs/dev/architecture/state.md` §5.3.
	let earlier = producers(shared, pool, &fact.source);

	crate::verbose::detail(format!(
		"{} job(s) hold {} while {} is decided",
		earlier.len(),
		fact.source.display(),
		fact.target.display()
	));

	let source = match earlier.is_empty() {
		true => fact.source.clone(),
		false => supersede(shared, pool, &earlier),
	};

	// Registered before anything is published, because the first thing
	// published may be the last thing this batch does: a parked job settles
	// immediately.
	if let Some(batch) = parked_batch {
		shared.batches.open(batch, &id);
	}

	// A later rename replaces a parked job for the same file.
	crate::daemon::park::superseded(shared, &source);

	match crate::daemon::park::decide(
		shared,
		pool,
		watched_root,
		&source,
		&fact.target,
		&id,
		parked_batch,
		policy,
	) {
		crate::daemon::park::Verdict::Run => {}
		crate::daemon::park::Verdict::Parked | crate::daemon::park::Verdict::Repaired => return,
		crate::daemon::park::Verdict::Ignore => {
			// Not a job at all, so the batch must not wait for it.
			if let Some(batch) = parked_batch {
				shared.batches.forget(batch, &id);
			}

			return;
		}
	}

	let work = crate::daemon::queue::Work {
		id,
		kind: crate::daemon::queue::Kind::Rename {
			root: watched_root.to_path_buf(),
			bytes: fact.target.clone(),
			source,
			target: fact.target,
		},
	};

	if !pool.submit(work) {
		shared.dropped.fetch_add(1, Ordering::Relaxed);
		eprintln!(
			"retype: conversion is paused for {}: {} jobs are waiting",
			watched_root.display(),
			crate::daemon::queue::CAPACITY
		);
	}
}

/// Facts held for a directory that asked for more settled time.
#[derive(Default)]
struct Held {
	facts: Vec<retype_core::correlate::RenameFact>,
	due: Option<std::time::Instant>,
}

impl Held {
	fn hold(&mut self, fact: retype_core::correlate::RenameFact, wait: std::time::Duration) {
		self.facts.push(fact);
		// The clock restarts on every event: settling means quiet.
		self.due = Some(std::time::Instant::now() + wait);
	}
}

/// Answer every held directory whose quiet period has passed.
///
/// The facts are coalesced before they are decided, so a rename chain that arrived
/// inside the hold is one job from the first name to the last, the same collapse a
/// single window gets, `docs/dev/architecture/state.md` §5.3.
fn release(shared: &Shared, pool: &Pool, watched_root: &Path, held: &mut std::collections::BTreeMap<PathBuf, Held>) {
	let now = std::time::Instant::now();

	let due: Vec<PathBuf> = held
		.iter()
		.filter(|(_, held)| held.due.is_some_and(|due| due <= now))
		.map(|(directory, _)| directory.clone())
		.collect();

	for directory in due {
		let Some(entry) = held.remove(&directory) else {
			continue;
		};

		let batch = retype_core::job::JobId::now().ok().map(|id| id.as_str().to_string());

		for fact in retype_core::correlate::coalesce(entry.facts) {
			decide(shared, pool, watched_root, fact, batch.as_deref());
		}
	}
}

/// The window to collect with, shortened so a held directory is not waited on past its
/// own deadline.
fn less_than(held: &std::collections::BTreeMap<PathBuf, Held>, window: Window) -> Window {
	let soonest = held.values().filter_map(|held| held.due).min();

	let Some(due) = soonest else {
		return window;
	};

	let left = due.saturating_duration_since(std::time::Instant::now());

	Window {
		first_wait: left.min(window.first_wait),
		quiet: left.min(window.quiet),
		cap: window.cap,
	}
}

/// Every job still in flight that this path belongs to.
///
/// Two of them are worth matching, and the second is the common one. A rename of
/// what a job is writing is the user changing their mind about the result. A
/// rename of the *source* name is the same decision taken a moment later, and it
/// is the one people make in practice: while a job converts, the file shows its
/// source name again, because that is the name its bytes belong under,
/// `docs/dev/architecture/state.md` §5.4.
fn producers(shared: &Shared, pool: &Pool, path: &Path) -> Vec<Job> {
	let Ok(jobs) = shared.jobs.lock() else {
		return Vec::new();
	};

	let in_flight = pool.ids();

	jobs.iter()
		.filter(|job| {
			matches!(job.state, JobState::Queued | JobState::Running | JobState::Partial)
				&& in_flight.contains(&job.id)
				&& (job.source.path == path || job.outputs.iter().any(|output| output.path == path))
		})
		.cloned()
		.collect()
}

/// Retire the job a rename overtook, and answer with the name the bytes are
/// actually under plus a fresh id for the replacement.
///
/// The undo record goes with it. It is a name for bytes that are still where the
/// earlier job left them, so the replacement records its own when it starts; the
/// alternative is rewriting the record's id and intent for no gain.
fn supersede(shared: &Shared, pool: &Pool, earlier: &[Job]) -> PathBuf {
	for job in earlier {
		let (_, queued) = pool.supersede(&job.id);

		// A queued job never started, so nothing will write its terminal record for
		// it. A running one notices the flag and records itself.
		if let Some(work) = queued {
			eprintln!("retype: {} was replaced by a later rename before it started", job.id);
			retire(shared, &work, JobState::Superseded);
		}

		let _ = retype_core::undo::Store::open(&shared.state_dir).remove(&job.id);
	}

	// The name the bytes belong under is the one the earliest job was reading: a
	// chain of renames says nothing about the format, and the original name does.
	earlier.first().map(|job| job.source.path.clone()).unwrap_or_default()
}

/// Record a job that will not run, so its id stays answerable.
fn retire(shared: &Shared, work: &crate::daemon::queue::Work, state: JobState) {
	use crate::daemon::queue::Kind;

	let (source, target, consume_source) = match &work.kind {
		Kind::Rename { source, target, .. } => (source.clone(), target.clone(), true),
		Kind::Immediate(request) => (request.input.clone(), request.input.clone(), false),
		Kind::Deferred { source, .. } => (source.clone(), source.clone(), true),
	};

	let mut job = Job {
		id: work.id.clone(),
		created: now_ms(),
		updated: now_ms(),
		source: Source {
			path: source,
			format: String::new(),
			size: 0,
			mtime_ns: 0,
		},
		intent: None,
		outputs: vec![Output::pending(
			target,
			String::new(),
			retype_core::job::Tier::Converted,
		)],
		state,
		backend: None,
		engine: None,
		consume_source: true,
		machine: shared.machine.clone(),
		error: Some(JobError {
			code: Code::JobSuperseded.to_string(),
			message: "replaced by a later rename before it started".to_string(),
		}),
		parked: None,
	};

	job.outputs[0].state = OutputState::Skipped;
	job.consume_source = consume_source;
	publish(shared, &job);
}

/// Where a job's bytes are, and what has to happen to them afterwards.
enum Input {
	/// A rename put them at `renamed`: relabel before reading, then consume or keep,
	///.
	Renamed { renamed: PathBuf },
	/// A parked job whose bytes are already under the source name,
	/// `docs/deferral.md` §9. Consumed on commit, like any rename-derived job.
	Named,
}

/// What a job will do, worked out once and then run.
struct Prepared {
	stem: String,
	format: String,
	resolved: Resolved,
	plans: Vec<Plan>,
}

/// Turn one rename into jobs, or explain in the log why it is not one.
#[allow(clippy::too_many_arguments)]
pub fn convert(
	shared: &Shared,
	root: &Path,
	bytes: &Path,
	source: &Path,
	target: &Path,
	id: &retype_core::job::JobId,
	cancellation: &Cancellation,
) {
	let Some(prepared) = prepare(shared, root, source, target) else {
		return;
	};

	let input = Input::Renamed {
		renamed: bytes.to_path_buf(),
	};

	// A rename's collisions are its own concern gap table: only a job a
	// person re-runs can be told to overwrite.
	run(shared, source, target, prepared, input, false, id, cancellation);
}

/// Run a job whose time has come, `docs/deferral.md` §7: the intent is the one the
/// journal recorded and the bytes are already under the source name.
pub fn convert_deferred(
	shared: &Shared,
	root: &Path,
	source: &Path,
	intent: &str,
	force: bool,
	id: &retype_core::job::JobId,
	cancellation: &Cancellation,
) {
	let Some(prepared) = prepare_from(shared, root, source, intent) else {
		return;
	};

	run(shared, source, source, prepared, Input::Named, force, id, cancellation);
}

/// Read a rename as a job, or say why it is not one.
fn prepare(shared: &Shared, root: &Path, source: &Path, target: &Path) -> Option<Prepared> {
	let (source_name, target_name) = (
		source.file_name().and_then(|name| name.to_str())?,
		target.file_name().and_then(|name| name.to_str())?,
	);

	let registry = shared.registry();
	let Some((stem, format)) = registry.split(source_name) else {
		// Not a file whose format we know, so not ours to act on.
		return None;
	};

	let parsed = match resolve::intent_of(&registry, source_name, target_name) {
		Ok(Some(intent)) => intent,
		Ok(None) => return None,
		Err(problems) => {
			eprintln!("retype: {}: {problems}", target.display());
			return None;
		}
	};

	build(shared, root, stem, format, parsed, target)
}

/// Read a recorded intent as a job, which is what a parked job carries.
fn prepare_from(shared: &Shared, root: &Path, source: &Path, text: &str) -> Option<Prepared> {
	let source_name = source.file_name().and_then(|name| name.to_str())?;
	let registry = shared.registry();
	let (stem, format) = registry.split(source_name)?;

	let parsed = match intent::parse(text) {
		Ok(parsed) => parsed,
		Err(problems) => {
			eprintln!("retype: {}: {problems}", source.display());
			return None;
		}
	};

	build(shared, root, stem, format, parsed, source)
}

/// Resolve and compile, which is the same work whether the request came from a name
/// or from a journal record.
fn build(
	shared: &Shared,
	root: &Path,
	stem: &str,
	format: &str,
	parsed: intent::Intent,
	reported: &Path,
) -> Option<Prepared> {
	let registry = shared.registry();
	let mut resolved = match resolve::resolve(
		&parsed,
		Origin {
			format,
			is_directory: false,
		},
		&registry,
	) {
		Ok(resolved) => resolved,
		Err(problems) => {
			eprintln!("retype: {}: {problems}", reported.display());
			return None;
		}
	};

	let plans = match plan::compile(&registry, &retype_core::tools::Machine, &resolved) {
		Ok(plans) => plans,
		Err(problems) => {
			eprintln!("retype: {}: {problems}", reported.display());
			return None;
		}
	};

	// The directory has the last word on what this intent may ask for, `docs/config.md`
	// §12. A refused operation is reported and the job does not run: the file keeps the
	// name its bytes fit.
	if let Ok(policy) = shared.policies.policy(root, reported.parent().unwrap_or(root)).as_ref() {
		let problems = policy.apply(&mut resolved);

		if !problems.is_empty() {
			eprintln!("retype: {}: {problems}", reported.display());
			return None;
		}
	}

	Some(Prepared {
		stem: stem.to_string(),
		format: format.to_string(),
		resolved,
		plans,
	})
}

/// Convert, record what happened, and dispose of the source.
#[allow(clippy::too_many_arguments)]
fn run(
	shared: &Shared,
	source: &Path,
	target: &Path,
	prepared: Prepared,
	input: Input,
	force: bool,
	id: &retype_core::job::JobId,
	cancellation: &Cancellation,
) {
	let Prepared {
		stem,
		format,
		resolved,
		plans,
	} = prepared;

	let id = id.clone();
	let now = now_ms();
	let mut outputs = Vec::new();

	running(shared, &id, source, &format, target, &resolved);

	// The rename names the request; the file is called what the tokens say, the
	// same way `convert` derives its paths.
	let derived = retype_core::output::derive(&stem, &format, &resolved.groups);

	// Keep the bytes before touching them: an undo has to be possible even if this
	// process dies mid-conversion. Retention and quota are policy, so a machine that
	// does not keep records, or has no room for this one, does not pay for it and
	// the conversion runs anyway.
	let store = retype_core::undo::Store::open(&shared.state_dir);
	let registry = shared.registry();
	let policy = registry.undo();
	let bytes = match &input {
		Input::Renamed { renamed } => renamed.clone(),
		Input::Named => source.to_path_buf(),
	};
	let mut disposal = execute::Disposal::Remove;
	let undo = if !policy.enabled {
		None
	} else {
		match store.make_room(&bytes, policy) {
			Ok(retype_core::undo::Room::Kept { dropped }) => {
				for id in &dropped {
					eprintln!("retype: dropped undo record {id} to stay within the quota");
				}

				Some(
					store.save(retype_core::undo::Subject {
						id: id.clone(),
						intent: print::intent(&resolved, &shared.registry()),
						machine: shared.machine.clone(),
						bytes: bytes.clone(),
						// The path the bytes belong to, which is not `root` plus the
						// name: a file inside a subdirectory would be restored one
						// directory too high.
						source_path: source.to_path_buf(),
						source_ext: format.clone(),
						consume_source: true,
						outputs: derived
							.iter()
							.map(|derived| retype_core::undo::Output {
								path: target.parent().unwrap_or(Path::new("")).join(&derived.path),
								ext: derived.format.clone(),
							})
							.collect(),
					}),
				)
			}
			Ok(retype_core::undo::Room::NoRoom(reason)) => {
				// Nothing can be kept, so the source goes where the user can still find
				// it. A quota that is full is not a quota problem, so nothing is evicted
				// for a record that will not exist.
				disposal = match reason {
					retype_core::undo::NoRoom::TooLarge => execute::Disposal::Trash,
					retype_core::undo::NoRoom::Policy => execute::Disposal::Remove,
				};

				eprintln!("retype: {}: no undo record: {}", target.display(), reason.reason());
				None
			}
			Err(error) => {
				eprintln!("retype: cannot measure {} for undo: {error}", target.display());
				None
			}
		}
	};

	// One job, one source: the bytes are relabelled and then read for every output,
	//. Opening the source per output moved the bytes for the first one and
	// left the rest with nothing to read.
	// Whether this job came from a person rather than from a rename, decided before the
	// input is consumed below.
	let named = matches!(input, Input::Named);

	let mut renamed = match input {
		Input::Renamed { renamed } => match execute::Rename::open(&renamed, &stem, &format) {
			Ok(renamed) => {
				// The bytes are back under the source name because this process moved them.
				// The watcher records that, so a platform that does not report its own
				// process's renames does not lose the identity with the old name.
				shared.note_own_rename(source);
				Some(renamed)
			}
			Err(error) => {
				eprintln!("retype: {}: {error}", target.display());
				store.remove(&id).ok();

				// The file the job was about to read is gone, usually because a later
				// rename moved it. A job must not stay `running` for that: say which
				// happened, and let the cancellation decide whether it was overtaken.
				let (state, output_state) = match cancellation.is_superseded() {
					true => (JobState::Superseded, OutputState::Skipped),
					false => (JobState::Failed, OutputState::Failed),
				};

				let mut outputs = Vec::new();
				if let Some(plan) = plans.first() {
					outputs.push(record(
						target,
						plan,
						output_state,
						None,
						None,
						Some((error.code(), error.to_string())),
					));
				}

				publish(shared, &settled(shared, &id, now, source, &format, &resolved, &plans, outputs, state));
				return;
			}
		},
		Input::Named => None,
	};

	let read_from = match &renamed {
		Some(renamed) => renamed.input().to_path_buf(),
		None => source.to_path_buf(),
	};

	for plan in &plans {
		// Plans carry their group, because a kept source has one and no plan.
		let Some(derived) = derived.iter().find(|derived| derived.group == plan.group) else {
			continue;
		};

		let output = target.parent().unwrap_or(Path::new("")).join(&derived.path);

		// A file that was not this job's source is not overwritten by a rename. The
		// rename target itself is the source bytes under their new name, so that one
		// path is the job's own. A deferred job is stricter: anything that appeared
		// while it waited belongs to the person who put it there, `docs/deferral.md` §8.
		let collision = output.exists() && output != read_from && (named || output != target);

		if !force && collision {
			let hint = match named {
				true => "`jobs resolve --now --force` replaces it".to_string(),
				false => "rename the file to a name that derives a free path".to_string(),
			};

			outputs.push(record(
				&output,
				plan,
				OutputState::Failed,
				None,
				None,
				Some((Code::IoExists, format!("{} already exists; {hint}", output.display()))),
			));
			continue;
		}

		shared.recent.note(source, &output);

		let log = crate::state::log_path(&shared.state_dir, &id);

		match execute::run_logged(plan, &read_from, &output, cancellation, Some(&log)) {
			Ok(report) => outputs.push(record(
				&output,
				plan,
				OutputState::Done,
				Some(report.bytes),
				match report.stderr.trim().is_empty() {
					true => None,
					false => Some(report.stderr.clone()),
				},
				None,
			)),
			Err(error) => {
				// A superseded job did not fail: the work was overtaken, so its output
				// is skipped rather than failed, `docs/dev/architecture/state.md` §5.3.
				// The code the failure came with, not a generic one: an archive that
				// was refused for its names is a diagnosis, and a caller's exit code
				// should say so rather than "the tool failed".
				let (state, message, code) = match cancellation.is_superseded() {
					true => (
						OutputState::Skipped,
						"replaced by a later rename".to_string(),
						Code::JobSuperseded,
					),
					false => (OutputState::Failed, error.to_string(), error.code()),
				};

				if state == OutputState::Failed {
					eprintln!("retype: {}: {error}", output.display());
				}

				outputs.push(record(&output, plan, state, None, None, Some((code, message))));
			}
		}
	}

	// A cancelled job is its own outcome: nothing was converted and nothing is
	// wrong with the file, `docs/dev/architecture/state.md` §4.
	let cancelled = cancellation.is_cancelled()
		|| outputs.iter().any(|output| {
			output
				.error
				.as_ref()
				.is_some_and(|error| error.code == Code::JobCancelled.to_string())
		});

	let state = match (
		cancelled,
		outputs.iter().all(|output| output.state == OutputState::Done),
	) {
		(true, _) if cancellation.is_superseded() => JobState::Superseded,
		(true, _) => JobState::Cancelled,
		(false, true) => JobState::Done,
		(false, false) if outputs.iter().any(|output| output.state == OutputState::Done) => JobState::Partial,
		(false, false) => JobState::Failed,
	};

	if let Some(Err(error)) = &undo {
		eprintln!("retype: cannot keep {} for undo: {error}", target.display());
	}

	// The job either used the source up or left it alone: a job with any output
	// consumed its source, and one that produced nothing puts the source name back so
	// no path keeps bytes its name denies.
	match state {
		JobState::Done | JobState::Partial => {
			let disposed = match renamed.take() {
				Some(renamed) => renamed.consume(disposal),
				None => execute::consume(source, disposal),
			};

			if let Err(error) = disposed {
				eprintln!("retype: cannot dispose of {}: {error}", target.display());
			}
		}
		_ => {
			if let Some(renamed) = renamed {
				renamed.keep();
			}
		}
	}

	let job = settled(shared, &id, now, source, &format, &resolved, &plans, outputs, state);

	publish(shared, &job);
	mark(shared, &job.outputs, job.intent.as_deref().unwrap_or_default());

	// Nothing was produced, so there is nothing to undo: the source is where it
	// always was and the record would only describe a file that never changed.
	if state != JobState::Done && state != JobState::Partial {
		let _ = store.remove(&id);
	}
}

/// A job's last word, whether it got as far as an output or not.
///
/// Every path out of a job goes through this, so a job that could not start leaves a
/// terminal record rather than a `running` one nothing will ever finish.
#[allow(clippy::too_many_arguments)]
fn settled(
	shared: &Shared,
	id: &retype_core::job::JobId,
	now: i64,
	source: &Path,
	format: &str,
	resolved: &resolve::Resolved,
	plans: &[Plan],
	outputs: Vec<Output>,
	state: JobState,
) -> Job {
	Job {
		id: id.clone(),
		created: now,
		updated: now_ms(),
		source: Source {
			path: source.to_path_buf(),
			format: format.to_string(),
			size: 0,
			mtime_ns: 0,
		},
		intent: Some(print::intent(resolved, &shared.registry())),
		outputs,
		state,
		backend: plans.first().map(|plan| plan.backend.name().to_string()),
		engine: plans
			.first()
			.and_then(|plan| plan.engine.clone())
			.map(|(name, candidate)| retype_core::job::Engine { name, candidate }),
		machine: shared.machine.clone(),
		error: None,
		parked: carried(shared, id),
		consume_source: true,
	}
}

/// The reason a job waited, for the record that follows it: a job that was parked or
/// repaired still says why after it has run, and only its deadline is gone,
/// `docs/deferral.md` §11.
pub fn carried(shared: &Shared, id: &retype_core::job::JobId) -> Option<retype_core::job::Parked> {
	let mut parked = shared
		.jobs
		.lock()
		.ok()?
		.iter()
		.find(|job| job.id == *id)?
		.parked
		.clone()?;

	parked.deadline = None;
	Some(parked)
}

/// Write the intent onto each output that was produced, `docs/provenance.md` §6.2.
///
/// Best effort: a filesystem that refuses attributes, or a policy that turns them
/// off, leaves the journal as the only record and changes nothing else.
pub fn mark(shared: &Shared, outputs: &[Output], intent: &str) {
	if !shared.registry().provenance().xattr {
		return;
	}

	for output in outputs.iter().filter(|output| output.state == OutputState::Done) {
		if let Err(error) = retype_core::xattr::write_intent(&output.path, intent) {
			eprintln!("retype: cannot record provenance on {}: {error}", output.path.display());
		}
	}
}

/// Put a snapshot in the journal and in the list `jobs.list` answers from.
///
/// The journal is the record; the list is the same records, kept in memory so a
/// client does not read a file per request. A later snapshot for the same id
/// replaces the earlier one in both.
pub fn publish(shared: &Shared, job: &Job) {
	if let Ok(mut journal) = shared.journal.lock() {
		if let Err(error) = journal.append(job) {
			eprintln!("retype: cannot journal {}: {error}", job.id);
		}
	}

	// One event per snapshot, so a client polling `events` sees the same
	// transitions the journal records, `docs/dev/architecture/ipc.md` §7.
	if let Ok(mut events) = shared.events.lock() {
		events.push(job);
	}

	if let Ok(mut jobs) = shared.jobs.lock() {
		match jobs.iter_mut().find(|existing| existing.id == job.id) {
			Some(existing) => *existing = job.clone(),
			None => jobs.push(job.clone()),
		}
	}

	// The last job of a batch is the one that speaks for it.
	if is_settled(job.state) {
		if let Some(ids) = shared.batches.finish(&job.id) {
			announce(shared, &ids);
		}
	}
}

/// Whether a job has reached a state a notification could describe. `running` and
/// `queued` have not.
fn is_settled(state: JobState) -> bool {
	!matches!(state, JobState::Queued | JobState::Running)
}

/// Say what a finished batch did, without holding a lock while saying it.
fn announce(shared: &Shared, ids: &[JobId]) {
	let Some(channel) = shared.notifier.lock().ok().and_then(|notifier| *notifier) else {
		return;
	};

	let jobs: Vec<Job> = match shared.jobs.lock() {
		Ok(jobs) => jobs.iter().filter(|job| ids.contains(&job.id)).cloned().collect(),
		Err(_) => return,
	};

	let Some(message) = crate::notify::Message::of(&jobs, shared.registry().undo().enabled) else {
		return;
	};

	// Undo runs in the same thread as the wait for the button, and the state directory
	// is all it needs: the journal is a cache of history, the records are the truth.
	let state_dir = shared.state_dir.clone();

	std::thread::spawn(move || {
		channel.send(&message, |ids| {
			for id in ids {
				match retype_core::undo::Store::open(&state_dir).restore(id) {
					Ok(_) => eprintln!("retype: undid {id}"),
					Err(error) => eprintln!("retype: cannot undo {id}: {error}"),
				}
			}
		});
	});
}

/// The record a job writes before it starts, so a client can see a job that is
/// running rather than only one that finished, `docs/dev/architecture/state.md` §3.
fn running(
	shared: &Shared,
	id: &retype_core::job::JobId,
	source: &Path,
	source_format: &str,
	target: &Path,
	resolved: &resolve::Resolved,
) {
	let metadata = std::fs::metadata(source).ok();
	let mtime_ns = metadata
		.as_ref()
		.and_then(|metadata| metadata.modified().ok())
		.and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
		.map(|elapsed| elapsed.as_nanos() as i64)
		.unwrap_or_default();

	let job = Job {
		id: id.clone(),
		created: now_ms(),
		updated: now_ms(),
		source: Source {
			path: source.to_path_buf(),
			format: source_format.to_string(),
			size: metadata.map(|metadata| metadata.len()).unwrap_or_default(),
			mtime_ns,
		},
		intent: Some(print::intent(resolved, &shared.registry())),
		outputs: vec![Output {
			path: target.to_path_buf(),
			format: String::new(),
			state: OutputState::Running,
			bytes: None,
			tier: retype_core::job::Tier::Converted,
			identity: None,
			stderr: None,
			error: None,
		}],
		state: JobState::Running,
		backend: None,
		engine: None,
		machine: shared.machine.clone(),
		error: None,
		// A parked job keeps the reason it waited when its running snapshot
		// replaces the parked one.
		parked: carried(shared, id),
		consume_source: true,
	};

	// One publish path for every transition: a deferred job already has a record,
	// and pushing a second one would leave `jobs.get` answering from the old
	// `running` snapshot after the job had finished.
	publish(shared, &job);
}

fn record(
	path: &Path,
	plan: &Plan,
	state: OutputState,
	bytes: Option<u64>,
	stderr: Option<String>,
	error: Option<(Code, String)>,
) -> Output {
	Output {
		path: path.to_path_buf(),
		format: plan.format.clone(),
		state,
		bytes,
		tier: plan.tier,
		// Recorded on success only: an identity for a file that was never written
		// would tell a resume to skip work that was never done.
		identity: match state {
			OutputState::Done => retype_core::job::FileIdentity::of(path),
			_ => None,
		},
		stderr,
		error: error.map(|(code, message)| JobError {
			code: code.to_string(),
			message,
		}),
	}
}

/// Every registered root, for a caller that wants to report them.
pub fn roots(state_dir: &Path) -> Result<Vec<PathBuf>> {
	Ok(watches::load(state_dir)?
		.into_iter()
		.map(|registration| registration.path)
		.collect())
}
