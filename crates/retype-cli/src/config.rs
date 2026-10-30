//! The user configuration and how it merges with the shipped one,
//! `docs/config.md` §2 and §12.
//!
//! One loader for both files: nothing is parsed twice and nothing is validated by
//! a second set of rules. A machine without a user file is not a special case, it
//! is the built-in table as it stands.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use retype_core::error::Diagnoses;
use retype_core::registry::Registry;

use crate::error::Error;

/// The path `--config` named, if any. It is one fact about the process, like the
/// state directory, so it is set once at startup rather than threaded through
/// every call that may need to read configuration.
static EXPLICIT: OnceLock<PathBuf> = OnceLock::new();

/// Record what `--config` said. A second call is a bug in the caller, not a
/// reason to panic: the first answer is the one the process ran with.
pub fn use_path(path: Option<PathBuf>) {
	if let Some(path) = path {
		let _ = EXPLICIT.set(path);
	}
}

/// `--config` wins, then `RETYPE_CONFIG`, then the platform's directory.
pub fn user_path() -> Option<PathBuf> {
	if let Some(explicit) = EXPLICIT.get() {
		return Some(explicit.clone());
	}

	if let Some(configured) = std::env::var_os("RETYPE_CONFIG") {
		if !configured.is_empty() {
			return Some(PathBuf::from(configured));
		}
	}

	config_dir().map(|base| base.join("retype").join("retype.toml"))
}

/// The user configuration merged over the built-in table.
pub fn load() -> std::result::Result<Registry, Error> {
	let Some(path) = user_path() else {
		return Registry::builtin().map_err(Error::Config);
	};

	// A file the caller named has to be there. Falling back to the built-in table
	// would answer a different question than the one that was asked.
	if EXPLICIT.get().is_some() {
		match std::fs::metadata(&path) {
			Ok(_) => {}
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
				return Err(Error::Config(Diagnoses::one(retype_core::error::Error::Diagnostic(
					retype_core::error::Diagnostic::global(
						retype_core::error::Code::CfgMissing,
						format!("{} does not exist", path.display()),
					),
				))));
			}
			Err(error) => return Err(Error::Io(error)),
		}
	}

	load_from(&path)
}

/// The same, from a named path: a missing file is not an error, a malformed one
/// is, `docs/config.md` §2.
pub fn load_from(path: &Path) -> std::result::Result<Registry, Error> {
	let text = match std::fs::read_to_string(path) {
		Ok(text) => text,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
			return Registry::builtin().map_err(Error::Config);
		}
		Err(error) => return Err(Error::Io(error)),
	};

	let label = path.display().to_string();
	Registry::with_user(&text, &label).map_err(Error::Config)
}

#[cfg(unix)]
fn config_dir() -> Option<PathBuf> {
	if cfg!(target_os = "macos") {
		return std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library").join("Application Support"));
	}

	std::env::var_os("XDG_CONFIG_HOME")
		.map(PathBuf::from)
		.filter(|path| path.is_absolute())
		.or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
}

#[cfg(windows)]
fn config_dir() -> Option<PathBuf> {
	std::env::var_os("APPDATA")
		.map(PathBuf::from)
		.or_else(|| std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join("AppData/Roaming")))
}

#[cfg(not(any(unix, windows)))]
fn config_dir() -> Option<PathBuf> {
	None
}

/// Report what the configuration said in passing, on stderr, `docs/config.md` §2.
pub fn report_warnings(registry: &Registry) {
	for warning in registry.warnings() {
		eprintln!("warning: {}: {}", warning.code, warning.message);
	}
}

/// The diagnostics of a configuration that will not load, as one error.
pub fn failure(problems: Diagnoses) -> Error {
	Error::Config(problems)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_missing_user_file_is_the_built_in_table() {
		let dir = tempfile::tempdir().unwrap();
		let absent = load_from(&dir.path().join("absent.toml")).expect("loads");
		let builtin = Registry::with_user("version = 'retype-config-1'\n", "test").expect("loads");

		assert_eq!(absent.formats().count(), builtin.formats().count());
		assert_eq!(absent.backends(), builtin.backends());
		assert!(absent.warnings().is_empty());
	}

	#[test]
	fn a_user_file_reaches_the_registry() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("retype.toml");
		std::fs::write(
			&path,
			"version = 'retype-config-1'\n\n[ops.quality.argv.magick]\nargs = ['-quality', '{value}']\n",
		)
		.unwrap();

		let registry = load_from(&path).expect("loads");
		let op = registry.op("quality").expect("declared");

		assert!(op.fragment("magick").is_some());
	}

	#[test]
	fn a_broken_user_file_is_a_configuration_error() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("retype.toml");
		std::fs::write(&path, "version = 'retype-config-1'\n\n[ops.quality]\nslot = 1\n").unwrap();

		assert!(load_from(&path).is_err());
	}
}
