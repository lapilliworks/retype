//! One error type, one set of codes, one category per code.
//!
//! `docs/reference/errors.md` defines the shape: a code, a category, a message, and
//! optionally detail, a hint and a span. Everything that fails in this crate
//! passes through here, and the numbers a script branches on are derived from
//! the category rather than decided per call site.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use retype_macros::CodeName;

/// Every code the product can emit, `docs/reference/errors.md` §3.
///
/// The variant name is the only place a code is written: `IoPermission` is
/// `E_IO_PERMISSION` because the derive says so, and there is no second copy to
/// keep in step. Renaming a variant therefore changes a code, which is a
/// breaking change on the wire, so the comparison with `docs/reference/errors.md` in the
/// tests is what makes that dangerous edit loud rather than silent.
///
/// A code is text on the wire and a client must survive one it has never heard
/// of, so the enum carries a passthrough variant that prints and reads as its
/// inner string and takes no part in `KNOWN`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, CodeName)]
#[serde(from = "String", into = "String")]
pub enum Code {
	// Grammar, fixed by `retype-grammar-1`.
	Anchor,
	Charset,
	IntentEmpty,
	TooLong,
	TooMany,
	GroupEmpty,
	TokenEmpty,
	FormatUnknown,
	FormatFolder,
	NameUnknown,
	ValueMissing,
	ValueUnexpected,
	ValueSyntax,
	ValueRange,
	Duplicate,
	Exclusive,
	MetaConflict,
	Noop,
	Inapplicable,
	Unsupported,

	// Invocation.
	UsageArgs,
	UsageConflict,
	UsageOutputCount,

	// Configuration, `docs/config.md`.
	CfgParse,
	CfgVersion,
	CfgValidation,
	CfgMissing,
	CfgUnknownKey,
	CfgNameClash,
	CfgShadowed,

	// Watches, `docs/watch.md`.
	WatchPausedByPolicy,

	// External tools.
	ToolMissing,
	ToolVersion,
	ToolFailed,
	ToolTimeout,

	// Filesystem and state.
	IoFailed,
	IoPermission,
	IoSpace,
	IoNotFound,
	IoExists,
	IoLocked,
	IoCorruptJournal,

	// Jobs.
	JobState,
	JobCancelled,
	JobInterrupted,
	JobPartial,
	JobSuperseded,
	JobResumeUnavailable,
	JobNotFound,
	JobAmbiguous,

	// Transport, `docs/dev/architecture/ipc.md` §8.
	IpcVersion,
	IpcHandshake,
	IpcFrame,
	IpcMethod,
	IpcUnavailable,
	IpcAmbiguous,
	IpcResync,
	IpcSupervised,
	IpcAuth,
	IpcInternal,

	// Bugs.
	InternalInvariant,
	InternalPanic,
	InternalUnexpected,

	/// A code from another version, printed and parsed as its inner string.
	#[code(passthrough)]
	Unknown(String),
}

impl From<String> for Code {
	fn from(raw: String) -> Self {
		match Self::parse(&raw) {
			Some(known) => known,
			None => Self::Unknown(raw),
		}
	}
}

impl From<Code> for String {
	fn from(code: Code) -> Self {
		code.to_string()
	}
}

/// How a failure should be treated, `docs/reference/errors.md` §4. A script branches on
/// this rather than on a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
	/// The request cannot be satisfied as written. Never retried.
	Diagnosis,
	/// This machine cannot satisfy it. Needs a human.
	Environment,
	/// Try again.
	Transient,
	/// A bug, always with a report.
	Internal,
}

impl Category {
	/// The exit code for the category, `docs/cli.md` §6. A caller may narrow
	/// this further, for instance to separate a filesystem failure from an
	/// unreachable daemon, but it may not widen it.
	pub fn exit_code(self) -> u8 {
		match self {
			Self::Diagnosis => 3,
			Self::Environment => 4,
			Self::Transient => 5,
			Self::Internal => 1,
		}
	}

	pub fn is_retryable(self) -> bool {
		matches!(self, Self::Transient)
	}
}

/// A byte range inside the string that produced a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
	pub start: usize,
	pub end: usize,
}

impl Span {
	pub fn new(start: usize, end: usize) -> Self {
		Self { start, end }
	}
}

/// A grammar diagnostic, `docs/grammar.md` §6. Carried inside [`Error`] so that
/// there is one error type rather than one per layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
	pub code: Code,
	pub message: String,
	/// Present for a grammar diagnostic, absent for anything that is not one
	/// string's fault, such as a configuration file that fails to load.
	pub span: Option<Span>,
	pub hint: Option<String>,
}

impl Diagnostic {
	pub fn new(code: Code, message: impl Into<String>, span: Span) -> Self {
		Self {
			code,
			message: message.into(),
			span: Some(span),
			hint: None,
		}
	}

	pub fn global(code: Code, message: impl Into<String>) -> Self {
		Self {
			code,
			message: message.into(),
			span: None,
			hint: None,
		}
	}

	pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
		self.hint = Some(hint.into());
		self
	}
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error("{0}")]
	Io(#[from] std::io::Error),

	#[error("{} already exists", .0.display())]
	Exists(PathBuf),

	#[error("{} has no parent directory", .0.display())]
	NoParent(PathBuf),

	#[error("{} has no file name", .0.display())]
	NoFileName(PathBuf),

	#[error("{} is not a job id", .0)]
	JobId(String),

	#[error("cannot read randomness: {0}")]
	Random(String),

	#[error("cannot encode a journal record: {0}")]
	Json(String),

	#[error("{}", .0.message)]
	Diagnostic(Diagnostic),
}

impl From<Diagnostic> for Error {
	fn from(diagnostic: Diagnostic) -> Self {
		Self::Diagnostic(diagnostic)
	}
}

pub type Result<T> = std::result::Result<T, Error>;

/// Several diagnostics about one input, ordered by offset and capped.
///
/// Grammar parsing and configuration loading both report everything wrong at
/// once rather than the first thing, because a user who has to fix one mistake
/// per attempt learns the rules one at a time.
#[derive(Debug)]
pub struct Diagnoses(Vec<Error>);

impl Diagnoses {
	/// How many are kept before the rest are dropped, `docs/grammar.md` §6.
	pub const LIMIT: usize = 8;

	pub fn new(mut errors: Vec<Error>) -> Self {
		errors.sort_by_key(|error| error.span().map(|span| span.start).unwrap_or_default());
		errors.truncate(Self::LIMIT);
		Self(errors)
	}

	pub fn one(error: Error) -> Self {
		Self(vec![error])
	}

	pub fn iter(&self) -> impl Iterator<Item = &Error> {
		self.0.iter()
	}

	pub fn first(&self) -> Option<&Error> {
		self.0.first()
	}

	pub fn len(&self) -> usize {
		self.0.len()
	}

	pub fn is_empty(&self) -> bool {
		self.0.is_empty()
	}

	pub fn codes(&self) -> Vec<Code> {
		self.0.iter().map(Error::code).collect()
	}
}

impl std::fmt::Display for Diagnoses {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		for (index, error) in self.0.iter().enumerate() {
			if index > 0 {
				formatter.write_str("; ")?;
			}
			match error.span() {
				Some(span) => write!(formatter, "{} at {}..{}: {error}", error.code(), span.start, span.end)?,
				None => write!(formatter, "{}: {error}", error.code())?,
			}
		}
		Ok(())
	}
}

impl std::error::Error for Diagnoses {}

/// The code for an operating system failure, so that a layer holding a bare
/// `io::Error` does not invent its own mapping.
pub fn code_for_io(error: &std::io::Error) -> Code {
	match error.kind() {
		std::io::ErrorKind::PermissionDenied => Code::IoPermission,
		std::io::ErrorKind::NotFound => Code::IoNotFound,
		std::io::ErrorKind::AlreadyExists => Code::IoExists,
		std::io::ErrorKind::StorageFull => Code::IoSpace,
		_ => Code::IoFailed,
	}
}

pub fn category_for_io(_error: &std::io::Error) -> Category {
	Category::Transient
}

impl Error {
	pub fn diagnostic(code: Code, message: impl Into<String>, span: Span) -> Self {
		Self::Diagnostic(Diagnostic::new(code, message, span))
	}

	pub fn code(&self) -> Code {
		match self {
			Self::Io(error) => code_for_io(error),
			Self::Exists(_) => Code::IoExists,
			Self::NoParent(_) | Self::NoFileName(_) => Code::UsageArgs,
			Self::JobId(_) => Code::UsageArgs,
			Self::Random(_) | Self::Json(_) => Code::InternalUnexpected,
			Self::Diagnostic(diagnostic) => diagnostic.code.clone(),
		}
	}

	pub fn category(&self) -> Category {
		match self {
			Self::Io(_) | Self::Exists(_) => Category::Transient,
			Self::NoParent(_) | Self::NoFileName(_) | Self::JobId(_) => Category::Diagnosis,
			Self::Random(_) | Self::Json(_) => Category::Internal,
			Self::Diagnostic(_) => Category::Diagnosis,
		}
	}

	pub fn exit_code(&self) -> u8 {
		self.category().exit_code()
	}

	pub fn span(&self) -> Option<Span> {
		match self {
			Self::Diagnostic(diagnostic) => diagnostic.span,
			_ => None,
		}
	}

	pub fn hint(&self) -> Option<&str> {
		match self {
			Self::Diagnostic(diagnostic) => diagnostic.hint.as_deref(),
			_ => None,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn every_variant_has_a_code_and_a_category() {
		let errors = [
			Error::Exists(PathBuf::from("/a")),
			Error::NoParent(PathBuf::from("/a")),
			Error::NoFileName(PathBuf::from("/a")),
			Error::JobId("01".to_string()),
			Error::Random("nope".to_string()),
			Error::Json("nope".to_string()),
			Error::diagnostic(Code::Anchor, "no", Span::new(0, 1)),
		];

		for error in errors {
			assert!(error.code().as_str().starts_with("E_"), "{error}");
			assert!(error.exit_code() > 0, "{error}");
		}
	}

	#[test]
	fn io_kinds_decide_the_code_rather_than_the_caller() {
		let denied = Error::Io(std::io::Error::from(std::io::ErrorKind::PermissionDenied));

		assert_eq!(denied.code(), Code::IoPermission);
		assert_eq!(denied.category(), Category::Transient);
	}

	#[test]
	fn a_diagnostic_is_never_retried_and_carries_its_span() {
		let error = Error::from(
			Diagnostic::new(Code::Charset, "a space is not allowed", Span::new(2, 3)).with_hint("remove it"),
		);

		assert_eq!(error.category(), Category::Diagnosis);
		assert!(!error.category().is_retryable());
		assert_eq!(error.exit_code(), 3);
		assert_eq!(error.span(), Some(Span::new(2, 3)));
		assert_eq!(error.hint(), Some("remove it"));
	}

	#[test]
	fn an_internal_failure_exits_one() {
		assert_eq!(Error::Random("nope".to_string()).exit_code(), 1);
	}

	#[test]
	fn the_categories_are_the_documented_four() {
		assert_eq!(Category::Diagnosis.exit_code(), 3);
		assert_eq!(Category::Environment.exit_code(), 4);
		assert_eq!(Category::Transient.exit_code(), 5);
		assert_eq!(Category::Internal.exit_code(), 1);
	}

	#[test]
	fn every_known_code_is_unique_and_well_formed() {
		let mut seen = std::collections::HashSet::new();

		for code in Code::KNOWN {
			let text = code.as_str();
			assert!(seen.insert(text), "duplicate code {text}");
			assert!(text.starts_with("E_"), "{text} does not start with E_");
			assert!(
				text.bytes().all(|byte| byte.is_ascii_uppercase() || byte == b'_'),
				"{text} is not upper case with underscores"
			);
			assert_eq!(code.to_string(), text, "printing and borrowing disagree");
		}
	}

	#[test]
	fn a_known_code_round_trips_through_its_string() {
		for code in Code::KNOWN {
			let text = code.as_str();
			assert_eq!(Code::from(text.to_string()), *code);
			assert_eq!(Code::parse(text), Some(code.clone()));
		}
	}

	#[test]
	fn an_unknown_code_is_preserved_rather_than_mapped() {
		let future = Code::from("E_FROM_THE_FUTURE".to_string());

		assert_eq!(future, Code::Unknown("E_FROM_THE_FUTURE".to_string()));
		assert_eq!(future.as_str(), "E_FROM_THE_FUTURE");
		assert_eq!(
			future.to_string(),
			"E_FROM_THE_FUTURE",
			"printing must not decorate an unknown code"
		);
	}

	#[test]
	fn the_documented_codes_and_the_known_codes_are_the_same_set() {
		const DOCS: &str = include_str!("../../../docs/reference/errors.md");

		// Every backticked token that looks like a code and is not a bare
		// namespace prefix, such as the `E_IO_` heading of a table row.
		let documented: std::collections::BTreeSet<&str> = DOCS
			.split('`')
			.skip(1)
			.step_by(2)
			.filter(|token| token.starts_with("E_"))
			.filter(|token| !token.ends_with('_'))
			.filter(|token| {
				token
					.bytes()
					.all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
			})
			.collect();

		let known: std::collections::BTreeSet<&str> = Code::KNOWN.iter().map(|code| code.as_str()).collect();

		let undocumented: Vec<_> = known.difference(&documented).collect();
		let unbuilt: Vec<_> = documented.difference(&known).collect();

		assert!(undocumented.is_empty(), "codes with no documentation: {undocumented:?}");
		assert!(
			unbuilt.is_empty(),
			"documented codes this build cannot emit: {unbuilt:?}"
		);
	}

	#[test]
	fn codes_serialise_as_their_string() {
		assert_eq!(serde_json::to_string(&Code::IpcVersion).unwrap(), "\"E_IPC_VERSION\"");

		let parsed: Code = serde_json::from_str("\"E_FROM_THE_FUTURE\"").unwrap();
		assert_eq!(parsed, Code::Unknown("E_FROM_THE_FUTURE".to_string()));
	}
}
