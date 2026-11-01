//! The `retype://` handler a desktop entry or Start-menu shortcut calls.
//!
//! A click on a notification or a link names one job and one action. The handler
//! starts nothing: an undo can run in-process from the store, and every action that
//! needs the daemon reports unavailable rather than replacing a job that the user
//! may not know is gone. The GUI owns the editor action; the CLI owns undo, cancel
//! and resolve.

use std::path::Path;
use std::process::ExitCode;

use retype_core::error::Code;
use serde_json::json;

use crate::error::Result;
use crate::output;

pub fn run(state_dir: &Path, uri: &str, json: bool) -> Result<ExitCode> {
	let Some(rest) = uri.strip_prefix("retype://") else {
		return Ok(ExitCode::from(output::fail(
			&Code::UsageArgs,
			format!("'{uri}' is not a retype:// URI"),
		)));
	};

	let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
	let mut parts = path.split('/').filter(|part| !part.is_empty());

	if parts.next() != Some("job") {
		return Ok(ExitCode::from(output::fail(
			&Code::UsageArgs,
			format!("'{uri}' names no job"),
		)));
	}

	let Some(id) = parts.next() else {
		return Ok(ExitCode::from(output::fail(
			&Code::UsageArgs,
			format!("'{uri}' names no job id"),
		)));
	};

	let action = query
		.split('&')
		.find_map(|pair| pair.strip_prefix("action="))
		.unwrap_or("undo");

	match action {
		"undo" => crate::undo::run(state_dir, Some(id), false, json),
		"cancel" | "resolve" => {
			let mut client = match crate::client::connect(state_dir) {
				Ok(client) => client,
				Err(_) => {
					return Ok(ExitCode::from(output::fail(
						&Code::IpcUnavailable,
						"the daemon is not running; start it and try again",
					)));
				}
			};

			let method = match action {
				"cancel" => "jobs.cancel",
				_ => "jobs.resolve",
			};
			let params = match action {
				"cancel" => json!({"id": id}),
				_ => json!({"id": id, "mode": "now"}),
			};

			let answer = client.call(method, params)?;

			if json {
				output::success(answer);
			} else if let Some(state) = answer.get("state").and_then(|value| value.as_str()) {
				println!("{id}  {state}");
			}

			Ok(ExitCode::SUCCESS)
		}
		other => Ok(ExitCode::from(output::fail(
			&Code::UsageArgs,
			format!("'{other}' is not an action this build handles; use undo, cancel or resolve"),
		))),
	}
}
