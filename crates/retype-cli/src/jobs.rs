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
use crate::error::Result;

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

/// Every job whose id starts with `prefix`.
fn matching(client: &mut retype_ipc::Client, prefix: &str) -> Result<Vec<JobId>> {
	let answer = client.call_without_params("jobs.list")?;
	let prefix = prefix.to_ascii_uppercase();

	let mut found: Vec<JobId> = answer
		.get("jobs")
		.and_then(Value::as_array)
		.map(|jobs| {
			jobs.iter()
				.filter_map(|job| job.get("id").and_then(Value::as_str))
				.filter(|id| id.to_ascii_uppercase().starts_with(&prefix))
				.filter_map(|id| JobId::parse(id).ok())
				.collect()
		})
		.unwrap_or_default();

	found.sort();
	found.dedup();

	Ok(found)
}
