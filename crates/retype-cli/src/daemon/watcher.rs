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

use std::collections::BTreeMap;
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

#[cfg(test)]
#[path = "watcher_tests.rs"]
mod tests;

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
	/// Serializes parked-job decisions, timer admission and cancellation.
	pub(crate) parked_admission: Mutex<()>,
	/// What clients replay from, `docs/dev/architecture/ipc.md` §7.
	pub events: Mutex<crate::daemon::events::Events>,
	/// Installed watcher state, distinct from the persisted registrations.
	pub(crate) watch_health: crate::daemon::watch_health::WatchHealth,
	/// Each directory's `.retype.toml`, read once per change rather than once per file,
	/// `docs/config.md` §12.
	pub policies: retype_core::policy::Cache,
	/// The jobs of each rename batch, so one notification answers for all of them.
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
	pub(crate) fn lock_parked_admission(&self) -> retype_core::error::Result<std::sync::MutexGuard<'_, ()>> {
		self.parked_admission
			.lock()
			.map_err(|_| retype_core::error::Error::Io(std::io::Error::other("parked admission lock poisoned")))
	}

	/// Note a path this process moved itself, for the watcher to record.
	pub fn note_own_rename(&self, path: &Path) {
		let Some(sighting) = retype_core::correlate::Sighting::of(path) else {
			return;
		};

		if let Ok(mut moved) = self.own_renames.lock() {
			moved.push(sighting);
		}
	}

	/// Put back the sightings whose rename the watcher has not seen yet.
	fn restore_own_renames(&self, own: Vec<retype_core::correlate::Sighting>) {
		if own.is_empty() {
			return;
		}

		if let Ok(mut moved) = self.own_renames.lock() {
			moved.extend(own);
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

		thread::spawn(move || {
			let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
				supervise(&shared, &pool, &state_dir, &stop, ready);
			}));
			if result.is_err() {
				eprintln!("retype: watcher supervisor stopped unexpectedly");
				shared.watch_health.supervisor_failed();
			}
		})
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
	let mut workers: BTreeMap<PathBuf, Worker> = BTreeMap::new();
	// A missing path is retried on a slow clock so a deleted directory does not
	// fill the daemon log every second, `docs/watch.md` §1.
	let mut retry: BTreeMap<PathBuf, Instant> = BTreeMap::new();
	let mut config_stamp = config_stamp();

	while !stop.load(Ordering::Relaxed) {
		reload_config(shared, &mut config_stamp);

		let wanted = match watches::load(state_dir) {
			Ok(registrations) => {
				shared.watch_health.set_registry_error(None);
				registrations
			}
			Err(error) => {
				let message = format!("cannot read watch registrations: {error}");
				eprintln!("retype: {message}");
				shared.watch_health.set_registry_error(Some(message));
				shared.watch_health.clear_roots();
				stop_workers(&mut workers);
				retry.clear();
				let _ = ready.send(());
				thread::sleep(RELOAD);
				continue;
			}
		};

		refresh_workers(shared, pool, wanted, &mut workers, &mut retry);

		// Everything registered by now has a live watcher, so the daemon may say it
		// is ready. Later rounds have nobody waiting, and the send is ignored.
		let _ = ready.send(());

		thread::sleep(RELOAD);
	}

	shared.watch_health.clear_roots();
	stop_workers(&mut workers);
}

struct Worker {
	stop: Arc<AtomicBool>,
	handle: Option<JoinHandle<()>>,
	health: Arc<super::watch_health::Root>,
}

impl Worker {
	fn matches(&self, root: &Path, recursive: bool) -> bool {
		self.health.recursive == recursive
			&& self.health.identity.is_none_or(|identity| {
				retype_core::correlate::Identity::of(root).is_some_and(|current| current == identity)
			})
	}

	fn is_finished(&self) -> bool {
		self.handle.as_ref().is_none_or(JoinHandle::is_finished)
	}

	fn stop(mut self) {
		self.stop.store(true, Ordering::Relaxed);
		if let Some(handle) = self.handle.take() {
			let _ = handle.join();
		}
	}
}

impl Drop for Worker {
	fn drop(&mut self) {
		self.stop.store(true, Ordering::Relaxed);
	}
}

fn stop_workers(workers: &mut BTreeMap<PathBuf, Worker>) {
	for (_, worker) in std::mem::take(workers) {
		worker.stop();
	}
}

fn refresh_workers(
	shared: &Arc<Shared>,
	pool: &Arc<Pool>,
	registrations: Vec<watches::Registration>,
	workers: &mut BTreeMap<PathBuf, Worker>,
	retry: &mut BTreeMap<PathBuf, Instant>,
) {
	let wanted: BTreeMap<PathBuf, bool> = registrations
		.into_iter()
		.filter(|registration| {
			!registration.paused
				&& registration.path.is_dir()
				&& retype_core::policy::Policy::open(&registration.path).is_ok_and(|policy| !policy.paused)
		})
		.map(|registration| (registration.path, registration.recursive))
		.collect();

	let stale: Vec<(PathBuf, bool)> = workers
		.iter()
		.filter_map(|(path, worker)| {
			let matches = wanted
				.get(path)
				.is_some_and(|recursive| worker.matches(path, *recursive));
			let finished = worker.is_finished();
			(!matches || finished).then(|| (path.clone(), matches && finished))
		})
		.collect();

	for (path, unexpected) in stale {
		let worker = workers.remove(&path).expect("stale worker exists");
		let health = Arc::clone(&worker.health);
		if !unexpected {
			health.stopping();
		}
		worker.stop();
		if unexpected {
			if !matches!(health.phase(), super::watch_health::Phase::Failed(_)) {
				health.fail("the watcher stopped unexpectedly");
			}
			shared.watch_health.set_root(&path, health);
			retry.insert(path, Instant::now() + Duration::from_secs(30));
		} else {
			shared.watch_health.remove_root(&path);
			retry.remove(&path);
		}
	}

	shared.watch_health.retain_roots(&wanted);
	retry.retain(|path, _| wanted.contains_key(path));
	for (path, recursive) in wanted {
		if workers.contains_key(&path) || retry.get(&path).is_some_and(|at| *at > Instant::now()) {
			continue;
		}
		start_worker(shared, pool, &path, recursive, workers, retry);
	}
}

fn start_worker(
	shared: &Arc<Shared>,
	pool: &Arc<Pool>,
	path: &Path,
	recursive: bool,
	workers: &mut BTreeMap<PathBuf, Worker>,
	retry: &mut BTreeMap<PathBuf, Instant>,
) {
	let identity = retype_core::correlate::Identity::of(path);
	let health = Arc::new(super::watch_health::Root::new(recursive, identity));
	shared.watch_health.set_root(path, Arc::clone(&health));
	match spawn_worker(
		Arc::clone(shared),
		Arc::clone(pool),
		path,
		recursive,
		Arc::clone(&health),
	) {
		Ok(worker) => {
			retry.remove(path);
			workers.insert(path.to_path_buf(), worker);
		}
		Err(error) => {
			let message = format!("cannot install watcher: {error}");
			eprintln!("retype: {}: {message}", path.display());
			health.fail(message);
			retry.insert(path.to_path_buf(), Instant::now() + Duration::from_secs(30));
		}
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

/// The state directory the way an event names it: resolved on Unix, where a path can
/// be written through a symlink that the platform reports resolved.
fn as_events_name(path: &Path) -> PathBuf {
	#[cfg(unix)]
	{
		path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
	}

	#[cfg(not(unix))]
	{
		path.to_path_buf()
	}
}

fn spawn_worker(
	shared: Arc<Shared>,
	pool: Arc<Pool>,
	root: &Path,
	recursive: bool,
	health: Arc<super::watch_health::Root>,
) -> std::result::Result<Worker, retype_watch::notify::Error> {
	let (watcher, receiver) = retype_watch::watch_mode(root, recursive)?;
	if health.identity.is_some() && health.identity != retype_core::correlate::Identity::of(root) {
		return Err(retype_watch::notify::Error::generic(
			"directory changed while its watcher was starting",
		));
	}
	let stop = Arc::new(AtomicBool::new(false));
	let window = Window::default();
	let watched_root = root.to_path_buf();
	let worker_stop = Arc::clone(&stop);
	let worker_health = Arc::clone(&health);
	// Event paths are resolved, so a filter on the state directory has to be too:
	// macOS reports `/private/var` for a path written as `/var`, and a comparison
	// against the unresolved form matches nothing.
	let state_dir = as_events_name(&shared.state_dir);

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
		let primed = retype_watch::prime_checked(
			&mut correlator,
			&watched_root,
			recursive,
			|path| {
				!path.starts_with(&state_dir)
					&& !retype_core::convert::is_temp_file(path)
					&& registry.watch().considers(&watched_root, path)
			},
			|| worker_stop.load(Ordering::Relaxed),
		);
		shared.primed.fetch_add(primed.primed as u64, Ordering::Relaxed);
		if worker_stop.load(Ordering::Relaxed) {
			return;
		}
		if let Some(error) = primed.error {
			let message = format!("initial scan incomplete: {error}");
			eprintln!("retype: {}: {message}", watched_root.display());
			worker_health.fail(message);
			return;
		}
		worker_health.ready();
		crate::verbose::note(format!(
			"primed {} file(s) under {}",
			primed.primed,
			watched_root.display()
		));
		// Renames waiting out a directory's own debounce, by directory.
		let mut held: std::collections::BTreeMap<PathBuf, Held> = std::collections::BTreeMap::new();

		while !worker_stop.load(Ordering::Relaxed) {
			// A directory that asked for a longer debounce is answered when its own clock
			// says so, before this pass waits for anything else.
			release(&shared, &pool, &watched_root, &mut held);

			let batch = retype_watch::collect(&receiver, less_than(&held, window));
			if worker_stop.load(Ordering::Relaxed) {
				break;
			}
			if batch.disconnected {
				let message = "the filesystem event stream disconnected";
				eprintln!("retype: {}: {message}", watched_root.display());
				worker_health.fail(message);
				break;
			}

			// State is not part of the watched tree even when the tree contains it, and the
			// undo record it holds is a hard link to a file the user may rename.
			let batch = batch.without_prefix(&state_dir);

			if batch.is_empty() {
				continue;
			}

			if crate::verbose::level() >= 2 {
				// The line names every path the batch still has, with the kind and any flag:
				// a dropped-events notice is a flag, not a path.
				let mut described: Vec<String> = Vec::new();

				for event in &batch.events {
					let paths: Vec<String> = event.paths.iter().map(|path| path.display().to_string()).collect();

					if paths.is_empty() {
						continue;
					}

					let mut label = format!("{:?}", event.kind);

					if let Some(flag) = event.flag() {
						label.push_str(&format!(" {flag:?}"));
					}

					if let Some(info) = event.info() {
						label.push_str(&format!(" {info}"));
					}

					described.push(format!("{label}: {}", paths.join(", ")));
				}

				crate::verbose::detail(format!(
					"batch: {} event(s): {}",
					batch.events.len(),
					described.join("; ")
				));
			}

			// A platform that dropped events says so in the batch. What it lost is gone, so
			// the batch cannot be read as a description of the tree: the tree is scanned
			// instead, and identity correlation names what moved while nothing was watching,
			// `docs/dev/measurement/events.md` §3.
			if retype_watch::needs_rescan(&batch) {
				worker_health.degraded("filesystem events were lost; a rescan may not recover every rename");
				for error in &batch.errors {
					eprintln!("retype: {}: watching: {error}", watched_root.display());
				}

				crate::verbose::note("events were dropped; scanning the tree");

				// The file this process restored is under the tree the scan is about to read,
				// so anchor its identity at the restored name first: the scan then names the
				// user's rename from that name, not from the job's output.
				for sighting in shared.take_own_renames() {
					let _ = correlator.observe(sighting);
				}

				let registry = shared.registry();
				let scanned_paths: u64 = batch.events.iter().map(|event| event.paths.len() as u64).sum();
				let parked_batch = retype_core::job::JobId::now().ok().map(|id| id.as_str().to_string());
				let (facts, scanned) = retype_watch::rescan(
					&mut correlator,
					&watched_root,
					recursive,
					|path| {
						!path.starts_with(&state_dir)
							&& !retype_core::convert::is_temp_file(path)
							&& registry.watch().considers(&watched_root, path)
					},
					|| worker_stop.load(Ordering::Relaxed),
				);

				shared.primed.fetch_add(scanned as u64, Ordering::Relaxed);
				shared.observed.fetch_add(scanned_paths, Ordering::Relaxed);

				for fact in facts {
					crate::verbose::detail(format!("rename {} -> {}", fact.source.display(), fact.target.display()));
					dispatch(
						&shared,
						&pool,
						&watched_root,
						fact,
						window,
						&mut held,
						parked_batch.as_deref(),
					);
				}

				continue;
			}

			let observed: u64 = batch.events.iter().map(|event| event.paths.len() as u64).sum();

			// One batch per window, so the jobs parked in one tick can be answered
			// together, `docs/deferral.md` §10.
			let parked_batch = retype_core::job::JobId::now().ok().map(|id| id.as_str().to_string());

			// Our own renames are carried into the batch: a platform may not report a
			// rename the process performed, and the file may have moved again by the
			// time the batch is decided.
			let mut own = shared.take_own_renames();
			let facts = retype_watch::feed_with(&mut correlator, &batch, &mut own);
			shared.restore_own_renames(own);

			for fact in &facts {
				crate::verbose::detail(format!("rename {} -> {}", fact.source.display(), fact.target.display()));
			}

			// After `feed`, so a caller that sees the count has had its file recorded:
			// the sighting is what a later rename correlates against.
			shared.observed.fetch_add(observed, Ordering::Relaxed);

			for fact in facts {
				dispatch(
					&shared,
					&pool,
					&watched_root,
					fact,
					window,
					&mut held,
					parked_batch.as_deref(),
				);
			}
		}

		crate::verbose::detail(format!("stopped watching {}", watched_root.display()));
	});

	Ok(Worker {
		stop,
		handle: Some(handle),
		health,
	})
}

/// One fact from a batch or from a rescan: honour the directory's debounce, or decide.
fn dispatch(
	shared: &Shared,
	pool: &Pool,
	watched_root: &Path,
	fact: retype_core::correlate::RenameFact,
	window: Window,
	held: &mut BTreeMap<PathBuf, Held>,
	parked_batch: Option<&str>,
) {
	// A directory may ask for more settled time than the window gives, which is what
	// `debounce_ms` is for: an application that saves by renaming twice should be
	// converted once, under the name it finally chose.
	let directory = fact.target.parent().unwrap_or(watched_root);

	let debounce = match shared.policies.policy(watched_root, directory).as_ref() {
		Ok(policy) => policy.debounce_ms.unwrap_or_default(),
		Err(problem) => {
			eprintln!("retype: {problem}");
			return;
		}
	};

	let wait = Duration::from_millis(debounce);

	if wait > window.first_wait {
		held.entry(directory.to_path_buf()).or_default().hold(fact, wait);
		return;
	}

	decide(shared, pool, watched_root, fact, parked_batch);
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

	let admission = match shared.lock_parked_admission() {
		Ok(guard) => guard,
		Err(error) => {
			eprintln!(
				"retype: cannot decide the rename to {}: {error}; check the file before retrying",
				fact.target.display()
			);
			return;
		}
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

	// A later rename replaces a parked job for the same file. The same lock
	// prevents a timer from queueing that job between the lookup and this update.
	if let Err(error) = crate::daemon::park::superseded(shared, &source, &admission) {
		eprintln!(
			"retype: cannot replace a parked job for {}: {error}; check {} before retrying",
			source.display(),
			fact.target.display()
		);
		return;
	}
	drop(admission);

	// Registered before anything is published, because the first thing
	// published may be the last thing this batch does: a parked job settles
	// immediately.
	if let Some(batch) = parked_batch {
		shared.batches.open(batch, &id);
	}

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

	admit_rename(shared, pool, watched_root, &source, &fact.target, &id, parked_batch);
}

fn admit_rename(
	shared: &Shared,
	pool: &Pool,
	root: &Path,
	source: &Path,
	target: &Path,
	id: &JobId,
	batch: Option<&str>,
) {
	if let Err(error) = restore_queued_source(shared, source, target) {
		forget_batch(shared, batch, id);
		eprintln!("retype: cannot restore {} before admission: {error}", target.display());
		return;
	}

	let Some(prepared) = prepare(shared, root, source, target) else {
		forget_batch(shared, batch, id);
		return;
	};
	let job = match queued_rename(shared, source, target, id, &prepared) {
		Ok(job) => job,
		Err(error) => {
			forget_batch(shared, batch, id);
			eprintln!("retype: cannot prepare {} for admission: {error}", source.display());
			return;
		}
	};
	let work = crate::daemon::queue::Work {
		id: id.clone(),
		kind: crate::daemon::queue::Kind::Deferred {
			root: root.to_path_buf(),
			source: source.to_path_buf(),
			intent: job.intent.clone().expect("a queued rename has an intent"),
			force: false,
		},
	};

	match pool.submit_after(work, || publish_checked(shared, &job)) {
		Ok(true) => {}
		Ok(false) => {
			forget_batch(shared, batch, id);
			shared.dropped.fetch_add(1, Ordering::Relaxed);
			eprintln!(
				"retype: cannot queue {}: the queue is full or closed; the source was restored to {}",
				target.display(),
				source.display()
			);
		}
		Err(error) => {
			forget_batch(shared, batch, id);
			eprintln!(
				"retype: job {id} did not start because admission could not be recorded: {error}; \
				 its outcome after restart is uncertain, so check the job before retrying"
			);
		}
	}
}

fn forget_batch(shared: &Shared, batch: Option<&str>, id: &JobId) {
	if let Some(batch) = batch {
		shared.batches.forget(batch, id);
	}
}

/// Restore the exact source name before accepting a queued rename.
pub(super) fn restore_queued_source(shared: &Shared, source: &Path, target: &Path) -> retype_core::error::Result<()> {
	let metadata = std::fs::symlink_metadata(target)?;
	if !metadata.file_type().is_file() {
		return Err(std::io::Error::other("the renamed input is not a regular file").into());
	}
	let expected = retype_core::job::FileIdentity::of(target)
		.ok_or_else(|| std::io::Error::other("cannot identify the renamed input"))?;
	retype_core::fsx::rename_noreplace(target, source)?;
	shared.recent.note(source, target);
	shared.note_own_rename(source);
	let is_regular = std::fs::symlink_metadata(source).is_ok_and(|metadata| metadata.file_type().is_file());
	let matches = retype_core::job::FileIdentity::of(source).is_some_and(|identity| expected.matches(&identity));
	if !is_regular || !matches {
		match retype_core::fsx::rename_noreplace(source, target) {
			Ok(()) => {
				shared.recent.note(target, source);
				shared.note_own_rename(target);
			}
			Err(error) => eprintln!(
				"retype: cannot restore {} after an input changed: {error}",
				target.display()
			),
		}
		return Err(std::io::Error::other("the renamed input changed during restoration").into());
	}
	retype_core::fsx::sync_file(source)?;
	retype_core::fsx::sync_directory(source.parent().unwrap_or(Path::new(".")))?;
	Ok(())
}

fn queued_rename(
	shared: &Shared,
	source: &Path,
	target: &Path,
	id: &JobId,
	prepared: &Prepared,
) -> retype_core::error::Result<Job> {
	let metadata = std::fs::metadata(source)?;
	let mtime_ns = metadata
		.modified()
		.ok()
		.and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
		.and_then(|elapsed| i64::try_from(elapsed.as_nanos()).ok())
		.unwrap_or_default();
	let source_name = source
		.file_name()
		.and_then(|name| name.to_str())
		.unwrap_or(&prepared.stem);
	let derived = retype_core::output::derive_with_source_name(&prepared.stem, source_name, &prepared.resolved.groups);
	let outputs = prepared
		.plans
		.iter()
		.map(|plan| {
			let output = derived
				.iter()
				.find(|output| output.group == plan.group)
				.expect("plan has an output");
			Output::pending(
				target.parent().unwrap_or(Path::new("")).join(&output.path),
				&plan.format,
				plan.tier,
			)
		})
		.collect();
	let mut job = settled(
		shared,
		id,
		now_ms(),
		source,
		&prepared.format,
		&prepared.resolved,
		&prepared.plans,
		outputs,
		JobState::Queued,
	);
	job.source.size = metadata.len();
	job.source.mtime_ns = mtime_ns;
	Ok(job)
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
	let in_flight = pool.ids();
	let Ok(jobs) = shared.jobs.lock() else {
		return Vec::new();
	};

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
		source_disposition: None,
	};

	job.outputs[0].state = OutputState::Skipped;
	job.consume_source = consume_source;
	publish(shared, &job);
}

/// Where a job's bytes are, and what has to happen to them afterwards.
enum Input {
	/// A rename put them at `renamed`: relabel before reading, then consume or keep.
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

	// A rename's collisions are its own concern: only a job a
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

	let started = running(shared, &id, source, &format, target, &resolved);
	if let Err(error) = publish_checked(shared, &started) {
		report_unrecorded_failure(shared, &started, &error);
		return;
	}

	// The rename names the request; the file is called what the tokens say, the
	// same way `convert` derives its paths.
	let source_name = source.file_name().and_then(|name| name.to_str()).unwrap_or(&stem);
	let derived = retype_core::output::derive_with_source_name(&stem, source_name, &resolved.groups);

	let store = retype_core::undo::Store::open(&shared.state_dir);
	let lease = match store.lease(&id) {
		Ok(lease) => lease,
		Err(error) => {
			fail_before_conversion(shared, &started, "cannot acquire job ownership", &error);
			return;
		}
	};

	// One job, one source: the bytes are relabelled and then read for every output,
	// Opening the source per output moved the bytes for the first one and
	// left the rest with nothing to read.
	// Whether this job came from a person rather than from a rename, decided before the
	// input is consumed below.
	let named = matches!(input, Input::Named);

	let renamed = match input {
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

				publish(
					shared,
					&settled(shared, &id, now, source, &format, &resolved, &plans, outputs, state),
				);
				return;
			}
		},
		Input::Named => None,
	};

	let read_from = match &renamed {
		Some(renamed) => renamed.input().to_path_buf(),
		None => source.to_path_buf(),
	};
	let input_is_temporary = renamed.as_ref().is_some_and(execute::Rename::is_temporary);
	let snapshot = match retype_core::safety::Snapshot::open(&read_from) {
		Ok(snapshot) => snapshot,
		Err(error) => {
			fail_before_conversion(shared, &started, "cannot create input snapshot", &error);
			return;
		}
	};
	if let Err(error) = super::park::verify_input(&started, &snapshot.original) {
		let mut failed = started.clone();
		failed.fail(JobError::new(error.code().to_string(), error.to_string()));
		publish(shared, &failed);
		return;
	}
	let registry = shared.registry();
	let undo = store.save_with_policy(
		retype_core::undo::Subject {
			id: id.clone(),
			intent: print::intent(&resolved, &registry),
			machine: shared.machine.clone(),
			bytes: read_from.clone(),
			source_path: source.to_path_buf(),
			source_ext: format.clone(),
			consume_source: super::disposition::consumes_source(&resolved) || input_is_temporary,
			outputs: derived
				.iter()
				.filter(|output| !output.keep_source)
				.map(|output| retype_core::undo::Output {
					path: target.parent().unwrap_or(Path::new("")).join(&output.path),
					ext: output.format.clone(),
				})
				.collect(),
		},
		registry.undo(),
	);
	let has_backup = match &undo {
		Ok(Some(record)) => {
			if record.source_fingerprint.as_ref() != Some(&snapshot.original) {
				eprintln!("retype: input changed while its recovery copy was being prepared; source retained");
				publish(
					shared,
					&settled(
						shared,
						&id,
						now,
						source,
						&format,
						&resolved,
						&plans,
						outputs,
						JobState::Failed,
					),
				);
				return;
			}
			true
		}
		Ok(None) => false,
		Err(error) => {
			fail_before_conversion(shared, &started, "cannot save undo record", error);
			return;
		}
	};
	let mut validation_failure = None;
	let mut output_gate = super::transaction::OutputGate::default();
	let checkpoint = settled(
		shared,
		&id,
		now,
		source,
		&format,
		&resolved,
		&plans,
		Vec::new(),
		JobState::Running,
	);

	for plan in &plans {
		// Plans carry their group, because a kept source has one and no plan.
		let Some(derived) = derived.iter().find(|derived| derived.group == plan.group) else {
			continue;
		};

		let output = target.parent().unwrap_or(Path::new("")).join(&derived.path);
		if let Some(reason) = output_gate.skipped() {
			outputs.push(record(&output, plan, OutputState::Skipped, None, None, Some(reason)));
			continue;
		}

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

		let publication = if force {
			retype_core::convert::Publication::Replace
		} else {
			retype_core::convert::Publication::Create
		};
		let transaction = super::transaction::OutputTransaction {
			shared,
			job: &checkpoint,
			original: &read_from,
			completed: &outputs,
			store: has_backup.then_some(&store),
		};
		match transaction.run(plan, &snapshot.path, &output, cancellation, publication) {
			Ok(report) => {
				if let Some(reason) = report.validation.reason() {
					validation_failure.get_or_insert_with(|| reason.to_string());
				}
				outputs.push(record(
					&output,
					plan,
					OutputState::Done,
					Some(report.bytes),
					match report.stderr.trim().is_empty() {
						true => None,
						false => Some(report.stderr.clone()),
					},
					None,
				));
			}
			Err(error) => {
				output_gate.observe(&error);
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

	// Cancellation stops remaining work but keeps any outputs already completed.
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

	let mut job = settled(shared, &id, now, source, &format, &resolved, &plans, outputs, state);
	if state == JobState::Done {
		let source = if input_is_temporary {
			super::disposition::Source::Displaced { target }
		} else {
			super::disposition::Source::Live {
				path: &read_from,
				fingerprint: &snapshot.original,
			}
		};
		let completion = super::disposition::Completion {
			source,
			store: &store,
			has_backup,
			requested_outputs: plans.len(),
			validation_failure: validation_failure.as_deref(),
		};
		if let Err(error) = super::disposition::finish_done(shared, &mut job, completion, cancellation) {
			report_unrecorded_completion(shared, &job, &error);
			return;
		}
	} else {
		if let Err(error) = persist(shared, &job) {
			report_unrecorded_completion(shared, &job, &error);
			return;
		}
		if state == JobState::Cancelled && job.outputs.iter().any(|output| output.state == OutputState::Done) {
			super::cancel::finish_after_persist(shared, &store, &mut job);
		}
	}
	drop(lease);
	publish_persisted(shared, &job);
	mark(shared, &job.outputs, job.intent.as_deref().unwrap_or_default());
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
		consume_source: super::disposition::consumes_source(resolved),
		source_disposition: None,
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

/// Persist a checkpoint without exposing completion before publication or source cleanup.
pub fn persist(shared: &Shared, job: &Job) -> retype_core::error::Result<()> {
	let mut journal = shared
		.journal
		.lock()
		.map_err(|_| retype_core::error::Error::Io(std::io::Error::other("journal lock poisoned")))?;
	journal.append(job)
}

/// Journal and register a snapshot before IPC readers or events can observe it.
pub fn publish_checked(shared: &Shared, job: &Job) -> retype_core::error::Result<()> {
	record_job(shared, job, || persist(shared, job))
}

/// Report publication failures for transitions whose caller cannot recover them.
pub fn publish(shared: &Shared, job: &Job) {
	if let Err(error) = publish_checked(shared, job) {
		eprintln!("retype: cannot publish {}: {error}", job.id);
	}
}

/// Stop before any output and expose a non-durable failure if the journal is unavailable.
pub(super) fn fail_before_conversion(shared: &Shared, started: &Job, context: &str, error: &retype_core::error::Error) {
	let mut failed = started.clone();
	failed.outputs.clear();
	let mut message = format!("{context}; conversion did not start: {error}");
	if error.code() == Code::IoSpace {
		message.push_str("; free space before trying again");
	}
	failed.fail(JobError::new(error.code().to_string(), &message));
	if let Err(journal_error) = publish_checked(shared, &failed) {
		let message = format!("{message}; status could not be recorded and is not durable: {journal_error}");
		eprintln!("retype: job {}: {message}", failed.id);
		failed.fail(JobError::new(error.code().to_string(), message));
		if let Err(error) = record_job(shared, &failed, || Ok(())) {
			eprintln!("retype: cannot report unrecorded failure for {}: {error}", failed.id);
		}
	} else {
		eprintln!("retype: job {}: {message}", failed.id);
	}
}

/// Expose a failure that could not be journalled, without claiming it is durable.
pub fn report_unrecorded_failure(shared: &Shared, job: &Job, error: &retype_core::error::Error) {
	let mut failed = job.clone();
	let message = format!(
		"running state could not be recorded: {error}; conversion did not start and this status is not durable"
	);
	eprintln!("retype: job {}: {message}", failed.id);
	failed.fail(JobError::new(error.code().to_string(), message));
	if let Err(error) = record_job(shared, &failed, || Ok(())) {
		eprintln!("retype: cannot report unrecorded failure for {}: {error}", failed.id);
	}
}

/// Expose an incomplete result without claiming that the final state is durable.
pub(super) fn report_unrecorded_completion(shared: &Shared, job: &Job, error: &retype_core::error::Error) {
	let mut incomplete = job.clone();
	incomplete.set_state(if job.outputs.iter().any(|output| output.state == OutputState::Done) {
		JobState::Partial
	} else {
		JobState::Failed
	});
	let source = match &job.source_disposition {
		Some(retype_core::job::SourceDisposition::Removed) => {
			"the source was removed and its recovery record remains protected"
		}
		Some(retype_core::job::SourceDisposition::Unresolved { .. }) => {
			"the source disposition is unresolved and recovery remains protected when available"
		}
		_ => "the source was not removed",
	};
	let mut message = format!("completion could not be recorded: {error}; {source}; this status is not durable");
	if error.code() == Code::IoSpace {
		message.push_str("; free space before trying again");
	}
	incomplete.error = Some(JobError::new(error.code().to_string(), &message));
	eprintln!("retype: job {}: {message}", incomplete.id);
	if let Err(error) = record_job(shared, &incomplete, || Ok(())) {
		eprintln!(
			"retype: cannot report unrecorded completion for {}: {error}",
			incomplete.id
		);
	}
}

/// Expose a durable snapshot after the caller has finished its destructive decisions.
pub fn publish_persisted(shared: &Shared, job: &Job) {
	if let Err(error) = record_job(shared, job, || Ok(())) {
		eprintln!("retype: cannot publish persisted job {}: {error}", job.id);
	}
}

fn record_job(
	shared: &Shared,
	job: &Job,
	write: impl FnOnce() -> retype_core::error::Result<()>,
) -> retype_core::error::Result<()> {
	/* Readers must not answer "not found" while a visible journal record is still syncing. */
	let mut jobs = shared
		.jobs
		.lock()
		.map_err(|_| retype_core::error::Error::Io(std::io::Error::other("job list lock poisoned")))?;
	write()?;
	match jobs.iter_mut().find(|existing| existing.id == job.id) {
		Some(existing) => *existing = job.clone(),
		None => jobs.push(job.clone()),
	}
	shared
		.events
		.lock()
		.map_err(|_| retype_core::error::Error::Io(std::io::Error::other("event ring lock poisoned")))?
		.push(job);
	drop(jobs);

	// The last job of a batch is the one that speaks for it.
	if is_settled(job.state) {
		if let Some(ids) = shared.batches.finish(&job.id) {
			announce(shared, &ids);
		}
	}
	Ok(())
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
) -> Job {
	let metadata = std::fs::metadata(source).ok();
	let mtime_ns = metadata
		.as_ref()
		.and_then(|metadata| metadata.modified().ok())
		.and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
		.map(|elapsed| elapsed.as_nanos() as i64)
		.unwrap_or_default();

	Job {
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
		consume_source: super::disposition::consumes_source(resolved),
		source_disposition: None,
	}
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
