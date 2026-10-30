//! Canonical printing, `docs/grammar.md` §10.
//!
//! The canonical form is what the journal records, what an editor shows and what
//! a notification says, so it has to be the form that parses back to the same
//! thing. `=` appears only when the compact spelling would read differently, so
//! the common case stays short.

use crate::registry::Registry;
use crate::resolve::{Op, Resolved};
use crate::value::Value;

/// Print an intent in canonical form.
pub fn intent(resolved: &Resolved, registry: &Registry) -> String {
	let mut groups = Vec::with_capacity(resolved.groups.len());

	for (index, group) in resolved.groups.iter().enumerate() {
		let mut text = group.format.clone();

		for op in group.ordered() {
			text.push('+');
			text.push_str(&token(op, registry));
		}

		// The control tokens are part of the intent rather than of a group, and
		// the only place a trailing shorthand can be written is the end of a
		// group, so they go on the first one.
		if index == 0 {
			if resolved.meta.ask {
				text.push_str("+ask");
			}
			if resolved.meta.now {
				text.push_str("+now");
			}
		}

		groups.push(text);
	}

	let mut text = groups.join(",");
	if resolved.meta.ask || resolved.meta.now {
		// Nothing: the control tokens are inside the text above.
	}
	text.shrink_to_fit();
	text
}

/// One token, with `=` only when the compact spelling would resolve differently.
fn token(op: &Op, registry: &Registry) -> String {
	let mut compact = op.name.clone();

	if let Some(argument) = &op.value {
		compact.push_str(&value(&argument.parsed));
	}

	match registry.resolve(&compact) {
		Some(resolved)
			if resolved.op.key == op.key
				&& resolved.value == op.value.as_ref().map(|argument| value(&argument.parsed)).as_deref() =>
		{
			compact
		}
		_ => match &op.value {
			Some(argument) => format!("{}={}", op.name, value(&argument.parsed)),
			None => op.name.clone(),
		},
	}
}

/// A value in the form its class produces, `docs/grammar.md` C4.
pub fn value(value: &Value) -> String {
	match value {
		Value::Int(number) => number.to_string(),
		Value::Dim { width, height } | Value::Ratio { width, height } => format!("{width}x{height}"),
		Value::Size(bytes) => size(*bytes),
		Value::Time(seconds) => duration(*seconds),
		Value::Range { from, to } => {
			let from = from.map(duration_or_number).unwrap_or_default();
			let to = to.map(duration_or_number).unwrap_or_default();
			format!("{from}-{to}")
		}
		Value::Enum(declared) => declared.clone(),
		Value::Code(tag) => tag.clone(),
		Value::Text(text) => text.clone(),
	}
}

/// The largest unit that expresses the byte count exactly, and failing that the
/// largest that needs at most three decimal places. `kb` always suffices, so
/// every byte count has a spelling.
fn size(bytes: u64) -> String {
	const UNITS: [(&str, u64); 3] = [("gb", 1_000_000_000), ("mb", 1_000_000), ("kb", 1_000)];

	for (unit, scale) in UNITS {
		if bytes % scale == 0 {
			return format!("{}{unit}", bytes / scale);
		}
	}

	for (unit, scale) in UNITS {
		let step = scale / 1_000;

		// Three decimal places of this unit, if the byte count fills them evenly.
		if bytes % step != 0 {
			continue;
		}

		let mut text = format!("{}.{:03}", bytes / scale, (bytes % scale) / step);
		while text.ends_with('0') {
			text.pop();
		}
		if text.ends_with('.') {
			text.pop();
		}

		return format!("{text}{unit}");
	}

	unreachable!("a kb is a thousand bytes, so three decimal places always suffice")
}

/// The tagged form with zero units left out, so `1:00:30` prints `1h30s`.
fn duration(seconds: u64) -> String {
	let hours = seconds / 3600;
	let minutes = (seconds % 3600) / 60;
	let remainder = seconds % 60;

	let mut text = String::new();
	if hours > 0 {
		text.push_str(&format!("{hours}h"));
	}
	if minutes > 0 {
		text.push_str(&format!("{minutes}m"));
	}
	if remainder > 0 || text.is_empty() {
		text.push_str(&format!("{remainder}s"));
	}

	text
}

/// A range end is a number or a duration depending on what the range counts,
/// which the value itself records: only a duration can print a unit.
fn duration_or_number(number: u64) -> String {
	number.to_string()
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::intent;
	use crate::resolve::{self, Source};

	fn resolved(text: &str) -> (Resolved, Registry) {
		let registry = Registry::builtin().expect("built-in registry");
		let intent = intent::parse(text).expect("the intent should scan");
		let resolved = resolve::resolve(
			&intent,
			Source {
				format: "heic",
				is_directory: false,
			},
			&registry,
		)
		.expect("the intent should resolve");

		(resolved, registry)
	}

	fn canonical(text: &str) -> String {
		let (resolved, registry) = resolved(text);
		intent(&resolved, &registry)
	}

	#[test]
	fn a_plain_group_prints_its_format() {
		assert_eq!(canonical("jpg"), "jpg");
	}

	#[test]
	fn an_alias_prints_as_its_primary_name() {
		assert_eq!(canonical("webp+q85"), "webp+quality85");
		assert_eq!(canonical("webp+grey"), "webp+gray");
	}

	#[test]
	fn tokens_print_in_slot_order() {
		assert_eq!(canonical("webp+nometa+q80+cap1000"), "webp+cap1000+quality80+nometa");
	}

	#[test]
	fn groups_keep_their_written_order() {
		assert_eq!(canonical("webp+q80,jpg,avif+q60"), "webp+quality80,jpg,avif+quality60");
	}

	#[test]
	fn an_alias_form_prints_as_the_operation_it_became() {
		assert_eq!(canonical("webp+1080p"), "webp+cap1080");
		assert_eq!(canonical("webp+5mb"), "webp+size5mb");
		assert_eq!(canonical("webp+4k"), "webp+cap2160");
	}

	#[test]
	fn leading_zeros_are_not_printed() {
		assert_eq!(canonical("webp+q085"), "webp+quality85");
	}

	#[test]
	fn a_duration_prints_in_tagged_form() {
		assert_eq!(duration(3630), "1h30s");
		assert_eq!(duration(3600), "1h");
		assert_eq!(duration(150), "2m30s");
		assert_eq!(duration(90), "1m30s");
		assert_eq!(duration(60), "1m");
		assert_eq!(duration(0), "0s");
	}

	#[test]
	fn a_size_prints_in_its_largest_exact_unit() {
		assert_eq!(size(5_000_000), "5mb");
		assert_eq!(size(2_000_000_000), "2gb");
		assert_eq!(size(1_500_000), "1500kb");
	}

	#[test]
	fn the_control_tokens_print_as_words() {
		let (mut resolved, registry) = resolved("webp+q80");
		resolved.meta.ask = true;

		assert_eq!(intent(&resolved, &registry), "webp+quality80+ask");
	}

	#[test]
	fn asking_prints_before_demanding_now() {
		let (mut resolved, registry) = resolved("jpg");
		resolved.meta.ask = true;
		resolved.meta.now = true;

		assert_eq!(intent(&resolved, &registry), "jpg+ask+now");
	}

	#[test]
	fn the_shorthand_and_the_word_canonicalise_the_same_way() {
		assert_eq!(canonical("webp+q80+"), canonical("webp+q80+ask"));
		assert_eq!(canonical("webp+q80!"), canonical("webp+q80+now"));
	}

	#[test]
	fn printing_is_idempotent_over_a_corpus() {
		let corpus = [
			"jpg",
			"webp+q85",
			"webp+1080p",
			"jpg,png",
			"jpg,jpg+q60,jpg+q90",
			"webp+nometa+q80+cap1000+rotate90",
			"avif+q60+effort4",
			"webp+q80+",
			"webp+q80!",
			"webp+q60+now,avif+q90+ask",
		];

		for written in corpus {
			let once = canonical(written);
			let twice = canonical(&once);

			assert_eq!(once, twice, "{written} printed as {once}, which printed as {twice}");
		}
	}

	/// A group's meaning: its format, whether it keeps the source, and its
	/// operations, without the spans and spellings printing may change.
	type Meaning = Vec<(String, bool, Vec<(String, Option<Value>)>)>;

	fn meaning(resolved: &Resolved) -> Meaning {
		resolved
			.groups
			.iter()
			.map(|group| {
				let mut ops: Vec<(String, Option<Value>)> = group
					.ops
					.iter()
					.map(|op| {
						(
							op.key.clone(),
							op.value.as_ref().map(|argument| argument.parsed.clone()),
						)
					})
					.collect();
				ops.sort_by(|left, right| left.0.cmp(&right.0));
				(group.format.clone(), group.keep_source, ops)
			})
			.collect()
	}

	#[test]
	fn a_reparse_yields_the_same_operations() {
		let corpus = [
			"webp+q85+cap1000",
			"jpg,jpg+q60+now",
			"webp+5mb+nometa!",
			"webp+1080p+rotate90",
			"avif+effort4+q60",
			"avif+size2mb",
		];

		for written in corpus {
			let (first, registry) = resolved(written);
			let printed = intent(&first, &registry);
			let (second, _) = resolved(&printed);

			assert_eq!(meaning(&first), meaning(&second), "{written} printed {printed}");
			assert_eq!(first.meta, second.meta, "{written} printed {printed}");
		}
	}
}
