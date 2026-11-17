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

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use retype_core::job::JobState;
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
			return Ok(ExitCode::from(match error.detail {
				Some(detail) => output::fail_with_detail(&error.code, &error.message, *detail),
				None => output::fail(&error.code, &error.message),
			}));
		}
		Err(other) => return Err(other.into()),
	};

	let id = submitted.id.as_str().to_string();
	// Never hold the daemon's connection while waiting for a person or a conversion.
	drop(client);
	if submitted.state == JobState::NeedsAttention {
		if let Some(exit) = confirm_or_report(request, &id)? {
			return Ok(exit);
		}
	}

	crate::verbose::note(format!("submitted {id} for {input}", input = request.input));

	let started = Instant::now();
	let mut announced = Duration::ZERO;

	let job = loop {
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
			Err(error) => {
				return Ok(ExitCode::from(output::fail_with_detail(
					&error.code(),
					format!("job {id}: {error}; inspect the recorded job before retrying"),
					serde_json::json!({"id": id}),
				)));
			}
		};
		if job.state == JobState::NeedsAttention {
			if let Some(exit) = confirm_or_report(request, &id)? {
				return Ok(exit);
			}
		} else {
			break job;
		}
	};

	if request.json {
		if let Some((code, message)) = output::job_failure(&job) {
			return Ok(ExitCode::from(output::fail_with_detail(
				&code,
				message,
				serde_json::json!({"job": job}),
			)));
		}
		// The job keeps its own `v`; the envelope names the CLI shape.
		output::success(serde_json::json!({"job": job}));
		return Ok(ExitCode::SUCCESS);
	}

	if !request.quiet && output::job_failure(&job).is_none() {
		if let Some(warning) = output::source_disposition_warning(&job) {
			eprintln!("retype: {warning}");
		}
	}

	let mut failure: Option<u8> = None;

	for output in &job.outputs {
		match output.state {
			retype_core::job::OutputState::Done => {
				if !request.quiet && !request.json {
					println!(
						"{}  {}  {}",
						output.path.display(),
						output
							.fidelity
							.as_ref()
							.map(|evidence| evidence.reported_tier(output.tier))
							.unwrap_or("unclaimed"),
						print::size(output.bytes.unwrap_or_default())
					);
				}
			}
			_ => {
				// Preparation can stop before any output runs; the job carries that failure.
				let Some(error) = &output.error else { continue };
				let code = error.code.as_str();
				let message = error.message.as_str();

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

		failure = Some(exit_code_of(&error.code));
	}

	if failure.is_none() {
		if let Some((code, message)) = output::job_failure(&job) {
			eprintln!("retype: {code}: {message}");
			failure = Some(exit_code_of(code.as_str()));
		}
	}

	match failure {
		Some(code) => Ok(ExitCode::from(code)),
		None => Ok(ExitCode::SUCCESS),
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Confirmation {
	Run,
	Cancel,
	Pending,
}

fn confirm_or_report(request: &Request<'_>, id: &str) -> Result<Option<ExitCode>> {
	let job = {
		let mut client = crate::client::connect_or_spawn(request.state_dir, request.allow_spawn)?;
		client.jobs_get(id)?
	};
	let asks = job.parked.as_ref().is_some_and(|parked| parked.reason == "ask");
	let confirmation = if asks && io::stdin().is_terminal() && io::stderr().is_terminal() {
		prompt_confirmation(&mut io::stdin().lock(), &mut io::stderr().lock(), request)?
	} else {
		Confirmation::Pending
	};

	match confirmation {
		Confirmation::Run => {
			let mut client = crate::client::connect_or_spawn(request.state_dir, request.allow_spawn)?;
			client.jobs_resolve(id, retype_ipc::ResolveMode::Now, request.force)?;
			Ok(None)
		}
		Confirmation::Cancel => {
			let mut client = crate::client::connect_or_spawn(request.state_dir, request.allow_spawn)?;
			client.jobs_cancel(id)?;
			Ok(Some(ExitCode::from(output::fail(
				&retype_core::error::Code::JobCancelled,
				format!("job {id} was cancelled"),
			))))
		}
		Confirmation::Pending => {
			let force = if request.force { " --force" } else { "" };
			let reason = if job.parked.as_ref().is_some_and(|parked| parked.reason == "storage") {
				"is waiting for undo storage; allow expiry, explicitly clean recovery or increase [undo] quota"
			} else {
				"needs confirmation"
			};
			let message = format!(
				"job {id} {reason}; run `retype jobs resolve {id} --now{force}` \
				 or `retype jobs cancel {id}`"
			);
			let code = if job.parked.as_ref().is_some_and(|parked| parked.reason == "storage") {
				retype_core::error::Code::UndoLimit
			} else {
				retype_core::error::Code::JobState
			};
			Ok(Some(ExitCode::from(output::fail_with_detail(
				&code,
				message,
				serde_json::json!({"id": id, "state": "needs-attention", "reason": job.parked.as_ref().map(|parked| &parked.reason)}),
			))))
		}
	}
}

fn prompt_confirmation(
	input: &mut impl BufRead,
	output: &mut impl Write,
	request: &Request<'_>,
) -> io::Result<Confirmation> {
	write!(
		output,
		"retype: convert {} as {}? [y/n; Enter leaves pending] ",
		request.input, request.intent
	)?;
	output.flush()?;
	let mut answer = String::new();
	input.read_line(&mut answer)?;
	Ok(match answer.trim().to_ascii_lowercase().as_str() {
		"y" | "yes" => Confirmation::Run,
		"n" | "no" => Confirmation::Cancel,
		_ => Confirmation::Pending,
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn confirmation_never_infers_consent_from_an_empty_or_unknown_answer() {
		let request = Request {
			input: "photo.png",
			intent: "jpg+ask",
			output: None,
			force: false,
			dry_run: false,
			state_dir: Path::new("."),
			quiet: false,
			json: false,
			allow_spawn: false,
		};

		for (answer, expected) in [
			("yes\n", Confirmation::Run),
			("N\n", Confirmation::Cancel),
			("\n", Confirmation::Pending),
			("", Confirmation::Pending),
			("maybe\n", Confirmation::Pending),
		] {
			let mut input = io::Cursor::new(answer);
			let mut output = Vec::new();
			assert_eq!(
				prompt_confirmation(&mut input, &mut output, &request).unwrap(),
				expected
			);
		}
	}
}
