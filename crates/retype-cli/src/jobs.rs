//! `retype jobs`, `docs/cli.md` §5.
//!
//! The daemon owns the jobs, so this is a client and nothing else: it asks, prints
//! and exits. `jobs cancel` accepts a full id or a unique prefix, and an ambiguous
//! prefix is an error naming the candidates rather than a guess.

use std::path::Path;
use std::process::ExitCode;

use retype_core::job::JobId;
use serde_json::Value;

use crate::client;
use crate::error::{Result, exit_code};

pub fn list(
	state_dir: &Path,
	state: Option<&str>,
	limit: Option<usize>,
	json: bool,
	allow_spawn: bool,
) -> Result<ExitCode> {
	let mut client = client::connect_or_spawn(state_dir, allow_spawn)?;
	let answer = client.call_without_params("jobs.list")?;

	let jobs: Vec<Value> = answer
		.get("jobs")
		.and_then(Value::as_array)
		.cloned()
		.unwrap_or_default();

	let mut shown = 0;

	for job in jobs.iter().rev() {
		let job_state = job.get("state").and_then(Value::as_str).unwrap_or_default();

		if let Some(wanted) = state {
			if job_state != wanted {
				continue;
			}
		}

		if let Some(limit) = limit {
			if shown >= limit {
				break;
			}
		}

		shown += 1;

		match json {
			true => println!("{job}"),
			false => println!(
				"{:<28}{:<16}{}",
				job.get("id").and_then(Value::as_str).unwrap_or_default(),
				job_state,
				job.get("source").and_then(Value::as_str).unwrap_or_default()
			),
		}
	}

	Ok(ExitCode::SUCCESS)
}

/// One job, as the daemon recorded it. The record is JSON because it is the
/// thing a bug report or a script wants, and every other shape would be a lossy
/// rendering of it.
pub fn get(state_dir: &Path, prefix: &str, allow_spawn: bool) -> Result<ExitCode> {
	let mut client = client::connect_or_spawn(state_dir, allow_spawn)?;

	let candidates = matching(&mut client, prefix)?;
	let id = match candidates.len() {
		1 => candidates[0].clone(),
		0 => {
			eprintln!("retype: E_JOB_STATE: no job matches '{prefix}'");
			return Ok(ExitCode::from(7));
		}
		_ => {
			eprintln!("retype: E_IPC_AMBIGUOUS: '{prefix}' matches {} jobs", candidates.len());

			for candidate in &candidates {
				eprintln!("  {candidate}");
			}

			return Ok(ExitCode::from(7));
		}
	};

	println!("{}", client.call("jobs.get", serde_json::json!({"id": id.as_str()}))?);

	Ok(ExitCode::SUCCESS)
}

/// What a job's tool printed.
///
/// The file holds everything and the journal holds a tail, so a job whose log is
/// gone — an older job, or one pruned with its undo record — still answers from
/// the record. `--follow` prints what arrives while the job runs and stops when
/// it finishes.
pub fn logs(state_dir: &Path, prefix: &str, follow: bool, allow_spawn: bool) -> Result<ExitCode> {
	let mut client = client::connect_or_spawn(state_dir, allow_spawn)?;

	let candidates = matching(&mut client, prefix)?;
	let id = match candidates.len() {
		1 => candidates[0].clone(),
		0 => {
			eprintln!("retype: E_JOB_STATE: no job matches '{prefix}'");
			return Ok(ExitCode::from(7));
		}
		_ => {
			eprintln!("retype: E_IPC_AMBIGUOUS: '{prefix}' matches {} jobs", candidates.len());

			for candidate in &candidates {
				eprintln!("  {candidate}");
			}

			return Ok(ExitCode::from(7));
		}
	};

	let path = crate::state::log_path(state_dir, &id);
	let mut printed = 0;

	if let Some((text, read)) = read_from(&path, 0) {
		print!("{text}");
		printed = read;
	}

	if !follow {
		if printed == 0 {
			// No file: the record's tail is what there is, and saying so is better
			// than an empty screen.
			for tail in recorded_tails(&mut client, &id)? {
				eprintln!("{tail}");
			}
		}

		return Ok(ExitCode::SUCCESS);
	}

	loop {
		if let Some((text, read)) = read_from(&path, printed) {
			print!("{text}");
			printed = read;
		}

		let job = client.call("jobs.get", serde_json::json!({"id": id.as_str()}))?;
		let state = job.get("state").and_then(Value::as_str).unwrap_or_default();

		// One more read after the job ends, because the last write and the
		// transition are not atomic with respect to each other.
		if matches!(state, "done" | "failed" | "partial" | "cancelled") {
			if let Some((text, _)) = read_from(&path, printed) {
				print!("{text}");
			}

			return Ok(ExitCode::SUCCESS);
		}

		std::thread::sleep(std::time::Duration::from_millis(100));
	}
}

/// Everything in `path` from `offset`, and the new offset.
fn read_from(path: &Path, offset: u64) -> Option<(String, u64)> {
	use std::io::{Read, Seek, SeekFrom};

	let mut file = std::fs::File::open(path).ok()?;
	let length = file.metadata().ok()?.len();

	if length <= offset {
		return Some((String::new(), offset));
	}

	file.seek(SeekFrom::Start(offset)).ok()?;
	let mut bytes = Vec::new();
	file.read_to_end(&mut bytes).ok()?;

	Some((String::from_utf8_lossy(&bytes).to_string(), length))
}

/// The tails the journal kept, for a job whose log file is gone.
fn recorded_tails(client: &mut retype_ipc::Client, id: &JobId) -> Result<Vec<String>> {
	let job = client.call("jobs.get", serde_json::json!({"id": id.as_str()}))?;
	let mut tails = Vec::new();

	for output in job
		.get("outputs")
		.and_then(Value::as_array)
		.cloned()
		.unwrap_or_default()
	{
		if let Some(tail) = output.get("stderr").and_then(Value::as_str) {
			tails.push(tail.to_string());
		}

		if let Some(message) = output
			.get("error")
			.and_then(Value::as_object)
			.and_then(|error| error.get("message"))
			.and_then(Value::as_str)
		{
			tails.push(message.to_string());
		}
	}

	if let Some(error) = job.get("error").and_then(Value::as_object) {
		if let Some(message) = error.get("message").and_then(Value::as_str) {
			tails.push(message.to_string());
		}
	}

	Ok(tails)
}

pub fn resume(state_dir: &Path, prefix: &str, force: bool, json: bool, allow_spawn: bool) -> Result<ExitCode> {
	let mut client = client::connect_or_spawn(state_dir, allow_spawn)?;

	let candidates = matching(&mut client, prefix)?;
	let id = match candidates.len() {
		1 => candidates[0].clone(),
		0 => {
			eprintln!("retype: E_JOB_STATE: no job matches '{prefix}'");
			return Ok(ExitCode::from(7));
		}
		_ => {
			eprintln!("retype: E_IPC_AMBIGUOUS: '{prefix}' matches {} jobs", candidates.len());

			for candidate in &candidates {
				eprintln!("  {candidate}");
			}

			return Ok(ExitCode::from(7));
		}
	};

	let answer = client.call("jobs.resume", serde_json::json!({"id": id.as_str(), "force": force}))?;

	match json {
		true => println!("{answer}"),
		false => println!(
			"{}  {}",
			id.as_str(),
			answer.get("state").and_then(Value::as_str).unwrap_or("resumed")
		),
	}

	Ok(ExitCode::SUCCESS)
}

pub fn cancel(state_dir: &Path, prefix: &str, json: bool, allow_spawn: bool) -> Result<ExitCode> {
	let mut client = client::connect_or_spawn(state_dir, allow_spawn)?;

	let candidates = matching(&mut client, prefix)?;
	let id = match candidates.len() {
		1 => candidates[0].clone(),
		0 => {
			eprintln!("retype: E_JOB_STATE: no job matches '{prefix}'");
			return Ok(ExitCode::from(7));
		}
		_ => {
			eprintln!("retype: E_IPC_AMBIGUOUS: '{prefix}' matches {} jobs", candidates.len());

			for candidate in &candidates {
				eprintln!("  {candidate}");
			}

			return Ok(ExitCode::from(7));
		}
	};

	let answer = client.call("jobs.cancel", serde_json::json!({"id": id.as_str()}))?;

	match json {
		true => println!("{answer}"),
		false => println!(
			"{}  {}",
			id.as_str(),
			answer.get("state").and_then(Value::as_str).unwrap_or("cancelled")
		),
	}

	Ok(ExitCode::SUCCESS)
}

/// Act on a parked job, `docs/deferral.md` §8: run it now, or run what a repair
/// produced.
pub fn resolve(state_dir: &Path, prefix: &str, convert: bool, json: bool, allow_spawn: bool) -> Result<ExitCode> {
	let mut client = client::connect_or_spawn(state_dir, allow_spawn)?;
	let id = match single(&mut client, prefix)? {
		Ok(id) => id,
		Err(code) => return Ok(code),
	};

	let mode = match convert {
		true => "convert",
		false => "now",
	};

	let answer = match client.call("jobs.resolve", serde_json::json!({"id": id.as_str(), "mode": mode})) {
		Ok(answer) => answer,
		Err(retype_ipc::ClientError::Rpc(error)) => {
			eprintln!("retype: {}: {}", error.code, error.message);
			return Ok(ExitCode::from(exit_code(&error.code)));
		}
		Err(other) => return Err(other.into()),
	};

	match json {
		true => println!("{answer}"),
		false => println!(
			"{}  {}",
			id.as_str(),
			answer.get("state").and_then(Value::as_str).unwrap_or("queued")
		),
	}

	Ok(ExitCode::SUCCESS)
}

/// The one job a prefix names, or the exit code to answer with.
fn single(client: &mut retype_ipc::Client, prefix: &str) -> Result<std::result::Result<JobId, ExitCode>> {
	let candidates = matching(client, prefix)?;

	match candidates.len() {
		1 => Ok(Ok(candidates[0].clone())),
		0 => {
			eprintln!("retype: E_JOB_STATE: no job matches '{prefix}'");
			Ok(Err(ExitCode::from(7)))
		}
		_ => {
			eprintln!("retype: E_IPC_AMBIGUOUS: '{prefix}' matches {} jobs", candidates.len());

			for candidate in &candidates {
				eprintln!("  {candidate}");
			}

			Ok(Err(ExitCode::from(7)))
		}
	}
}

/// Every job whose id starts with `prefix`.
fn matching(client: &mut retype_ipc::Client, prefix: &str) -> Result<Vec<JobId>> {
	let answer = client.call_without_params("jobs.list")?;
	let prefix = prefix.to_ascii_lowercase();

	let mut found: Vec<JobId> = answer
		.get("jobs")
		.and_then(Value::as_array)
		.map(|jobs| {
			jobs.iter()
				.filter_map(|job| job.get("id").and_then(Value::as_str))
				.filter(|id| id.to_ascii_lowercase().starts_with(&prefix))
				.filter_map(|id| JobId::parse(id).ok())
				.collect()
		})
		.unwrap_or_default();

	found.sort();
	found.dedup();

	Ok(found)
}
