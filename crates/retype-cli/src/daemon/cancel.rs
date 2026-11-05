//! Retention after cancellation with completed outputs.

use retype_core::error::{Code, Diagnostic, Error, Result};
use retype_core::job::{Job, JobError, OutputState};
use retype_core::undo::Store;

use super::watcher::{self, Shared};

/// The cancellation snapshot is already durable when this is called.
pub fn finish_after_persist(shared: &Shared, store: &Store, job: &mut Job) -> Option<String> {
	match finish_retention(store, job) {
		Ok(()) => None,
		Err(error) => {
			let message =
				format!("undo retention was not started: {error}; the recovery record remains protected when present");
			let mut warned = job.clone();
			warned.error = Some(JobError::new(error.code().to_string(), &message));
			match watcher::persist(shared, &warned) {
				Ok(()) => *job = warned,
				Err(error) => eprintln!("retype: could not record the cancellation warning: {error}"),
			}
			eprintln!("retype: job {}: {message}", job.id);
			Some(message)
		}
	}
}

/// Start retention only when the independent original and completed outputs are
/// still verifiable. Cancellation itself never removes either.
pub fn finish_retention(store: &Store, job: &Job) -> Result<()> {
	let record = store
		.load(&job.id)?
		.ok_or_else(|| unavailable("no undo record exists for this job"))?;
	if store.source(&job.id)?.is_none() {
		return Err(unavailable("the retained original is missing or cannot be verified"));
	}
	let original = record
		.source_fingerprint
		.as_ref()
		.ok_or_else(|| unavailable("the original has no recorded fingerprint"))?;
	let intact = original
		.matches(&record.source_path)
		.map_err(|error| unavailable(&format!("{} cannot be verified: {error}", record.source_path.display())))?;
	if !intact {
		return Err(unavailable(&format!(
			"{} no longer holds the original",
			record.source_path.display()
		)));
	}

	let mut completed = 0;
	for output in job.outputs.iter().filter(|output| output.state == OutputState::Done) {
		completed += 1;
		let commit = record
			.commits
			.iter()
			.find(|commit| commit.output.path == output.path)
			.ok_or_else(|| unavailable(&format!("{} has no recovery record", output.path.display())))?;
		let matches = commit
			.fingerprint
			.matches(&output.path)
			.map_err(|error| unavailable(&format!("{} cannot be verified: {error}", output.path.display())))?;
		if !matches {
			return Err(unavailable(&format!(
				"{} changed after publication",
				output.path.display()
			)));
		}
	}
	if completed == 0 {
		return Err(unavailable("no completed outputs can be verified"));
	}
	store.finish(&job.id)
}

fn unavailable(message: &str) -> Error {
	Error::Diagnostic(Diagnostic::global(Code::JobResumeUnavailable, message))
}
