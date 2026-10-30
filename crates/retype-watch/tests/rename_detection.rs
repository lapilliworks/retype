//! A real rename, from real events, on every platform the product ships to.
//!
//! The unit tests in the library cover how each platform's event shape is
//! translated. This covers delivery: that a watcher sees a rename at all, and
//! that the batch it produces still yields the fact the user asked for. It is
//! the regression test for `docs/events.md`, and it is deliberately three
//! scenarios rather than all nine measured ones, because the rest are covered
//! by synthetic events and repeating them against a real watcher buys flake
//! rather than confidence.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use retype_core::correlate::{Correlator, RenameFact};
use retype_core::registry::Registry;
use retype_core::resolve::intent_of;
use retype_watch::{Event, NotifyResult, RecommendedWatcher, Window, collect, feed, watch};

/// Generous, because a loaded CI runner delays the first event and macOS
/// batches FSEvents. A batch with no events costs one `first_wait`.
fn window() -> Window {
	Window {
		first_wait: Duration::from_secs(1),
		quiet: Duration::from_millis(250),
		cap: Duration::from_secs(5),
	}
}

struct Scene {
	// Declared first so it stops watching before `_dir` removes the tree.
	_watcher: RecommendedWatcher,
	receiver: Receiver<NotifyResult<Event>>,
	root: PathBuf,
	correlator: Correlator,
	_dir: tempfile::TempDir,
}

impl Scene {
	fn new() -> Self {
		let dir = tempfile::tempdir().unwrap();
		// Canonicalised for the same reason the daemon does it: macOS reports
		// resolved paths, `docs/events.md` §3.3.
		let root = dir.path().canonicalize().unwrap();
		let (watcher, receiver) = watch(&root).unwrap();

		Self {
			_watcher: watcher,
			receiver,
			root,
			correlator: Correlator::new(1024),
			_dir: dir,
		}
	}

	/// Create a file and let the correlator see it, so the rename under test is
	/// a rename of something that already existed.
	fn existing(&mut self, name: &str) -> PathBuf {
		let path = self.root.join(name);
		fs::write(&path, b"source bytes").unwrap();
		let batch = collect(&self.receiver, window());
		feed(&mut self.correlator, &batch);
		path
	}

	fn rename(&mut self, from: &Path, to: &Path) -> Vec<RenameFact> {
		fs::rename(from, to).unwrap();
		let batch = collect(&self.receiver, window());
		feed(&mut self.correlator, &batch)
	}
}

fn intent(source: &str, target: &str) -> Option<String> {
	let registry = Registry::builtin().expect("built-in registry");

	intent_of(&registry, source, target)
		.expect("the name should parse")
		.map(|intent| intent.text)
}

#[test]
fn an_in_place_rename_is_detected() {
	let mut scene = Scene::new();
	let source = scene.existing("a.heic");
	let target = scene.root.join("a.jpg");

	let facts = scene.rename(&source, &target);

	assert_eq!(facts.len(), 1, "one gesture is one conversion");
	assert_eq!(facts[0].source, source, "the source keeps its own extension");
	assert_eq!(facts[0].target, target);
	assert_eq!(intent("a.heic", "a.jpg").as_deref(), Some("jpg"));
}

#[test]
fn a_case_only_rename_is_a_fact_with_no_intent() {
	let mut scene = Scene::new();
	let source = scene.existing("b.heic");
	let target = scene.root.join("b.HEIC");

	let facts = scene.rename(&source, &target);

	assert!(facts.len() <= 1, "whether the platform reports it at all varies");
	assert_eq!(
		intent("b.heic", "b.HEIC"),
		None,
		"the same format with different spelling"
	);
}

#[test]
fn a_stem_change_is_neither_paired_nor_an_intent() {
	let mut scene = Scene::new();
	let source = scene.existing("c.heic");
	let target = scene.root.join("c-renamed.jpg");

	let facts = scene.rename(&source, &target);

	assert!(facts.is_empty(), "renaming the stem is not a conversion");
	assert_eq!(intent("c.heic", "c-renamed.jpg"), None);
}
