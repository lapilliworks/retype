//! `retype convert`, `docs/reference/cli.md` §2.
//!
//! The conversion itself belongs to the daemon: a submitted job shares the
//! journal, the runner pool and the cancellation path with the jobs a watcher
//! creates, so `convert` cannot drift from what a rename does. This module is the
//! client half, which submits and waits.
//!
//! Waiting is polling because the connection is request and answer, `docs/dev/architecture/ipc.md`
//! §5. One `jobs.get` per tenth of a second is nothing next to an encode, and it
//! keeps the daemon free to answer everyone else, including a cancel.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use retype_core::job::{JobState, Tier};
use retype_core::print;
use retype_core::registry::Registry;
use retype_ipc::{JobSubmit, SubmitMode};

use crate::error::{Result, exit_code_of};
use crate::output;

pub struct Request<'a> {
	pub input: &'a str,
	pub intent: &'a str,
	/// `-o`, which requires exactly one output.
	pub output: Option<&'a str>,
	pub force: bool,
	pub dry_run: bool,
	pub state_dir: &'a Path,
	pub quiet: bool,
	pub json: bool,
	pub allow_spawn: bool,
}

pub fn run(registry: &Registry, request: &Request<'_>) -> Result<ExitCode> {
	if request.dry_run {
		return crate::explain::run(
			registry,
			&crate::explain::Request {
				source: request.input,
				intent: request.intent,
				state_dir: Some(request.state_dir),
				json: request.json,
				quiet: request.quiet,
				output: request.output,
				force: request.force,
				dry_run: true,
			},
		);
	}

	let context = crate::client::context(request.state_dir, request.allow_spawn)?;
	let mut client = crate::client::connect_or_spawn(request.state_dir, request.allow_spawn)?;

	let submitted = match client.jobs_submit(&JobSubmit {
		input: PathBuf::from(request.input),
		intent: request.intent.to_string(),
		output: request.output.map(PathBuf::from),
		force: request.force,
		mode: SubmitMode::Immediate,
	}) {
		Ok(answer) => answer,
		Err(retype_ipc::ClientError::Rpc(error)) => {
			return Ok(ExitCode::from(output::fail(&error.code, &error.message)));
		}
		Err(other) => return Err(other.into()),
	};

	// The submission connection ends here: a client that held it for the whole
	// conversion would monopolise the daemon's connection slot, and then the
	// `jobs cancel` that should be able to stop this job could not get in,
	// `docs/dev/architecture/ipc.md` §5.
	drop(client);

	crate::verbose::note(format!(
		"submitted {} for {input}",
		submitted.id.as_str(),
		input = request.input
	));

	let started = Instant::now();
	let mut announced = Duration::ZERO;
	let id = submitted.id.as_str().to_string();

	let job = match retype_client::jobs::wait(&context, &id, |_job| {
		// A long encode should say it is still working rather than look hung. Two
		// seconds is slow enough to stay out of a one-line pipeline, and frequent
		// enough to be visible.
		let elapsed = started.elapsed();
		if !request.quiet && elapsed.saturating_sub(announced) >= Duration::from_secs(2) {
			eprintln!("retype: job {id} still running ({}s)", elapsed.as_secs());
			announced = elapsed;
		}
	}) {
		Ok(job) => job,
		Err(retype_client::Error::Client(retype_ipc::ClientError::Rpc(error))) => {
			return Ok(ExitCode::from(output::fail(&error.code, &error.message)));
		}
		Err(other) => return Err(other.into()),
	};

	if request.json {
		// The job keeps its own `v`; the envelope names the CLI shape.
		output::success(serde_json::json!({"job": job}));
	}

	let mut failure: Option<u8> = None;

	for output in &job.outputs {
		match output.state {
			retype_core::job::OutputState::Done => {
				if !request.quiet && !request.json {
					println!(
						"{}  {}  {}",
						output.path.display(),
						tier(output.tier),
						print::size(output.bytes.unwrap_or_default())
					);
				}
			}
			_ => {
				let code = output
					.error
					.as_ref()
					.map(|error| error.code.as_str())
					.unwrap_or("E_TOOL_FAILED");
				let message = output
					.error
					.as_ref()
					.map(|error| error.message.as_str())
					.unwrap_or("failed");

				if !request.json {
					eprintln!("retype: {code}: {message}");
				}

				failure.get_or_insert(exit_code_of(code));
			}
		}
	}

	if let Some(error) = &job.error {
		if !request.json {
			eprintln!("retype: {}: {}", error.code, error.message);
		}

		failure.get_or_insert(exit_code_of(&error.code));
	}

	if job.state == JobState::Cancelled {
		failure.get_or_insert(4);
	}

	match failure {
		Some(code) => Ok(ExitCode::from(code)),
		None => Ok(ExitCode::SUCCESS),
	}
}

fn tier(tier: Tier) -> String {
	match tier {
		Tier::Exact => "exact".to_string(),
		Tier::Converted => "converted".to_string(),
		Tier::Delegated => "delegated".to_string(),
	}
}
