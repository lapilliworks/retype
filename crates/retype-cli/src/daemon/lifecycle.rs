//! Persisted restart budgets and clean-stop intent, shared by all followers.
use retype_core::error::{Code, Diagnostic, Error, Result};
use retype_core::job::JobId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Lifecycle {
	pub boot: String,
	pub state: String,
	pub cause: Option<String>,
	pub counts: BTreeMap<String, u32>,
	pub unknown: u32,
	pub counted_boot: Option<String>,
}

pub fn read(state: &Path) -> Result<Lifecycle> {
	match std::fs::read(state.join("lifecycle.json")) {
		Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| Error::Json(error.to_string())),
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Lifecycle::default()),
		Err(error) => Err(error.into()),
	}
}
fn write(state: &Path, lifecycle: &Lifecycle) -> Result<()> {
	let bytes = serde_json::to_vec(lifecycle).map_err(|error| Error::Json(error.to_string()))?;
	retype_core::convert::write_atomically(&state.join("lifecycle.json"), |path| {
		std::fs::write(path, &bytes)?;
		Ok(())
	})
}

pub fn reset(state: &Path) -> Result<()> {
	std::fs::create_dir_all(state)?;
	let _lock = super::Lock::acquire(&state.join("restart.lock"), state)
		.map_err(|error| Diagnostic::global(Code::IpcUnavailable, error.to_string()))?;
	write(state, &Lifecycle::default())
}

pub fn begin(state: &Path) -> Result<()> {
	let mut lifecycle = read(state)?;
	if matches!(lifecycle.state.as_str(), "clean" | "stopping") {
		lifecycle = Lifecycle::default();
	}
	lifecycle.boot = JobId::now()?.to_string();
	lifecycle.state = "running".into();
	lifecycle.cause = None;
	write(state, &lifecycle)
}

pub fn stopping(state: &Path) -> Result<()> {
	let mut lifecycle = read(state)?;
	lifecycle.state = "stopping".into();
	write(state, &lifecycle)
}

pub fn finish(state: &Path, cause: Option<String>) -> Result<()> {
	let mut lifecycle = read(state)?;
	lifecycle.state = if cause.is_some() { "failed" } else { "clean" }.into();
	lifecycle.cause = cause;
	write(state, &lifecycle)
}

impl Lifecycle {
	fn permit(&mut self) -> Result<()> {
		if self.boot.is_empty() || matches!(self.state.as_str(), "clean" | "stopping") {
			return Err(Diagnostic::global(
				Code::IpcUnavailable,
				"daemon was explicitly stopped; start it explicitly to watch again",
			)
			.into());
		}
		if self.counted_boot.as_ref() != Some(&self.boot) {
			self.counted_boot = Some(self.boot.clone());
			if let Some(cause) = &self.cause {
				self.unknown = 0;
				*self.counts.entry(cause.clone()).or_default() += 1;
			} else {
				self.unknown += 1;
			}
		}
		let exhausted = self.cause.as_ref().map_or(self.unknown >= 3, |cause| {
			self.counts.get(cause).copied().unwrap_or_default() >= 3
		});
		if exhausted {
			return Err(Diagnostic::global(Code::IpcUnavailable, "automatic daemon recovery stopped after three crashes with this cause, or three consecutive unknown causes; inspect daemon.log and start explicitly").into());
		}
		Ok(())
	}
}

pub fn permit_restart(state: &Path) -> Result<()> {
	let _lock = super::Lock::acquire(&state.join("restart.lock"), state)
		.map_err(|error| Diagnostic::global(Code::IpcUnavailable, error.to_string()))?;
	let mut lifecycle = read(state)?;
	if matches!(lifecycle.state.as_str(), "clean" | "stopping") {
		return lifecycle.permit();
	}
	let _daemon = match super::Lock::acquire(&state.join("lock"), state) {
		Ok(lock) => lock,
		Err(crate::error::Error::AlreadyRunning(_)) => return Ok(()),
		Err(error) => return Err(Diagnostic::global(Code::IpcUnavailable, error.to_string()).into()),
	};
	let result = lifecycle.permit();
	write(state, &lifecycle)?;
	result
}

pub struct Guard<'a>(pub &'a Path);
impl Drop for Guard<'_> {
	fn drop(&mut self) {
		if std::thread::panicking() {
			let _ = finish(self.0, Some("daemon-panic".into()));
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn restart_budgets_count_boots_once_and_known_causes_separately() {
		let mut lifecycle = Lifecycle {
			boot: "one".into(),
			state: "failed".into(),
			cause: Some("a".into()),
			..Default::default()
		};
		lifecycle.permit().unwrap();
		lifecycle.permit().unwrap();
		assert_eq!(lifecycle.counts["a"], 1);
		lifecycle.boot = "two".into();
		lifecycle.cause = Some("b".into());
		lifecycle.permit().unwrap();
		lifecycle.boot = "three".into();
		lifecycle.cause = Some("a".into());
		lifecycle.permit().unwrap();
		lifecycle.boot = "four".into();
		assert!(lifecycle.permit().is_err());
		assert_eq!(lifecycle.counts["b"], 1);
	}
	#[test]
	fn unknown_crashes_are_consecutive_and_clean_stops_never_restart() {
		let mut lifecycle = Lifecycle {
			state: "running".into(),
			..Default::default()
		};
		for boot in ["one", "two"] {
			lifecycle.boot = boot.into();
			lifecycle.permit().unwrap();
		}
		lifecycle.boot = "three".into();
		assert!(lifecycle.permit().is_err());
		lifecycle.state = "stopping".into();
		assert!(lifecycle.permit().is_err());
	}
}
