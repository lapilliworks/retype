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
use retype_core::identifier::normalize;
use serde_json::json;

use crate::error::Result;
use crate::output;

/// What a URI asks for, with every name in its canonical spelling.
struct Parsed {
	id: String,
	action: String,
}

/// Read `retype://job/<id>[?action=...]`.
///
/// The scheme, the object name and the action are identifiers a person may have
/// typed, so they are read in either case and normalized. The job id itself is
/// normalized by the same rule as a handle on the command line.
fn parse(uri: &str) -> std::result::Result<Parsed, String> {
	let Some((scheme, rest)) = uri.split_once("://") else {
		return Err(format!("'{uri}' is not a retype:// URI"));
	};

	if !scheme.eq_ignore_ascii_case("retype") {
		return Err(format!("'{uri}' is not a retype:// URI"));
	}

	let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
	let mut parts = path.split('/').filter(|part| !part.is_empty());

	match parts.next() {
		Some(kind) if kind.eq_ignore_ascii_case("job") => {}
		_ => return Err(format!("'{uri}' names no job")),
	}

	let Some(id) = parts.next() else {
		return Err(format!("'{uri}' names no job id"));
	};

	let action = query
		.split('&')
		.find_map(|pair| {
			let (name, value) = pair.split_once('=')?;
			name.eq_ignore_ascii_case("action").then_some(value)
		})
		.unwrap_or("undo");

	Ok(Parsed {
		id: normalize(id),
		action: normalize(action),
	})
}

pub fn run(state_dir: &Path, uri: &str, json: bool) -> Result<ExitCode> {
	let Parsed { id, action } = match parse(uri) {
		Ok(parsed) => parsed,
		Err(message) => return Ok(ExitCode::from(output::fail(&Code::UsageArgs, message))),
	};

	match action.as_str() {
		"undo" => crate::undo::run(state_dir, Some(&id), false, json),
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

			let method = match action.as_str() {
				"cancel" => "jobs.cancel",
				_ => "jobs.resolve",
			};
			let params = match action.as_str() {
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

#[cfg(test)]
mod tests {
	use super::parse;

	#[test]
	fn a_uri_is_read_in_either_case() {
		let parsed = parse("RETYPE://JOB/01M6B4RARARX3WrTTKRGPP1783?ACTION=UNDO").expect("a URI");

		assert_eq!(parsed.id, "01m6b4rararx3wrttkrgpp1783");
		assert_eq!(parsed.action, "undo");
	}

	#[test]
	fn the_action_defaults_to_undo() {
		let parsed = parse("retype://job/01m6b4rararx3wrttkrgpp1783").expect("a URI");

		assert_eq!(parsed.action, "undo");
	}

	#[test]
	fn a_uri_without_a_job_is_refused() {
		assert!(parse("retype://photo/01m6b4rararx3wrttkrgpp1783").is_err());
		assert!(parse("retype://job").is_err());
		assert!(parse("https://job/01m6b4rararx3wrttkrgpp1783").is_err());
	}
}
