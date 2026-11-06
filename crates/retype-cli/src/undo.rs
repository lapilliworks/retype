//! `retype undo`, `docs/cli.md` §5 and `docs/dev/architecture/state.md` §5.
//!
//! Undo works from the record rather than from the journal, so it answers even
//! when the journal is gone. It puts the source back under the name its bytes
//! belong to, removes every output the job produced, and deletes the record: an
//! interrupted conversion is undone by the same command as a finished one.

use std::path::Path;
use std::process::ExitCode;

use retype_core::error::Code;
use retype_core::job::JobId;
use retype_core::undo::{Record, Store};

use crate::error::Result;
use crate::output;

pub fn run(state_dir: &Path, id: Option<&str>, last: bool, json: bool) -> Result<ExitCode> {
	if last && id.is_some() {
		return Ok(ExitCode::from(output::fail(
			&Code::UsageConflict,
			"undo takes an id or --last, not both",
		)));
	}

	// Validate before asking the daemon, so a usage mistake has the same code and
	// message whether or not a daemon is reachable.
	if !last && id.is_none() {
		return Ok(ExitCode::from(output::fail(
			&Code::UsageArgs,
			"undo needs an id or --last",
		)));
	}

	// A reachable daemon owns the undo store. A busy pipe is not an absent daemon.
	let client = match crate::client::connect(state_dir) {
		Ok(client) => Some(client),
		Err(crate::error::Error::Client(error)) if error.is_connection_busy() => {
			return Err(crate::error::Error::Client(error));
		}
		Err(_) => None,
	};
	if let Some(mut client) = client {
		let request = retype_ipc::UndoRequest {
			id: id.map(str::to_string),
			last,
		};

		return match client.undo(&request) {
			Ok(answer) => {
				if json {
					output::success(serde_json::to_value(answer)?);
				} else if answer.consume_source {
					println!("{}  restored", answer.source.display());
				} else {
					println!("{}  outputs removed, source kept", answer.source.display());
				}

				Ok(ExitCode::SUCCESS)
			}
			Err(retype_ipc::ClientError::Rpc(error)) => Ok(ExitCode::from(output::fail(&error.code, &error.message))),
			Err(other) => Err(crate::error::Error::Client(other)),
		};
	}

	// Hold the daemon's state lock through offline undo, even if its endpoint is stale.
	crate::state::ensure_private_directory(state_dir)?;
	let _lock = crate::daemon::Lock::acquire(&state_dir.join("lock"), state_dir)?;
	let store = Store::open(state_dir);
	let records = store.list()?;

	let record = match last {
		true => match records.first() {
			Some(record) => record.clone(),
			None => {
				return Ok(ExitCode::from(output::fail(
					&Code::JobState,
					"there is nothing to undo",
				)));
			}
		},
		false => match resolve(&records, id.expect("validated above")) {
			Ok(record) => record,
			Err(failure) => return Ok(failure),
		},
	};

	let restored = match store.restore(&record.id) {
		Ok(restored) => restored,
		Err(error) => {
			return Ok(ExitCode::from(output::fail(&error.code(), error.to_string())));
		}
	};

	if json {
		output::success(serde_json::json!({
			"id": restored.id,
			"source": restored.source_path.display().to_string(),
			"consume_source": restored.consume_source,
			"removed": restored.outputs.iter().map(|output| output.path.display().to_string()).collect::<Vec<_>>(),
		}));
	} else if restored.consume_source {
		println!("{}  restored", restored.source_path.display());
	} else {
		println!("{}  outputs removed, source kept", restored.source_path.display());
	}

	Ok(ExitCode::SUCCESS)
}

/// A full id, or a prefix unique among the records.
fn resolve(records: &[Record], written: &str) -> std::result::Result<Record, ExitCode> {
	if !JobId::valid_prefix(written) {
		return Err(ExitCode::from(output::fail(
			&Code::UsageArgs,
			format!(
				"'{written}' is not a job handle; use at least {} characters",
				JobId::PREFIX_MIN
			),
		)));
	}

	let matches: Vec<&Record> = records.iter().filter(|record| record.id.starts_with(written)).collect();

	match matches.len() {
		1 => Ok(matches[0].clone()),
		0 => Err(ExitCode::from(output::fail(
			&Code::JobNotFound,
			format!("no undo record matches '{written}'"),
		))),
		_ => {
			let mut message = format!("'{written}' matches {} records", matches.len());
			for record in matches {
				message.push_str(&format!("\n  {}", record.id.as_str()));
			}

			Err(ExitCode::from(output::fail(&Code::JobAmbiguous, message)))
		}
	}
}

/// The ids the store holds, for callers that want them.
pub fn ids(state_dir: &Path) -> Result<Vec<JobId>> {
	Ok(Store::open(state_dir)
		.list()?
		.into_iter()
		.map(|record| record.id)
		.collect())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn offline_undo_refuses_while_the_state_lock_is_held() {
		let dir = tempfile::tempdir().unwrap();
		let state = dir.path().join("state");
		let source = dir.path().join("original.tar");
		std::fs::write(&source, b"original bytes").unwrap();
		let store = Store::open(&state);
		let id = JobId::now().unwrap();
		store
			.save(retype_core::undo::Subject {
				id: id.clone(),
				intent: "zip".to_string(),
				machine: "test".to_string(),
				bytes: source.clone(),
				source_path: source.clone(),
				source_ext: "tar".to_string(),
				outputs: Vec::new(),
				consume_source: true,
			})
			.unwrap();
		std::fs::remove_file(&source).unwrap();
		let lock = crate::daemon::Lock::acquire(&state.join("lock"), &state).unwrap();

		let result = run(&state, Some(id.as_str()), false, false);
		assert!(matches!(result, Err(crate::error::Error::AlreadyRunning(_))));
		assert!(!source.exists());
		assert_eq!(
			std::fs::read(store.source(&id).unwrap().unwrap()).unwrap(),
			b"original bytes"
		);
		assert!(store.load(&id).unwrap().is_some());
		drop(lock);
	}

	#[test]
	fn offline_undo_without_a_record_keeps_its_state_error() {
		let dir = tempfile::tempdir().unwrap();
		let result = run(&dir.path().join("state"), None, true, false).unwrap();
		assert_eq!(result, ExitCode::from(7));
	}
}
