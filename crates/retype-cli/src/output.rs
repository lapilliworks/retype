//! What the command line writes, and the one place the JSON envelope is built.
//!
//! `docs/cli.md` §7 and `docs/errors.md` §7 make `--json` a contract rather than a
//! rendering: a consumer gets `{v, ok, error}` on failure and a versioned object on
//! success. Keeping the shape here means a command cannot invent its own.

use std::sync::atomic::{AtomicBool, Ordering};

use retype_core::error::{Code, Diagnoses, Error as CoreError};
use retype_core::job::{Job, JobState, OutputState};
use serde_json::{Value, json};

use crate::error::{Error, exit_code};

/// Identifier every envelope carries, `docs/cli.md` §3.
pub const VERSION: &str = "retype-cli-json-1";

static JSON: AtomicBool = AtomicBool::new(false);

pub fn start(json: bool) {
	JSON.store(json, Ordering::Relaxed);
}

pub fn json() -> bool {
	JSON.load(Ordering::Relaxed)
}

/// Add the envelope fields to a result object.
pub fn envelope(value: Value) -> Value {
	match value {
		Value::Object(mut object) => {
			object.insert("v".to_string(), json!(VERSION));
			object.insert("ok".to_string(), json!(true));
			Value::Object(object)
		}
		other => json!({"v": VERSION, "ok": true, "value": other}),
	}
}

/// Write a result, adding the envelope only when `--json` asked for one.
pub fn success(value: Value) {
	println!("{}", envelope(value));
}

/// Write a record exactly as the producer wrote it, without the CLI envelope.
///
/// `jobs get` answers with the journal record, which carries its own `v`; an
/// envelope would overwrite it and lose the schema the record came from.
pub fn record(value: Value) {
	println!("{value}");
}

/// A human-readable category for a code.
pub fn category_name(code: &Code) -> &'static str {
	code.category().as_str()
}

/// The message without the code a diagnostic already carries, so the code is not
/// printed twice: `E_FORMAT_UNKNOWN: E_FORMAT_UNKNOWN at ...`.
pub fn clean_message(code: &Code, message: &str) -> String {
	let prefix = format!("{code}: ");
	message.strip_prefix(&prefix).unwrap_or(message).to_string()
}

fn diagnostic(error: &CoreError) -> Value {
	json!({
		"code": error.code(),
		"category": category_name(&error.code()),
		"message": clean_message(&error.code(), &error.to_string()),
		"span": error.span().map(|span| json!({"start": span.start, "end": span.end})),
		"hint": error.hint(),
	})
}

fn envelope_error(error: Value, diagnostics: Option<Value>) -> Value {
	let mut object = json!({
		"v": VERSION,
		"ok": false,
		"error": error,
	});

	if let Some(diagnostics) = diagnostics {
		object["diagnostics"] = diagnostics;
	}

	object
}

/// The reason a settled job did not fulfill its request, if any.
pub fn job_failure(job: &Job) -> Option<(Code, String)> {
	let reason = job.error.as_ref().or_else(|| {
		job.outputs
			.iter()
			.filter(|output| output.state == OutputState::Failed)
			.find_map(|output| output.error.as_ref())
	});
	if let Some(error) = reason {
		return Some((Code::from(error.code.clone()), error.message.clone()));
	}
	if job.state == JobState::Done && job.outputs.iter().all(|output| output.state == OutputState::Done) {
		return None;
	}
	let code = match job.state {
		JobState::Cancelled => Code::JobCancelled,
		JobState::Superseded => Code::JobSuperseded,
		_ => Code::JobState,
	};
	Some((code, format!("job {} did not finish ({:?})", job.id, job.state)))
}

/// Report one failure and answer with the exit status it maps to.
pub fn fail(code: &Code, message: impl AsRef<str>) -> u8 {
	fail_with_optional_detail(code, message.as_ref(), None)
}

/// Report a failure with machine-readable context.
pub fn fail_with_detail(code: &Code, message: impl AsRef<str>, detail: Value) -> u8 {
	fail_with_optional_detail(code, message.as_ref(), Some(detail))
}

fn fail_with_optional_detail(code: &Code, message: &str, detail: Option<Value>) -> u8 {
	let message = clean_message(code, message);

	if json() {
		let mut error = json!({
			"code": code,
			"category": category_name(code),
			"message": message,
		});
		if let Some(detail) = detail {
			error["detail"] = detail;
		}
		println!("{}", envelope_error(error, None));
	} else {
		eprintln!("retype: {code}: {message}");
	}

	exit_code(code)
}

/// Report a parse, configuration or plan failure with every diagnostic it carries.
pub fn fail_diagnoses(problems: &Diagnoses) -> u8 {
	let first = problems.iter().next();
	let code = first.map(CoreError::code).unwrap_or(Code::InternalUnexpected);
	let message = first
		.map(|error| clean_message(&error.code(), &error.to_string()))
		.unwrap_or_else(|| "the request could not be planned".to_string());

	if json() {
		let diagnostics: Vec<Value> = problems.iter().map(diagnostic).collect();
		println!(
			"{}",
			envelope_error(
				json!({
					"code": code,
					"category": category_name(&code),
					"message": message,
				}),
				Some(json!(diagnostics))
			)
		);
	} else {
		eprintln!("retype: {problems}");
	}

	exit_code(&code)
}

/// Report a command-line error.
pub fn fail_error(error: &Error) -> u8 {
	let code = error.code();
	let message = clean_message(&code, &error.to_string());

	if json() {
		println!(
			"{}",
			envelope_error(
				json!({
					"code": code,
					"category": category_name(&code),
					"message": message,
				}),
				None
			)
		);
	} else {
		eprintln!("retype: {error}");
	}

	error.exit_code()
}

/// A usage error with the code the taxonomy names.
pub fn usage(code: Code, message: impl AsRef<str>) -> u8 {
	fail(&code, message)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn job(state: JobState) -> Job {
		let mut job: Job = serde_json::from_value(json!({
			"id": retype_core::job::JobId::now().unwrap(),
			"created": 0, "updated": 0,
			"source": {"path": "photo.png", "format": "png", "size": 1, "mtime_ns": 0},
			"intent": "webp", "outputs": [], "state": state,
			"machine": "test"
		}))
		.unwrap();
		job.outputs.push(retype_core::job::Output::pending(
			"photo.webp".into(),
			"webp",
			retype_core::job::Tier::Converted,
		));
		job
	}

	#[test]
	fn incomplete_jobs_report_the_specific_failure_or_a_state_error() {
		let mut job = job(JobState::Partial);
		job.outputs[0].state = OutputState::Failed;
		job.outputs[0].error = Some(retype_core::job::JobError::new("E_IO_SPACE", "disk full"));
		assert_eq!(job_failure(&job), Some((Code::IoSpace, "disk full".to_string())));
		job.outputs.clear();
		assert_eq!(job_failure(&job).unwrap().0, Code::JobState);
		job.state = JobState::Done;
		assert_eq!(job_failure(&job), None);
	}
}
