//! `retype convert` as a daemon job, `docs/dev/architecture/ipc.md` §6 `jobs.submit`.
//!
//! A conversion asked for at a terminal is an `immediate` job: it skips the queue
//! so a person does not wait behind a directory's renames, and shares everything
//! else with them, one journal, one runner pool, one cancellation path.
//!
//! Source disposition uses the same validation, recovery and ownership checks as a
//! watched rename. An explicit keep request leaves the source visible.

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
pub fn prepare(registry: &Registry, request: &Request, state_dir: &Path) -> std::result::Result<Prepared, Diagnoses> {
	prepare_with(registry, request, Some(state_dir), &Cancellation::default())
}

/// Validate declarations without running tools or reading input contents.
pub fn declare(registry: &Registry, request: &Request) -> std::result::Result<Prepared, Diagnoses> {
	prepare_with(registry, request, None, &Cancellation::default())
}

fn prepare_with(
	registry: &Registry,
	request: &Request,
	state_dir: Option<&Path>,
	cancellation: &Cancellation,
) -> std::result::Result<Prepared, Diagnoses> {
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
	let plans = match state_dir {
		Some(state_dir) => crate::fidelity::plans_cancellable(
			registry,
			&tools::CancellableMachine(cancellation),
			&resolved,
			state_dir,
			cancellation,
		)?,
		None => plan::compile(registry, &crate::explain::Declared, &resolved)?,
	};
	let paths = targets(request, &plans, stem, &resolved)?;

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
		Ok(metadata) if !metadata.is_file() => problems.push(Error::Diagnostic(Diagnostic::global(
			Code::UsageArgs,
			format!("{} is not a regular file", request.input.display()),
		))),
		Ok(_) => {
			if let Err(error) = retype_core::safety::InputStamp::of(&request.input) {
				problems.push(Error::Diagnostic(Diagnostic::global(
					error.code(),
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
	resolved: &resolve::Resolved,
) -> std::result::Result<Vec<PathBuf>, Diagnoses> {
	let source_name = request.input.file_name().and_then(|name| name.to_str()).unwrap_or(stem);
	let derived = output::derive_with_source_name(stem, source_name, &resolved.groups);

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
	let mut source = source(prepared);
	source.fingerprint = super::watcher::carried_fingerprint(shared, id);

	Job {
		id: id.clone(),
		created: now,
		updated: now,
		source,
		intent: Some(print::intent(&prepared.resolved, &shared.registry())),
		outputs: prepared
			.plans
			.iter()
			.zip(&prepared.paths)
			.map(|(plan, path)| {
				let mut output = Output::pending(path.clone(), plan.format.clone(), plan.tier);
				output.fidelity = plan.fidelity.clone();
				output
			})
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
		consume_source: super::disposition::consumes_source(&prepared.resolved),
		source_disposition: None,
		admission: None,
	}
}

/// Run one submitted conversion and publish its terminal record.
pub fn run(shared: &Shared, request: &Request, id: &JobId, cancellation: &Cancellation) {
	let admitted = shared
		.jobs
		.lock()
		.ok()
		.and_then(|jobs| jobs.iter().find(|job| &job.id == id).cloned());
	let mut preparing = match admitted {
		Some(job) => job,
		None => match declare(&shared.registry(), request) {
			Ok(prepared) => queued(shared, &prepared, id),
			Err(problems) => {
				publish(shared, &unsatisfied(shared, request, problems, id));
				return;
			}
		},
	};
	#[cfg(windows)]
	let input_guard = preparing.admission.as_ref().map(|stamp| stamp.preparation_guard());
	preparing.set_state(JobState::Running);
	if let Err(error) = super::watcher::publish_checked(shared, &preparing) {
		super::watcher::report_unrecorded_failure(shared, &preparing, &error);
		return;
	}
	let snapshot = match (|| {
		if let Some(stamp) = &preparing.admission {
			stamp.verify(&request.input)?;
		}
		let snapshot = retype_core::safety::Snapshot::open_cancellable(&request.input, cancellation)?;
		if let Some(stamp) = &preparing.admission {
			stamp.verify_captured(&request.input, &snapshot.original, cancellation)?;
		}
		super::park::verify_input(&preparing, &snapshot.original)?;
		Ok::<_, retype_core::error::Error>(snapshot)
	})() {
		Ok(snapshot) => snapshot,
		Err(error) => {
			if cancellation.is_cancelled() {
				preparing.set_state(JobState::Cancelled);
			} else {
				preparing.fail(JobError::new(error.code().to_string(), error.to_string()));
			}
			publish(shared, &preparing);
			return;
		}
	};
	#[cfg(windows)]
	drop(input_guard);
	preparing.source.fingerprint = Some(snapshot.original.clone());
	if let Err(error) = super::watcher::publish_checked(shared, &preparing) {
		super::watcher::report_unrecorded_failure(shared, &preparing, &error);
		return;
	}
	let prepared = match prepare_with(&shared.registry(), request, Some(&shared.state_dir), cancellation) {
		Ok(prepared) => prepared,
		Err(problems) => {
			let mut failed = unsatisfied(shared, request, problems, id);
			if cancellation.is_cancelled() {
				failed.set_state(JobState::Cancelled);
				failed.error = Some(JobError::new(
					Code::JobCancelled.to_string(),
					"cancelled during preparation",
				));
			}
			publish(shared, &failed);
			return;
		}
	};
	if let Some(stamp) = &preparing.admission {
		if let Err(error) = stamp.verify_captured(&request.input, &snapshot.original, cancellation) {
			if cancellation.is_cancelled() {
				preparing.set_state(JobState::Cancelled);
			} else {
				preparing.fail(JobError::new(error.code().to_string(), error.to_string()));
			}
			publish(shared, &preparing);
			return;
		}
	}
	let mut started = queued(shared, &prepared, id);
	started.admission = preparing.admission;
	started.parked = super::watcher::carried(shared, id);
	if cancellation.is_cancelled() {
		started.set_state(JobState::Cancelled);
		publish(shared, &started);
		return;
	}
	if prepared.resolved.meta.ask && started.parked.is_none() {
		started.set_state(JobState::NeedsAttention);
		started.parked = Some(retype_core::job::Parked {
			reason: "ask".to_string(),
			deadline: None,
			batch: None,
			estimate: None,
			requested: Some(request.intent.clone()),
			corrected: None,
			dropped: Vec::new(),
			consume: false,
			force: request.force,
			input_fingerprint: Some(snapshot.original.clone()),
		});
		if let Err(error) = super::watcher::publish_parked(shared, &mut started, cancellation) {
			super::watcher::report_unrecorded_failure(shared, &started, &error);
		}
		return;
	}
	started.set_state(JobState::Running);
	if let Err(error) = crate::daemon::watcher::publish_checked(shared, &started) {
		crate::daemon::watcher::report_unrecorded_failure(shared, &started, &error);
		return;
	}

	/* The live source and independent recovery input remain available during conversion. */
	let store = retype_core::undo::Store::open(&shared.state_dir);
	let lease = match store.lease(id) {
		Ok(lease) => lease,
		Err(error) => {
			super::watcher::fail_before_conversion(shared, &started, "cannot acquire job ownership", &error);
			return;
		}
	};
	let undo_record = store.save_with_policy_cancellable(
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
			consume_source: started.consume_source,
		},
		shared.registry().undo(),
		cancellation,
	);

	if let Err(error) = &undo_record {
		if error.code() == Code::UndoLimit {
			super::watcher::park_storage(
				shared,
				started,
				snapshot.original.clone(),
				false,
				request.force,
				error,
				cancellation,
			);
			return;
		}
		if cancellation.is_cancelled() {
			started.set_state(JobState::Cancelled);
			started.error = Some(JobError::new(
				Code::JobCancelled.to_string(),
				"cancelled during backup; incomplete recovery remains protected",
			));
			publish(shared, &started);
			return;
		}
		super::watcher::fail_before_conversion(shared, &started, "cannot save undo record", error);
		return;
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
	let recovery = match store.retain(lease, undo_record.expect("backup errors handled above")) {
		Ok(recovery) => recovery,
		Err(error) => {
			super::watcher::fail_before_conversion(shared, &started, "cannot bind recovery ownership", &error);
			return;
		}
	};
	let mut outputs = Vec::new();
	let mut validation_failure = None;
	let mut output_gate = super::transaction::OutputGate::default();

	for (plan, path) in prepared.plans.iter().zip(&prepared.paths) {
		if let Some(reason) = output_gate.skipped() {
			outputs.push(record(path, plan, OutputState::Skipped, None, None, Some(reason)));
			continue;
		}
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
			recovery: recovery.retained(),
		};
		match transaction.run(plan, &snapshot.path, path, cancellation, publication) {
			Ok(report) => {
				if let Some(reason) = report.validation.reason() {
					validation_failure.get_or_insert_with(|| reason.to_string());
				}
				outputs.push(record(
					path,
					plan,
					OutputState::Done,
					Some(report.bytes),
					tail(&report),
					None,
				));
			}
			Err(error) => {
				output_gate.observe(&error);
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
	if job.state == JobState::Done {
		let completion = super::disposition::Completion {
			source: super::disposition::Source::Live {
				path: &prepared.input,
				fingerprint: &snapshot.original,
			},
			recovery,
			requested_outputs: prepared.plans.len(),
			validation_failure: validation_failure.as_deref(),
		};
		job = match super::disposition::finish_done(shared, job, completion, cancellation) {
			Ok(recorded) => recorded.into_job(),
			Err(failure) => {
				super::watcher::report_unrecorded_completion(shared, &failure.job, &failure.error);
				return;
			}
		};
	} else {
		if let Err(error) = crate::daemon::watcher::persist(shared, &job) {
			super::watcher::report_unrecorded_completion(shared, &job, &error);
			return;
		}
		if job.state == JobState::Cancelled {
			super::cancel::finish_after_persist(shared, Ok(recovery), &mut job);
		} else {
			drop(recovery);
		}
	}
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
	let metadata = std::fs::metadata(&prepared.input).ok();
	let mtime_ns = metadata
		.as_ref()
		.and_then(|metadata| metadata.modified().ok())
		.and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
		.and_then(|elapsed| i64::try_from(elapsed.as_nanos()).ok())
		.unwrap_or_default();
	Source {
		path: prepared.input.clone(),
		format: prepared.format.clone(),
		size: metadata.map(|metadata| metadata.len()).unwrap_or_default(),
		mtime_ns,
		fingerprint: None,
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
		fidelity: plan.fidelity.clone(),
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
			fingerprint: super::watcher::carried_fingerprint(shared, id),
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
		source_disposition: None,
		admission: None,
	}
}
