//! `retype convert`, `docs/cli.md` §2.
//!
//! One input, one intent, and the outputs the intent names. The conversion runs in
//! this process for now: the daemon path and the queue land with the job runner,
//! and both journal the same record, so a job is undoable either way once `undo`
//! exists.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use retype_core::error::{Code, Diagnostic};
use retype_core::job::JobId;
use retype_core::job::{Job, JobState, Output, OutputState, Source, now_ms};
use retype_core::journal::Journal;
use retype_core::output;
use retype_core::plan;
use retype_core::print;
use retype_core::registry::Registry;
use retype_core::resolve::{self, Source as Origin};
use retype_core::{execute, intent};

use crate::error::Result;

pub struct Request<'a> {
	pub input: &'a str,
	pub intent: &'a str,
	/// `-o`, which requires exactly one output.
	pub output: Option<&'a str>,
	pub force: bool,
	pub dry_run: bool,
	pub state_dir: &'a Path,
	pub quiet: bool,
}

pub fn run(registry: &Registry, request: &Request<'_>) -> Result<ExitCode> {
	let input = PathBuf::from(request.input);

	if request.dry_run {
		return crate::explain::run(
			registry,
			&crate::explain::Request {
				source: request.input,
				intent: request.intent,
				json: false,
				quiet: request.quiet,
			},
		);
	}

	let source = match source_of(registry, &input) {
		Ok(source) => source,
		Err(problem) => {
			eprintln!("retype: {}: {}", problem.code, problem.message);
			return Ok(ExitCode::from(3));
		}
	};

	let parsed = match intent::parse(request.intent) {
		Ok(parsed) => parsed,
		Err(problems) => {
			eprintln!("retype: {problems}");
			return Ok(ExitCode::from(3));
		}
	};

	let resolved = match resolve::resolve(
		&parsed,
		Origin {
			format: &source.format,
			is_directory: source.is_directory,
		},
		registry,
	) {
		Ok(resolved) => resolved,
		Err(problems) => {
			eprintln!("retype: {problems}");
			return Ok(ExitCode::from(3));
		}
	};

	let plans = match plan::compile(registry, &retype_core::tools::Machine, &resolved) {
		Ok(plans) => plans,
		Err(problems) => {
			eprintln!("retype: {problems}");

			// "Cannot be done here" and "cannot be done" are different answers,
			// `docs/cli.md` §6.
			let code = match problems
				.codes()
				.iter()
				.any(|code| matches!(code, Code::ToolMissing | Code::ToolVersion))
			{
				true => 4,
				false => 3,
			};

			return Ok(ExitCode::from(code));
		}
	};

	let paths = match targets(request, &plans, &source, &resolved, &input) {
		Ok(paths) => paths,
		Err(problem) => {
			eprintln!("retype: {}: {}", problem.code, problem.message);
			return Ok(ExitCode::from(3));
		}
	};

	let (mut journal, _) = Journal::open(&request.state_dir.join("jobs.jsonl"))?;
	let id = JobId::now()?;
	let now = now_ms();

	let mut outputs = Vec::new();
	let mut states = Vec::new();
	// The first thing that went wrong, as an exit code, `docs/cli.md` §6.
	let mut failure: Option<u8> = None;

	for (plan, path) in plans.iter().zip(&paths) {
		if path.exists() && !request.force {
			eprintln!(
				"retype: {}: {} already exists, and --force was not given",
				Code::IoExists,
				path.display()
			);
			states.push(OutputState::Failed);
			outputs.push(record(path, plan, OutputState::Failed, None, None));
			failure.get_or_insert(5);
			continue;
		}

		match execute::run(plan, &input, path) {
			Ok(report) => {
				if !request.quiet {
					println!("{}  {}  {} bytes", path.display(), tier_name(plan), report.bytes);
				}

				states.push(OutputState::Done);
				outputs.push(record(path, plan, OutputState::Done, Some(report.bytes), None));
			}
			Err(error) => {
				eprintln!("retype: {error}");
				states.push(OutputState::Failed);
				outputs.push(record(path, plan, OutputState::Failed, None, Some(error.to_string())));
				failure.get_or_insert(4);
			}
		}
	}

	let state = match (states.contains(&OutputState::Done), states.len()) {
		(_, 0) => JobState::Failed,
		(true, count) if count == states.len() => JobState::Done,
		(true, _) => JobState::Partial,
		(false, _) => JobState::Failed,
	};

	let job = Job {
		id,
		created: now,
		updated: now_ms(),
		source: Source {
			path: input.clone(),
			format: source.format.clone(),
			size: std::fs::metadata(&input).map(|meta| meta.len()).unwrap_or_default(),
			mtime_ns: 0,
		},
		intent: Some(print::intent(&resolved, registry)),
		outputs,
		state,
		backend: plans.first().map(|plan| plan.backend.name().to_string()),
		machine: crate::daemon::machine::load_or_create(request.state_dir)?,
		error: None,
	};

	journal.append(&job)?;

	match failure {
		Some(code) => Ok(ExitCode::from(code)),
		None => Ok(ExitCode::SUCCESS),
	}
}

/// The paths the plans write to, in plan order, with `-o` applied.
fn targets(
	request: &Request<'_>,
	plans: &[plan::Plan<'_>],
	source: &Named,
	resolved: &resolve::Resolved,
	input: &Path,
) -> std::result::Result<Vec<PathBuf>, Diagnostic> {
	let derived = output::derive(&source.stem, &source.format, &resolved.groups);

	match request.output {
		Some(_) if plans.len() > 1 => Err(Diagnostic::global(
			Code::UsageOutputCount,
			format!("-o names one path and this intent produces {} outputs", plans.len()),
		)),
		Some(path) => Ok(vec![PathBuf::from(path)]),
		// A derived name is a file name; the output belongs beside the input.
		None => Ok(derived
			.iter()
			.map(|derived| match input.parent() {
				Some(parent) => parent.join(&derived.path),
				None => derived.path.clone(),
			})
			.collect()),
	}
}

fn record(path: &Path, plan: &plan::Plan<'_>, state: OutputState, bytes: Option<u64>, error: Option<String>) -> Output {
	Output {
		path: path.to_path_buf(),
		format: plan.format.clone(),
		state,
		bytes,
		tier: plan.tier,
		identity: None,
		error: error.map(|message| retype_core::job::JobError {
			code: Code::ToolFailed.to_string(),
			message,
		}),
	}
}

struct Named {
	stem: String,
	format: String,
	is_directory: bool,
}

/// The same reading of a name `explain` uses: a declared suffix makes it a file,
/// a bare format names a format without a file, and no dot means a directory.
fn source_of(registry: &Registry, path: &Path) -> std::result::Result<Named, Diagnostic> {
	let name = path
		.file_name()
		.and_then(|name| name.to_str())
		.ok_or_else(|| Diagnostic::global(Code::UsageArgs, format!("{} has no file name", path.display())))?;

	if let Some((stem, format)) = registry.split(name) {
		return Ok(Named {
			stem: stem.to_string(),
			format: format.to_string(),
			is_directory: false,
		});
	}

	if path.is_dir() {
		return Ok(Named {
			stem: name.to_string(),
			format: "folder".to_string(),
			is_directory: true,
		});
	}

	Err(Diagnostic::global(
		Code::FormatUnknown,
		format!("'{name}' ends in a suffix this build has no format for, so it is not ours to convert"),
	))
}

/// The tier as the command line spells it.
fn tier_name(plan: &plan::Plan<'_>) -> &'static str {
	match plan.tier {
		retype_core::job::Tier::Exact => "exact",
		retype_core::job::Tier::Converted => "converted",
		retype_core::job::Tier::Delegated => "delegated",
	}
}
