//! The user configuration, through the shared loader.
//!
//! `--config` is one fact about the process, so it stays here. Everything else
//! belongs to `retype-config`, which the daemon and the GUI also use.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use retype_core::registry::Registry;

use crate::error::{Error, Result};

/// The path `--config` named, if any.
static EXPLICIT: OnceLock<PathBuf> = OnceLock::new();

/// Record what `--config` said. A second call is a bug in the caller, not a
/// reason to panic: the first answer is the one the process ran with.
pub fn use_path(path: Option<PathBuf>) {
	if let Some(path) = path {
		let _ = EXPLICIT.set(path);
	}
}

fn explicit() -> Option<&'static Path> {
	EXPLICIT.get().map(PathBuf::as_path)
}

/// `--config` wins, then `RETYPE_CONFIG`, then the platform's directory.
pub fn user_path() -> Option<PathBuf> {
	retype_config::user_path(explicit())
}

/// The user configuration merged over the built-in table.
pub fn load() -> Result<Registry> {
	retype_config::load(explicit()).map_err(Error::from)
}

/// The same, from a named path: a missing file is not an error, a malformed one
/// is, `docs/reference/config.md` §2.
pub fn load_from(path: &Path) -> Result<Registry> {
	retype_config::load_from(path).map_err(Error::from)
}

/// Report what the configuration said in passing, on stderr, `docs/reference/config.md` §2.
pub fn report_warnings(registry: &Registry) {
	for warning in registry.warnings() {
		eprintln!("warning: {}: {}", warning.code, warning.message);
	}
}
