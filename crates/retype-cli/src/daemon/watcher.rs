//! Watching the registered directories and converting what a rename asks for,
//! `docs/watch.md` and `docs/events.md`.
//!
//! One thread per root, each collecting a window of events and handing the batch
//! to the correlator. A batch that produces no rename fact produces no work, which
//! is the common case: an editor's save, a sync client's write, a copy that never
//! finished.
//!
//! Conversions run in the watcher thread for now, one at a time. The queue is what
//! makes them cancellable and concurrent, and it is the next thing to land; until
//! then a slow conversion delays the next rename rather than losing it, because
//! events queue in the channel while the thread is busy.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use retype_core::correlate::Correlator;
use retype_core::error::Code;
use retype_core::execute::Cancellation;
use retype_core::job::{Job, JobError, JobState, Output, OutputState, Source, now_ms};
use retype_core::journal::Journal;
use retype_core::plan::{self, Plan};
use retype_core::registry::Registry;
use retype_core::resolve::{self, Source as Origin};
use retype_core::{execute, print, watches};
use retype_watch::{Window, feed};

use crate::daemon::queue::Pool;
use crate::error::Result;

/// What the daemon, its watchers and its runners share.
pub struct Shared {
	pub registry: Registry,
	pub journal: Mutex<Journal>,
	/// The jobs this process has run, for `jobs.list`.
	pub jobs: Mutex<Vec<Job>>,
	/// What this process has converted, so its own writes are not mistaken for a
	/// rename request.
	pub recent: Recent,
	/// Automatic jobs refused because the queue was full, `docs/watch.md` §8.
	pub dropped: AtomicU64,
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
	fn note(&self, consumed: &Path, produced: &Path) {
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
	let supervisor = {
		let shared = Arc::clone(&shared);
		let stop = Arc::clone(&stop);
		let state_dir = state_dir.to_path_buf();

		thread::spawn(move || supervise(&shared, &pool, &state_dir, &stop))
	};

	Ok(Some(Watcher {
		stop,
		threads: vec![supervisor],
	}))
}

/// How often the registration file is re-read.
const RELOAD: Duration = Duration::from_secs(1);

fn supervise(shared: &Arc<Shared>, pool: &Arc<Pool>, state_dir: &Path, stop: &AtomicBool) {
	// One worker per root, keyed by the canonical path the registration holds.
	let mut workers: BTreeMap<PathBuf, (Arc<AtomicBool>, JoinHandle<()>)> = BTreeMap::new();

	while !stop.load(Ordering::Relaxed) {
		let wanted = match watches::load(state_dir) {
			Ok(registrations) => registrations,
			Err(error) => {
				eprintln!("retype: cannot read watch registrations: {error}");
				thread::sleep(RELOAD);
				continue;
			}
		};

		// Stop what is no longer registered, or whose recursion changed, which
		// needs a new watcher rather than a reconfigured one.
		let keep: BTreeSet<PathBuf> = wanted
			.iter()
			.filter(|registration| {
				workers
					.get(&registration.path)
					.is_none_or(|_| registration.path.exists())
			})
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
			if workers.contains_key(&registration.path) {
				continue;
			}

			match spawn_worker(
				Arc::clone(shared),
				Arc::clone(pool),
				&registration.path,
				registration.recursive,
			) {
				Ok((worker_stop, handle)) => {
					workers.insert(registration.path.clone(), (worker_stop, handle));
				}
				Err(error) => eprintln!("retype: cannot watch {}: {error}", registration.path.display()),
			}
		}

		thread::sleep(RELOAD);
	}

	for (_, (worker_stop, handle)) in workers {
		worker_stop.store(true, Ordering::Relaxed);
		let _ = handle.join();
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
		let mut correlator = Correlator::new(256);

		while !worker_stop.load(Ordering::Relaxed) {
			let batch = retype_watch::collect(&receiver, window);
			if batch.is_empty() {
				continue;
			}

			for fact in feed(&mut correlator, &batch) {
				if retype_core::convert::is_temp_file(&fact.source) || retype_core::convert::is_temp_file(&fact.target)
				{
					continue;
				}

				if shared.recent.is_echo(&fact.source, &fact.target) {
					continue;
				}

				let Ok(id) = retype_core::job::JobId::now() else {
					continue;
				};

				// A rename chain that spans two batches: the file being renamed is
				// what an earlier job is writing, so the earlier job is spent and the
				// chain becomes one conversion, `docs/state.md` §5.3.
				// The rename just put the bytes at the target, so that is where they
				// are. What the chain changes is the *source* name: after two renames
				// the file is not what its intermediate name claimed, it is what the
				// job that was overtaken was reading, `docs/state.md` §5.3.
				let earlier = producers(shared.as_ref(), pool.as_ref(), &fact.source);

				let source = match earlier.is_empty() {
					true => fact.source.clone(),
					false => supersede(shared.as_ref(), pool.as_ref(), &earlier),
				};

				let work = crate::daemon::queue::Work {
					id,
					kind: crate::daemon::queue::Kind::Rename {
						root: watched_root.clone(),
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
		}
	});

	Ok((stop, handle))
}

/// Every job still in flight that this path belongs to.
///
/// Two of them are worth matching, and the second is the common one. A rename of
/// what a job is writing is the user changing their mind about the result. A
/// rename of the *source* name is the same decision taken a moment later, and it
/// is the one people make in practice: while a job converts, the file shows its
/// source name again, because that is the name its bytes belong under,
/// `docs/state.md` §5.4.
fn producers(shared: &Shared, pool: &Pool, path: &Path) -> Vec<Job> {
	let Ok(jobs) = shared.jobs.lock() else {
		return Vec::new();
	};

	let in_flight = pool.ids();

	jobs.iter()
		.filter(|job| {
			matches!(
				job.state,
				JobState::Pending | JobState::Queued | JobState::Running | JobState::Partial
			) && in_flight.contains(&job.id)
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

	let (source, target) = match &work.kind {
		Kind::Rename { source, target, .. } => (source.clone(), target.clone()),
		Kind::Immediate(request) => (request.input.clone(), request.input.clone()),
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
		machine: shared.machine.clone(),
		error: Some(JobError {
			code: Code::JobSuperseded.to_string(),
			message: "replaced by a later rename before it started".to_string(),
		}),
	};

	job.outputs[0].state = OutputState::Skipped;
	publish(shared, &job);
}

/// Turn one rename into jobs, or explain in the log why it is not one.
pub fn convert(
	shared: &Shared,
	root: &Path,
	bytes: &Path,
	source: &Path,
	target: &Path,
	id: &retype_core::job::JobId,
	cancellation: &Cancellation,
) {
	let (Some(source_name), Some(target_name)) = (
		source.file_name().and_then(|name| name.to_str()),
		target.file_name().and_then(|name| name.to_str()),
	) else {
		return;
	};

	let Some((stem, source_format)) = shared.registry.split(source_name) else {
		// Not a file whose format we know, so not ours to act on.
		return;
	};

	let intent = match resolve::intent_of(&shared.registry, source_name, target_name) {
		Ok(Some(intent)) => intent,
		Ok(None) => return,
		Err(problems) => {
			eprintln!("retype: {}: {problems}", target.display());
			return;
		}
	};

	let resolved = match resolve::resolve(
		&intent,
		Origin {
			format: source_format,
			is_directory: false,
		},
		&shared.registry,
	) {
		Ok(resolved) => resolved,
		Err(problems) => {
			eprintln!("retype: {}: {problems}", target.display());
			return;
		}
	};

	let plans = match plan::compile(&shared.registry, &retype_core::tools::Machine, &resolved) {
		Ok(plans) => plans,
		Err(problems) => {
			eprintln!("retype: {}: {problems}", target.display());
			return;
		}
	};

	let id = id.clone();
	let now = now_ms();
	let mut outputs = Vec::new();
	running(shared, &id, root, source, source_format, target, &resolved);

	// The rename names the request; the file is called what the tokens say, the
	// same way `convert` derives its paths.
	let derived = retype_core::output::derive(stem, source_format, &resolved.groups);

	// Keep the bytes before touching them: an undo has to be possible even if this
	// process dies mid-conversion. Retention is a policy, so a machine that does
	// not keep records does not pay for one.
	let store = retype_core::undo::Store::open(&shared.state_dir);
	let policy = shared.registry.undo();
	let undo = if !policy.enabled {
		None
	} else {
		Some(
			store.save(retype_core::undo::Subject {
				id: id.clone(),
				intent: print::intent(&resolved, &shared.registry),
				machine: shared.machine.clone(),
				bytes: target.to_path_buf(),
				source_path: root.join(source_name),
				source_ext: source_format.to_string(),
				outputs: derived
					.iter()
					.map(|derived| retype_core::undo::Output {
						path: target.parent().unwrap_or(Path::new("")).join(&derived.path),
						ext: derived.format.clone(),
					})
					.collect(),
			}),
		)
	};

	// One job, one source: the bytes are relabelled and then read for every output,
	//. Opening the source per output moved the bytes for the first one and
	// left the rest with nothing to read.
	let renamed = match execute::Rename::open(bytes, stem, source_format) {
		Ok(renamed) => renamed,
		Err(error) => {
			eprintln!("retype: {}: {error}", target.display());
			store.remove(&id).ok();
			return;
		}
	};

	for (plan, derived) in plans.iter().zip(&derived) {
		let output = target.parent().unwrap_or(Path::new("")).join(&derived.path);

		shared.recent.note(source, &output);

		let log = crate::state::log_path(&shared.state_dir, &id);

		match execute::run_logged(plan, renamed.input(), &output, cancellation, Some(&log)) {
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
				// is skipped rather than failed, `docs/state.md` §5.3.
				let (state, message) = match cancellation.is_superseded() {
					true => (OutputState::Skipped, "replaced by a later rename".to_string()),
					false => (OutputState::Failed, error.to_string()),
				};

				if state == OutputState::Failed {
					eprintln!("retype: {}: {error}", output.display());
				}

				outputs.push(record(&output, plan, state, None, None, Some(message)));
			}
		}
	}

	// A cancelled job is its own outcome: nothing was converted and nothing is
	// wrong with the file, `docs/state.md` §4.
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

	// The job either used the renamed file up or left it alone: a job with any output
	// consumed its source, and one that produced nothing puts the source name back so
	// no path keeps bytes its name denies.
	match state {
		JobState::Done | JobState::Partial => {
			if let Err(error) = renamed.consume() {
				eprintln!("retype: cannot remove {}: {error}", target.display());
			}
		}
		_ => renamed.keep(),
	}

	let job = Job {
		id: id.clone(),
		created: now,
		updated: now_ms(),
		source: Source {
			path: root.join(source_name),
			format: source_format.to_string(),
			size: 0,
			mtime_ns: 0,
		},
		intent: Some(print::intent(&resolved, &shared.registry)),
		outputs,
		state,
		backend: plans.first().map(|plan| plan.backend.name().to_string()),
		engine: plans
			.first()
			.and_then(|plan| plan.engine.clone())
			.map(|(name, candidate)| retype_core::job::Engine { name, candidate }),
		machine: shared.machine.clone(),
		error: None,
	};

	publish(shared, &job);
	mark(shared, &job.outputs, job.intent.as_deref().unwrap_or_default());

	// Nothing was produced, so there is nothing to undo: the source is where it
	// always was and the record would only describe a file that never changed.
	if state != JobState::Done && state != JobState::Partial {
		let _ = store.remove(&id);
	}
}

/// Write the intent onto each output that was produced, `docs/provenance.md` §6.2.
///
/// Best effort: a filesystem that refuses attributes, or a policy that turns them
/// off, leaves the journal as the only record and changes nothing else.
pub fn mark(shared: &Shared, outputs: &[Output], intent: &str) {
	if !shared.registry.provenance().xattr {
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

	if let Ok(mut jobs) = shared.jobs.lock() {
		match jobs.iter_mut().find(|existing| existing.id == job.id) {
			Some(existing) => *existing = job.clone(),
			None => jobs.push(job.clone()),
		}
	}
}

/// The record a job writes before it starts, so a client can see a job that is
/// running rather than only one that finished, `docs/state.md` §3.
fn running(
	shared: &Shared,
	id: &retype_core::job::JobId,
	root: &Path,
	source: &Path,
	source_format: &str,
	target: &Path,
	resolved: &resolve::Resolved,
) {
	let name = source.file_name().and_then(|name| name.to_str()).unwrap_or_default();

	let job = Job {
		id: id.clone(),
		created: now_ms(),
		updated: now_ms(),
		source: Source {
			path: root.join(name),
			format: source_format.to_string(),
			size: 0,
			mtime_ns: 0,
		},
		intent: Some(print::intent(resolved, &shared.registry)),
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
	};

	if let Ok(mut journal) = shared.journal.lock() {
		if let Err(error) = journal.append(&job) {
			eprintln!("retype: cannot journal {}: {error}", target.display());
		}
	}

	// Visible while it runs, which is what makes it cancellable by a client that
	// has to find its id first.
	if let Ok(mut jobs) = shared.jobs.lock() {
		jobs.push(job);
	}
}

fn record(
	path: &Path,
	plan: &Plan<'_>,
	state: OutputState,
	bytes: Option<u64>,
	stderr: Option<String>,
	error: Option<String>,
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
		error: error.map(|message| JobError {
			code: Code::ToolFailed.to_string(),
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
