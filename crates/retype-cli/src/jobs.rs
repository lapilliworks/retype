//! `retype jobs`, `docs/cli.md` §5.
//!
//! The daemon owns the jobs, so this is a client and nothing else: it asks, prints
//! and exits. `jobs cancel` accepts a full id or a unique prefix, and an ambiguous
//! prefix is an error naming the candidates rather than a guess.

use std::path::Path;
use std::process::ExitCode;

use retype_core::error::Code;
use retype_core::identifier::normalize;
use retype_core::job::JobId;
use retype_core::print;
use serde_json::Value;

use crate::client;
use crate::error::Result;
use crate::output;

/// The states `jobs list --state` accepts, `docs/dev/architecture/state.md` §3.
pub const STATES: [&str; 9] = [
	"pending",
	"queued",
	"running",
	"done",
	"partial",
	"failed",
	"cancelled",
	"superseded",
	"needs-attention",
];

pub fn list(
	state_dir: &Path,
	state: Option<&str>,
	limit: Option<usize>,
	since: Option<&str>,
	json: bool,
	allow_spawn: bool,
) -> Result<ExitCode> {
	let state = state.map(normalize);

	let since = match since {
		Some(written) => match JobId::parse(written) {
			Ok(id) => Some(id),
			Err(_) => {
				return Ok(ExitCode::from(output::fail(
					&Code::UsageArgs,
					format!("'{written}' is not a job id"),
				)));
			}
		},
		None => None,
	};

	let mut client = client::connect_or_spawn(state_dir, allow_spawn)?;
	let answer = client.call_without_params("jobs.list")?;

	let jobs: Vec<Value> = answer
		.get("jobs")
		.and_then(Value::as_array)
		.cloned()
		.unwrap_or_default();

	let ids: Vec<&str> = jobs
		.iter()
		.filter_map(|job| job.get("id").and_then(Value::as_str))
		.collect();

	let mut shown = 0;

	for job in jobs.iter().rev() {
		let job_state = job.get("state").and_then(Value::as_str).unwrap_or_default();
		let job_id = job.get("id").and_then(Value::as_str).unwrap_or_default();

		if let Some(since) = &since {
			if job_id <= since.as_str() {
				continue;
			}
		}

		if let Some(wanted) = state.as_deref() {
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
			true => {
				let mut item = job.clone();
				if let Some(object) = item.as_object_mut() {
					object.insert("kind".to_string(), Value::String("job".to_string()));
				}
				println!("{item}");
			}
			false => println!(
				"{:<28}{:<16}{}",
				shortest_prefix(job_id, &ids),
				job_state,
				job.get("source").and_then(Value::as_str).unwrap_or_default()
			),
		}
	}

	Ok(ExitCode::SUCCESS)
}

/// The shortest handle that names one id among `ids`, or the id itself when nothing
/// shorter is unique. Listings print this so copy-paste is short by default.
fn shortest_prefix(id: &str, ids: &[&str]) -> String {
	for length in JobId::PREFIX_MIN..=id.len() {
		let prefix = &id[..length];
		if ids.iter().filter(|other| other.starts_with(prefix)).count() == 1 {
			return prefix.to_string();
		}
	}

	id.to_string()
}

/// One job, as the daemon recorded it. The record is JSON because it is the
/// thing a bug report or a script wants, and every other shape would be a lossy
/// rendering of it.
pub fn get(state_dir: &Path, prefix: &str, allow_spawn: bool) -> Result<ExitCode> {
	if let Some(code) = handle(prefix) {
		return Ok(code);
	}

	let mut client = client::connect_or_spawn(state_dir, allow_spawn)?;

	let id = match one(&mut client, prefix)? {
		Ok(id) => id,
		Err(code) => return Ok(code),
	};

	output::record(client.call("jobs.get", serde_json::json!({"id": id.as_str()}))?);

	Ok(ExitCode::SUCCESS)
}

/// What a job's tool printed.
///
/// The file holds everything and the journal holds a tail, so a job whose log is
/// gone — an older job, or one pruned with its undo record — still answers from
/// the record. `--follow` prints what arrives while the job runs and stops when
/// it finishes.
pub fn logs(state_dir: &Path, prefix: &str, follow: bool, json: bool, allow_spawn: bool) -> Result<ExitCode> {
	if let Some(code) = handle(prefix) {
		return Ok(code);
	}

	let mut client = client::connect_or_spawn(state_dir, allow_spawn)?;
	let id = match one(&mut client, prefix)? {
		Ok(id) => id,
		Err(code) => return Ok(code),
	};

	let path = crate::state::log_path(state_dir, &id);
	let mut printed = 0;
	let mut held = String::new();

	if let Some((text, read)) = read_from(&path, 0) {
		held.push_str(&text);
		printed = read;
	}

	let mut source = "file";

	if !follow {
		if printed == 0 {
			// No file: the record's tail is what there is, and saying so is better
			// than an empty screen.
			source = "record";
			held = recorded_tails(&mut client, &id)?.join("\n");
		}

		if json {
			output::success(serde_json::json!({
				"id": id.as_str(),
				"source": source,
				"log": held,
			}));
		} else if source == "record" {
			eprintln!("retype: {} has no log file, answering from the record", id.as_str());
			eprintln!("{held}");
		} else {
			print!("{held}");
			if !held.ends_with('\n') && !held.is_empty() {
				println!();
			}
		}

		return Ok(ExitCode::SUCCESS);
	}

	if json {
		if !held.is_empty() {
			output::success(serde_json::json!({"id": id.as_str(), "source": source, "log": held}));
		}
	} else {
		print!("{held}");
	}

	loop {
		if let Some((text, read)) = read_from(&path, printed) {
			printed = read;

			if json {
				output::success(serde_json::json!({"id": id.as_str(), "source": "file", "log": text}));
			} else {
				print!("{text}");
			}
		}

		let job = client.call("jobs.get", serde_json::json!({"id": id.as_str()}))?;
		let state = job.get("state").and_then(Value::as_str).unwrap_or_default();

		// One more read after the job ends, because the last write and the
		// transition are not atomic with respect to each other.
		if terminal(state) {
			if let Some((text, _)) = read_from(&path, printed) {
				if json {
					output::success(serde_json::json!({"id": id.as_str(), "source": "file", "log": text}));
				} else {
					print!("{text}");
				}
			}

			return Ok(ExitCode::SUCCESS);
		}

		std::thread::sleep(std::time::Duration::from_millis(100));
	}
}

/// Whether a job state will not change again. `pending` and `needs-attention` are
/// waiting states, and a superseded job is as finished as a cancelled one.
pub fn terminal(state: &str) -> bool {
	matches!(state, "done" | "failed" | "partial" | "cancelled" | "superseded")
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
	if let Some(code) = handle(prefix) {
		return Ok(code);
	}

	let mut client = client::connect_or_spawn(state_dir, allow_spawn)?;

	let id = match one(&mut client, prefix)? {
		Ok(id) => id,
		Err(code) => return Ok(code),
	};

	let answer = client.call("jobs.resume", serde_json::json!({"id": id.as_str(), "force": force}))?;

	match json {
		true => output::success(answer),
		false => println!(
			"{}  {}",
			id.as_str(),
			answer.get("state").and_then(Value::as_str).unwrap_or("resumed")
		),
	}

	Ok(ExitCode::SUCCESS)
}

pub fn cancel(state_dir: &Path, prefix: &str, json: bool, allow_spawn: bool) -> Result<ExitCode> {
	if let Some(code) = handle(prefix) {
		return Ok(code);
	}

	let mut client = client::connect_or_spawn(state_dir, allow_spawn)?;

	let id = match one(&mut client, prefix)? {
		Ok(id) => id,
		Err(code) => return Ok(code),
	};

	let answer = client.call("jobs.cancel", serde_json::json!({"id": id.as_str()}))?;

	match json {
		true => output::success(answer),
		false => println!(
			"{}  {}",
			id.as_str(),
			answer.get("state").and_then(Value::as_str).unwrap_or("cancelled")
		),
	}

	Ok(ExitCode::SUCCESS)
}

/// Walk the parked jobs and read one key each, `docs/deferral.md` §8.
///
/// The terminal path exists so deferral does not depend on notifications: a person at
/// a prompt can run a job now, run what can be carried, or refuse it.
pub fn interactive(state_dir: &Path, json: bool, allow_spawn: bool) -> Result<ExitCode> {
	use std::io::{BufRead, IsTerminal, Write};

	if !std::io::stdin().is_terminal() {
		return Ok(ExitCode::from(output::fail(
			&Code::UsageArgs,
			"--interactive needs a terminal; `jobs list --state pending` and `jobs resolve` work without one",
		)));
	}

	let mut client = client::connect_or_spawn(state_dir, allow_spawn)?;
	let parked = parked(&mut client)?;

	if parked.is_empty() {
		if !json {
			eprintln!("retype: nothing is parked");
		}

		return Ok(ExitCode::SUCCESS);
	}

	let mut stdin = std::io::stdin().lock();

	for id in parked {
		let job = client.call("jobs.get", serde_json::json!({"id": id.as_str()}))?;

		if !json {
			print!("{}", describe(&job));
		}

		loop {
			print!("[n]ow  [c]onvert  [r]efuse  [s]kip  [q]uit > ");
			let _ = std::io::stdout().flush();

			let mut line = String::new();
			if stdin.read_line(&mut line)? == 0 {
				return Ok(ExitCode::SUCCESS);
			}

			let key = line.trim().chars().next().unwrap_or_default();
			let Some(action) = choose(key, &job) else {
				eprintln!("retype: '{key}' is not one of the keys offered for this job");
				continue;
			};

			match action {
				Action::Quit => return Ok(ExitCode::SUCCESS),
				Action::Skip => break,
				Action::Now => {
					answer(client.call("jobs.resolve", serde_json::json!({"id": id.as_str(), "mode": "now"}))?);
					break;
				}
				Action::Convert => {
					answer(client.call(
						"jobs.resolve",
						serde_json::json!({"id": id.as_str(), "mode": "convert"}),
					)?);
					break;
				}
				Action::Refuse => {
					answer(client.call("jobs.cancel", serde_json::json!({"id": id.as_str()}))?);
					break;
				}
			}
		}
	}

	Ok(ExitCode::SUCCESS)
}

/// What one key asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
	Now,
	Convert,
	Refuse,
	Skip,
	Quit,
}

/// The action a key names, or nothing when the key does not apply to this job: a job
/// that cannot run as asked has no `now`, and one with no corrected intent has no
/// `convert`.
fn choose(key: char, job: &Value) -> Option<Action> {
	let key = key.to_ascii_lowercase();
	let state = job.get("state").and_then(Value::as_str).unwrap_or_default();
	let corrected = job.pointer("/parked/corrected").and_then(Value::as_str);

	match key {
		'n' if state == "pending" => Some(Action::Now),
		'c' if corrected.is_some() => Some(Action::Convert),
		'r' => Some(Action::Refuse),
		's' => Some(Action::Skip),
		'q' => Some(Action::Quit),
		_ => None,
	}
}

/// The jobs that are waiting for a person or a timer, oldest first.
fn parked(client: &mut retype_ipc::Client) -> Result<Vec<JobId>> {
	let answer = client.call_without_params("jobs.list")?;

	let mut parked: Vec<(i64, JobId)> = answer
		.get("jobs")
		.and_then(Value::as_array)
		.map(|jobs| {
			jobs.iter()
				.filter(|job| matches!(job["state"].as_str(), Some("pending" | "needs-attention")))
				.filter_map(|job| {
					let id = JobId::parse(job["id"].as_str()?).ok()?;
					let created = job["created"].as_i64().unwrap_or_default();
					Some((created, id))
				})
				.collect()
		})
		.unwrap_or_default();

	parked.sort_by_key(|(created, _)| *created);
	Ok(parked.into_iter().map(|(_, id)| id).collect())
}

/// Four lines a person can act on, `docs/deferral.md` §6.
fn describe(job: &Value) -> String {
	let field = |path: &str| job.pointer(path).and_then(Value::as_str).unwrap_or_default();

	let mut text = format!("\n{}\n", field("/source/path"));
	let reason = field("/parked/reason");

	let estimate = job
		.pointer("/parked/estimate")
		.map(|estimate| {
			let ms = estimate["ms"].as_u64().unwrap_or_default();
			let bytes = estimate["bytes"].as_u64().unwrap_or_default();
			format!("  (about {}, {} estimated)", duration(ms), print::size(bytes))
		})
		.unwrap_or_default();

	text.push_str(&format!("  waiting {reason}{estimate}\n"));
	text.push_str(&format!("  asked   {}\n", job["intent"].as_str().unwrap_or_default()));

	if let Some(corrected) = job.pointer("/parked/corrected").and_then(Value::as_str) {
		text.push_str(&format!("  runs    {corrected}\n"));
	}

	text
}

fn duration(ms: u64) -> String {
	match ms {
		0..=999 => format!("{ms}ms"),
		ms if ms < 60_000 => format!("{:.1}s", ms as f64 / 1000.0),
		ms => format!("{}m{:02}s", ms / 60_000, ms % 60_000 / 1000),
	}
}

/// Report what the daemon answered, on stderr so it does not mix with the prompt.
fn answer(answer: Value) {
	if let Some(state) = answer.get("state").and_then(Value::as_str) {
		eprintln!("  {state}");
	}
}

/// Act on a parked job, `docs/deferral.md` §8: run it now, or run what a repair
/// produced.
pub fn resolve(
	state_dir: &Path,
	prefix: &str,
	convert: bool,
	force: bool,
	json: bool,
	allow_spawn: bool,
) -> Result<ExitCode> {
	if let Some(code) = handle(prefix) {
		return Ok(code);
	}

	let mut client = client::connect_or_spawn(state_dir, allow_spawn)?;
	let id = match single(&mut client, prefix)? {
		Ok(id) => id,
		Err(code) => return Ok(code),
	};

	let mode = match convert {
		true => "convert",
		false => "now",
	};

	let answer = match client.call(
		"jobs.resolve",
		serde_json::json!({"id": id.as_str(), "mode": mode, "force": force}),
	) {
		Ok(answer) => answer,
		Err(retype_ipc::ClientError::Rpc(error)) => {
			return Ok(ExitCode::from(output::fail(&error.code, &error.message)));
		}
		Err(other) => return Err(other.into()),
	};

	match json {
		true => output::success(answer),
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
	match one(client, prefix)? {
		Ok(id) => Ok(Ok(id)),
		Err(code) => Ok(Err(code)),
	}
}

/// A handle a person may type, `docs/dev/architecture/state.md` §2. Checked before
/// the daemon is asked, so a typo is a usage error whether or not one is running.
fn handle(prefix: &str) -> Option<ExitCode> {
	match JobId::valid_prefix(prefix) {
		true => None,
		false => Some(ExitCode::from(output::fail(
			&Code::UsageArgs,
			format!(
				"'{prefix}' is not a job handle; use at least {} characters",
				JobId::PREFIX_MIN
			),
		))),
	}
}

/// The one job a prefix names, or the exit code to answer with.
fn one(client: &mut retype_ipc::Client, prefix: &str) -> Result<std::result::Result<JobId, ExitCode>> {
	let candidates = matching(client, prefix)?;

	match candidates.len() {
		1 => Ok(Ok(candidates[0].clone())),
		0 => Ok(Err(ExitCode::from(output::fail(
			&Code::JobNotFound,
			format!("no job matches '{prefix}'"),
		)))),
		_ => {
			let mut message = format!("'{prefix}' matches {} jobs", candidates.len());
			for candidate in &candidates {
				message.push_str(&format!("\n  {candidate}"));
			}

			Ok(Err(ExitCode::from(output::fail(&Code::JobAmbiguous, message))))
		}
	}
}

/// Every job whose id starts with `prefix`.
fn matching(client: &mut retype_ipc::Client, prefix: &str) -> Result<Vec<JobId>> {
	let answer = client.call_without_params("jobs.list")?;
	let prefix = normalize(prefix);

	let mut found: Vec<JobId> = answer
		.get("jobs")
		.and_then(Value::as_array)
		.map(|jobs| {
			jobs.iter()
				.filter_map(|job| job.get("id").and_then(Value::as_str))
				.filter_map(|id| JobId::parse(id).ok())
				.filter(|id| id.starts_with(&prefix))
				.collect()
		})
		.unwrap_or_default();

	found.sort();
	found.dedup();

	Ok(found)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn job(state: &str, corrected: Option<&str>) -> Value {
		let mut parked = serde_json::json!({"reason": "intent"});
		if let Some(corrected) = corrected {
			parked["corrected"] = Value::String(corrected.to_string());
		}

		serde_json::json!({"state": state, "parked": parked})
	}

	#[test]
	fn a_key_offers_only_what_the_job_can_do() {
		let pending = job("pending", None);
		let attention = job("needs-attention", Some("webp"));

		assert_eq!(choose('n', &pending), Some(Action::Now));
		assert_eq!(
			choose('n', &attention),
			None,
			"a job that cannot run as asked has no now"
		);

		assert_eq!(choose('c', &pending), None, "nothing was reduced");
		assert_eq!(choose('c', &attention), Some(Action::Convert));

		assert_eq!(choose('r', &pending), Some(Action::Refuse));
		assert_eq!(choose('s', &attention), Some(Action::Skip));
		assert_eq!(choose('q', &pending), Some(Action::Quit));
		assert_eq!(choose('x', &pending), None);
	}

	#[test]
	fn a_listing_prints_the_shortest_handle_that_is_unique() {
		let ids = ["01m6a4rararx3wrttkrgpp1783", "01m6b4rararx3wrttkrgpp1783"];

		assert_eq!(shortest_prefix(ids[0], &ids), "01m6a");
		assert_eq!(shortest_prefix(ids[0], &ids[..1]), "01m6");
	}

	#[test]
	fn a_key_is_read_case_insensitively() {
		assert_eq!(choose('Q', &job("pending", None)), Some(Action::Quit));
	}

	#[test]
	fn an_estimate_reads_as_a_person_would_say_it() {
		assert_eq!(duration(400), "400ms");
		assert_eq!(duration(2_400), "2.4s");
		assert_eq!(duration(62_000), "1m02s");
		assert_eq!(print::size(900), "900 bytes");
		assert_eq!(print::size(1_200_000), "1.2mb");
	}
}
