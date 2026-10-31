//! Continuing a `partial` job, `docs/state.md` §5.2.
//!
//! A partial job is one where at least one output succeeded and at least one did
//! not. Resume runs the ones that are missing, skips the ones that are still what
//! the job produced, and takes its input from whichever place still holds the
//! source bytes: the source path, or the undo record when a rename has taken the
//! name away.
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
		&shared.registry,
	)
	.map_err(|problems| {
		Diagnostic::global(
			Code::JobState,
			format!("the recorded intent '{text}' no longer resolves: {problems}"),
		)
	})?;

	let plans = plan::compile(&shared.registry, &retype_core::tools::Machine, &resolved).map_err(|problems| {
		Diagnostic::global(
			Code::ToolMissing,
			format!("the recorded intent can no longer be planned: {problems}"),
		)
	})?;

	let input = source_bytes(shared, job)?;
	let cancellation = execute::Cancellation::default();
	let mut outputs = Vec::with_capacity(plans.len());

	for (index, plan) in plans.iter().enumerate() {
		let recorded = job.outputs.get(index);

		// An output that is still what the job produced needs no work; anything
		// else, including a different file with the same name, is run again.
		if !force {
			if let Some(recorded) = recorded {
				if still_produced(recorded) {
					outputs.push(recorded.clone());
					continue;
				}
			}
		}

		let path = recorded
			.map(|output| output.path.clone())
			.unwrap_or_else(|| output_path(&job.source.path, &resolved, index));

		let log = crate::state::log_path(&shared.state_dir, &job.id);

		match execute::run_logged(plan, &input, &path, &cancellation, Some(&log)) {
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
			Err(error) => outputs.push(Output {
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
			}),
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
	};

	if let Ok(mut journal) = shared.journal.lock() {
		journal.append(&resumed)?;
	}

	if let Ok(mut jobs) = shared.jobs.lock() {
		match jobs.iter_mut().find(|existing| existing.id == resumed.id) {
			Some(existing) => *existing = resumed.clone(),
			None => jobs.push(resumed.clone()),
		}
	}

	// A finished job has nothing left to undo from the store: its source name is
	// still where the rename put it, and the outputs are the result.
	if state == JobState::Done {
		Store::open(&shared.state_dir).remove(&job.id)?;
	}

	Ok(state)
}

/// Where the source bytes still are, in the order `docs/state.md` §5.2 gives.
fn source_bytes(shared: &Shared, job: &Job) -> Result<PathBuf> {
	if job.source.path.exists() {
		return Ok(job.source.path.clone());
	}

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
