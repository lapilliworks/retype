//! Where state lives, through the shared client crate.
//!
//! The layout is shared with the GUI. This module is the command line's spelling
//! of it, kept so the daemon and the commands do not have to import the client
//! crate's module path everywhere.

use std::path::{Path, PathBuf};

use retype_core::job::JobId;

use crate::error::Result;

pub use retype_client::state::ENDPOINT_FILE;

pub fn resolve(explicit: Option<PathBuf>) -> Result<PathBuf> {
	Ok(retype_client::state::resolve(explicit)?)
}

pub fn publish_endpoint(state_dir: &Path, endpoint: &str) -> Result<()> {
	Ok(retype_client::state::publish_endpoint(state_dir, endpoint)?)
}

pub fn read_endpoint(state_dir: &Path) -> Result<String> {
	match retype_client::state::read_endpoint(state_dir) {
		Ok(endpoint) => Ok(endpoint),
		Err(retype_client::Error::DaemonUnavailable) => Err(crate::error::Error::DaemonUnavailable),
		Err(error) => Err(error.into()),
	}
}

pub fn clear_endpoint(state_dir: &Path) {
	retype_client::state::clear_endpoint(state_dir)
}

pub fn state_dir() -> Result<PathBuf> {
	Ok(retype_client::state::state_dir()?)
}

pub fn log_path(state_dir: &Path, id: &JobId) -> PathBuf {
	retype_client::state::log_path(state_dir, id)
}

#[cfg(unix)]
pub fn socket_path(state_dir: &Path) -> Result<PathBuf> {
	Ok(retype_client::state::socket_path(state_dir)?)
}

pub fn ensure_private_directory(path: &Path) -> Result<()> {
	Ok(retype_client::state::ensure_private_directory(path)?)
}
