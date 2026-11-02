//! One publication protocol for rename, immediate and resumed outputs.

use std::path::Path;

use retype_core::convert::Publication;
use retype_core::error::{Error, Result};
use retype_core::execute::{self, Cancellation, Report};
use retype_core::job::{FileIdentity, Job, JobState, Output, OutputState};
use retype_core::plan::Plan;
use retype_core::undo::Store;

use super::watcher::{Shared, persist};

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
	) -> Result<Report> {
		let log = crate::state::log_path(&self.shared.state_dir, &self.job.id);
		let before_commit = |staged: &Path, report: &Report| {
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
	}
}
