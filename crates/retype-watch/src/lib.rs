//! Filesystem event collection, and what a rename looks like once translated.
//!
//! Kept out of `retype-core`, which holds domain logic and no watcher: this is
//! the only crate that knows `notify` exists, and what it produces is core
//! vocabulary, a [`retype_core::correlate::RenameFact`].
//!
//! The platform differences absorbed here are measured rather than assumed, and
//! recorded in `docs/dev/measurement/events.md`: one event with both paths on Linux, two events
//! with one path each on Windows, and a single-path event with no rename kind at
//! all on macOS.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub use notify;

use notify::event::{EventKind, ModifyKind, RenameMode};
pub use notify::{Event, RecommendedWatcher, Result as NotifyResult};
use notify::{RecursiveMode, Watcher};
use retype_core::correlate::{Correlator, RenameFact, Sighting, coalesce, dedupe};

/// How long to wait for a gesture to finish arriving.
///
/// All three are needed. `first_wait` covers the latency before the first event,
/// `quiet` closes a batch whose events have stopped coming, and `cap` keeps a
/// directory that never goes quiet from holding a batch open forever.
#[derive(Debug, Clone, Copy)]
pub struct Window {
	pub first_wait: Duration,
	pub quiet: Duration,
	pub cap: Duration,
}

impl Default for Window {
	fn default() -> Self {
		Self {
			first_wait: Duration::from_millis(400),
			quiet: Duration::from_millis(200),
			cap: Duration::from_secs(3),
		}
	}
}

/// The events one gesture produced, and anything the watcher reported while
/// producing them.
#[derive(Debug, Default)]
pub struct Batch {
	pub events: Vec<Event>,
	pub errors: Vec<notify::Error>,
}

impl Batch {
	pub fn is_empty(&self) -> bool {
		self.events.is_empty() && self.errors.is_empty()
	}
}

/// Watch a directory and its subtree, sending events to the returned receiver.
pub fn watch(root: &Path) -> notify::Result<(RecommendedWatcher, mpsc::Receiver<notify::Result<Event>>)> {
	watch_mode(root, true)
}

/// Watch one root, recursively or not, `docs/config.md` §9.
pub fn watch_mode(
	root: &Path,
	recursive: bool,
) -> notify::Result<(RecommendedWatcher, mpsc::Receiver<notify::Result<Event>>)> {
	let (sender, receiver) = mpsc::channel();
	let mut watcher = notify::recommended_watcher(move |event| {
		let _ = sender.send(event);
	})?;
	watcher.watch(
		root,
		match recursive {
			true => RecursiveMode::Recursive,
			false => RecursiveMode::NonRecursive,
		},
	)?;
	Ok((watcher, receiver))
}

/// Collect events until the batch closes.
pub fn collect(receiver: &mpsc::Receiver<notify::Result<Event>>, window: Window) -> Batch {
	let mut batch = Batch::default();
	let start = Instant::now();
	let mut deadline = start + window.first_wait;

	while Instant::now() < deadline && start.elapsed() < window.cap {
		match receiver.recv_timeout(window.quiet) {
			Ok(Ok(event)) => {
				batch.events.push(event);
				deadline = Instant::now() + window.quiet;
			}
			Ok(Err(error)) => batch.errors.push(error),
			Err(mpsc::RecvTimeoutError::Timeout) => {}
			Err(mpsc::RecvTimeoutError::Disconnected) => break,
		}
	}
	batch
}

/// Feed a batch through both strategies and return the facts it supports.
pub fn feed(correlator: &mut Correlator, batch: &Batch) -> Vec<RenameFact> {
	invalidate_renamed_directories(correlator, batch);
	let mut facts = observe_names(correlator, batch);
	facts.extend(observe_identity(correlator, batch));
	correlator.end_batch();
	coalesce(dedupe(facts))
}

/// Record a path this process moved itself, so correlation keeps up even when the
/// platform never reports the rename it performed.
///
/// A job restores a renamed file to its source name before converting it. macOS does
/// not always report a rename the process performed, and a missing report would leave
/// the identity under the name the file no longer has, so the user's next rename of it
/// would be silent.
pub fn observe_path(correlator: &mut Correlator, path: &Path) {
	if let Some(sighting) = sighting(path) {
		correlator.observe(sighting);
	}
}

/// Record the identity of every file under a root, so a file that existed
/// before the daemon started can still be correlated by identity when it is
/// renamed. Returns how many files were primed.
///
/// Directories in the ignore list are pruned, matching `docs/watch.md` §4. A
/// directory's own `patterns` do not prune priming: an ignored file is still
/// primed, because a later rename can make it interesting.
pub fn prime(
	correlator: &mut Correlator,
	root: &Path,
	recursive: bool,
	mut admit: impl FnMut(&Path) -> bool,
	mut stop: impl FnMut() -> bool,
) -> usize {
	let mut primed = 0;

	fn walk(
		correlator: &mut Correlator,
		directory: &Path,
		recursive: bool,
		admit: &mut impl FnMut(&Path) -> bool,
		stop: &mut impl FnMut() -> bool,
		primed: &mut usize,
	) {
		if stop() {
			return;
		}

		let entries = match fs::read_dir(directory) {
			Ok(entries) => entries,
			Err(_) => return,
		};

		for entry in entries.flatten() {
			if stop() {
				return;
			}

			let path = entry.path();
			let Ok(meta) = fs::symlink_metadata(&path) else {
				continue;
			};

			if meta.is_dir() {
				if recursive && admit(&path) {
					walk(correlator, &path, true, admit, stop, primed);
				}
				continue;
			}

			if !meta.is_file() || !admit(&path) {
				continue;
			}

			if let Some(sighting) = sighting(&path) {
				correlator.observe(sighting);
				*primed += 1;
			}
		}
	}

	walk(correlator, root, recursive, &mut admit, &mut stop, &mut primed);
	primed
}

/// A directory rename changes every path beneath it without per-file events, so
/// the identities below the old name are stale. The new tree is primed by the
/// caller.
fn invalidate_renamed_directories(correlator: &mut Correlator, batch: &Batch) {
	for event in &batch.events {
		let EventKind::Modify(ModifyKind::Name(kind)) = event.kind else {
			continue;
		};

		for pair in event.paths.windows(2) {
			let [source, target] = pair else { continue };
			let is_directory = fs::symlink_metadata(target).is_ok_and(|meta| meta.is_dir());
			if is_directory {
				correlator.invalidate_prefix(source);
			}
		}

		// A single-path `To` carries only the new name, and the old prefix is
		// unknowable without an identity. `From` is enough to invalidate the name
		// that just left, which is what a later identity lookup would otherwise
		// still hold.
		if matches!(kind, RenameMode::From) {
			if let Some(source) = event.paths.first() {
				correlator.invalidate_prefix(source);
			}
		}
	}
}

/// Feed a batch through name pairing alone, as Windows needs and as measurement
/// separates from identity.
pub fn feed_names(correlator: &mut Correlator, batch: &Batch) -> Vec<RenameFact> {
	let facts = observe_names(correlator, batch);
	correlator.end_batch();
	coalesce(dedupe(facts))
}

/// Feed a batch through identity alone, which is the only strategy macOS has.
pub fn feed_identity(correlator: &mut Correlator, batch: &Batch) -> Vec<RenameFact> {
	let facts = observe_identity(correlator, batch);
	correlator.end_batch();
	coalesce(dedupe(facts))
}

fn observe_names(correlator: &mut Correlator, batch: &Batch) -> Vec<RenameFact> {
	let mut facts = Vec::new();

	for event in &batch.events {
		match (&event.kind, event.paths.as_slice()) {
			(EventKind::Modify(ModifyKind::Name(RenameMode::Both)), [source, target, ..]) => {
				if let Some(fact) = correlator.pair(source, target) {
					facts.push(fact);
				}
			}
			(EventKind::Modify(ModifyKind::Name(RenameMode::From)), [source, ..]) => {
				correlator.rename_from(source);
			}
			(EventKind::Modify(ModifyKind::Name(RenameMode::To)), [target, ..]) => {
				if let Some(fact) = correlator.rename_to(target) {
					facts.push(fact);
				}
			}
			_ => {}
		}
	}
	facts
}

/// Observe every path the batch mentions, then forget the ones that are gone.
///
/// The order is not defensive. A path that was renamed away and a path that
/// arrived can carry the same identity, so observing before forgetting is what
/// turns a pair of single-path events into a fact, `docs/dev/measurement/events.md` §1.5.
fn observe_identity(correlator: &mut Correlator, batch: &Batch) -> Vec<RenameFact> {
	let mut facts = Vec::new();

	let sightings = sightings(batch);

	for sighting in sightings.iter().filter_map(|(_, sighting)| sighting.as_ref()) {
		if let Some(fact) = correlator.observe(sighting.clone()) {
			facts.push(fact);
		}
	}
	for (path, sighting) in &sightings {
		if sighting.is_none() {
			correlator.forget(path);
		}
	}
	facts
}

fn sightings(batch: &Batch) -> Vec<(PathBuf, Option<Sighting>)> {
	let mut seen = HashSet::new();
	let mut paths = Vec::new();

	for event in &batch.events {
		for path in &event.paths {
			if seen.insert(path.clone()) {
				paths.push(path.clone());
			}
		}
	}

	paths
		.into_iter()
		.map(|path| {
			let sighting = sighting(&path);
			(path, sighting)
		})
		.collect()
}

fn sighting(path: &Path) -> Option<Sighting> {
	let meta = fs::symlink_metadata(path).ok()?;
	// Directories appear in the stream, Windows reports the parent on a move,
	// and a symlink is not a file we may convert. Recording them as absent
	// forgets them instead of correlating them: without this a directory named
	// `a.heic` renamed to `a.jpg` is a conversion.
	if !meta.is_file() {
		return None;
	}

	Some(Sighting {
		path: path.to_path_buf(),
		identity: retype_core::correlate::Identity::of(path)?,
		links: links(&meta),
	})
}

#[cfg(unix)]
fn links(meta: &fs::Metadata) -> u64 {
	use std::os::unix::fs::MetadataExt;
	meta.nlink()
}

#[cfg(not(unix))]
fn links(_meta: &fs::Metadata) -> u64 {
	1
}

#[cfg(test)]
mod tests {
	use super::*;
	use notify::event::{CreateKind, RenameMode};
	use notify::event::{DataChange, ModifyKind};
	use std::path::PathBuf;
	use tempfile::TempDir;

	fn dir() -> TempDir {
		tempfile::tempdir().unwrap()
	}

	fn batch(events: Vec<Event>) -> Batch {
		Batch {
			events,
			errors: Vec::new(),
		}
	}

	fn rename(kind: RenameMode, paths: &[&Path]) -> Event {
		let mut event = Event::new(EventKind::Modify(ModifyKind::Name(kind)));
		for path in paths {
			event = event.add_path(path.to_path_buf());
		}
		event
	}

	fn created(path: &Path) -> Event {
		Event::new(EventKind::Create(CreateKind::File)).add_path(path.to_path_buf())
	}

	fn fresh() -> Correlator {
		Correlator::new(1024)
	}

	fn sources(facts: &[RenameFact]) -> Vec<(PathBuf, PathBuf)> {
		facts
			.iter()
			.map(|fact| (fact.source.clone(), fact.target.clone()))
			.collect()
	}

	#[test]
	fn priming_lets_a_pre_existing_file_correlate_by_identity() {
		let dir = dir();
		let source = dir.path().join("a.heic");
		let target = dir.path().join("a.jpg");
		fs::write(&source, b"heic").unwrap();

		let mut correlator = fresh();
		assert_eq!(prime(&mut correlator, dir.path(), true, |_| true, || false), 1);

		fs::rename(&source, &target).unwrap();
		let facts = feed_identity(&mut correlator, &batch(vec![rename(RenameMode::Any, &[&target])]));

		assert_eq!(sources(&facts), vec![(source, target)]);
	}

	#[test]
	fn one_event_with_both_paths_is_a_fact() {
		let dir = dir();
		let source = dir.path().join("a.heic");
		let target = dir.path().join("a.jpg");
		fs::write(&source, b"heic").unwrap();
		fs::rename(&source, &target).unwrap();

		let facts = feed_names(
			&mut fresh(),
			&batch(vec![rename(RenameMode::Both, &[&source, &target])]),
		);

		assert_eq!(sources(&facts), vec![(source.clone(), target)]);
	}

	#[test]
	fn split_from_and_to_are_one_fact() {
		let dir = dir();
		let source = dir.path().join("a.heic");
		let target = dir.path().join("a.jpg");
		fs::write(&source, b"heic").unwrap();
		fs::rename(&source, &target).unwrap();

		let facts = feed_names(
			&mut fresh(),
			&batch(vec![
				rename(RenameMode::From, &[&source]),
				rename(RenameMode::To, &[&target]),
			]),
		);

		assert_eq!(facts.len(), 1);
		assert_eq!(facts[0].source, source);
		assert_eq!(facts[0].target, target);
	}

	#[test]
	fn linux_triple_report_is_deduped() {
		let dir = dir();
		let source = dir.path().join("a.heic");
		let target = dir.path().join("a.jpg");
		fs::write(&source, b"heic").unwrap();
		fs::rename(&source, &target).unwrap();

		let facts = feed_names(
			&mut fresh(),
			&batch(vec![
				rename(RenameMode::From, &[&source]),
				rename(RenameMode::To, &[&target]),
				rename(RenameMode::Both, &[&source, &target]),
			]),
		);

		assert_eq!(facts.len(), 1, "one gesture, three events, one fact");
	}

	#[test]
	fn a_single_path_event_is_not_a_name_pair() {
		let dir = dir();
		let source = dir.path().join("a.heic");
		let target = dir.path().join("a.jpg");
		fs::write(&source, b"heic").unwrap();
		fs::rename(&source, &target).unwrap();

		let facts = feed_names(
			&mut fresh(),
			&batch(vec![
				rename(RenameMode::Any, &[&source]),
				rename(RenameMode::Any, &[&target]),
			]),
		);

		assert!(
			facts.is_empty(),
			"macOS reports renames this way: docs/dev/measurement/events.md §3"
		);
	}

	#[test]
	fn a_cross_directory_pair_is_not_a_fact() {
		let dir = dir();
		fs::create_dir(dir.path().join("sub")).unwrap();
		let source = dir.path().join("a.heic");
		let target = dir.path().join("sub").join("a.jpg");
		fs::write(&source, b"heic").unwrap();
		fs::rename(&source, &target).unwrap();

		let facts = feed_names(
			&mut fresh(),
			&batch(vec![rename(RenameMode::Both, &[&source, &target])]),
		);

		assert!(facts.is_empty(), "a move is not a conversion");
	}

	#[cfg(unix)]
	#[test]
	fn identity_finds_what_names_missed() {
		let dir = dir();
		let source = dir.path().join("a.heic");
		let target = dir.path().join("a.jpg");
		fs::write(&source, b"heic").unwrap();

		let mut correlator = fresh();
		feed_identity(&mut correlator, &batch(vec![created(&source)]));
		fs::rename(&source, &target).unwrap();

		let facts = feed_identity(
			&mut correlator,
			&batch(vec![
				rename(RenameMode::Any, &[&source]),
				rename(RenameMode::Any, &[&target]),
			]),
		);

		assert_eq!(sources(&facts), vec![(source, target)]);
	}

	#[cfg(unix)]
	#[test]
	fn identity_ignores_a_rename_that_was_then_deleted() {
		let dir = dir();
		let source = dir.path().join("a.heic");
		let target = dir.path().join("a.jpg");
		fs::write(&source, b"heic").unwrap();

		let mut correlator = fresh();
		feed_identity(&mut correlator, &batch(vec![created(&source)]));
		fs::rename(&source, &target).unwrap();
		fs::remove_file(&target).unwrap();

		let facts = feed_identity(&mut correlator, &batch(vec![rename(RenameMode::Any, &[&target])]));

		assert!(facts.is_empty(), "the target is gone, so there is nothing to convert");
	}

	#[cfg(unix)]
	#[test]
	fn both_strategies_on_one_batch_agree() {
		let dir = dir();
		let source = dir.path().join("a.heic");
		let target = dir.path().join("a.jpg");
		fs::write(&source, b"heic").unwrap();

		let mut correlator = fresh();
		feed(&mut correlator, &batch(vec![created(&source)]));
		fs::rename(&source, &target).unwrap();

		let facts = feed(
			&mut correlator,
			&batch(vec![rename(RenameMode::Both, &[&source, &target])]),
		);

		assert_eq!(facts.len(), 1, "both strategies saw one gesture");
	}

	#[cfg(unix)]
	#[test]
	fn a_directory_is_never_a_rename() {
		let dir = dir();
		let source = dir.path().join("a.heic");
		let target = dir.path().join("a.jpg");
		fs::create_dir(&source).unwrap();

		let mut correlator = fresh();
		feed_identity(&mut correlator, &batch(vec![created(&source)]));
		fs::rename(&source, &target).unwrap();

		let facts = feed_identity(
			&mut correlator,
			&batch(vec![
				rename(RenameMode::Any, &[&source]),
				rename(RenameMode::Any, &[&target]),
			]),
		);

		assert!(
			facts.is_empty(),
			"a directory is not a conversion, whatever it is called"
		);
	}

	#[cfg(unix)]
	#[test]
	fn a_symlink_is_never_tracked() {
		use std::os::unix::fs::symlink;

		let dir = dir();
		let real = dir.path().join("real.heic");
		let source = dir.path().join("a.heic");
		let target = dir.path().join("a.jpg");
		fs::write(&real, b"heic").unwrap();
		symlink(&real, &source).unwrap();

		let mut correlator = fresh();
		feed_identity(&mut correlator, &batch(vec![created(&source)]));
		fs::rename(&source, &target).unwrap();

		let facts = feed_identity(
			&mut correlator,
			&batch(vec![
				rename(RenameMode::Any, &[&source]),
				rename(RenameMode::Any, &[&target]),
			]),
		);

		assert!(
			facts.is_empty(),
			"following a link would convert something outside the tree"
		);
	}

	#[test]
	fn a_write_is_not_a_rename() {
		let dir = dir();
		let path = dir.path().join("a.heic");
		fs::write(&path, b"heic").unwrap();

		let mut correlator = fresh();
		feed(&mut correlator, &batch(vec![created(&path)]));
		fs::write(&path, b"more heic").unwrap();

		let event = Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content))).add_path(path);
		let facts = feed_names(&mut correlator, &batch(vec![event]));

		assert!(facts.is_empty());
	}

	#[test]
	fn watcher_errors_are_kept() {
		let (_sender, receiver) = mpsc::channel();
		drop(_sender);
		let window = Window {
			first_wait: Duration::from_millis(1),
			quiet: Duration::from_millis(1),
			cap: Duration::from_millis(10),
		};

		let batch = collect(&receiver, window);

		assert!(batch.is_empty());
	}
}
