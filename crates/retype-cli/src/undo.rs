//! `retype undo`, `docs/cli.md` §5 and `docs/state.md` §5.
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

	// A reachable daemon owns the undo store. Asking it is what keeps an undo from
	// racing a conversion that still has the record open, `docs/ipc.md` §1.
	if let Ok(mut client) = crate::client::connect(state_dir) {
		let answer = client.call("undo", serde_json::json!({"id": id, "last": last}));

		return match answer {
			Ok(answer) => {
				if json {
					output::success(answer);
				} else {
					let source = answer
						.get("source")
						.and_then(|value| value.as_str())
						.unwrap_or_default();
					match answer
						.get("consume_source")
						.and_then(|value| value.as_bool())
						.unwrap_or(false)
					{
						true => println!("{source}  restored"),
						false => println!("{source}  outputs removed, source kept"),
					}
				}

				Ok(ExitCode::SUCCESS)
			}
			Err(retype_ipc::ClientError::Rpc(error)) => Ok(ExitCode::from(output::fail(&error.code, &error.message))),
			Err(other) => Err(crate::error::Error::Client(other)),
		};
	}

	let store = Store::open(state_dir);
	let records = store.list()?;

	let record = match (last, id) {
		(true, _) => match records.first() {
			Some(record) => record.clone(),
			None => {
				return Ok(ExitCode::from(output::fail(
					&Code::JobState,
					"there is nothing to undo",
				)));
			}
		},
		(false, Some(written)) => match resolve(&records, written) {
			Ok(record) => record,
			Err(failure) => return Ok(failure),
		},
		(false, None) => {
			return Ok(ExitCode::from(output::fail(
				&Code::UsageArgs,
				"undo needs an id or --last",
			)));
		}
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
	let wanted = written.to_ascii_lowercase();
	let matches: Vec<&Record> = records
		.iter()
		.filter(|record| record.id.as_str().to_ascii_lowercase().starts_with(&wanted))
		.collect();

	match matches.len() {
		1 => Ok(matches[0].clone()),
		0 => Err(ExitCode::from(output::fail(
			&Code::JobState,
			format!("no undo record matches '{written}'"),
		))),
		_ => {
			let mut message = format!("'{written}' matches {} records", matches.len());
			for record in matches {
				message.push_str(&format!("\n  {}", record.id.as_str()));
			}

			Err(ExitCode::from(output::fail(&Code::IpcAmbiguous, message)))
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
