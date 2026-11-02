//! Rename correlation.
//!
//! Turns filesystem observations into rename facts, without touching the
//! filesystem. The event source lives in the daemon; this is the part that
//! decides what the events mean, and it is pure so the decision can be tested
//! without a platform.
//!
//! Scope is: in-place renames only. Source and target share a directory,
//! the stem is unchanged, and the extension differs. Anything else is a move or
//! a plain rename and carries no intent.
//!
//! Identity is the mechanism: a rename does not change the device or inode, so
//! an event whose path carries an identity already known under another path is
//! a rename, whatever the platform reported. Filesystems that cannot supply a
//! trustworthy identity fall back to the pair of names when the event source
//! provides both.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

/// Filesystem identity of a file: device and inode, or volume and file id.
///
/// `high` is the top half of a 128-bit Windows file id, which ReFS uses; NTFS
/// and every Unix filesystem leave it zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Identity {
	pub device: u64,
	pub inode: u64,
	pub high: u64,
}

impl Identity {
	/// The identity this platform reports for the file at `path`.
	///
	/// Unix reads the device and inode from the metadata. Windows has neither in
	/// `std`, so the volume and file id come from a handle, the same source the
	/// journal records for a produced output.
	pub fn of(path: &Path) -> Option<Self> {
		#[cfg(unix)]
		{
			use std::os::unix::fs::MetadataExt;

			let metadata = std::fs::metadata(path).ok()?;

			Some(Self {
				device: metadata.dev(),
				inode: metadata.ino(),
				high: 0,
			})
		}

		#[cfg(windows)]
		{
			let (device, file, _) = crate::job::windows_file_id(path)?;

			Some(Self {
				device,
				inode: file as u64,
				high: (file >> 64) as u64,
			})
		}

		#[cfg(not(any(unix, windows)))]
		{
			let _ = path;
			None
		}
	}
}

/// What a rename fact says: this path became that path, in one directory.
///
/// Nothing about formats, because deciding where a stem ends needs the registry:
/// `archive.tar.gz` has one format and the stem `archive`. What the new name
/// asked for is `resolve::intent_of`'s question, and this layer only pairs
/// names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameFact {
	pub source: PathBuf,
	pub target: PathBuf,
}

/// One observation of a path that exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sighting {
	pub path: PathBuf,
	pub identity: Identity,
	/// Link count. Multiple names are ambiguous while the previously known name still exists.
	pub links: u64,
}

impl Sighting {
	/// The sighting this platform reports for the file at `path`, or nothing when
	/// the path is gone or is not a regular file.
	pub fn of(path: &Path) -> Option<Self> {
		let metadata = std::fs::symlink_metadata(path).ok()?;

		// Directories appear in the stream, Windows reports the parent on a move, and
		// a symlink is not a file we may convert. Recording them as absent forgets them
		// instead of correlating them: without this a directory named `a.heic` renamed
		// to `a.jpg` is a conversion.
		if !metadata.is_file() {
			return None;
		}

		#[cfg(unix)]
		let (identity, links) = {
			use std::os::unix::fs::MetadataExt;
			(
				Identity {
					device: metadata.dev(),
					inode: metadata.ino(),
					high: 0,
				},
				metadata.nlink(),
			)
		};

		#[cfg(windows)]
		let (identity, links) = {
			let info = crate::job::windows_file_info(path)?;
			(
				Identity {
					device: info.volume,
					inode: info.file as u64,
					high: (info.file >> 64) as u64,
				},
				info.links,
			)
		};

		#[cfg(not(any(unix, windows)))]
		let (identity, links) = (Identity::of(path)?, 1);

		Some(Self {
			path: path.to_path_buf(),
			identity,
			links,
		})
	}
}

/// Bounded identity map with insertion-order eviction.
#[derive(Debug)]
pub struct Correlator {
	capacity: usize,
	by_identity: HashMap<Identity, PathBuf>,
	by_path: HashMap<PathBuf, Identity>,
	order: VecDeque<PathBuf>,
	/// Sources reported by an event source that splits a rename into two
	/// events, waiting for their target. Bounded, and cleared at the end of a
	/// batch.
	pending: VecDeque<PathBuf>,
	pending_capacity: usize,
}

/// Maximum unmatched rename sources kept between events.
const PENDING_CAPACITY: usize = 128;

impl Correlator {
	pub fn new(capacity: usize) -> Self {
		Self {
			capacity: capacity.max(1),
			by_identity: HashMap::new(),
			by_path: HashMap::new(),
			order: VecDeque::new(),
			pending: VecDeque::new(),
			pending_capacity: PENDING_CAPACITY,
		}
	}

	pub fn len(&self) -> usize {
		self.by_identity.len()
	}

	pub fn is_empty(&self) -> bool {
		self.by_identity.is_empty()
	}

	/// Record a path that exists, and report a rename when its identity was
	/// last seen under a different path.
	pub fn observe(&mut self, sighting: Sighting) -> Option<RenameFact> {
		if sighting.links > 1 {
			// A hard link gives one identity more than one name, and that is ambiguous
			// only while the name we knew is still there: a link created beside it must
			// not read as a rename, while a rename leaves the old name gone.
			//
			// User-created hard links must not trigger conversions. Identity is also
			// the only way some platforms report a genuine rename.
			let crowded = self
				.by_identity
				.get(&sighting.identity)
				.is_some_and(|previous| previous != &sighting.path && previous.exists());

			if crowded {
				self.forget(&sighting.path);
				return None;
			}
		}

		let fact = match self.by_identity.get(&sighting.identity) {
			Some(previous) if previous != &sighting.path => renamed(previous, &sighting.path),
			_ => None,
		};

		self.remember(sighting.path, sighting.identity);
		fact
	}

	/// Record a source from an event source that reports a rename as two
	/// events, as Windows does. The fact is produced when `rename_to` arrives.
	pub fn rename_from(&mut self, source: &Path) {
		if self.pending.len() == self.pending_capacity {
			self.pending.pop_front();
		}
		self.pending.push_back(source.to_path_buf());
	}

	/// Match a target against a pending source in the same directory.
	///
	/// Windows reports `From` then `To` for each file, and a batch of renames
	/// can interleave, so matching is by directory and oldest first rather than
	/// by adjacency.
	pub fn rename_to(&mut self, target: &Path) -> Option<RenameFact> {
		let index = self
			.pending
			.iter()
			.position(|source| source.parent() == target.parent())?;
		let source = self.pending.remove(index)?;
		self.pair(&source, target)
	}

	/// Drop sources that never found a target. Called at the end of a batch:
	/// an unmatched `From` is a file that left the watched tree, not a rename.
	pub fn end_batch(&mut self) {
		self.pending.clear();
	}

	/// Report a rename when the event source supplies both names.
	pub fn pair(&mut self, source: &Path, target: &Path) -> Option<RenameFact> {
		let fact = renamed(source, target);
		if let Some(identity) = self.by_path.remove(source) {
			self.remember(target.to_path_buf(), identity);
		}
		fact
	}

	/// Whether this identity is still recorded under some path.
	pub fn knows_identity(&self, identity: &Identity) -> bool {
		self.by_identity.contains_key(identity)
	}

	/// Forget a path that no longer exists.
	///
	/// The identity is dropped only when this path is still the one it belongs to.
	/// A batch that reports a rename names both the old and the new path, and the
	/// new one is observed before the old one is forgotten; dropping the identity
	/// then would leave the file that just moved with no identity at all, and the
	/// next rename of it would be silent.
	pub fn forget(&mut self, path: &Path) {
		if let Some(identity) = self.by_path.remove(path) {
			if self.by_identity.get(&identity).is_some_and(|current| current == path) {
				self.by_identity.remove(&identity);
			}
		}
	}

	/// Forget every path under a directory that was renamed.
	///
	/// A directory rename changes its children's paths without per-file events,
	/// so their entries are stale. Re-priming fills them in again.
	pub fn invalidate_prefix(&mut self, dir: &Path) {
		self.by_path.retain(|path, identity| {
			let stale = path.starts_with(dir);
			if stale {
				self.by_identity.remove(identity);
			}
			!stale
		});
		self.order.retain(|path| !path.starts_with(dir));
	}

	fn remember(&mut self, path: PathBuf, identity: Identity) {
		// A path that now carries another identity does not stand for the old one.
		if let Some(previous) = self.by_path.insert(path.clone(), identity) {
			if self.by_identity.get(&previous).is_some_and(|current| current == &path) {
				self.by_identity.remove(&previous);
			}
		}

		// One identity belongs to one path: a rename moves it, and the old name has
		// to leave `by_path` too, or forgetting that stale name would drop the
		// identity that now belongs here.
		if let Some(old) = self.by_identity.insert(identity, path.clone()) {
			if old != path {
				self.by_path.remove(&old);
			}
		}

		self.order.push_back(path);

		while self.by_identity.len() > self.capacity {
			let Some(oldest) = self.order.pop_front() else { break };
			if let Some(identity) = self.by_path.remove(&oldest) {
				self.by_identity.remove(&identity);
			}
		}
	}
}

/// Remove duplicate facts within a batch.
///
/// Linux reports one rename as `From`, `To` and `Both`, so a feeder handling
/// every shape can produce the same fact more than once.
pub fn dedupe(facts: Vec<RenameFact>) -> Vec<RenameFact> {
	let mut unique: Vec<RenameFact> = Vec::new();
	for fact in facts {
		let duplicate = unique
			.iter()
			.any(|seen| seen.source == fact.source && seen.target == fact.target);
		if !duplicate {
			unique.push(fact);
		}
	}
	unique
}

/// Collapse rename chains inside one batch.
///
/// A platform that reports a rename as two events produces one fact per step,
/// so `e.heic` to `e.jpg` to `e.webp` arrives as two facts with an intermediate
/// the user never asked for. Converting the intermediate would re-encode twice
/// and produce a file nobody requested, so a chain becomes one fact from the
/// first source to the last target, keeping the last intent.
///
/// Only facts inside one batch are collapsed: a rename made minutes later is a
/// separate gesture, and the user meant both.
pub fn coalesce(facts: Vec<RenameFact>) -> Vec<RenameFact> {
	let mut collapsed: Vec<RenameFact> = Vec::new();
	// Target path of an earlier fact, mapped to its index in `collapsed`.
	let mut produced: HashMap<PathBuf, usize> = HashMap::new();

	for fact in facts {
		match produced.remove(&fact.source) {
			Some(index) => {
				let earlier = &mut collapsed[index];
				let target = fact.target.clone();
				earlier.target = target.clone();
				produced.insert(target, index);
			}
			None => {
				produced.insert(fact.target.clone(), collapsed.len());
				collapsed.push(fact);
			}
		}
	}

	collapsed
}

/// Whether two names are a rename in place.
///
/// A move is not a rename so the directory must match. The names must
/// also share what a stem can plausibly be: pairing by directory alone would let
/// two renames in one batch cross over, and pairing needs no format knowledge if
/// the comparison stops at the first dot. `archive.tar.gz` and `archive.zip`
/// therefore agree, and `a.heic` and `b.webp` do not.
///
/// Whether the new name asked for anything is the grammar's question, and only
/// the registry can answer it.
fn renamed(source: &Path, target: &Path) -> Option<RenameFact> {
	if source == target || source.parent() != target.parent() {
		return None;
	}

	let source_prefix = stem_prefix(source.file_name()?.to_str()?);
	let target_prefix = stem_prefix(target.file_name()?.to_str()?);

	if source_prefix.is_empty() || source_prefix != target_prefix {
		return None;
	}

	Some(RenameFact {
		source: source.to_path_buf(),
		target: target.to_path_buf(),
	})
}

/// The text before the first dot, which is the longest a stem can be for any
/// format spelling. A name with no dot is its own prefix.
fn stem_prefix(name: &str) -> &str {
	name.split_once('.').map(|(prefix, _)| prefix).unwrap_or(name)
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::fs;

	fn id(inode: u64) -> Identity {
		Identity {
			device: 1,
			inode,
			high: 0,
		}
	}

	fn sighting(path: &str, inode: u64) -> Sighting {
		Sighting {
			path: PathBuf::from(path),
			identity: id(inode),
			links: 1,
		}
	}

	#[test]
	fn first_sighting_is_not_a_rename() {
		let mut correlator = Correlator::new(16);
		assert!(correlator.observe(sighting("/dir/photo.heic", 1)).is_none());
		assert_eq!(correlator.len(), 1);
	}

	#[test]
	fn identity_under_a_new_path_is_a_rename() {
		let mut correlator = Correlator::new(16);
		correlator.observe(sighting("/dir/photo.heic", 1));

		let rename = correlator.observe(sighting("/dir/photo.webp+q80", 1)).unwrap();

		assert_eq!(rename.source, PathBuf::from("/dir/photo.heic"));
		assert_eq!(rename.target, PathBuf::from("/dir/photo.webp+q80"));
	}

	#[test]
	fn a_batch_that_forgets_the_old_name_keeps_the_new_identity() {
		// macOS reports one path per event, so a rename arrives as the old name and
		// the new name in the same batch. The new one is observed first and the old
		// one forgotten second, and the file that just moved has to keep its identity
		// for the rename that follows.
		let mut correlator = Correlator::new(16);
		correlator.observe(sighting("/dir/photo.heic", 1));

		let moved = correlator.observe(sighting("/dir/photo.jpg", 1)).unwrap();
		assert_eq!(moved.source, PathBuf::from("/dir/photo.heic"));
		correlator.forget(Path::new("/dir/photo.heic"));

		let next = correlator
			.observe(sighting("/dir/photo.webp", 1))
			.expect("the moved file still has an identity");
		assert_eq!(next.source, PathBuf::from("/dir/photo.jpg"));
		assert_eq!(next.target, PathBuf::from("/dir/photo.webp"));
	}

	#[test]
	fn primed_file_renames_correctly() {
		// The daemon primes identities at registration, so a file that existed
		// before start still correlates.
		let mut correlator = Correlator::new(16);
		for (index, name) in ["a.jpg", "b.jpg", "c.jpg"].iter().enumerate() {
			correlator.observe(sighting(&format!("/dir/{name}"), index as u64 + 1));
		}

		let rename = correlator.observe(sighting("/dir/b.webp", 2)).unwrap();
		assert_eq!(rename.source, PathBuf::from("/dir/b.jpg"));
	}

	#[test]
	fn a_case_only_rename_is_a_fact_even_though_it_means_nothing() {
		// Whether a name asked for anything is the grammar's question, and it
		// needs the registry to answer it. The fact says only that the name
		// changed.
		let mut correlator = Correlator::new(16);
		correlator.observe(sighting("/dir/photo.heic", 1));

		let fact = correlator.observe(sighting("/dir/photo.HEIC", 1)).unwrap();

		assert_eq!(fact.source, PathBuf::from("/dir/photo.heic"));
		assert_eq!(fact.target, PathBuf::from("/dir/photo.HEIC"));
	}

	#[test]
	fn a_stem_change_is_not_paired() {
		let mut correlator = Correlator::new(16);
		correlator.observe(sighting("/dir/photo.heic", 1));

		assert!(correlator.observe(sighting("/dir/holiday.jpg", 1)).is_none());
	}

	#[test]
	fn cross_directory_is_a_move() {
		let mut correlator = Correlator::new(16);
		correlator.observe(sighting("/dir/a/photo.heic", 1));
		assert!(correlator.observe(sighting("/dir/b/photo.jpg", 1)).is_none());

		// The identity follows the file, so a later in-place rename still works.
		let rename = correlator.observe(sighting("/dir/b/photo.webp", 1)).unwrap();
		assert_eq!(rename.source, PathBuf::from("/dir/b/photo.jpg"));
	}

	#[test]
	fn a_hard_link_beside_the_known_name_is_not_a_rename() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("photo.heic");
		let link = dir.path().join("photo.png");
		fs::write(&source, b"heic").unwrap();

		let mut correlator = Correlator::new(16);
		correlator.observe(Sighting::of(&source).unwrap());

		fs::hard_link(&source, &link).unwrap();
		let linked = Sighting::of(&link).unwrap();
		assert_eq!(linked.links, 2, "the fixture is a hard link");
		assert!(correlator.observe(linked).is_none(), "a link is not a rename");
	}

	#[test]
	fn a_hard_linked_file_that_moved_is_a_rename() {
		let dir = tempfile::tempdir().unwrap();
		let undo = dir.path().join("undo");
		fs::create_dir(&undo).unwrap();
		let source = dir.path().join("photo.png");
		let target = dir.path().join("photo.jpg");
		fs::write(&source, b"pixels").unwrap();

		// The first sighting is before any link exists, as the watcher sees it.
		let mut correlator = Correlator::new(16);
		correlator.observe(Sighting::of(&source).unwrap());

		// A second user-created link must not hide a genuine rename.
		fs::hard_link(&source, undo.join("source.png")).unwrap();
		fs::rename(&source, &target).unwrap();

		let moved = Sighting::of(&target).unwrap();
		assert_eq!(moved.links, 2, "the fixture still has another link");
		let fact = correlator
			.observe(moved)
			.expect("the old name is gone, so nothing is ambiguous");

		assert_eq!(fact.source, source);
		assert_eq!(fact.target, target);
	}

	#[test]
	fn pair_reports_without_identity() {
		let mut correlator = Correlator::new(16);
		let rename = correlator
			.pair(Path::new("/dir/clip.mov"), Path::new("/dir/clip.mp4+mute"))
			.unwrap();

		assert_eq!(rename.source, PathBuf::from("/dir/clip.mov"));
		assert_eq!(rename.target, PathBuf::from("/dir/clip.mp4+mute"));
	}

	#[test]
	fn split_rename_events_are_paired() {
		let mut correlator = Correlator::new(16);
		correlator.rename_from(Path::new("/dir/a.heic"));
		let rename = correlator.rename_to(Path::new("/dir/a.jpg")).unwrap();

		assert_eq!(rename.source, PathBuf::from("/dir/a.heic"));
	}

	#[test]
	fn interleaved_split_events_are_paired_without_adjacency() {
		let mut correlator = Correlator::new(16);
		// As a batch may interleave: all sources first, then all targets.
		for name in ["a.heic", "b.heic", "c.heic"] {
			correlator.rename_from(Path::new("/dir/").join(name).as_path());
		}

		let first = correlator.rename_to(Path::new("/dir/a.jpg")).unwrap();
		let second = correlator.rename_to(Path::new("/dir/b.webp")).unwrap();

		assert_eq!(first.source, PathBuf::from("/dir/a.heic"));
		assert_eq!(second.source, PathBuf::from("/dir/b.heic"));
	}

	#[test]
	fn split_rename_in_another_directory_is_a_move() {
		let mut correlator = Correlator::new(16);
		correlator.rename_from(Path::new("/dir/a.heic"));
		assert!(correlator.rename_to(Path::new("/dir/sub/a.jpg")).is_none());
	}

	#[test]
	fn unmatched_source_is_dropped_at_batch_end() {
		let mut correlator = Correlator::new(16);
		correlator.rename_from(Path::new("/dir/a.heic"));
		correlator.end_batch();
		assert!(correlator.rename_to(Path::new("/dir/b.jpg")).is_none());
	}

	#[test]
	fn pending_sources_are_bounded() {
		let mut correlator = Correlator::new(16);
		correlator.pending_capacity = 2;
		for name in ["a", "b", "c"] {
			correlator.rename_from(Path::new(&format!("/dir/{name}.heic")));
		}
		// The oldest was evicted, so only `b` and `c` can match.
		assert!(correlator.rename_to(Path::new("/dir/a.jpg")).is_none());
		assert!(correlator.rename_to(Path::new("/dir/c.jpg")).is_some());
	}

	#[test]
	fn pair_with_different_stems_is_ignored() {
		let mut correlator = Correlator::new(16);
		assert!(
			correlator
				.pair(Path::new("/dir/a.mov"), Path::new("/dir/b.mp4"))
				.is_none()
		);
	}

	#[test]
	fn pair_keeps_identity_tracking() {
		let mut correlator = Correlator::new(16);
		correlator.observe(sighting("/dir/clip.mov", 7));
		correlator.pair(Path::new("/dir/clip.mov"), Path::new("/dir/clip.mp4"));

		correlator.observe(sighting("/dir/clip.mp4", 7));
		let rename = correlator.observe(sighting("/dir/clip.webm", 7)).unwrap();
		assert_eq!(rename.source, PathBuf::from("/dir/clip.mp4"));
	}

	#[test]
	fn forgetting_a_path_drops_the_identity() {
		let mut correlator = Correlator::new(16);
		correlator.observe(sighting("/dir/photo.heic", 1));
		correlator.forget(Path::new("/dir/photo.heic"));
		assert!(correlator.is_empty());

		// A recycled inode must not be read as a rename of the deleted file.
		assert!(correlator.observe(sighting("/dir/other.jpg", 1)).is_none());
	}

	#[test]
	fn directory_rename_invalidates_children() {
		let mut correlator = Correlator::new(16);
		correlator.observe(sighting("/dir/photos/a.heic", 1));
		correlator.observe(sighting("/dir/keep.heic", 2));

		correlator.invalidate_prefix(Path::new("/dir/photos"));

		assert_eq!(correlator.len(), 1);
		// The stale path is gone, so a rename under the new directory is not
		// reported as coming from the old one.
		assert!(correlator.observe(sighting("/dir/moved/a.jpg", 1)).is_none());
	}

	#[test]
	fn capacity_evicts_oldest() {
		let mut correlator = Correlator::new(2);
		correlator.observe(sighting("/dir/a.heic", 1));
		correlator.observe(sighting("/dir/b.heic", 2));
		correlator.observe(sighting("/dir/c.heic", 3));

		assert_eq!(correlator.len(), 2);
		// `a` was evicted, so its inode is unknown and no rename is reported.
		assert!(correlator.observe(sighting("/dir/a.jpg", 1)).is_none());
		// `c` is still known.
		assert!(correlator.observe(sighting("/dir/c.jpg", 3)).is_some());
	}

	#[test]
	fn duplicates_are_removed() {
		let fact = RenameFact {
			source: PathBuf::from("/dir/a.heic"),
			target: PathBuf::from("/dir/a.jpg"),
		};
		let unique = dedupe(vec![fact.clone(), fact.clone()]);
		assert_eq!(unique.len(), 1);
	}

	#[test]
	fn chain_collapses_to_one_fact() {
		let facts = vec![
			RenameFact {
				source: PathBuf::from("/dir/e.heic"),
				target: PathBuf::from("/dir/e.jpg"),
			},
			RenameFact {
				source: PathBuf::from("/dir/e.jpg"),
				target: PathBuf::from("/dir/e.webp"),
			},
		];

		let collapsed = coalesce(facts);

		assert_eq!(collapsed.len(), 1);
		assert_eq!(collapsed[0].source, PathBuf::from("/dir/e.heic"));
		assert_eq!(collapsed[0].target, PathBuf::from("/dir/e.webp"));
	}

	#[test]
	fn three_step_chain_collapses() {
		let facts = vec![
			RenameFact {
				source: PathBuf::from("/dir/a.heic"),
				target: PathBuf::from("/dir/a.jpg"),
			},
			RenameFact {
				source: PathBuf::from("/dir/a.jpg"),
				target: PathBuf::from("/dir/a.png"),
			},
			RenameFact {
				source: PathBuf::from("/dir/a.png"),
				target: PathBuf::from("/dir/a.webp"),
			},
		];

		let collapsed = coalesce(facts);

		assert_eq!(collapsed.len(), 1);
		assert_eq!(collapsed[0].source, PathBuf::from("/dir/a.heic"));
		assert_eq!(collapsed[0].target, PathBuf::from("/dir/a.webp"));
	}

	#[test]
	fn unrelated_facts_are_not_collapsed() {
		let facts = vec![
			RenameFact {
				source: PathBuf::from("/dir/a.heic"),
				target: PathBuf::from("/dir/a.jpg"),
			},
			RenameFact {
				source: PathBuf::from("/dir/b.heic"),
				target: PathBuf::from("/dir/b.webp"),
			},
		];

		assert_eq!(coalesce(facts).len(), 2);
	}

	#[test]
	fn a_compound_format_is_paired_like_any_other() {
		// The correlator compares what a stem can be, which is the text before
		// the first dot, so a format with two parts needs no special case here.
		let mut correlator = Correlator::new(16);
		correlator.observe(sighting("/dir/data.zip", 1));

		let rename = correlator.observe(sighting("/dir/data.tar.gz", 1)).unwrap();

		assert_eq!(rename.source, PathBuf::from("/dir/data.zip"));
		assert_eq!(rename.target, PathBuf::from("/dir/data.tar.gz"));
	}

	/// Deterministic pseudo random numbers, so the model test is reproducible
	/// without a dependency.
	struct XorShift(u64);

	impl XorShift {
		fn next(&mut self) -> u64 {
			self.0 ^= self.0 << 13;
			self.0 ^= self.0 >> 7;
			self.0 ^= self.0 << 17;
			self.0
		}

		fn below(&mut self, bound: u64) -> u64 {
			self.next() % bound
		}
	}

	#[test]
	fn model_agrees_on_random_rename_sequences() {
		const FORMATS: [&str; 5] = ["heic", "jpg", "webp", "png", "tar.gz"];

		for seed in 1..=8u64 {
			let mut rng = XorShift(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
			let mut correlator = Correlator::new(4096);
			// Model: identity -> current path, plus each path's current extension.
			let mut live: HashMap<u64, PathBuf> = HashMap::new();
			let mut next_inode = 1u64;

			for _ in 0..200 {
				let choice = rng.below(10);

				if choice < 3 || live.is_empty() {
					// Create a file.
					let inode = next_inode;
					next_inode += 1;
					let path = PathBuf::from(format!("/dir/file{inode}.{}", FORMATS[0]));
					correlator.observe(sighting(path.to_str().unwrap(), inode));
					live.insert(inode, path);
				} else if choice < 8 {
					// Rename one file to a new extension.
					let mut inodes: Vec<u64> = live.keys().copied().collect();
					inodes.sort_unstable();
					let inode = inodes[rng.below(inodes.len() as u64) as usize];
					let current = live[&inode].clone();
					let name = current.file_name().unwrap().to_str().unwrap();
					let stem = name.split_once('.').map(|(stem, _)| stem).unwrap();
					let ext = FORMATS[rng.below(FORMATS.len() as u64) as usize];
					let target = PathBuf::from(format!("/dir/{stem}.{ext}"));

					// The correlator pairs names that share a stem prefix, so a
					// target that differs from the current path is a fact and a
					// target that repeats it is not.
					let expected = target != current;
					let rename = correlator.observe(sighting(target.to_str().unwrap(), inode));

					assert_eq!(
						rename.is_some(),
						expected,
						"seed {seed}: inode {inode} {} -> {} should {} be a rename",
						current.display(),
						target.display(),
						if expected { "" } else { "not" }
					);
					if let Some(rename) = rename {
						assert_eq!(rename.source, current);
						assert_eq!(rename.target, target);
					}
					live.insert(inode, target);
				} else {
					// Delete one file.
					let mut inodes: Vec<u64> = live.keys().copied().collect();
					inodes.sort_unstable();
					let inode = inodes[rng.below(inodes.len() as u64) as usize];
					let path = live.remove(&inode).unwrap();
					correlator.forget(&path);
				}
			}
		}
	}
}
