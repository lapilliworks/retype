//! Recovery after durable cancellation: retain completed outputs or release a proven redundant copy.

use retype_core::error::{Code, Diagnostic, Error, Result};
use retype_core::job::{Job, JobError, JobState, OutputState};
use retype_core::undo::Recovery;

use super::watcher::{self, Shared};

/// The cancellation snapshot is already durable when this is called.
pub fn finish_after_persist(shared: &Shared, recovery: Result<Recovery<'_>>, job: &mut Job) -> Option<String> {
	finish_after_persist_cancellable(shared, recovery, job, &retype_core::execute::Cancellation::default())
}

pub fn finish_after_persist_cancellable(
	shared: &Shared,
	recovery: Result<Recovery<'_>>,
	job: &mut Job,
	cancellation: &retype_core::execute::Cancellation,
) -> Option<String> {
	match recovery.and_then(|recovery| finish_retention_cancellable(recovery, job, cancellation)) {
		Ok(()) => None,
		Err(error) => {
			let action = if job.outputs.iter().any(|output| output.state == OutputState::Done) {
				"undo retention was not started"
			} else {
				"redundant recovery was not released"
			};
			let message = format!("{action}: {error}; the recovery record remains protected when present");
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

/// Start retention for verified outputs, or release a verified redundant recovery copy.
/// Cancellation never removes an output, and an ambiguous recovery record stays pinned.
pub fn finish_retention_cancellable(
	recovery: Recovery<'_>,
	job: &Job,
	cancellation: &retype_core::execute::Cancellation,
) -> Result<()> {
	if job.state != JobState::Cancelled {
		return Err(unavailable("recovery cleanup requires a durably cancelled job"));
	}
	if recovery.id() != &job.id {
		return Err(unavailable("cancellation belongs to another recovery job"));
	}
	let has_completed_outputs = job.outputs.iter().any(|output| output.state == OutputState::Done);
	let Recovery::Retained(recovery) = recovery else {
		return if has_completed_outputs {
			Err(unavailable("no undo record exists for this job"))
		} else {
			Ok(())
		};
	};
	if !has_completed_outputs {
		return recovery.release_unpublished_cancellable(cancellation);
	}
	let store = recovery.store();
	/* Publications update the durable ledger after execution acquired recovery ownership. */
	let record = store
		.load(&job.id)?
		.ok_or_else(|| unavailable("no undo record exists for this job"))?;
	if store.source_cancellable(&job.id, cancellation)?.is_none() {
		return Err(unavailable("the retained original is missing or cannot be verified"));
	}
	let original = record
		.source_fingerprint
		.as_ref()
		.ok_or_else(|| unavailable("the original has no recorded fingerprint"))?;
	let intact = original
		.matches_cancellable(&record.source_path, cancellation)
		.map_err(|error| unavailable(&format!("{} cannot be verified: {error}", record.source_path.display())))?;
	if !intact {
		return Err(unavailable(&format!(
			"{} no longer holds the original",
			record.source_path.display()
		)));
	}

	for output in job.outputs.iter().filter(|output| output.state == OutputState::Done) {
		let commit = record
			.commits
			.iter()
			.find(|commit| commit.output.path == output.path)
			.ok_or_else(|| unavailable(&format!("{} has no recovery record", output.path.display())))?;
		let matches = commit
			.fingerprint
			.matches_cancellable(&output.path, cancellation)
			.map_err(|error| unavailable(&format!("{} cannot be verified: {error}", output.path.display())))?;
		if !matches {
			return Err(unavailable(&format!(
				"{} changed after publication",
				output.path.display()
			)));
		}
	}
	recovery.finish()
}

fn unavailable(message: &str) -> Error {
	Error::Diagnostic(Diagnostic::global(Code::JobResumeUnavailable, message))
}
