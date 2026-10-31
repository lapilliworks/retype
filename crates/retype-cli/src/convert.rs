//! `retype convert`, `docs/cli.md` §2.
//!
//! The conversion itself belongs to the daemon: a submitted job shares the
//! journal, the runner pool and the cancellation path with the jobs a watcher
//! creates, so `convert` cannot drift from what a rename does. This module is the
//! client half, which submits and waits.
//!
//! Waiting is polling because the connection is request and answer, `docs/ipc.md`
//! §5. One `jobs.get` per tenth of a second is nothing next to an encode, and it
//! keeps the daemon free to answer everyone else, including a cancel.

use std::path::Path;
use std::process::ExitCode;
use std::thread::sleep;
use std::time::Duration;

use retype_core::registry::Registry;
use serde_json::Value;

use crate::client;
use crate::error::{Error, Result, exit_code_of};
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

/// How often a submission is asked about while it runs.
const POLL: Duration = Duration::from_millis(100);

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

	let mut client = client::connect_or_spawn(request.state_dir, request.allow_spawn)?;
	let mut params = serde_json::json!({
		"input": request.input,
		"intent": request.intent,
		"force": request.force,
		"mode": "immediate",
	});

	if let Some(output) = request.output {
		params["output"] = Value::String(output.to_string());
	}

	let submitted = match client.call("jobs.submit", params) {
		Ok(answer) => answer,
		// Everything that can fail before a byte moves fails at submission, with
		// the code the same failure would have had locally, `docs/cli.md` §6.
		Err(retype_ipc::ClientError::Rpc(error)) => {
			return Ok(ExitCode::from(output::fail(&error.code, &error.message)));
		}
		Err(other) => return Err(Error::Client(other)),
	};

	// The submission connection ends here: a client that held it for the whole
	// conversion would monopolise the daemon's connection slot, and then the
	// `jobs cancel` that should be able to stop this job could not get in,
	// `docs/ipc.md` §5.
	drop(client);

	crate::verbose::note(format!(
		"submitted {} for {input}",
		submitted.get("id").and_then(Value::as_str).unwrap_or_default(),
		input = request.input
	));

	let id = submitted
		.get("id")
		.and_then(Value::as_str)
		.unwrap_or_default()
		.to_string();
	let job = wait(request.state_dir, request.allow_spawn, &id, request.quiet)?;

	if request.json {
		output::success(job.clone());
	}

	let outputs = job
		.get("outputs")
		.and_then(Value::as_array)
		.cloned()
		.unwrap_or_default();
	let mut failure: Option<u8> = None;

	for output in &outputs {
		let path = output.get("path").and_then(Value::as_str).unwrap_or_default();
		let state = output.get("state").and_then(Value::as_str).unwrap_or_default();

		match state {
			"done" => {
				if !request.quiet && !request.json {
					println!(
						"{path}  {}  {} bytes",
						output.get("tier").and_then(Value::as_str).unwrap_or_default(),
						output.get("bytes").and_then(Value::as_u64).unwrap_or_default()
					);
				}
			}
			_ => {
				let error = output.get("error").and_then(Value::as_object);
				let code = error
					.and_then(|error| error.get("code"))
					.and_then(Value::as_str)
					.unwrap_or("E_TOOL_FAILED");
				let message = error
					.and_then(|error| error.get("message"))
					.and_then(Value::as_str)
					.unwrap_or("failed");

				if !request.json {
					eprintln!("retype: {code}: {message}");
				}

				failure.get_or_insert(exit_code_of(code));
			}
		}
	}

	let state = job.get("state").and_then(Value::as_str).unwrap_or_default();

	// `error` is present but null on a job that had none, so it has to be read as
	// an object rather than as a key.
	if let Some(error) = job.get("error").and_then(Value::as_object) {
		let code = error.get("code").and_then(Value::as_str).unwrap_or("E_TOOL_FAILED");

		if !request.json {
			eprintln!(
				"retype: {code}: {}",
				error.get("message").and_then(Value::as_str).unwrap_or("failed")
			);
		}

		failure.get_or_insert(exit_code_of(code));
	}

	if state == "cancelled" {
		failure.get_or_insert(4);
	}

	match failure {
		Some(code) => Ok(ExitCode::from(code)),
		None => Ok(ExitCode::SUCCESS),
	}
}

/// Ask until the job reaches a state that will not change, on a fresh connection
/// each time so the daemon is never held.
fn wait(state_dir: &Path, allow_spawn: bool, id: &str, quiet: bool) -> Result<Value> {
	let started = std::time::Instant::now();
	let mut announced = Duration::ZERO;

	loop {
		let job = {
			let mut client = client::connect_or_spawn(state_dir, allow_spawn)?;
			client.call("jobs.get", serde_json::json!({"id": id}))?
		};

		let state = job.get("state").and_then(Value::as_str).unwrap_or_default();

		if crate::jobs::terminal(state) {
			return Ok(job);
		}

		// A long encode should say it is still working rather than look hung. Two
		// seconds is slow enough to stay out of a one-line pipeline, and frequent
		// enough to be visible.
		let elapsed = started.elapsed();
		if !quiet && elapsed.saturating_sub(announced) >= Duration::from_secs(2) {
			eprintln!("retype: job {id} still running ({}s)", elapsed.as_secs());
			announced = elapsed;
		}

		sleep(POLL);
	}
}
