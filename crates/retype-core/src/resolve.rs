//! Resolving a scanned intent against the registry, `docs/grammar.md` §6 steps
//! 6 to 8 and §9.
//!
//! Scanning turned a string into groups, tokens and positions. This turns those
//! into operations, and says what is wrong with them: an unknown name, a
//! repeated operation, two operations that cannot coexist, one that does not
//! apply to the format, one whose vertical has not landed.

use crate::error::{Code, Diagnoses, Error, Span};
use crate::intent::{self, Intent, Meta, Raw, Token};
use crate::print;
use crate::registry::Registry;
use crate::value::{self, Value};

/// What the source is, because some rules depend on it.
#[derive(Debug, Clone, Copy)]
pub struct Source<'a> {
	pub format: &'a str,
	pub is_directory: bool,
}

/// An intent whose tokens and formats are all real.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
	pub meta: Meta,
	pub groups: Vec<Group>,
	/// Conditions resolved in the user's favour rather than refused.
	pub warnings: Vec<crate::error::Diagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
	pub format: String,
	/// The source is kept rather than converted, `docs/grammar.md` §11.
	pub keep_source: bool,
	pub ops: Vec<Op>,
}

impl Group {
	/// The pipeline, in the order the user wrote it.
	///
	/// Reordering would be a guess: `quality` and `cap` commute, `resize` and
	/// `rotate` do not, and the registry has no table of which pairs do. A fixed
	/// sequence would therefore be right for some combinations and silently wrong
	/// for others, so the string is the sequence, `docs/grammar.md` C2.
	pub fn pipeline(&self) -> &[Op] {
		&self.ops
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Op {
	pub key: String,
	/// Primary spelling, which is what canonical printing uses.
	pub name: String,
	pub slot: u16,
	pub value: Option<Argument>,
	pub span: Span,
}

/// A value as written and as read: the written form keeps the user's spelling
/// for the derived name, and the parsed form is what later stages act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Argument {
	pub written: Raw,
	pub parsed: Value,
}

pub fn resolve(intent: &Intent, source: Source<'_>, registry: &Registry) -> Result<Resolved, Diagnoses> {
	let mut problems = Vec::new();
	let mut groups = Vec::new();
	let mut meta = intent.meta;
	let mut warnings = intent.warnings.clone();

	for group in &intent.groups {
		// `jpeg` is `jpg`: a format is accepted in every spelling it is declared
		// under, and the rest of the pipeline sees one name.
		let format = if group.format == "folder" {
			if !source.is_directory {
				problems.push(error(
					Code::FormatFolder,
					"'folder' names a directory, and the source is a file",
					group.span,
				));
			}
			group.format.clone()
		} else {
			match registry.canonical_format(&group.format) {
				Some(canonical) => canonical.to_string(),
				None => {
					problems.push(error(
						Code::FormatUnknown,
						format!("'{}' is not a format this build knows", group.format),
						group.span,
					));
					continue;
				}
			}
		};

		// Duplicates and exclusivity are per group: two outputs may each ask for
		// their own quality, and may not each ask for two.
		let mut keys: Vec<String> = Vec::new();
		let mut exclusive: Vec<(String, String)> = Vec::new();
		let mut ops = Vec::new();

		let mut tokens = Vec::new();
		for token in &group.tokens {
			expand(registry, token, &mut tokens, &mut problems);
		}

		for token in &tokens {
			let token = desugar(token);

			let Some(found) = registry.resolve(&token.name) else {
				problems.push(unknown_name(&token, registry));
				continue;
			};

			// A preset reaches this loop only unexpanded when it was malformed,
			// which `expand` already reported.
			let Some(op) = found.op() else {
				continue;
			};

			if !op.is_implemented() {
				problems.push(error(
					Code::Unsupported,
					format!("'{}' is reserved and its vertical has not landed", op.name),
					token.span,
				));
				continue;
			}

			if !op.applies_to(&format) {
				problems.push(error(
					Code::Inapplicable,
					format!("'{}' does not apply to {format}", op.name),
					token.span,
				));
				continue;
			}

			let written = found
				.value
				.map(str::to_string)
				.or_else(|| token.value.as_ref().map(|raw| raw.text.clone()));

			let written = match (op.param.is_some(), written) {
				(true, None) => {
					problems.push(error(
						Code::ValueMissing,
						format!("'{}' needs a value", op.name),
						token.span,
					));
					continue;
				}
				(false, Some(_)) => {
					problems.push(error(
						Code::ValueUnexpected,
						format!("'{}' takes no value", op.name),
						token.span,
					));
					continue;
				}
				(_, written) => written.map(|text| Raw {
					text,
					span: token.value.as_ref().map(|value| value.span).unwrap_or(token.span),
				}),
			};

			// `ask` and `now` are the words for the shorthand the scanner strips
			// from the end of an intent, so they set the flags rather than
			// becoming operations of their own. That is what lets the printer
			// write them in the canonical form and still round trip.
			if op.key == "ask" || op.key == "now" {
				let flag = op.key == "ask";
				if flag {
					meta.ask = true;
				} else {
					meta.now = true;
				}
				continue;
			}

			let value = match (&op.param, written) {
				(Some(param), Some(written)) => match value::interpret(param, &written.text) {
					Ok(parsed) => Some(Argument { written, parsed }),
					Err(fault) => {
						problems.push(error(fault.code, fault.message, written.span));
						continue;
					}
				},
				_ => None,
			};

			if keys.contains(&op.key) {
				problems.push(error(
					Code::Duplicate,
					format!("'{}' is given more than once", op.name),
					token.span,
				));
				continue;
			}
			keys.push(op.key.clone());

			for group_name in &op.exclusive {
				let conflict = exclusive
					.iter()
					.find(|(name, other)| name == group_name && *other != op.key);

				if let Some((_, other)) = conflict {
					let other = registry
						.op(other)
						.map(|op| op.name.clone())
						.unwrap_or_else(|| other.clone());
					problems.push(error(
						Code::Exclusive,
						format!("'{}' and '{other}' cannot be combined", op.name),
						token.span,
					));
					continue;
				}

				exclusive.push((group_name.clone(), op.key.clone()));
			}

			ops.push(Op {
				key: op.key.clone(),
				name: op.name.clone(),
				slot: op.slot,
				value,
				span: token.span,
			});
		}

		groups.push(Group {
			format,
			keep_source: group.format == source.format && ops.is_empty(),
			ops,
		});
	}

	if meta.ask && meta.now {
		meta.now = false;
		warnings.push(crate::error::Diagnostic::global(
			Code::MetaConflict,
			"ignored 'now' because the intent also asks, and asking wins",
		));
	}

	if groups.len() == 1 && groups[0].keep_source {
		problems.push(error(
			Code::Noop,
			format!(
				"'{}' is already the source format, so nothing would happen",
				groups[0].format
			),
			intent.groups[0].span,
		));
	}

	if !problems.is_empty() {
		return Err(Diagnoses::new(problems));
	}

	Ok(Resolved { meta, groups, warnings })
}

/// The intent a rename carries, or none if it carries nothing, `docs/grammar.md`
/// §3.
///
/// The source stem is decided by the registry rather than by the last dot, so
/// `archive.tar.gz` renamed to `archive.zip` carries `zip`: the stem is
/// `archive`, because `tar.gz` is one declared format. A source whose format is
/// not declared has no stem we can trust, and nothing to convert from.
pub fn intent_of(registry: &Registry, source_name: &str, target_name: &str) -> Result<Option<Intent>, Diagnoses> {
	let Some((stem, source_format)) = registry.split(source_name) else {
		return Ok(None);
	};

	let Some(intent) = intent::parse_rename(target_name, stem)? else {
		return Ok(None);
	};

	// A rename that asks for the format the file already has is a rename, not a
	// request: `photo.heic` to `photo.HEIC` should not raise `E_NOOP` at someone
	// who only changed the case.
	if intent.groups.len() == 1 && intent.groups[0].tokens.is_empty() && intent.groups[0].format == source_format {
		return Ok(None);
	}

	Ok(Some(intent))
}

/// Alias desugaring, `docs/grammar.md` §9: a whole token before name resolution,
/// never inside a value.
/// Replace a preset by the tokens it stands for, `docs/config.md` §7.
///
/// A preset is a macro: the tokens it stands for join the group in its place, so
/// they share the group's duplicate and exclusivity rules and its order. Cycles
/// are refused when the configuration loads, so the recursion terminates.
fn expand(registry: &Registry, token: &Token, out: &mut Vec<Token>, problems: &mut Vec<Error>) {
	let Some(found) = registry.resolve(&token.name) else {
		// Left for the main loop, which knows how to report an unknown name.
		out.push(token.clone());
		return;
	};

	let Some(preset) = found.preset() else {
		out.push(token.clone());
		return;
	};

	let written = found
		.value
		.map(str::to_string)
		.or_else(|| token.value.as_ref().map(|raw| raw.text.clone()));

	let value = match (&preset.param, written) {
		(Some(param), Some(text)) => match crate::value::interpret(param, &text) {
			Ok(parsed) => Some(print::value(&parsed)),
			Err(fault) => {
				problems.push(error(fault.code, fault.message, token.span));
				return;
			}
		},
		(Some(_), None) => {
			problems.push(error(
				Code::ValueMissing,
				format!("'{}' needs a value", preset.name),
				token.span,
			));
			return;
		}
		(None, Some(_)) => {
			problems.push(error(
				Code::ValueUnexpected,
				format!("'{}' takes no value", preset.name),
				token.span,
			));
			return;
		}
		(None, None) => None,
	};

	for text in &preset.tokens {
		let text = match &value {
			Some(value) => text.replace("{value}", value),
			None => text.clone(),
		};

		match intent::parse_token(&text) {
			Ok(mut inner) => {
				// The token is the preset's, so a diagnostic points at the preset
				// rather than at a position inside another string.
				inner.span = token.span;
				inner.name_span = token.span;
				expand(registry, &inner, out, problems);
			}
			Err(problem) => problems.push(Error::Diagnostic(crate::error::Diagnostic::new(
				Code::CfgValidation,
				format!(
					"preset '{}' expands to '{text}', which does not parse: {problem}",
					preset.key
				),
				token.span,
			))),
		}
	}
}

fn desugar(token: &Token) -> Token {
	if token.value.is_some() {
		return token.clone();
	}

	let lower = token.name.to_ascii_lowercase();

	if lower == "4k" {
		return replaced(token, "cap", "2160");
	}

	if ["kb", "mb", "gb"]
		.iter()
		.any(|unit| lower.strip_suffix(unit).is_some_and(is_number))
	{
		return replaced(token, "size", &lower);
	}

	if let Some(digits) = lower.strip_suffix('p') {
		if is_number(digits) {
			return replaced(token, "cap", digits);
		}
	}

	token.clone()
}

fn replaced(token: &Token, name: &str, value: &str) -> Token {
	Token {
		name: name.to_string(),
		value: Some(Raw {
			text: value.to_string(),
			span: token.span,
		}),
		span: token.span,
		name_span: token.name_span,
	}
}

fn is_number(text: &str) -> bool {
	!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

fn unknown_name(token: &Token, registry: &Registry) -> Error {
	let mut error = error(
		Code::NameUnknown,
		format!("'{}' names no operation", token.name),
		token.name_span,
	);

	if let Error::Diagnostic(diagnostic) = &mut error {
		if let Some(suggestion) = closest(&token.name, registry.names()) {
			diagnostic.hint = Some(format!("did you mean '{suggestion}'?"));
		}
	}

	error
}

/// The nearest registered name within edit distance 2, `docs/grammar.md` §12.
fn closest<'a>(text: &str, names: impl Iterator<Item = &'a str>) -> Option<&'a str> {
	let mut best: Option<(usize, &str)> = None;

	for name in names {
		let distance = distance(text, name);
		if distance <= 2 && best.is_none_or(|(nearest, _)| distance < nearest) {
			best = Some((distance, name));
		}
	}

	best.map(|(_, name)| name)
}

/// Levenshtein distance, only ever used to phrase a hint.
fn distance(left: &str, right: &str) -> usize {
	let left: Vec<char> = left.chars().collect();
	let right: Vec<char> = right.chars().collect();

	let mut previous: Vec<usize> = (0..=right.len()).collect();
	let mut current = vec![0; right.len() + 1];

	for (row, left_char) in left.iter().enumerate() {
		current[0] = row + 1;
		for (column, right_char) in right.iter().enumerate() {
			let substitution = usize::from(left_char != right_char);
			current[column + 1] = (previous[column + 1] + 1)
				.min(current[column] + 1)
				.min(previous[column] + substitution);
		}
		std::mem::swap(&mut previous, &mut current);
	}

	previous[right.len()]
}

fn error(code: Code, message: impl Into<String>, span: Span) -> Error {
	Error::diagnostic(code, message, span)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::intent;
	use crate::registry::Registry;

	fn registry() -> Registry {
		Registry::builtin().expect("built-in registry")
	}

	fn image(format: &'static str) -> Source<'static> {
		Source {
			format,
			is_directory: false,
		}
	}

	fn resolve_text(text: &str, source: Source<'_>) -> Result<Resolved, Diagnoses> {
		let intent = intent::parse(text).expect("the intent should scan");
		resolve(&intent, source, &registry())
	}

	fn codes(text: &str, source: Source<'_>) -> Vec<Code> {
		resolve_text(text, source).unwrap_err().codes()
	}

	#[test]
	fn the_source_stem_comes_from_the_registry_not_the_last_dot() {
		let text = r#"
version = 'retype-config-1'

[formats.gz]
family = 'archive'
lossy = false
alpha = false
tier = 'exact'

[formats.zip]
family = 'archive'
lossy = false
alpha = false
tier = 'exact'

[formats.tar_gz]
family = 'archive'
lossy = false
alpha = false
tier = 'exact'
aliases = ['tar.gz']
"#;
		let registry = Registry::load(text).unwrap();

		let intent = intent_of(&registry, "archive.tar.gz", "archive.zip").unwrap();
		assert!(intent.is_some(), "the stem is 'archive', so 'zip' is an intent");
		assert_eq!(intent.unwrap().text, "zip");
	}

	#[test]
	fn a_rename_that_only_changes_the_case_carries_nothing() {
		assert!(intent_of(&registry(), "photo.heic", "photo.HEIC").unwrap().is_none());
	}

	#[test]
	fn a_rename_that_asks_for_the_same_format_carries_nothing() {
		assert!(intent_of(&registry(), "photo.jpeg", "photo.jpg").unwrap().is_none());
	}

	#[test]
	fn a_rename_that_keeps_the_stem_carries_the_new_format() {
		assert_eq!(
			intent_of(&registry(), "photo.heic", "photo.jpg").unwrap().unwrap().text,
			"jpg"
		);
	}

	#[test]
	fn a_rename_that_changes_the_stem_carries_nothing() {
		assert!(intent_of(&registry(), "photo.heic", "holiday.jpg").unwrap().is_none());
	}

	#[test]
	fn a_source_with_no_declared_format_carries_nothing() {
		assert!(intent_of(&registry(), "README", "README.md").unwrap().is_none());
	}

	#[test]
	fn an_unanchored_name_that_looks_like_an_intent_is_a_diagnosis() {
		let error = intent_of(&registry(), "photo.heic", "photo2+jpg").unwrap_err();

		assert_eq!(error.codes(), vec![Code::Anchor]);
	}

	#[test]
	fn a_known_token_becomes_its_operation() {
		let resolved = resolve_text("webp+quality85", image("heic")).unwrap();
		let op = &resolved.groups[0].ops[0];

		assert_eq!(op.key, "quality");
		assert_eq!(op.name, "quality");
		assert_eq!(op.value.as_ref().unwrap().written.text, "85");
		assert_eq!(op.slot, 50);
	}

	#[test]
	fn an_alias_resolves_but_is_never_printed() {
		let resolved = resolve_text("webp+q85", image("heic")).unwrap();

		assert_eq!(resolved.groups[0].ops[0].name, "quality");
	}

	#[test]
	fn aliases_desugar_whole_tokens() {
		let resized = resolve_text("webp+1080p", image("heic")).unwrap();
		assert_eq!(resized.groups[0].ops[0].key, "cap");
		assert_eq!(resized.groups[0].ops[0].value.as_ref().unwrap().written.text, "1080");

		let four_k = resolve_text("webp+4k", image("heic")).unwrap();
		assert_eq!(four_k.groups[0].ops[0].value.as_ref().unwrap().written.text, "2160");

		let size = resolve_text("webp+5mb", image("heic")).unwrap();
		assert_eq!(size.groups[0].ops[0].key, "size");
		assert_eq!(size.groups[0].ops[0].value.as_ref().unwrap().written.text, "5mb");
	}

	#[test]
	fn an_unknown_name_suggests_a_near_one() {
		let error = resolve_text("webp+gauy", image("heic")).unwrap_err();
		let first = error.first().unwrap();

		assert_eq!(first.code(), Code::NameUnknown);
		assert!(first.hint().unwrap_or_default().contains("gray"));
	}

	#[test]
	fn an_unknown_format_is_a_diagnosis() {
		assert_eq!(codes("xyz+q80", image("heic")), vec![Code::FormatUnknown]);
	}

	#[test]
	fn an_operation_that_does_not_apply_to_the_format_is_a_diagnosis() {
		assert_eq!(codes("png+quality80", image("heic")), vec![Code::Inapplicable]);
	}

	#[test]
	fn a_reserved_operation_is_unsupported_rather_than_unknown() {
		assert_eq!(codes("jpg+trim1h", image("heic")), vec![Code::Unsupported]);
	}

	#[test]
	fn a_missing_value_is_a_diagnosis() {
		assert_eq!(codes("webp+quality", image("heic")), vec![Code::ValueMissing]);
	}

	#[test]
	fn a_value_on_an_operation_that_takes_none_is_a_diagnosis() {
		assert_eq!(codes("webp+nometa80", image("heic")), vec![Code::ValueUnexpected]);
	}

	#[test]
	fn a_value_is_read_as_its_class_requires() {
		assert_eq!(codes("webp+quality1e2", image("heic")), vec![Code::ValueSyntax]);
		assert_eq!(codes("webp+qualityabc", image("heic")), vec![Code::ValueSyntax]);
		assert_eq!(codes("webp+rotate45", image("heic")), vec![Code::ValueSyntax]);
		assert_eq!(codes("webp+resize0x0", image("heic")), vec![Code::ValueSyntax]);
		assert_eq!(codes("webp+size5", image("heic")), vec![Code::ValueSyntax]);
	}

	#[test]
	fn a_value_outside_the_declared_bounds_is_out_of_range() {
		assert_eq!(codes("webp+quality101", image("heic")), vec![Code::ValueRange]);
		assert_eq!(codes("webp+quality0", image("heic")), vec![Code::ValueRange]);
		assert_eq!(codes("webp+cap8", image("heic")), vec![Code::ValueRange]);
	}

	#[test]
	fn a_value_inside_its_bounds_is_accepted_and_parsed() {
		let resolved = resolve_text("webp+quality85+cap1000+rotate90", image("heic")).unwrap();
		let values: Vec<&Value> = resolved.groups[0]
			.pipeline()
			.iter()
			.filter_map(|op| op.value.as_ref().map(|argument| &argument.parsed))
			.collect();

		assert_eq!(
			values,
			vec![&Value::Int(85), &Value::Int(1000), &Value::Enum("90".to_string())],
			"the written order, end to end"
		);
	}

	#[test]
	fn one_operation_twice_in_a_group_is_a_diagnosis() {
		assert_eq!(codes("jpg+quality80+quality90", image("heic")), vec![Code::Duplicate]);
	}

	#[test]
	fn the_same_operation_in_two_groups_is_not_a_duplicate() {
		let resolved = resolve_text("webp+q60,avif+q90", image("heic")).unwrap();

		assert_eq!(resolved.groups[0].ops[0].key, "quality");
		assert_eq!(resolved.groups[1].ops[0].key, "quality");
	}

	#[test]
	fn two_operations_in_one_exclusive_group_are_a_diagnosis() {
		assert_eq!(codes("jpg+cap800+width100", image("heic")), vec![Code::Exclusive]);
	}

	#[test]
	fn keeping_the_source_alone_is_a_no_op() {
		assert_eq!(codes("jpg", image("jpg")), vec![Code::Noop]);
	}

	#[test]
	fn keeping_the_source_beside_a_conversion_is_fine() {
		let resolved = resolve_text("jpg,webp+q80", image("jpg")).unwrap();

		assert!(resolved.groups[0].keep_source);
		assert!(!resolved.groups[1].keep_source);
	}

	#[test]
	fn a_group_with_tokens_re_encodes_even_the_source_format() {
		let resolved = resolve_text("jpg+q60", image("jpg")).unwrap();

		assert!(!resolved.groups[0].keep_source);
		assert_eq!(resolved.groups[0].ops[0].key, "quality");
	}

	#[test]
	fn folder_requires_a_directory() {
		assert_eq!(codes("folder", image("jpg")), vec![Code::FormatFolder]);
	}

	fn preset_text(builtin: &str) -> String {
		format!("version = 'retype-config-1'\n\n{builtin}")
	}

	#[test]
	fn a_preset_expands_where_it_stands() {
		let registry = Registry::load(&preset_text(
			"\
[formats.webp]
family = 'image'
lossy = true
alpha = true
tier = 'converted'

[ops.quality]
slot = 50
param = { class = 'int', min = 1, max = 100 }
formats = ['webp']

[ops.cap]
slot = 28
param = { class = 'int', min = 16, max = 16384 }
formats = ['webp']

[ops.nometa]
slot = 60
formats = ['webp']

[presets.look]
tokens = ['quality75', 'nometa']
",
		))
		.expect("loads");

		let intent = intent::parse("webp+cap200+look").expect("scans");
		let resolved = resolve(
			&intent,
			Source {
				format: "heic",
				is_directory: false,
			},
			&registry,
		)
		.expect("resolves");
		let keys: Vec<&str> = resolved.groups[0].pipeline().iter().map(|op| op.key.as_str()).collect();

		assert_eq!(
			keys,
			["cap", "quality", "nometa"],
			"the preset's tokens sit where it stood"
		);
	}

	#[test]
	fn a_preset_that_repeats_an_operation_is_a_duplicate() {
		let registry = Registry::load(&preset_text(
			"\
[formats.webp]
family = 'image'
lossy = true
alpha = true
tier = 'converted'

[ops.quality]
slot = 50
param = { class = 'int', min = 1, max = 100 }
formats = ['webp']

[presets.look]
tokens = ['quality75']
",
		))
		.expect("loads");

		let intent = intent::parse("webp+quality80+look").expect("scans");
		let problems = resolve(
			&intent,
			Source {
				format: "heic",
				is_directory: false,
			},
			&registry,
		)
		.unwrap_err()
		.codes();

		assert_eq!(problems, [Code::Duplicate]);
	}

	#[test]
	fn a_preset_value_is_read_with_its_own_class() {
		let registry = Registry::load(&preset_text(
			"\
[formats.webp]
family = 'image'
lossy = true
alpha = true
tier = 'converted'

[ops.cap]
slot = 28
param = { class = 'int', min = 16, max = 16384 }
formats = ['webp']

[presets.thumb]
param = { class = 'int', min = 16, max = 4096 }
tokens = ['cap={value}']
",
		))
		.expect("loads");

		let intent = intent::parse("webp+thumb0200").expect("scans");
		let resolved = resolve(
			&intent,
			Source {
				format: "heic",
				is_directory: false,
			},
			&registry,
		)
		.expect("resolves");
		let cap = &resolved.groups[0].pipeline()[0];

		assert_eq!(
			cap.value.as_ref().map(|argument| argument.parsed.clone()),
			Some(Value::Int(200))
		);
	}

	#[test]
	fn a_preset_out_of_range_is_a_value_range() {
		let registry = Registry::load(&preset_text(
			"\
[formats.webp]
family = 'image'
lossy = true
alpha = true
tier = 'converted'

[ops.cap]
slot = 28
param = { class = 'int', min = 16, max = 16384 }
formats = ['webp']

[presets.thumb]
param = { class = 'int', min = 16, max = 4096 }
tokens = ['cap={value}']
",
		))
		.expect("loads");

		let intent = intent::parse("webp+thumb8").expect("scans");
		let problems = resolve(
			&intent,
			Source {
				format: "heic",
				is_directory: false,
			},
			&registry,
		)
		.unwrap_err()
		.codes();

		assert_eq!(problems, [Code::ValueRange]);
	}

	#[test]
	fn a_preset_that_expands_into_a_conflict_still_conflicts() {
		let registry = Registry::load(&preset_text(
			"\
[formats.webp]
family = 'image'
lossy = true
alpha = true
tier = 'converted'

[ops.quality]
slot = 50
exclusive = ['encode-target']
param = { class = 'int', min = 1, max = 100 }
formats = ['webp']

[ops.size]
slot = 50
exclusive = ['encode-target']
formats = ['webp']
param = { class = 'size', min = 1 }

[presets.small]
tokens = ['size5mb']
",
		))
		.expect("loads");

		let intent = intent::parse("webp+quality80+small").expect("scans");
		let problems = resolve(
			&intent,
			Source {
				format: "heic",
				is_directory: false,
			},
			&registry,
		)
		.unwrap_err()
		.codes();

		assert_eq!(
			problems,
			[Code::Exclusive],
			"an expansion is checked like any other token"
		);
	}

	#[test]
	fn the_pipeline_is_the_order_written() {
		let resolved = resolve_text("webp+nometa+quality80+cap1000", image("heic")).unwrap();
		let pipeline: Vec<&str> = resolved.groups[0].pipeline().iter().map(|op| op.key.as_str()).collect();

		assert_eq!(pipeline, vec!["nometa", "quality", "cap"]);
	}

	#[test]
	fn independent_problems_are_all_reported() {
		let diagnoses = resolve_text("xyz+qualty80,png+quality80,png+nometa80", image("heic")).unwrap_err();

		assert_eq!(diagnoses.len(), 3);
	}
}
