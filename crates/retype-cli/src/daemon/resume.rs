//! Continuing a `partial` job, `docs/dev/architecture/state.md` §5.2.
//!
//! A partial job is one where at least one output succeeded and at least one did
//! not. Resume runs the ones that are missing, skips the ones that are still what
//! the job produced, and takes its input from whichever place still holds the
//! source bytes: a private working copy of the job's digest-verified retained original.
//!
//! The record is the reason this works. A rename consumes the source name, so a
//! partial job's source is usually only in the store; a job with neither is
//! `E_JOB_RESUME_UNAVAILABLE`, which is the honest answer rather than a conversion
//! from something else.

use std::path::PathBuf;

use retype_core::error::{Code, Diagnostic, Error, Result};
use retype_core::job::{FileIdentity, Job, JobError, JobState, Output, OutputState, now_ms};
use retype_core::plan;
use retype_core::resolve::{self, Source as Origin};
use retype_core::undo::Store;
use retype_core::{execute, intent};

use crate::daemon::watcher::Shared;

/// Run the outputs a partial job did not produce, and report the state it ends in.
pub fn run(shared: &Shared, machine: &str, job: &Job, force: bool) -> Result<JobState> {
	let Some(text) = &job.intent else {
		return Err(unavailable("the job recorded no intent to run again"));
	};

	let parsed = intent::parse(text).map_err(|problems| {
		Diagnostic::global(
			Code::JobState,
			format!("the recorded intent '{text}' no longer parses: {problems}"),
		)
	})?;

	let resolved = resolve::resolve(
		&parsed,
		Origin {
			format: &job.source.format,
			is_directory: false,
		},
		&shared.registry(),
	)
	.map_err(|problems| {
		Diagnostic::global(
			Code::JobState,
			format!("the recorded intent '{text}' no longer resolves: {problems}"),
		)
	})?;

	let plans = plan::compile(&shared.registry(), &retype_core::tools::Machine, &resolved).map_err(|problems| {
		Diagnostic::global(
			Code::ToolMissing,
			format!("the recorded intent can no longer be planned: {problems}"),
		)
	})?;

	let store = Store::open(&shared.state_dir);
	let lease = store.lease(&job.id)?;
	store.pin(&job.id)?;
	let retained = source_bytes(shared, job)?;
	let snapshot = retype_core::safety::Snapshot::open(&retained)?;
	let input = snapshot.path.clone();
	let ledger = store.load(&job.id)?.ok_or_else(|| unavailable("no recovery ledger"))?;
	let cancellation = execute::Cancellation::default();
	let mut outputs = Vec::with_capacity(plans.len());
	let mut output_gate = super::transaction::OutputGate::default();

	for (index, plan) in plans.iter().enumerate() {
		let recorded = job.outputs.get(index);
		let stopped = output_gate.skipped();

		// Once stopped, retain verified prior outputs even if this resume requested replacement.
		if !force || stopped.is_some() {
			if let Some(recorded) = recorded {
				if still_produced(recorded)
					&& ledger.commits.iter().any(|commit| {
						commit.output.path == recorded.path
							&& commit.fingerprint.matches(&recorded.path).unwrap_or(false)
					}) {
					outputs.push(recorded.clone());
					continue;
				}
			}
		}

		let path = recorded
			.map(|output| output.path.clone())
			.unwrap_or_else(|| output_path(&job.source.path, &resolved, index));

		if let Some((code, message)) = stopped {
			let mut skipped = Output::pending(path, &plan.format, plan.tier);
			skipped.state = OutputState::Skipped;
			skipped.error = Some(JobError::new(code.to_string(), message));
			outputs.push(skipped);
			continue;
		}

		if blocked(recorded, &path, force) {
			outputs.push(Output {
				path,
				format: plan.format.clone(),
				state: OutputState::Failed,
				bytes: None,
				tier: plan.tier,
				identity: None,
				stderr: None,
				error: Some(JobError {
					code: Code::IoExists.to_string(),
					message:
						"the output path is occupied by a file this job did not produce; pass --force to replace it"
							.to_string(),
				}),
			});
			continue;
		}

		let publication = if force {
			retype_core::convert::Publication::Replace
		} else {
			retype_core::convert::Publication::Create
		};
		let transaction = super::transaction::OutputTransaction {
			shared,
			job,
			original: &job.source.path,
			completed: &outputs,
			store: Some(&store),
		};
		match transaction.run(plan, &input, &path, &cancellation, publication) {
			Ok(report) => outputs.push(Output {
				path: path.clone(),
				format: plan.format.clone(),
				state: OutputState::Done,
				bytes: Some(report.bytes),
				tier: plan.tier,
				identity: FileIdentity::of(&path),
				stderr: match report.stderr.trim().is_empty() {
					true => None,
					false => Some(report.stderr.clone()),
				},
				error: None,
			}),
			Err(error) => {
				output_gate.observe(&error);
				outputs.push(Output {
					path,
					format: plan.format.clone(),
					state: OutputState::Failed,
					bytes: None,
					tier: plan.tier,
					identity: None,
					stderr: None,
					error: Some(JobError {
						code: error.code().to_string(),
						message: error.to_string(),
					}),
				});
			}
		}
	}

	let state = match outputs.iter().all(|output| output.state == OutputState::Done) {
		true => JobState::Done,
		false if outputs.iter().any(|output| output.state == OutputState::Done) => JobState::Partial,
		false => JobState::Failed,
	};

	let resumed = Job {
		id: job.id.clone(),
		created: job.created,
		updated: now_ms(),
		source: job.source.clone(),
		intent: job.intent.clone(),
		outputs,
		state,
		backend: plans.first().map(|plan| plan.backend.name().to_string()),
		engine: plans
			.first()
			.and_then(|plan| plan.engine.clone())
			.map(|(name, candidate)| retype_core::job::Engine { name, candidate }),
		machine: machine.to_string(),
		error: None,
		parked: None,
		consume_source: job.consume_source,
	};

	if let Err(error) = crate::daemon::watcher::persist(shared, &resumed) {
		super::watcher::report_unrecorded_completion(shared, &resumed, &error);
		return Err(error);
	}
	if state == JobState::Done {
		store.finish(&job.id)?;
	}
	drop(lease);
	crate::daemon::watcher::publish_persisted(shared, &resumed);
	Ok(state)
}

/// Never substitute a live pathname for the job's verified retained original.
fn source_bytes(shared: &Shared, job: &Job) -> Result<PathBuf> {
	if let Some(bytes) = Store::open(&shared.state_dir).source(&job.id)? {
		return Ok(bytes);
	}

	Err(unavailable(
		"neither the source path nor the undo record holds the source bytes",
	))
}

fn output_path(source: &std::path::Path, resolved: &resolve::Resolved, index: usize) -> PathBuf {
	let stem = source.file_stem().and_then(|stem| stem.to_str()).unwrap_or_default();

	let format = resolved
		.groups
		.first()
		.map(|group| group.format.clone())
		.unwrap_or_default();

	let derived = retype_core::output::derive(stem, &format, &resolved.groups);

	derived
		.get(index)
		.map(|derived| source.parent().unwrap_or(std::path::Path::new("")).join(&derived.path))
		.unwrap_or_else(|| source.to_path_buf())
}

/// Whether a file at this path is somebody else's work.
///
/// Occupied paths are never rerun without explicit replacement. Matching completed
/// outputs are skipped separately using both identity and content fingerprints.
fn blocked(recorded: Option<&Output>, path: &std::path::Path, force: bool) -> bool {
	let _ = recorded;
	!force && std::fs::symlink_metadata(path).is_ok()
}

/// Whether an output still carries the identity and size the job recorded.
fn still_produced(output: &Output) -> bool {
	let Some(identity) = &output.identity else {
		return false;
	};

	let Ok(metadata) = std::fs::metadata(&output.path) else {
		return false;
	};

	if output.bytes.is_some_and(|bytes| bytes != metadata.len()) {
		return false;
	}

	FileIdentity::of(&output.path).is_some_and(|current| identity.matches(&current))
}

fn unavailable(message: &str) -> Error {
	Error::Diagnostic(Diagnostic::global(Code::JobResumeUnavailable, message))
}

#[cfg(test)]
mod tests {
	use super::*;
	use retype_core::job::{FileIdentity, Tier};

	#[test]
	fn an_occupied_output_is_only_replaced_on_purpose() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("photo.webp");
		std::fs::write(&path, b"mine").unwrap();

		let mut output = Output::pending(path.clone(), "webp", Tier::Converted);
		output.identity = Some(FileIdentity::Unix {
			dev: 1,
			ino: 1,
			birth_ns: None,
		});

		assert!(blocked(None, &path, false), "an unrecorded path is not ours");
		assert!(blocked(Some(&output), &path, false), "a different file is not ours");
		assert!(!blocked(None, &path, true), "--force replaces it");
		assert!(!blocked(None, &dir.path().join("absent.webp"), false));
	}

	#[test]
	fn an_output_without_identity_is_not_safe_to_overwrite() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("photo.webp");
		std::fs::write(&path, b"mine").unwrap();

		let output = Output::pending(path.clone(), "webp", Tier::Converted);

		assert!(blocked(Some(&output), &path, false));
	}
}
