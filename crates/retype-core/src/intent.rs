//! The intent grammar, `docs/grammar.md`, specification `retype-grammar-1`.
//!
//! This module turns an intent string into groups and tokens, and says why it
//! cannot when it cannot. Resolving a token name to an operation, checking
//! applicability and reading values against declared bounds need the operation
//! registry, which arrives with configuration, so tokens carry their value as
//! written and nothing here guesses what it means.

pub use crate::error::Diagnoses;

use crate::error::{Code, Diagnostic, Error, Span};

/// Limits from `docs/grammar.md` §5.
pub const MAX_LENGTH: usize = 255;
pub const MAX_GROUPS: usize = 8;
pub const MAX_TOKENS: usize = 32;

/// The characters an intent may contain, `docs/grammar.md` §4.
const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789.,+!=_-";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Meta {
	pub ask: bool,
	pub now: bool,
}

/// One output: a format, and the transformations asked of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
	pub format: String,
	pub tokens: Vec<Token>,
	pub span: Span,
}

/// One transformation as written. The value is raw, because whether it is an
/// integer, a duration or a preset name is a property of the operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
	pub name: String,
	pub value: Option<Raw>,
	pub span: Span,
	pub name_span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Raw {
	pub text: String,
	pub span: Span,
}

/// A parsed intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intent {
	pub text: String,
	pub meta: Meta,
	pub groups: Vec<Group>,
	/// Conditions the parser resolved in the user's favour rather than
	/// refusing. They are not failures, and they are not silent either.
	pub warnings: Vec<Diagnostic>,
}

/// The intent string a new name carries, if any, `docs/grammar.md` §3.
///
/// `Ok(None)` is a plain rename: nothing to convert. `Err` is a name that is
/// not anchored on the source stem yet looks like it carries intent, which is a
/// mistake worth reporting rather than silently ignoring.
pub fn anchor<'a>(new_name: &'a str, stem: &str) -> Result<Option<&'a str>, Error> {
	if let Some(rest) = new_name.strip_prefix(stem).and_then(|rest| rest.strip_prefix('.')) {
		return Ok(Some(rest));
	}

	let after_last_dot = new_name.rsplit_once('.').map(|(_, rest)| rest).unwrap_or(new_name);
	if after_last_dot.contains('+') || after_last_dot.contains(',') || after_last_dot.ends_with('!') {
		return Err(Error::diagnostic(
			Code::Anchor,
			format!("'{new_name}' does not begin with the source stem '{stem}'"),
			Span::new(0, new_name.len()),
		));
	}

	Ok(None)
}

/// Parse a new name against the source stem, `docs/grammar.md` §6.
pub fn parse_rename(new_name: &str, stem: &str) -> Result<Option<Intent>, Diagnoses> {
	match anchor(new_name, stem).map_err(Diagnoses::one)? {
		Some(intent) => Ok(Some(parse(intent)?)),
		None => Ok(None),
	}
}

/// Parse an intent string from any carrier other than a file name.
pub fn parse(text: &str) -> Result<Intent, Diagnoses> {
	let mut errors = Vec::new();

	for (offset, character) in text.char_indices() {
		if !character.is_ascii() || !CHARSET.contains(&(character as u8)) {
			errors.push(Error::diagnostic(
				Code::Charset,
				format!("'{character}' is not allowed in an intent"),
				Span::new(offset, offset + character.len_utf8()),
			));
		}
	}

	if text.len() > MAX_LENGTH {
		errors.push(Error::diagnostic(
			Code::TooLong,
			format!("an intent is at most {MAX_LENGTH} bytes, this one is {}", text.len()),
			Span::new(0, text.len()),
		));
	}

	if !errors.is_empty() {
		return Err(Diagnoses::new(errors));
	}

	let (meta, body, warnings) = meta(text);

	if body.is_empty() {
		errors.push(Error::diagnostic(
			Code::IntentEmpty,
			"an intent names at least one format",
			Span::new(0, text.len()),
		));
		return Err(Diagnoses::new(errors));
	}

	let (groups, tokens) = scan_groups(body, &mut errors);

	if groups.len() > MAX_GROUPS {
		errors.push(Error::diagnostic(
			Code::TooMany,
			format!(
				"an intent has at most {MAX_GROUPS} groups, this one has {}",
				groups.len()
			),
			Span::new(0, text.len()),
		));
	}
	if tokens > MAX_TOKENS {
		errors.push(Error::diagnostic(
			Code::TooMany,
			format!("an intent has at most {MAX_TOKENS} tokens, this one has {tokens}"),
			Span::new(0, text.len()),
		));
	}

	if !errors.is_empty() {
		return Err(Diagnoses::new(errors));
	}

	Ok(Intent {
		text: text.to_string(),
		meta,
		groups,
		warnings,
	})
}

/// Strip the meta suffixes, which live outside the groups, `docs/grammar.md` §6.
///
/// Asking wins over demanding `now`, because one of those outcomes is
/// recoverable: an unwanted prompt is dismissed with a key, while a refused
/// rename has to be performed again.
fn meta(text: &str) -> (Meta, &str, Vec<Diagnostic>) {
	let mut meta = Meta::default();
	let mut warnings = Vec::new();
	let mut body = text;

	if let Some(stripped) = body.strip_suffix('!') {
		meta.now = true;
		body = stripped;
	}
	if let Some(stripped) = body.strip_suffix('+') {
		meta.ask = true;
		body = stripped;
	}

	if meta.ask && meta.now {
		meta.now = false;
		warnings.push(Diagnostic::new(
			Code::MetaConflict,
			"ignored 'now' because the intent also asks, and asking wins",
			Span::new(body.len(), text.len()),
		));
	}

	(meta, body, warnings)
}

fn scan_groups(body: &str, errors: &mut Vec<Error>) -> (Vec<Group>, usize) {
	let mut groups = Vec::new();
	let mut tokens = 0;

	for (offset, group) in split(body, ',') {
		if group.trim().is_empty() {
			errors.push(Error::diagnostic(
				Code::GroupEmpty,
				"a group cannot be empty",
				Span::new(offset, offset + group.len() + 1),
			));
			continue;
		}

		let (group, count) = scan_group(offset, group, errors);
		tokens += count;
		if let Some(group) = group {
			groups.push(group);
		}
	}

	(groups, tokens)
}

fn scan_group(offset: usize, group: &str, errors: &mut Vec<Error>) -> (Option<Group>, usize) {
	let mut parts = split(group, '+');
	let (format_offset, format) = parts.remove(0);

	if !is_format(format) {
		errors.push(Error::diagnostic(
			Code::FormatUnknown,
			format!("'{format}' is not a format name"),
			Span::new(offset + format_offset, offset + format_offset + format.len()),
		));
		return (None, parts.len());
	}

	let mut tokens = Vec::new();
	for (token_offset, token) in parts {
		let start = offset + token_offset;
		let end = start + token.len();

		if token.is_empty() {
			errors.push(Error::diagnostic(
				Code::TokenEmpty,
				"a '+' must be followed by a token",
				Span::new(start, end + 1),
			));
			continue;
		}

		match scan_token(token, start) {
			Ok(token) => tokens.push(token),
			Err(error) => errors.push(error),
		}
	}

	let count = tokens.len();
	let group = Group {
		format: format.to_ascii_lowercase(),
		tokens,
		span: Span::new(offset, offset + group.len()),
	};

	(Some(group), count)
}

fn scan_token(token: &str, start: usize) -> Result<Token, Error> {
	let equals: Vec<usize> = token
		.bytes()
		.enumerate()
		.filter(|(_, byte)| *byte == b'=')
		.map(|(index, _)| index)
		.collect();

	if equals.len() > 1 {
		return Err(Error::diagnostic(
			Code::ValueSyntax,
			"a token carries one '=' at most",
			Span::new(start + equals[1], start + equals[1] + 1),
		));
	}

	let (name, value) = match equals.first() {
		Some(index) => (&token[..*index], Some(&token[index + 1..])),
		None => (token, None),
	};

	if !is_name(name) {
		return Err(Error::diagnostic(
			Code::NameUnknown,
			format!("'{name}' is not a token name"),
			Span::new(start, start + name.len()),
		));
	}

	let name_span = Span::new(start, start + name.len());
	let value = value.filter(|value| !value.is_empty()).map(|value| Raw {
		text: value.to_string(),
		span: Span::new(start + name.len() + 1, start + token.len()),
	});

	Ok(Token {
		name: name.to_ascii_lowercase(),
		value,
		span: Span::new(start, start + token.len()),
		name_span,
	})
}

/// A format is dot-separated alphanumeric parts, `docs/grammar.md` §5.
fn is_format(text: &str) -> bool {
	!text.is_empty()
		&& text
			.split('.')
			.all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_alphanumeric()))
}

/// A token name starts with a letter and continues with letters and digits,
/// `docs/grammar.md` §5. Case is matched insensitively.
fn is_name(text: &str) -> bool {
	let mut bytes = text.bytes();

	match bytes.next() {
		Some(byte) if byte.is_ascii_alphabetic() => {}
		_ => return false,
	}

	bytes.all(|byte| byte.is_ascii_alphanumeric())
}

/// Split at a delimiter, keeping the byte offset of each part.
fn split(text: &str, delimiter: char) -> Vec<(usize, &str)> {
	let mut parts = Vec::new();
	let mut start = 0;

	for (index, byte) in text.bytes().enumerate() {
		if byte == delimiter as u8 {
			parts.push((start, &text[start..index]));
			start = index + 1;
		}
	}
	parts.push((start, &text[start..]));

	parts
}

#[cfg(test)]
mod tests {
	use super::*;

	fn codes(result: &Result<Intent, Diagnoses>) -> Vec<Code> {
		match result {
			Ok(intent) => panic!("parsed unexpectedly: {intent:?}"),
			Err(diagnoses) => diagnoses.iter().map(|error| error.code()).collect(),
		}
	}

	#[test]
	fn anchoring_uses_the_source_stem() {
		assert_eq!(anchor("photo.jpg", "photo").unwrap(), Some("jpg"));
		assert_eq!(anchor("my.file.txt", "my.file").unwrap(), Some("txt"));
		assert_eq!(anchor("archive.tar.gz", "archive").unwrap(), Some("tar.gz"));
		assert_eq!(anchor("photo", "photo").unwrap(), None, "no dot, no intent");
	}

	#[test]
	fn an_unanchored_name_with_intent_syntax_is_an_error() {
		let error = anchor("photograph+jpg", "photo").unwrap_err();

		assert_eq!(error.code(), Code::Anchor);
		assert_eq!(error.exit_code(), 3);
	}

	#[test]
	fn a_plus_in_a_stem_is_not_a_separator() {
		assert_eq!(anchor("a+b.jpg", "a+b").unwrap(), Some("jpg"));
	}

	#[test]
	fn a_plain_rename_carries_no_intent() {
		let intent = parse_rename("holiday.heic", "photo").unwrap();

		assert!(intent.is_none());
	}

	#[test]
	fn the_simplest_intent_is_one_format() {
		let intent = parse("jpg").unwrap();

		assert_eq!(intent.groups.len(), 1);
		assert_eq!(intent.groups[0].format, "jpg");
		assert!(intent.groups[0].tokens.is_empty());
		assert_eq!(intent.meta, Meta::default());
	}

	#[test]
	fn formats_and_names_are_canonicalised() {
		let intent = parse("JPG+Quality=85").unwrap();

		assert_eq!(intent.groups[0].format, "jpg");
		assert_eq!(intent.groups[0].tokens[0].name, "quality");
	}

	#[test]
	fn tokens_carry_their_spans() {
		let intent = parse("jpg+quality=85").unwrap();
		let token = &intent.groups[0].tokens[0];

		assert_eq!(token.name_span, Span::new(4, 11));
		assert_eq!(token.value.as_ref().unwrap().text, "85");
		assert_eq!(token.value.as_ref().unwrap().span, Span::new(12, 14));
		assert_eq!(token.span, Span::new(4, 14));
	}

	#[test]
	fn a_value_may_be_absent() {
		let intent = parse("jpg+nometa").unwrap();

		assert!(intent.groups[0].tokens[0].value.is_none());
	}

	#[test]
	fn groups_are_split_and_keep_their_order() {
		let intent = parse("jpg,webp+quality=85,png").unwrap();

		assert_eq!(intent.groups.len(), 3);
		assert_eq!(intent.groups[0].format, "jpg");
		assert_eq!(intent.groups[1].format, "webp");
		assert_eq!(intent.groups[1].tokens[0].name, "quality");
		assert_eq!(intent.groups[1].tokens[0].value.as_ref().unwrap().text, "85");
		assert_eq!(intent.groups[2].format, "png");
	}

	#[test]
	fn a_compound_format_is_one_group() {
		let intent = parse("tar.gz").unwrap();

		assert_eq!(intent.groups[0].format, "tar.gz");
	}

	#[test]
	fn the_meta_suffixes_are_read_from_the_end() {
		assert_eq!(parse("jpg!").unwrap().meta, Meta { ask: false, now: true });
		assert_eq!(parse("jpg+").unwrap().meta, Meta { ask: true, now: false });
	}

	#[test]
	fn asking_wins_over_demanding_now() {
		let intent = parse("jpg+!").unwrap();

		assert_eq!(intent.meta, Meta { ask: true, now: false });
		assert_eq!(intent.warnings.len(), 1);
		assert_eq!(intent.warnings[0].code, Code::MetaConflict);
		assert_eq!(intent.warnings[0].span, Some(Span::new(3, 5)));
		assert!(intent.warnings[0].message.contains("ignored 'now'"));
	}

	#[test]
	fn an_empty_intent_is_a_diagnosis() {
		assert_eq!(codes(&parse("")), vec![Code::IntentEmpty]);
		assert_eq!(codes(&parse("!")), vec![Code::IntentEmpty]);
	}

	#[test]
	fn a_character_outside_the_set_is_a_diagnosis_with_its_span() {
		let result = parse("jpg+q 80");

		assert_eq!(codes(&result), vec![Code::Charset]);
		assert_eq!(result.unwrap_err().first().unwrap().span(), Some(Span::new(5, 6)));
	}

	#[test]
	fn an_over_long_intent_is_a_diagnosis() {
		let long = "j".repeat(MAX_LENGTH + 1);

		assert_eq!(codes(&parse(&long)), vec![Code::TooLong]);
	}

	#[test]
	fn empty_groups_are_diagnosed() {
		assert_eq!(codes(&parse(",jpg")), vec![Code::GroupEmpty]);
		assert_eq!(codes(&parse("jpg,,png")), vec![Code::GroupEmpty]);
	}

	#[test]
	fn too_many_groups_is_a_diagnosis() {
		let many = ["jpg"; MAX_GROUPS + 1].join(",");

		assert_eq!(codes(&parse(&many)), vec![Code::TooMany]);
	}

	#[test]
	fn too_many_tokens_is_a_diagnosis() {
		let tokens = ["q80"; MAX_TOKENS + 1].join("+");
		let intent = format!("jpg+{tokens}");

		assert_eq!(codes(&parse(&intent)), vec![Code::TooMany]);
	}

	#[test]
	fn an_empty_token_is_a_diagnosis() {
		assert_eq!(codes(&parse("jpg++q80")), vec![Code::TokenEmpty]);
	}

	#[test]
	fn a_second_equals_is_a_diagnosis() {
		assert_eq!(codes(&parse("jpg+q=80=1")), vec![Code::ValueSyntax]);
	}

	#[test]
	fn a_format_that_is_not_a_format_is_a_diagnosis() {
		assert_eq!(codes(&parse("jp g")), vec![Code::Charset]);
		assert_eq!(codes(&parse("jpg..png")), vec![Code::FormatUnknown]);
		assert_eq!(codes(&parse("+q80")), vec![Code::FormatUnknown]);
	}

	#[test]
	fn a_token_that_is_not_a_name_is_a_diagnosis() {
		assert_eq!(codes(&parse("jpg+80")), vec![Code::NameUnknown]);
	}

	#[test]
	fn independent_diagnoses_are_ordered_by_offset_and_capped() {
		let result = parse("jpg+80,png+90,webp+70,gif+60,ico+50,bmp+40,tiff+30,tga+20,jp2+10");
		let diagnoses = result.unwrap_err();

		assert_eq!(diagnoses.len(), Diagnoses::LIMIT);
		let starts: Vec<usize> = diagnoses
			.iter()
			.filter_map(|error| error.span())
			.map(|span| span.start)
			.collect();
		assert_eq!(starts, {
			let mut sorted = starts.clone();
			sorted.sort_unstable();
			sorted
		});
	}
}
