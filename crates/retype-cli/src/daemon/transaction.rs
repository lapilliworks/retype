//! One publication protocol for rename, immediate and resumed outputs.

use std::cell::Cell;
use std::path::Path;

use retype_core::convert::Publication;
use retype_core::error::{Code, Error, Result};
use retype_core::execute::{self, Cancellation, Report};
use retype_core::job::{FileIdentity, Job, JobState, Output, OutputState};
use retype_core::plan::Plan;
use retype_core::undo::Store;

use super::watcher::{Shared, persist};

#[derive(Debug)]
pub struct OutputFailure {
	error: Error,
	checkpoint_failed: bool,
}

impl OutputFailure {
	pub fn code(&self) -> Code {
		self.error.code()
	}

	fn stops_later_outputs(&self) -> bool {
		self.checkpoint_failed || self.code() == Code::IoSpace
	}
}

impl std::fmt::Display for OutputFailure {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		if self.checkpoint_failed {
			write!(formatter, "output safety or recovery checkpoint failed: {}", self.error)?;
		} else {
			std::fmt::Display::fmt(&self.error, formatter)?;
		}
		if self.code() == Code::IoSpace {
			write!(formatter, "; free space before trying again")?;
		}
		Ok(())
	}
}

/// Stops later outputs only when storage or a required safety checkpoint fails.
#[derive(Default)]
pub struct OutputGate {
	reason: Option<(Code, String)>,
}

impl OutputGate {
	pub fn observe(&mut self, failure: &OutputFailure) {
		if failure.stops_later_outputs() {
			self.reason = Some((failure.code(), failure.to_string()));
		}
	}

	pub fn skipped(&self) -> Option<(Code, String)> {
		self.reason
			.as_ref()
			.map(|(code, message)| (code.clone(), format!("not attempted after: {message}")))
	}
}

pub struct OutputTransaction<'a> {
	pub shared: &'a Shared,
	pub job: &'a Job,
	pub original: &'a Path,
	pub completed: &'a [Output],
	pub store: Option<&'a Store>,
}

impl OutputTransaction<'_> {
	pub fn run(
		&self,
		plan: &Plan,
		input: &Path,
		output: &Path,
		cancellation: &Cancellation,
		publication: Publication,
	) -> std::result::Result<Report, OutputFailure> {
		let log = crate::state::log_path(&self.shared.state_dir, &self.job.id);
		let checkpoint_failed = Cell::new(false);
		let before_commit = |staged: &Path, report: &Report| {
			let result = self.checkpoint(staged, report, plan, output);
			if result.is_err() {
				checkpoint_failed.set(true);
			}
			result
		};
		execute::run_recorded(
			plan,
			input,
			output,
			cancellation,
			Some(&log),
			publication,
			&before_commit,
		)
		.map_err(|error| OutputFailure {
			error,
			checkpoint_failed: checkpoint_failed.get(),
		})
	}

	fn checkpoint(&self, staged: &Path, report: &Report, plan: &Plan, output: &Path) -> Result<()> {
		if retype_core::fsx::protects_input(self.original, &report.output)? {
			return Err(Error::Exists(report.output.clone()));
		}
		let mut produced = Output::pending(output.to_path_buf(), &plan.format, plan.tier);
		produced.state = OutputState::Done;
		produced.bytes = Some(report.bytes);
		produced.identity = FileIdentity::of(staged);
		if let Some(store) = self.store {
			store.prepare_output(&self.job.id, produced.clone(), staged)?;
		}
		let mut checkpoint = self.job.clone();
		checkpoint.state = JobState::Running;
		for output in self.completed.iter().cloned().chain(Some(produced)) {
			if let Some(recorded) = checkpoint
				.outputs
				.iter_mut()
				.find(|recorded| recorded.path == output.path)
			{
				*recorded = output;
			} else {
				checkpoint.outputs.push(output);
			}
		}
		persist(self.shared, &checkpoint)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn storage_exhaustion_and_checkpoint_failures_stop_later_outputs() {
		for failure in [
			OutputFailure {
				error: Error::Io(std::io::Error::from(std::io::ErrorKind::StorageFull)),
				checkpoint_failed: false,
			},
			OutputFailure {
				error: Error::Io(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
				checkpoint_failed: true,
			},
		] {
			let mut gate = OutputGate::default();
			gate.observe(&failure);
			let (code, message) = gate.skipped().unwrap();
			assert_eq!(code, failure.code());
			assert!(message.contains("not attempted"));
			if code == Code::IoSpace {
				assert!(message.contains("free space"));
			}
		}
	}

	#[test]
	fn an_ordinary_output_failure_does_not_close_the_gate() {
		let failure = OutputFailure {
			error: Error::Io(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
			checkpoint_failed: false,
		};
		let mut gate = OutputGate::default();
		gate.observe(&failure);
		assert!(gate.skipped().is_none());
	}
}
