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

pub fn run(state_dir: &Path, id: Option<&str>, last: bool, json: bool) -> Result<ExitCode> {
	let store = Store::open(state_dir);
	let records = store.list()?;

	let record = match (last, id) {
		(true, _) => match records.first() {
			Some(record) => record.clone(),
			None => {
				eprintln!("retype: {}: there is nothing to undo", Code::JobState);
				return Ok(ExitCode::from(7));
			}
		},
		(false, Some(written)) => match resolve(&records, written) {
			Ok(record) => record,
			Err(failure) => return Ok(failure),
		},
		(false, None) => {
			eprintln!("retype: {}: undo needs an id or --last", Code::UsageArgs);
			return Ok(ExitCode::from(2));
		}
	};

	let restored = match store.restore(&record.id) {
		Ok(restored) => restored,
		Err(error) => {
			eprintln!("retype: {error}");
			return Ok(ExitCode::from(error.exit_code()));
		}
	};

	if json {
		println!(
			"{}",
			serde_json::json!({
				"v": "retype-cli-json-1",
				"id": restored.id,
				"source": restored.source_path.display().to_string(),
				"removed": restored.outputs.iter().map(|output| output.path.display().to_string()).collect::<Vec<_>>(),
			})
		);
	} else {
		println!("{}  restored", restored.source_path.display());
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
		0 => {
			eprintln!("retype: {}: no undo record matches '{written}'", Code::JobState);
			Err(ExitCode::from(7))
		}
		_ => {
			eprintln!(
				"retype: {}: '{written}' matches {} records",
				Code::IpcAmbiguous,
				matches.len()
			);

			for record in matches {
				eprintln!("  {}", record.id.as_str());
			}

			Err(ExitCode::from(7))
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
