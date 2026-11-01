//! What the command line writes, and the one place the JSON envelope is built.
//!
//! `docs/cli.md` §7 and `docs/errors.md` §7 make `--json` a contract rather than a
//! rendering: a consumer gets `{v, ok, error}` on failure and a versioned object on
//! success. Keeping the shape here means a command cannot invent its own.

use std::sync::atomic::{AtomicBool, Ordering};

use retype_core::error::{Code, Diagnoses, Error as CoreError};
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

/// A human-readable category for a code, derived from the exit status it maps to.
pub fn category_name(code: &Code) -> &'static str {
	match exit_code(code) {
		2 => "usage",
		3 => "diagnosis",
		4 => "environment",
		5 => "transient",
		6 => "daemon",
		7 => "state",
		_ => "internal",
	}
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

/// Report one failure and answer with the exit status it maps to.
pub fn fail(code: &Code, message: impl AsRef<str>) -> u8 {
	let message = clean_message(code, message.as_ref());

	if json() {
		println!(
			"{}",
			envelope_error(
				json!({
					"code": code,
					"category": category_name(code),
					"message": message,
				}),
				None
			)
		);
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
