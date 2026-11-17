//! The installed watcher's state, separate from persisted registrations.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
	Starting,
	Ready,
	Stopping,
	Degraded(String),
	Failed(String),
}

/// Shared by the supervisor and one worker. A replacement gets a new handle,
/// so an old worker cannot mark its successor ready.
#[derive(Debug)]
pub struct Root {
	pub recursive: bool,
	pub identity: Option<retype_core::correlate::Identity>,
	phase: Mutex<Phase>,
}

impl Root {
	pub fn new(recursive: bool, identity: Option<retype_core::correlate::Identity>) -> Self {
		Self {
			recursive,
			identity,
			phase: Mutex::new(Phase::Starting),
		}
	}

	pub fn phase(&self) -> Phase {
		self.phase
			.lock()
			.map(|phase| phase.clone())
			.unwrap_or_else(|_| Phase::Failed("watcher status is unavailable".to_string()))
	}

	pub fn ready(&self) {
		if let Ok(mut phase) = self.phase.lock() {
			if *phase == Phase::Starting {
				*phase = Phase::Ready;
			}
		}
	}

	pub fn degraded(&self, message: impl Into<String>) {
		if let Ok(mut phase) = self.phase.lock() {
			if !matches!(*phase, Phase::Failed(_) | Phase::Stopping) {
				*phase = Phase::Degraded(message.into());
			}
		}
	}

	pub fn fail(&self, message: impl Into<String>) {
		if let Ok(mut phase) = self.phase.lock() {
			*phase = Phase::Failed(message.into());
		}
	}

	pub fn stopping(&self) {
		if let Ok(mut phase) = self.phase.lock() {
			*phase = Phase::Stopping;
		}
	}
}

#[derive(Default)]
struct State {
	roots: BTreeMap<PathBuf, Arc<Root>>,
	registry_error: Option<String>,
}

#[derive(Default)]
pub struct WatchHealth {
	state: Mutex<State>,
}

impl WatchHealth {
	pub fn set_root(&self, path: &Path, root: Arc<Root>) {
		self.state
			.lock()
			.expect("watch status lock poisoned")
			.roots
			.insert(path.to_path_buf(), root);
	}

	pub fn remove_root(&self, path: &Path) {
		self.state
			.lock()
			.expect("watch status lock poisoned")
			.roots
			.remove(path);
	}

	pub fn clear_roots(&self) {
		self.state.lock().expect("watch status lock poisoned").roots.clear();
	}

	pub fn retain_roots(&self, wanted: &BTreeMap<PathBuf, bool>) {
		self.state
			.lock()
			.expect("watch status lock poisoned")
			.roots
			.retain(|path, root| wanted.get(path) == Some(&root.recursive));
	}

	pub fn phase(&self, path: &Path, recursive: bool) -> Option<Phase> {
		let state = match self.state.lock() {
			Ok(state) => state,
			Err(_) => return Some(Phase::Failed("watcher status is unavailable".to_string())),
		};
		let root = state.roots.get(path).cloned();
		drop(state);
		root.map(|root| {
			let matches = root.recursive == recursive
				&& root.identity.is_none_or(|identity| {
					retype_core::correlate::Identity::of(path).is_some_and(|current| current == identity)
				});
			if matches { root.phase() } else { Phase::Stopping }
		})
	}

	pub fn set_registry_error(&self, error: Option<String>) {
		self.state.lock().expect("watch status lock poisoned").registry_error = error;
	}

	pub fn registry_error(&self) -> Option<String> {
		match self.state.lock() {
			Ok(state) => state.registry_error.clone(),
			Err(_) => Some("watcher status is unavailable".to_string()),
		}
	}

	pub fn supervisor_failed(&self) {
		if let Ok(mut state) = self.state.lock() {
			state.roots.clear();
			state.registry_error = Some("watcher supervisor stopped unexpectedly".to_string());
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn an_old_worker_cannot_mark_its_replacement_ready() {
		let health = WatchHealth::default();
		let path = Path::new("/photos");
		let old = Arc::new(Root::new(true, None));
		health.set_root(path, Arc::clone(&old));
		let replacement = Arc::new(Root::new(true, None));
		health.set_root(path, Arc::clone(&replacement));

		old.ready();
		assert_eq!(health.phase(path, true), Some(Phase::Starting));
		replacement.ready();
		assert_eq!(health.phase(path, true), Some(Phase::Ready));
	}

	#[test]
	fn failure_or_degradation_cannot_be_overwritten_by_late_priming() {
		let root = Root::new(true, None);
		root.degraded("events were lost");
		root.ready();
		assert_eq!(root.phase(), Phase::Degraded("events were lost".to_string()));
		root.fail("watcher stopped");
		root.degraded("another issue");
		assert_eq!(root.phase(), Phase::Failed("watcher stopped".to_string()));
	}

	#[cfg(unix)]
	#[test]
	fn a_replaced_directory_is_not_reported_ready() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("photos");
		std::fs::create_dir(&path).unwrap();
		let health = WatchHealth::default();
		let root = Arc::new(Root::new(true, retype_core::correlate::Identity::of(&path)));
		root.ready();
		health.set_root(&path, root);
		std::fs::rename(&path, dir.path().join("old-photos")).unwrap();
		std::fs::create_dir(&path).unwrap();

		assert_eq!(health.phase(&path, true), Some(Phase::Stopping));
	}

	#[test]
	fn a_changed_recursion_mode_is_not_reported_ready() {
		let health = WatchHealth::default();
		let root = Arc::new(Root::new(true, None));
		root.ready();
		health.set_root(Path::new("/photos"), root);

		assert_eq!(health.phase(Path::new("/photos"), false), Some(Phase::Stopping));
	}
}
