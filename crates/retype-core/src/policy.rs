//! A directory's policy, `docs/config.md` §12.
//!
//! A `.retype.toml` beside the files says what this directory wants done, and it may
//! only *narrow*: it can forbid an operation, clamp a parameter, restrict which files
//! are considered, ask for confirmation, or take itself out of use. It cannot name a
//! tool, a format, an operation or a preset, because a directory can arrive with a
//! repository from anywhere.
//!
//! The nearest file wins, walking up from the file's own directory and stopping at the
//! watch root, so a subdirectory can tighten what its parent allows, `docs/watch.md`
//! §2.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

use crate::error::{Code, Diagnoses, Diagnostic, Error, Result};
use crate::resolve::Resolved;
use crate::value::Value;

/// The file a directory speaks through.
pub const FILE: &str = ".retype.toml";

/// What a directory asks for. Every field is an addition to the default, which is
/// "convert everything it can, and ask when it is expensive".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
	/// Globs the file name or the path relative to the directory must match. Empty
	/// means everything.
	pub patterns: Vec<String>,
	/// Operations refused here. Denying never adds anything.
	pub deny: Vec<String>,
	/// Upper bounds for operation parameters, by operation name.
	pub limits: BTreeMap<String, u64>,
	/// Whether an expensive rename is confirmed here, `docs/deferral.md` §5.
	pub confirm: bool,
	/// Whether the directory is watched at all.
	pub paused: bool,
	/// Where the policy came from, for a message that names the file.
	pub path: PathBuf,
}

impl Policy {
	/// A policy that narrows nothing, which is what a directory without a file has.
	fn open_ended(path: &Path) -> Self {
		Self {
			patterns: Vec::new(),
			deny: Vec::new(),
			limits: BTreeMap::new(),
			confirm: true,
			paused: false,
			path: path.to_path_buf(),
		}
	}

	/// Whether this directory is interested in a file.
	pub fn allows(&self, name: &str, relative: &Path) -> bool {
		if self.patterns.is_empty() {
			return true;
		}

		self.patterns.iter().any(|pattern| {
			let target = match pattern.contains('/') {
				true => relative.to_string_lossy(),
				false => name.into(),
			};

			glob(pattern, target.as_ref())
		})
	}

	/// Read a directory's file, or answer with the policy of a directory that has none.
	pub fn open(directory: &Path) -> Result<Self> {
		let path = directory.join(FILE);

		let text = match fs::read_to_string(&path) {
			Ok(text) => text,
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
				return Ok(Self::open_ended(&path));
			}
			Err(error) => return Err(Error::Io(error)),
		};

		Self::parse(&text, &path)
	}

	fn parse(text: &str, path: &Path) -> Result<Self> {
		let document: Document = toml::from_str(text).map_err(|error| {
			Error::Diagnostic(Diagnostic::global(
				Code::CfgParse,
				format!("{}: {error}", path.display()),
			))
		})?;

		let mut policy = Self::open_ended(path);

		if document.version != crate::registry::CONFIG_VERSION {
			return Err(Error::Diagnostic(Diagnostic::global(
				Code::CfgVersion,
				format!(
					"{} declares '{}', this build reads '{}'",
					path.display(),
					document.version,
					crate::registry::CONFIG_VERSION
				),
			)));
		}

		// V13 and V23: a directory narrows policy and cannot introduce a command or a
		// machine knob.
		let mut refused = Vec::new();

		for (key, what) in [
			("tools", "a tool"),
			("formats", "a format"),
			("ops", "an operation"),
			("presets", "a preset"),
			("watch", "a watch setting"),
			("backends", "a backend order"),
			("undo", "an undo setting"),
		] {
			if document.keys.contains_key(key) {
				refused.push(format!("'{key}' declares {what}"));
			}
		}

		if let Some(deferral) = &document.deferral {
			for key in deferral.keys.keys() {
				if key != "confirm" {
					refused.push(format!("[deferral] '{key}' is a machine setting"));
				}
			}
		}

		if !refused.is_empty() {
			return Err(Error::Diagnostic(Diagnostic::global(
				Code::CfgValidation,
				format!(
					"{} may only narrow this directory's policy: {}",
					path.display(),
					refused.join(", ")
				),
			)));
		}

		policy.patterns = document.patterns;
		policy.deny = document.deny;
		policy.limits = document.limits;
		policy.confirm = document
			.confirm
			.or_else(|| document.deferral.as_ref().and_then(|deferral| deferral.confirm))
			.unwrap_or(true);
		policy.paused = document.paused.unwrap_or(false);

		Ok(policy)
	}

	/// Apply the policy to an intent that has already resolved, `docs/config.md` §12.
	///
	/// A denied operation is a diagnosis rather than a silent removal: the file keeps
	/// its name, and the user is told which directory refused what. A limited parameter
	/// is clamped, and both spellings are rewritten so the derived name says what the
	/// file will actually be.
	pub fn apply(&self, resolved: &mut Resolved) -> Diagnoses {
		let mut problems = Vec::new();

		for group in &mut resolved.groups {
			for op in &mut group.ops {
				if self.deny.iter().any(|denied| denied == &op.name || denied == &op.key) {
					problems.push(Error::Diagnostic(Diagnostic::global(
						Code::Inapplicable,
						format!("'{}' is refused in this directory by {}", op.name, self.path.display()),
					)));
					continue;
				}

				let Some(limit) = self.limits.get(&op.name).or_else(|| self.limits.get(&op.key)) else {
					continue;
				};

				let Some(argument) = &mut op.value else {
					continue;
				};

				let clamped = clamp(&argument.parsed, *limit);

				if clamped == argument.parsed {
					continue;
				}

				// The written form is what a derived name is built from, so both have to
				// say the same thing or the file's name would lie about the dial.
				argument.written.text = crate::print::value(&clamped);
				argument.parsed = clamped;
			}
		}

		Diagnoses::new(problems)
	}
}

/// A parameter cut down to `limit`, or unchanged when the limit does not apply to its
/// class.
///
/// Only the classes a number can bound are clamped. A ratio, an enumeration, a tag or a
/// phrase has no ceiling, so a limit on one is a no-op rather than a guess about what
/// the author meant.
fn clamp(value: &Value, limit: u64) -> Value {
	match value {
		Value::Int(number) => Value::Int((*number).min(limit)),
		Value::Size(bytes) => Value::Size((*bytes).min(limit)),
		Value::Time(seconds) => Value::Time((*seconds).min(limit)),
		// A dimension is one number or two, and a limit on it is the number neither end
		// may exceed.
		Value::Dim { width, height } => {
			let limit = u32::try_from(limit).unwrap_or(u32::MAX);

			Value::Dim {
				width: (*width).min(limit),
				height: (*height).min(limit),
			}
		}
		other => other.clone(),
	}
}

/// The nearest policy for `directory`, walking up to `root`.
///
/// A file between the directory and the root wins over one above it, and the walk never
/// leaves the registration: a policy outside the watched tree is not this tree's
/// business.
pub fn nearest(root: &Path, directory: &Path) -> PathBuf {
	let mut at = directory;

	loop {
		let candidate = at.join(FILE);

		if candidate.exists() {
			return candidate;
		}

		if at == root || !at.starts_with(root) {
			return at.join(FILE);
		}

		match at.parent() {
			Some(parent) => at = parent,
			None => return at.join(FILE),
		}
	}
}

/// Either the policy a directory answers with, or the reason it could not be read.
pub type Cached = std::result::Result<Policy, String>;

/// One file's answer, and when it was read.
type Entry = (Option<SystemTime>, std::sync::Arc<Cached>);

/// A per-directory cache, re-read when the file's modification time changes.
///
/// `docs/watch.md` §2 says the cache is invalidated by the configuration watcher. A
/// touched file is cheaper to notice than a second watcher is to run, and the answer is
/// the same: a policy change is visible on the next batch.
#[derive(Default)]
pub struct Cache {
	entries: std::sync::Mutex<BTreeMap<PathBuf, Entry>>,
}

impl Cache {
	pub fn policy(&self, root: &Path, directory: &Path) -> std::sync::Arc<Cached> {
		let path = nearest(root, directory);
		let modified = fs::metadata(&path).and_then(|metadata| metadata.modified()).ok();

		if let Ok(entries) = self.entries.lock() {
			if let Some((stamp, cached)) = entries.get(&path) {
				if *stamp == modified {
					return std::sync::Arc::clone(cached);
				}
			}
		}

		let cached = std::sync::Arc::new(match path.exists() {
			true => Policy::open(path.parent().unwrap_or(Path::new(""))).map_err(|error| error.to_string()),
			false => Ok(Policy::open_ended(&path)),
		});

		if let Ok(mut entries) = self.entries.lock() {
			entries.insert(path, (modified, std::sync::Arc::clone(&cached)));
		}

		cached
	}
}

#[derive(Debug, Deserialize)]
struct Document {
	version: String,
	#[serde(default)]
	patterns: Vec<String>,
	#[serde(default)]
	deny: Vec<String>,
	#[serde(default)]
	limits: BTreeMap<String, u64>,
	#[serde(default)]
	confirm: Option<bool>,
	#[serde(default)]
	paused: Option<bool>,
	#[serde(default)]
	deferral: Option<DeferralKeys>,
	#[serde(flatten)]
	keys: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Deserialize)]
struct DeferralKeys {
	/// The one key a directory may set, and it means the same as the top-level spelling.
	#[serde(default)]
	confirm: Option<bool>,
	#[serde(flatten)]
	keys: BTreeMap<String, toml::Value>,
}

/// A glob, as `patterns` spells one: `*` for anything but a separator, `**` for
/// anything, `?` for one character, and everything else literal.
pub fn glob(pattern: &str, text: &str) -> bool {
	let pattern: Vec<char> = pattern.chars().collect();
	let text: Vec<char> = text.chars().collect();

	matches(&pattern, &text)
}

fn matches(pattern: &[char], text: &[char]) -> bool {
	match pattern.first() {
		None => text.is_empty(),
		Some('*') => match pattern.get(1) {
			// `**` crosses directories; a single `*` stops at a separator.
			Some('*') => (0..=text.len()).any(|start| matches(&pattern[2..], &text[start..])),
			_ => {
				for start in 0..=text.len() {
					if start > 0 && text[start - 1] == '/' {
						break;
					}

					if matches(&pattern[1..], &text[start..]) {
						return true;
					}
				}

				false
			}
		},
		Some('?') => matches!(text.first(), Some(character) if *character != '/') && matches(&pattern[1..], &text[1..]),
		Some(literal) => text.first() == Some(literal) && matches(&pattern[1..], &text[1..]),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::intent;
	use crate::registry::Registry;
	use crate::resolve::{self, Source};

	fn directory(text: &str) -> Policy {
		Policy::parse(text, Path::new("/photos/.retype.toml")).expect("a valid policy")
	}

	#[test]
	fn a_directory_without_a_file_narrows_nothing() {
		let policy = Policy::open_ended(Path::new("/photos/.retype.toml"));

		assert!(policy.allows("a.heic", Path::new("a.heic")));
		assert!(policy.confirm);
		assert!(!policy.paused);
	}

	#[test]
	fn a_directory_may_not_define_a_tool_a_format_or_a_machine_setting() {
		for text in [
			"version = 'retype-config-1'\n[tools.copy]\ncommand = 'cp'\n",
			"version = 'retype-config-1'\n[formats.jpg]\nfamily = 'image'\n",
			"version = 'retype-config-1'\n[ops.quality]\nslot = 1\n",
			"version = 'retype-config-1'\n[presets.thumb]\ntokens = ['jpg']\n",
			"version = 'retype-config-1'\n[watch]\nignore = ['x']\n",
			"version = 'retype-config-1'\n[deferral]\nmode = 'now'\n",
		] {
			let error = Policy::parse(text, Path::new("/photos/.retype.toml")).unwrap_err();
			assert_eq!(error.code(), Code::CfgValidation, "{text}");
		}
	}

	#[test]
	fn a_directory_that_only_narrows_loads() {
		let policy = directory(
			"\
version = 'retype-config-1'
patterns = ['*.heic', 'shots/*.png']
deny = ['nometa', 'quality']
limits = { quality = 85 }
confirm = false
paused = true
[deferral]
confirm = false
",
		);

		assert_eq!(policy.patterns, vec!["*.heic", "shots/*.png"]);
		assert_eq!(policy.deny, vec!["nometa", "quality"]);
		assert_eq!(policy.limits.get("quality"), Some(&85));
		assert!(!policy.confirm);
		assert!(policy.paused);
	}

	#[test]
	fn a_version_this_build_does_not_read_is_refused() {
		let error = Policy::parse("version = 'retype-config-9'\n", Path::new("/p/.retype.toml")).unwrap_err();

		assert_eq!(error.code(), Code::CfgVersion);
	}

	#[test]
	fn patterns_match_the_name_or_the_path() {
		let policy = directory("version = 'retype-config-1'\npatterns = ['*.heic', 'shots/*.png']\n");

		assert!(policy.allows("a.heic", Path::new("a.heic")));
		assert!(policy.allows("b.png", Path::new("shots/b.png")));
		assert!(!policy.allows("b.png", Path::new("b.png")));
		assert!(!policy.allows("a.jpg", Path::new("a.jpg")));
	}

	#[test]
	fn a_glob_is_star_and_question_mark_and_nothing_else() {
		assert!(glob("*.jpg", "photo.jpg"));
		assert!(!glob("*.jpg", "photo.jpeg"));
		assert!(glob("photo.???", "photo.jpg"));
		assert!(glob("**/cache/**", "a/b/cache/x.jpg"));
		assert!(glob("a/*/c", "a/b/c"));
		assert!(!glob("a/*/c", "a/b/d/c"), "one star stays in one directory");
		assert!(glob("photo-2.jpg", "photo-2.jpg"));
	}

	#[test]
	fn a_limit_is_a_ceiling_and_a_deny_is_a_refusal() {
		let registry = Registry::builtin().expect("the shipped registry loads");
		let policy = directory("version = 'retype-config-1'\ndeny = ['nometa']\nlimits = { quality = 85 }\n");

		let parsed = intent::parse("webp+quality95+nometa").expect("scans");
		let mut resolved = resolve::resolve(
			&parsed,
			Source {
				format: "heic",
				is_directory: false,
			},
			&registry,
		)
		.expect("resolves");

		let problems = policy.apply(&mut resolved);

		assert_eq!(problems.len(), 1);
		let first = problems.first().expect("one refusal");
		assert_eq!(first.code(), Code::Inapplicable);
		assert!(first.to_string().contains("nometa"), "{first}");

		let quality = resolved.groups[0]
			.pipeline()
			.iter()
			.find(|op| op.key == "quality")
			.expect("quality survived");

		assert_eq!(quality.value.as_ref().unwrap().parsed, Value::Int(85));
		assert_eq!(quality.value.as_ref().unwrap().written.text, "85");
	}

	#[test]
	fn the_nearest_file_wins_and_the_walk_stops_at_the_root() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		let deep = root.join("a/b/c");
		fs::create_dir_all(&deep).unwrap();

		// With no file anywhere, the answer is the root's name for "none": a directory
		// below the root shares that answer, and a file appearing anywhere re-resolves.
		assert_eq!(nearest(root, &deep), root.join(FILE));

		fs::write(root.join("a").join(FILE), "version = 'retype-config-1'\n").unwrap();
		assert_eq!(nearest(root, &deep), root.join("a").join(FILE));

		fs::write(deep.join(FILE), "version = 'retype-config-1'\n").unwrap();
		assert_eq!(nearest(root, &deep), deep.join(FILE));
	}

	#[test]
	fn the_cache_notices_a_file_that_changed() {
		let dir = tempfile::tempdir().unwrap();
		let cache = Cache::default();

		let fresh = cache.policy(dir.path(), dir.path());
		assert!(!fresh.as_ref().clone().expect("a policy").paused);

		fs::write(dir.path().join(FILE), "version = 'retype-config-1'\npaused = true\n").unwrap();

		// A modification time is saved to the nanosecond on most filesystems, but not on
		// all: the sleep is what makes the change visible rather than the cache stale.
		std::thread::sleep(std::time::Duration::from_millis(20));

		let changed = cache.policy(dir.path(), dir.path());
		assert!(changed.as_ref().clone().expect("a policy").paused);
	}
}
