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

		for op in group.pipeline() {
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

	let reads_back = registry.resolve(&compact).is_some_and(|resolved| {
		resolved.op().is_some_and(|found| found.key == op.key)
			&& resolved.value == op.value.as_ref().map(|argument| value(&argument.parsed)).as_deref()
	});

	match reads_back {
		true => compact,
		_ => match &op.value {
			Some(argument) => format!("{}={}", op.name, value(&argument.parsed)),
			None => op.name.clone(),
		},
	}
}

/// What the intent does, in one sentence, `docs/cli.md` §3.
///
/// The wording comes from each operation's `say`, so a user-declared operation
/// describes itself without a build, and the order is the order the operations
/// run in. It reads aloud, which is the point: this is the line a notification
/// shows and the line a user can check against what they meant.
pub fn sentence(resolved: &Resolved, registry: &Registry, source: &str) -> String {
	let mut text = resolved
		.groups
		.iter()
		.map(|group| group_sentence(group, registry, source))
		.collect::<Vec<String>>()
		.join("; ");

	if resolved.meta.ask {
		text.push_str(", after asking");
	}
	if resolved.meta.now {
		text.push_str(", without asking");
	}

	text
}

fn group_sentence(group: &crate::resolve::Group, registry: &Registry, source: &str) -> String {
	if group.keep_source {
		return format!("keep the source {source} as it is");
	}

	let mut text = format!("convert from {source} to {}", group.format);

	for (index, op) in group.pipeline().iter().enumerate() {
		text.push_str(if index == 0 { " with " } else { ", then " });
		text.push_str(&phrase(op, registry));
	}

	text
}

fn phrase(op: &Op, registry: &Registry) -> String {
	let value = op.value.as_ref().map(|argument| value(&argument.parsed));

	match registry.op(&op.key).and_then(|declared| declared.say.as_deref()) {
		Some(template) => template.replace("{value}", value.as_deref().unwrap_or_default()),
		// A declaration without a phrase still has to be readable, so the token
		// stands in for it rather than leaving a gap in the sentence.
		None => token(op, registry),
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

/// The largest unit that reads as at least one and needs at most three decimal
/// places, so `1_500_000` prints `1.5mb` rather than `0.0015gb`, and 1024 bytes
/// prints `1kib` rather than `1.024kb`.
///
/// Decimal and binary units both get a turn, because the byte count alone does
/// not say which one the user had in mind, and the larger unit that fits exactly
/// is the one they would have written.
pub fn size(bytes: u64) -> String {
	// A byte count below one unit is said in bytes; a fraction of a kilobyte reads
	// as a measurement that lost its precision.
	match bytes {
		0 => return "0 bytes".to_string(),
		1 => return "1 byte".to_string(),
		2..=999 => return format!("{bytes} bytes"),
		_ => {}
	}

	let bytes = u128::from(bytes);

	for (unit, scale) in UNITS {
		let scale = u128::from(scale);

		// Thousandths of this unit, if the count lands on one exactly.
		let thousandths = bytes * 1_000;
		if thousandths % scale != 0 || thousandths / scale < 1_000 {
			continue;
		}

		let thousandths = thousandths / scale;
		let mut text = format!("{}.{:03}", thousandths / 1_000, thousandths % 1_000);
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

/// A duration as a person reads it: tenths of a second under a minute, then
/// minutes and seconds, then hours and minutes. An estimate is not a stopwatch, so
/// nothing finer than a tenth is shown; the grammar's own spelling is `duration`.
pub fn short_duration(time: std::time::Duration) -> String {
	let seconds = time.as_secs_f64();

	match seconds {
		seconds if seconds < 60.0 => format!("{seconds:.1}s"),
		seconds if seconds < 3_600.0 => {
			let whole = seconds as u64;
			format!("{}m{:02}s", whole / 60, whole % 60)
		}
		seconds => {
			let whole = seconds as u64;
			format!("{}h{:02}m", whole / 3_600, whole % 3_600 / 60)
		}
	}
}

/// The same order the parser accepts, largest unit first.
const UNITS: [(&str, u64); 6] = [
	("gib", 1024 * 1024 * 1024),
	("gb", 1_000_000_000),
	("mib", 1024 * 1024),
	("mb", 1_000_000),
	("kib", 1024),
	("kb", 1_000),
];

/// The tagged form with zero units left out, so `1:00:30` prints `1h30s`. The
/// largest unit wins, so a week prints `1w` and not `7d`.
fn duration(seconds: u64) -> String {
	let weeks = seconds / 604_800;
	let days = (seconds % 604_800) / 86_400;
	let hours = (seconds % 86_400) / 3600;
	let minutes = (seconds % 3600) / 60;
	let remainder = seconds % 60;

	let mut text = String::new();
	if weeks > 0 {
		text.push_str(&format!("{weeks}w"));
	}
	if days > 0 {
		text.push_str(&format!("{days}d"));
	}
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

	fn sentence_of(text: &str) -> String {
		let (resolved, registry) = resolved(text);
		sentence(&resolved, &registry, "heic")
	}

	#[test]
	fn a_sentence_lists_the_actions_in_the_order_they_run() {
		assert_eq!(
			sentence_of("webp+q80+cap1080+rotate90"),
			"convert from heic to webp with quality at 80, then cap to 1080, then rotate 90 degrees"
		);
	}

	#[test]
	fn a_sentence_without_actions_names_the_formats() {
		assert_eq!(sentence_of("jpg"), "convert from heic to jpg");
	}

	#[test]
	fn a_kept_source_says_so() {
		assert_eq!(
			sentence_of("heic,webp+q80"),
			"keep the source heic as it is; convert from heic to webp with quality at 80"
		);
	}

	#[test]
	fn every_output_gets_a_clause() {
		assert_eq!(
			sentence_of("jpg,webp+q80"),
			"convert from heic to jpg; convert from heic to webp with quality at 80"
		);
	}

	#[test]
	fn asking_is_part_of_the_sentence() {
		assert_eq!(sentence_of("jpg!"), "convert from heic to jpg, without asking");
		assert_eq!(sentence_of("jpg+ask"), "convert from heic to jpg, after asking");
	}

	#[test]
	fn an_operation_without_a_phrase_still_reads() {
		let registry = Registry::load(
			"\
version = 'retype-config-1'

[formats.webp]
family = 'image'
lossy = true
alpha = true
tier = 'converted'

[ops.blur]
slot = 20
formats = ['webp']
param = { class = 'int', min = 1, max = 100 }
",
		)
		.expect("loads");

		let intent = intent::parse("webp+blur5").expect("scans");
		let resolved = resolve::resolve(
			&intent,
			Source {
				format: "heic",
				is_directory: false,
			},
			&registry,
		)
		.expect("resolves");

		assert_eq!(
			sentence(&resolved, &registry, "heic"),
			"convert from heic to webp with blur5",
			"the token stands in for a missing phrase"
		);
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
	fn tokens_keep_the_order_they_were_written_in() {
		assert_eq!(canonical("webp+nometa+q80+cap1000"), "webp+nometa+quality80+cap1000");
		assert_eq!(canonical("webp+cap1000+q80"), "webp+cap1000+quality80");
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
		assert_eq!(duration(604_800), "1w");
		assert_eq!(duration(90_000), "1d1h");
		assert_eq!(duration(1_814_400), "3w");
		assert_eq!(duration(150), "2m30s");
		assert_eq!(duration(90), "1m30s");
		assert_eq!(duration(60), "1m");
		assert_eq!(duration(0), "0s");
	}

	#[test]
	fn a_size_prints_in_the_largest_unit_that_reads_naturally() {
		assert_eq!(size(5_000_000), "5mb");
		assert_eq!(size(2_000_000_000), "2gb");
		assert_eq!(size(1_500_000), "1.5mb");
		assert_eq!(size(1_234), "1.234kb");
		assert_eq!(size(1_234_500), "1234.5kb");
		assert_eq!(size(500_000), "500kb");
		assert_eq!(size(0), "0 bytes");
		assert_eq!(size(1), "1 byte");
		assert_eq!(size(999), "999 bytes");
		assert_eq!(size(1_000), "1kb");
	}

	#[test]
	fn a_binary_count_prints_in_binary_units() {
		assert_eq!(size(1_024), "1kib");
		assert_eq!(size(1_536), "1.5kib");
		assert_eq!(size(1_048_576), "1mib");
		assert_eq!(size(1_073_741_824), "1gib");
	}

	#[test]
	fn a_count_that_reads_naturally_in_neither_keeps_the_larger_unit() {
		assert_eq!(size(1_024_000), "1.024mb");
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
