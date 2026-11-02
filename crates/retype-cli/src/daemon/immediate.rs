//! `retype convert` as a daemon job, `docs/dev/architecture/ipc.md` §6 `jobs.submit`.
//!
//! A conversion asked for at a terminal is an `immediate` job: it skips the queue
//! so a person does not wait behind a directory's renames, and shares everything
//! else with them, one journal, one runner pool, one cancellation path.
//!
//! The source is never consumed. A rename moved the file, so the job that answers
//! it replaces the name; `convert` was handed a path and leaves it where it is.
//! That difference is the whole of `consume_source`, `docs/dev/architecture/state.md` §5.3.

use std::path::{Path, PathBuf};

use retype_core::error::{Code, Diagnoses, Diagnostic, Error};
use retype_core::execute::Cancellation;
use retype_core::job::{FileIdentity, Job, JobError, JobId, JobState, Output, OutputState, Source, now_ms};
use retype_core::registry::Registry;
use retype_core::resolve::Source as Origin;
use retype_core::{execute, intent, output, plan, print, resolve, tools};

use crate::daemon::watcher::{Shared, mark, publish};

/// What a client asked for, as it arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
	pub input: PathBuf,
	pub intent: String,
	/// `-o`, which needs exactly one output.
	pub output: Option<String>,
	pub force: bool,
}

/// A request that has been read, resolved and planned: everything that can fail
/// before a byte moves.
pub struct Prepared {
	pub input: PathBuf,
	pub stem: String,
	pub format: String,
	pub resolved: resolve::Resolved,
	pub plans: Vec<plan::Plan>,
	pub paths: Vec<PathBuf>,
}

/// Read the request and choose a backend for each output. Nothing is written.
pub fn prepare(registry: &Registry, request: &Request) -> std::result::Result<Prepared, Diagnoses> {
	let name = request
		.input
		.file_name()
		.and_then(|name| name.to_str())
		.ok_or_else(|| {
			Diagnoses::one(Error::Diagnostic(Diagnostic::global(
				Code::FormatUnknown,
				format!("{} has no file name", request.input.display()),
			)))
		})?;

	// A directory has no format of its own: its whole name is its stem, and
	// `folder` is how the grammar says so, `docs/grammar.md` §3.
	let (stem, format, is_directory) = match request.input.is_dir() {
		true => (name.to_string(), "folder", true),
		false => {
			let (stem, format) = registry.split(name).ok_or_else(|| {
				Diagnoses::one(Error::Diagnostic(Diagnostic::global(
					Code::FormatUnknown,
					format!("'{name}' ends in a suffix this build has no format for"),
				)))
			})?;
			(stem.to_string(), format, false)
		}
	};
	let stem = stem.as_str();

	let parsed = intent::parse(&request.intent)?;
	let resolved = resolve::resolve(&parsed, Origin { format, is_directory }, registry)?;
	let plans = plan::compile(registry, &tools::Machine, &resolved)?;
	let paths = targets(request, &plans, stem, format, &resolved)?;

	validate(request, &paths)?;

	Ok(Prepared {
		input: request.input.clone(),
		stem: stem.to_string(),
		format: format.to_string(),
		resolved,
		plans,
		paths,
	})
}

/// Everything that can fail before a byte moves, `docs/cli.md` §2.
///
/// A `convert` is submitted, so a missing source or an occupied target has to be
/// a diagnosis at submission rather than a job that fails once a runner reaches
/// it. A rename is not checked here: its target is the name the user chose, and
/// `docs/errors.md` §5 keeps its outputs independent.
fn validate(request: &Request, paths: &[PathBuf]) -> std::result::Result<(), Diagnoses> {
	let mut problems = Vec::new();

	match std::fs::metadata(&request.input) {
		Ok(metadata) if metadata.is_dir() => {
			if let Err(error) = std::fs::read_dir(&request.input) {
				problems.push(Error::Diagnostic(Diagnostic::global(
					Code::IoPermission,
					format!("{}: {error}", request.input.display()),
				)));
			}
		}
		Ok(_) => {
			if let Err(error) = std::fs::File::open(&request.input) {
				problems.push(Error::Diagnostic(Diagnostic::global(
					retype_core::error::code_for_io(&error),
					format!("{}: {error}", request.input.display()),
				)));
			}
		}
		Err(error) => problems.push(Error::Diagnostic(Diagnostic::global(
			retype_core::error::code_for_io(&error),
			format!("{}: {error}", request.input.display()),
		))),
	}

	for path in paths {
		let aliases_input = match retype_core::fsx::protects_input(&request.input, path) {
			Ok(aliases) => aliases,
			Err(error) => {
				problems.push(Error::Io(error));
				continue;
			}
		};
		if aliases_input {
			problems.push(Error::Diagnostic(Diagnostic::global(
				Code::UsageConflict,
				format!(
					"{} is the source; converting into it would destroy the original",
					path.display()
				),
			)));
			continue;
		}

		if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
			if !parent.is_dir() {
				problems.push(Error::Diagnostic(Diagnostic::global(
					Code::IoNotFound,
					format!(
						"{} is not a directory, so {} cannot be written",
						parent.display(),
						path.display()
					),
				)));
			}
		}

		if path.is_dir() {
			problems.push(Error::Diagnostic(Diagnostic::global(
				Code::IoExists,
				format!("{} is a directory", path.display()),
			)));
		} else if !request.force && path.exists() {
			problems.push(Error::Diagnostic(Diagnostic::global(
				Code::IoExists,
				format!("{} already exists", path.display()),
			)));
		}
	}

	match problems.is_empty() {
		true => Ok(()),
		false => Err(Diagnoses::new(problems)),
	}
}

/// The paths the plans write to, in plan order, with `-o` applied.
fn targets(
	request: &Request,
	plans: &[plan::Plan],
	stem: &str,
	format: &str,
	resolved: &resolve::Resolved,
) -> std::result::Result<Vec<PathBuf>, Diagnoses> {
	let derived = output::derive(stem, format, &resolved.groups);

	match &request.output {
		Some(_) if plans.len() > 1 => Err(Diagnoses::one(Error::Diagnostic(Diagnostic::global(
			Code::UsageOutputCount,
			format!("-o names one path and this intent produces {} outputs", plans.len()),
		)))),
		Some(path) => Ok(vec![PathBuf::from(path)]),
		// A derived name is a file name, so it belongs beside the input, and the plan
		// says which group it came from because a kept source has no plan.
		None => Ok(plans
			.iter()
			.map(|plan| {
				let group = derived
					.iter()
					.find(|derived| derived.group == plan.group)
					.expect("every plan has a group");

				request.input.parent().unwrap_or(Path::new("")).join(&group.path)
			})
			.collect()),
	}
}

/// The record a submitted job writes before a runner reaches it, so a client can
/// see it waiting rather than only once it starts.
pub fn queued(shared: &Shared, prepared: &Prepared, id: &JobId) -> Job {
	let now = now_ms();

	Job {
		id: id.clone(),
		created: now,
		updated: now,
		source: source(prepared),
		intent: Some(print::intent(&prepared.resolved, &shared.registry())),
		outputs: prepared
			.plans
			.iter()
			.zip(&prepared.paths)
			.map(|(plan, path)| Output::pending(path.clone(), plan.format.clone(), plan.tier))
			.collect(),
		state: JobState::Queued,
		backend: prepared.plans.first().map(|plan| plan.backend.name().to_string()),
		engine: prepared
			.plans
			.first()
			.and_then(|plan| plan.engine.clone())
			.map(|(name, candidate)| retype_core::job::Engine { name, candidate }),
		machine: shared.machine.clone(),
		error: None,
		parked: None,
		consume_source: false,
	}
}

/// Run one submitted conversion and publish its terminal record.
pub fn run(shared: &Shared, request: &Request, id: &JobId, cancellation: &Cancellation) {
	// The plan was made when the job was accepted. Making it again is how a tool
	// that went away in between is reported instead of run.
	let prepared = match prepare(&shared.registry(), request) {
		Ok(prepared) => prepared,
		Err(problems) => {
			publish(shared, &unsatisfied(shared, request, problems, id));
			return;
		}
	};

	/* A submitted conversion retains its live source and an independent recovery input. */
	let store = retype_core::undo::Store::open(&shared.state_dir);
	let lease = match store.lease(id) {
		Ok(lease) => lease,
		Err(error) => {
			let mut failed = queued(shared, &prepared, id);
			failed.fail(JobError::new(error.code().to_string(), error.to_string()));
			publish(shared, &failed);
			return;
		}
	};
	let snapshot = match retype_core::safety::Snapshot::open(&prepared.input) {
		Ok(snapshot) => snapshot,
		Err(error) => {
			let mut failed = queued(shared, &prepared, id);
			failed.fail(JobError::new(error.code().to_string(), error.to_string()));
			publish(shared, &failed);
			return;
		}
	};
	let undo_record = store.save_with_policy(
		retype_core::undo::Subject {
			id: id.clone(),
			intent: print::intent(&prepared.resolved, &shared.registry()),
			machine: shared.machine.clone(),
			bytes: prepared.input.clone(),
			source_path: prepared.input.clone(),
			source_ext: prepared.format.clone(),
			outputs: prepared
				.plans
				.iter()
				.zip(&prepared.paths)
				.map(|(plan, path)| retype_core::undo::Output {
					path: path.clone(),
					ext: plan.format.clone(),
				})
				.collect(),
			consume_source: false,
		},
		shared.registry().undo(),
	);

	if let Err(error) = &undo_record {
		eprintln!("retype: cannot keep {} for undo: {error}", prepared.input.display());
	}

	if let Ok(Some(record)) = &undo_record {
		if record.source_fingerprint.as_ref() != Some(&snapshot.original) {
			let mut failed = queued(shared, &prepared, id);
			failed.fail(JobError::new(
				Code::IoFailed.to_string(),
				"input changed while its recovery copy was prepared",
			));
			publish(shared, &failed);
			return;
		}
	}
	let mut started = queued(shared, &prepared, id);
	started.set_state(JobState::Running);
	publish(shared, &started);

	let mut outputs = Vec::new();

	for (plan, path) in prepared.plans.iter().zip(&prepared.paths) {
		if path.exists() && !request.force {
			outputs.push(record(
				path,
				plan,
				OutputState::Failed,
				None,
				None,
				Some((Code::IoExists, format!("{} already exists", path.display()))),
			));
			continue;
		}

		let publication = if request.force {
			retype_core::convert::Publication::Replace
		} else {
			retype_core::convert::Publication::Create
		};
		let transaction = super::transaction::OutputTransaction {
			shared,
			job: &started,
			original: &prepared.input,
			completed: &outputs,
			store: matches!(undo_record, Ok(Some(_))).then_some(&store),
		};
		match transaction.run(plan, &snapshot.path, path, cancellation, publication) {
			Ok(report) => outputs.push(record(
				path,
				plan,
				OutputState::Done,
				Some(report.bytes),
				tail(&report),
				None,
			)),
			Err(error) => {
				// The failure's own code, so a refusal reads as a refusal rather than as
				// a tool that fell over.
				let code = match cancellation.is_cancelled() {
					true => Code::JobCancelled,
					false => error.code(),
				};

				let state = match cancellation.is_superseded() {
					true => OutputState::Skipped,
					false => OutputState::Failed,
				};

				outputs.push(record(path, plan, state, None, None, Some((code, error.to_string()))));
			}
		}
	}

	let state = match (
		cancellation.is_cancelled(),
		outputs.iter().all(|output| output.state == OutputState::Done),
		outputs.iter().any(|output| output.state == OutputState::Done),
	) {
		(true, _, _) => JobState::Cancelled,
		(false, true, _) => JobState::Done,
		(false, false, true) => JobState::Partial,
		(false, false, false) => JobState::Failed,
	};

	let mut job = started;
	job.outputs = outputs;
	job.state = state;
	job.updated = now_ms();
	if let Err(error) = crate::daemon::watcher::persist(shared, &job) {
		eprintln!("retype: recovery record retained because completion could not be persisted: {error}");
		return;
	}
	if job.state == JobState::Done && matches!(undo_record, Ok(Some(_))) {
		if let Err(error) = store.finish(id) {
			eprintln!("retype: undo record remains pinned: {error}");
		}
	}
	drop(lease);
	crate::daemon::watcher::publish_persisted(shared, &job);
	mark(shared, &job.outputs, job.intent.as_deref().unwrap_or_default());
}

/// The stderr tail, when the tool said anything worth keeping.
fn tail(report: &execute::Report) -> Option<String> {
	match report.stderr.trim().is_empty() {
		true => None,
		false => Some(report.stderr.clone()),
	}
}

fn source(prepared: &Prepared) -> Source {
	Source {
		path: prepared.input.clone(),
		format: prepared.format.clone(),
		size: std::fs::metadata(&prepared.input)
			.map(|meta| meta.len())
			.unwrap_or_default(),
		mtime_ns: 0,
	}
}

fn record(
	path: &Path,
	plan: &plan::Plan,
	state: OutputState,
	bytes: Option<u64>,
	stderr: Option<String>,
	error: Option<(Code, String)>,
) -> Output {
	Output {
		path: path.to_path_buf(),
		format: plan.format.clone(),
		state,
		bytes,
		tier: plan.tier,
		// Recorded on success only: an identity for a file that was never written
		// would tell a resume to skip work that was never done.
		identity: match state {
			OutputState::Done => FileIdentity::of(path),
			_ => None,
		},
		stderr,
		error: error.map(|(code, message)| JobError {
			code: code.to_string(),
			message,
		}),
	}
}

/// A job that could not be planned when its turn came, which is the one failure a
/// submitted job can suffer between acceptance and running.
fn unsatisfied(shared: &Shared, request: &Request, problems: Diagnoses, id: &JobId) -> Job {
	let code = problems.codes().first().cloned().unwrap_or(Code::ToolFailed);
	let name = request
		.input
		.file_name()
		.and_then(|name| name.to_str())
		.unwrap_or_default();
	let format = shared
		.registry()
		.split(name)
		.map(|(_, format)| format.to_string())
		.unwrap_or_default();
	let now = now_ms();

	Job {
		id: id.clone(),
		created: now,
		updated: now,
		source: Source {
			path: request.input.clone(),
			format,
			size: 0,
			mtime_ns: 0,
		},
		intent: Some(request.intent.clone()),
		outputs: Vec::new(),
		state: JobState::Failed,
		backend: None,
		engine: None,
		machine: shared.machine.clone(),
		error: Some(JobError {
			code: code.to_string(),
			message: problems.to_string(),
		}),
		parked: None,
		consume_source: false,
	}
}
