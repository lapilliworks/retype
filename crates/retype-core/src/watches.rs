//! Watch registrations, `docs/dev/architecture/state.md` §5.1.
//!
//! The daemon owns `watches.toml`: `watch` appends, `unwatch` removes, and the
//! daemon re-reads it on start so registrations survive a reboot. The file is
//! hand-editable and validated like any other configuration file, so a path a
//! user typed into it by hand behaves the same as one the command wrote.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Code, Diagnostic, Error, Result};
use crate::job::now_ms;

/// Schema identifier, `docs/dev/architecture/state.md` §5.1.
pub const WATCHES_VERSION: &str = "retype-watches-1";

/// One registered directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
	pub path: PathBuf,
	pub recursive: bool,
	/// When it was registered, in milliseconds since the epoch.
	pub added: i64,
	/// Paused by `retype watch --pause`. A directory file's `paused = true`
	/// outranks this, `docs/watch.md` §8.
	#[serde(default)]
	pub paused: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Document {
	version: String,
	#[serde(default, rename = "watch")]
	watches: Vec<Registration>,
}

pub fn path(state_dir: &Path) -> PathBuf {
	state_dir.join("watches.toml")
}

/// Every registration, in the order they were added.
pub fn load(state_dir: &Path) -> Result<Vec<Registration>> {
	let path = path(state_dir);

	let text = match std::fs::read_to_string(&path) {
		Ok(text) => text,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
		Err(error) => return Err(Error::Io(error)),
	};

	let document: Document = toml::from_str(&text).map_err(|error| {
		Error::Diagnostic(Diagnostic::global(
			Code::CfgParse,
			format!("{}: {error}", path.display()),
		))
	})?;

	if document.version != WATCHES_VERSION {
		return Err(Error::Diagnostic(Diagnostic::global(
			Code::CfgVersion,
			format!(
				"{} declares '{}', this build reads '{WATCHES_VERSION}'",
				path.display(),
				document.version
			),
		)));
	}

	Ok(document.watches)
}

/// Register a directory. Registering one twice is not an error: the first
/// registration stands, because the alternative is a second entry that a removal
/// then fails to describe.
pub fn register(state_dir: &Path, directory: &Path, recursive: bool) -> Result<Registration> {
	let canonical = canonical(directory)?;
	let mut registrations = load(state_dir)?;

	if let Some(existing) = registrations.iter().find(|registration| registration.path == canonical) {
		return Ok(existing.clone());
	}

	let registration = Registration {
		path: canonical,
		recursive,
		added: now_ms(),
		paused: false,
	};

	registrations.push(registration.clone());
	save(state_dir, &registrations)?;

	Ok(registration)
}

/// Remove a registration by canonical path. Removing something absent succeeds.
pub fn unregister(state_dir: &Path, directory: &Path) -> Result<bool> {
	// A registration whose path has vanished is still removable, which is the
	// only way to clean up after a deleted directory. The name a registration is
	// stored under is canonical, so a leaf that no longer exists is resolved by
	// canonicalising its parent and keeping the same name; that is what matches
	// macOS's `/var` symlink and the verbatim prefix Windows adds.
	let wanted = comparable(directory)?;

	let mut registrations = load(state_dir)?;
	let before = registrations.len();

	registrations.retain(|registration| registration.path != wanted);

	if registrations.len() == before {
		return Ok(false);
	}

	save(state_dir, &registrations)?;
	Ok(true)
}

/// The name a registration for `directory` is stored under, whether or not the
/// directory still exists.
fn comparable(directory: &Path) -> Result<PathBuf> {
	if let Ok(canonical) = std::fs::canonicalize(directory) {
		return Ok(canonical);
	}

	if let (Some(parent), Some(name)) = (directory.parent(), directory.file_name()) {
		if let Ok(parent) = std::fs::canonicalize(parent) {
			return Ok(parent.join(name));
		}
	}

	Ok(std::path::absolute(directory)?)
}

/// Set the paused state of a registration, creating it when absent. A path that
/// does not exist cannot be registered, which is the same answer `register` gives.
pub fn set_paused(state_dir: &Path, directory: &Path, recursive: bool, paused: bool) -> Result<Registration> {
	let canonical = canonical(directory)?;
	let mut registrations = load(state_dir)?;

	if let Some(existing) = registrations.iter_mut().find(|existing| existing.path == canonical) {
		existing.paused = paused;
		let registration = existing.clone();
		save(state_dir, &registrations)?;
		return Ok(registration);
	}

	let registration = Registration {
		path: canonical,
		recursive,
		added: now_ms(),
		paused,
	};

	registrations.push(registration.clone());
	save(state_dir, &registrations)?;
	Ok(registration)
}

fn save(state_dir: &Path, registrations: &[Registration]) -> Result<()> {
	std::fs::create_dir_all(state_dir)?;
	set_private_directory(state_dir)?;

	let document = Document {
		version: WATCHES_VERSION.to_string(),
		watches: registrations.to_vec(),
	};

	let text = toml::to_string(&document).map_err(|error| {
		Error::Diagnostic(Diagnostic::global(
			Code::InternalInvariant,
			format!("watch registrations do not serialize: {error}"),
		))
	})?;

	let path = path(state_dir);
	crate::convert::write_atomically(&path, |temporary| {
		std::fs::write(temporary, text)?;
		set_private_file(temporary)?;
		Ok(())
	})
}

#[cfg(unix)]
fn set_private_directory(path: &Path) -> Result<()> {
	use std::os::unix::fs::PermissionsExt;
	std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
	Ok(())
}

#[cfg(not(unix))]
fn set_private_directory(_path: &Path) -> Result<()> {
	Ok(())
}

#[cfg(unix)]
fn set_private_file(path: &Path) -> Result<()> {
	use std::os::unix::fs::PermissionsExt;
	std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
	Ok(())
}

#[cfg(not(unix))]
fn set_private_file(_path: &Path) -> Result<()> {
	Ok(())
}

/// A registration names a directory, and two spellings of one directory must not
/// become two registrations.
fn canonical(directory: &Path) -> Result<PathBuf> {
	match std::fs::canonicalize(directory) {
		Ok(path) => Ok(path),
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(Error::Io(error)),
		Err(error) => Err(Error::Io(error)),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn directory() -> tempfile::TempDir {
		tempfile::tempdir().unwrap()
	}

	#[test]
	fn an_absent_file_has_no_registrations() {
		let state = directory();

		assert!(load(state.path()).unwrap().is_empty());
	}

	#[test]
	fn a_registration_round_trips() {
		let state = directory();
		let watched = directory();

		let registration = register(state.path(), watched.path(), true).unwrap();
		let loaded = load(state.path()).unwrap();

		assert_eq!(loaded, vec![registration]);
		assert_eq!(loaded[0].path, std::fs::canonicalize(watched.path()).unwrap());
		assert!(loaded[0].recursive);
	}

	#[cfg(unix)]
	#[test]
	fn registration_does_not_follow_an_existing_registry_symlink_for_writing() {
		use std::os::unix::fs::symlink;

		let state = directory();
		let watched = directory();
		let unrelated = state.path().join("unrelated.toml");
		let original = format!("version = '{WATCHES_VERSION}'\n");
		std::fs::write(&unrelated, &original).unwrap();
		symlink(&unrelated, path(state.path())).unwrap();

		register(state.path(), watched.path(), true).unwrap();

		assert_eq!(std::fs::read_to_string(unrelated).unwrap(), original);
		assert_eq!(load(state.path()).unwrap().len(), 1);
		assert!(
			!std::fs::symlink_metadata(path(state.path()))
				.unwrap()
				.file_type()
				.is_symlink()
		);
	}

	#[test]
	fn registering_twice_keeps_one_entry() {
		let state = directory();
		let watched = directory();

		let first = register(state.path(), watched.path(), true).unwrap();
		let second = register(state.path(), watched.path(), false).unwrap();

		assert_eq!(first, second, "the first registration stands");
		assert_eq!(load(state.path()).unwrap().len(), 1);
	}

	#[test]
	fn a_relative_path_and_its_canonical_form_are_one_registration() {
		let state = directory();
		let watched = directory();
		let nested = watched.path().join("photos");
		std::fs::create_dir(&nested).unwrap();

		register(state.path(), &nested, true).unwrap();
		let dotted = nested.join("..").join("photos");
		register(state.path(), &dotted, true).unwrap();

		assert_eq!(load(state.path()).unwrap().len(), 1);
	}

	#[test]
	fn removal_is_by_canonical_path_and_is_idempotent() {
		let state = directory();
		let watched = directory();

		register(state.path(), watched.path(), true).unwrap();
		assert!(unregister(state.path(), watched.path()).unwrap());
		assert!(!unregister(state.path(), watched.path()).unwrap(), "already absent");
		assert!(load(state.path()).unwrap().is_empty());
	}

	#[test]
	fn a_hand_edited_file_is_read_like_any_other() {
		let state = directory();
		let watched = directory();
		std::fs::create_dir_all(state.path()).unwrap();
		std::fs::write(
			path(state.path()),
			format!(
				"version = '{WATCHES_VERSION}'\n\n[[watch]]\npath = '{}'\nrecursive = false\nadded = 1\n",
				std::fs::canonicalize(watched.path()).unwrap().display()
			),
		)
		.unwrap();

		let loaded = load(state.path()).unwrap();

		assert_eq!(loaded.len(), 1);
		assert!(!loaded[0].recursive);
	}

	#[test]
	fn an_unknown_version_is_refused() {
		let state = directory();
		std::fs::write(path(state.path()), "version = 'retype-watches-2'\n").unwrap();

		let error = load(state.path()).unwrap_err();

		assert_eq!(error.code(), Code::CfgVersion);
	}

	#[test]
	fn a_directory_that_does_not_exist_cannot_be_registered() {
		let state = directory();

		assert!(register(state.path(), &state.path().join("absent"), true).is_err());
	}
}
