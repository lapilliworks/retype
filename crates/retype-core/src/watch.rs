//! What the daemon considers at all, `docs/watch.md` §4 and §6.
//!
//! Three rules, evaluated before a directory has a say: the built-in directory list,
//! hidden names, and the user's own globs. A directory's `.retype.toml` narrows what is
//! left, which is why this comes first: a directory cannot un-ignore `.git`.

use std::path::{Component, Path};

/// Directory names the daemon never descends into.
///
/// A rename inside `target/` or `.git/` is real, and nothing wants it converted. The
/// list is fixed rather than configurable because a machine that tunes it then behaves
/// unlike every other machine, `docs/watch.md` §5.
pub const BUILT_IN: [&str; 11] = [
	".git",
	".hg",
	".svn",
	"node_modules",
	"target",
	".venv",
	"__pycache__",
	"System Volume Information",
	"$RECYCLE.BIN",
	".Trash",
	".Trashes",
];

/// The limits a ten minute video teaches, `docs/watch.md` §6.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
	/// Additional globs. They add to the built-in list and cannot remove from it.
	pub ignore: Vec<String>,
	/// Whether dot-files and dot-directories are considered.
	pub watch_hidden: bool,
	/// Whether a placeholder-capable provider is converted inside anyway.
	pub allow_synced: bool,
	/// The largest source an automatic conversion will touch.
	pub max_source_size: u64,
}

impl Default for Policy {
	fn default() -> Self {
		Self {
			ignore: Vec::new(),
			watch_hidden: false,
			allow_synced: false,
			max_source_size: 4 * 1024 * 1024 * 1024,
		}
	}
}

impl Policy {
	/// Whether a path inside `root` is the daemon's business.
	///
	/// A rename has two names and a caller checks both: the globs are about the file,
	/// and the file is the same file under either name.
	pub fn considers(&self, root: &Path, path: &Path) -> bool {
		let relative = path.strip_prefix(root).unwrap_or(path);

		// Every directory between the root and the file: a pruned subtree is pruned for
		// everything under it.
		let directories = relative.parent().unwrap_or(Path::new(""));

		for component in directories.components() {
			let Component::Normal(name) = component else {
				continue;
			};

			if BUILT_IN.iter().any(|built| name.eq_ignore_ascii_case(built)) {
				return false;
			}

			if !self.watch_hidden && is_hidden(name) {
				return false;
			}
		}

		let Some(name) = relative.file_name() else {
			return false;
		};

		if !self.watch_hidden && is_hidden(name) {
			return false;
		}

		// A pattern with a separator is about the path, one without is about the name,
		// the same rule `patterns` follows, `docs/config.md` §12.
		let name = name.to_string_lossy();
		let relative = relative.to_string_lossy();

		!self.ignore.iter().any(|pattern| match pattern.contains('/') {
			true => crate::policy::glob(pattern, &relative),
			false => crate::policy::glob(pattern, &name),
		})
	}

	/// Whether a source of this size may be converted without being asked.
	pub fn admits(&self, bytes: u64) -> bool {
		bytes <= self.max_source_size
	}
}

fn is_hidden(name: &std::ffi::OsStr) -> bool {
	name.to_string_lossy().starts_with('.')
}

#[cfg(test)]
mod tests {
	use super::*;

	fn policy() -> Policy {
		Policy::default()
	}

	#[test]
	fn a_built_in_directory_prunes_everything_under_it() {
		let root = Path::new("/photos");

		assert!(!policy().considers(root, Path::new("/photos/.git/objects/a.png")));
		assert!(!policy().considers(root, Path::new("/photos/target/debug/x.png")));
		assert!(!policy().considers(root, Path::new("/photos/a/node_modules/b/x.png")));
	}

	#[test]
	fn a_file_named_like_a_built_in_directory_is_still_a_file() {
		let root = Path::new("/photos");

		assert!(policy().considers(root, Path::new("/photos/target.png")));
	}

	#[test]
	fn hidden_names_are_skipped_unless_asked_for() {
		let root = Path::new("/photos");
		let mut policy = policy();

		assert!(!policy.considers(root, Path::new("/photos/.thumbnails/a.png")));
		assert!(!policy.considers(root, Path::new("/photos/.a.png")));

		policy.watch_hidden = true;

		assert!(policy.considers(root, Path::new("/photos/.thumbnails/a.png")));
		assert!(policy.considers(root, Path::new("/photos/.a.png")));
	}

	#[test]
	fn a_user_glob_adds_to_the_list_and_cannot_remove_from_it() {
		let root = Path::new("/photos");
		let policy = Policy {
			ignore: vec!["*.tmp".to_string(), "**/Caches/**".to_string()],
			..policy()
		};

		assert!(!policy.considers(root, Path::new("/photos/a.png.tmp")));
		assert!(!policy.considers(root, Path::new("/photos/Library/Caches/a.png")));
		assert!(policy.considers(root, Path::new("/photos/a.png")));

		// Ignoring cannot be undone by the same list.
		assert!(!policy.considers(root, Path::new("/photos/.git/a.png")));
	}

	#[test]
	fn the_size_ceiling_admits_its_own_value_and_nothing_larger() {
		let policy = policy();

		assert!(policy.admits(policy.max_source_size));
		assert!(!policy.admits(policy.max_source_size + 1));
	}
}
