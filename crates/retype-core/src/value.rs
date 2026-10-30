//! Value classes, `docs/grammar.md` §7.
//!
//! A token's value means whatever its operation declares. This module reads a
//! written value as the class says, then checks it against the bounds the
//! operation declares, and distinguishes the two failures: a value that is not
//! of the class at all, and one that is of the class but out of range.

use crate::error::Code;
use crate::registry::{Class, Param};

/// A value that has been read successfully.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
	Int(u64),
	Dim {
		width: u32,
		height: u32,
	},
	Ratio {
		width: u32,
		height: u32,
	},
	/// Bytes.
	Size(u64),
	/// Seconds.
	Time(u64),
	Range {
		from: Option<u64>,
		to: Option<u64>,
	},
	/// The declared spelling, not the written one.
	Enum(String),
	/// A language or region tag, case preserved.
	Code(String),
	Text(String),
}

/// Why a value was refused, before it is given a span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fault {
	pub code: Code,
	pub message: String,
}

fn syntax(message: impl Into<String>) -> Fault {
	Fault {
		code: Code::ValueSyntax,
		message: message.into(),
	}
}

fn out_of_range(message: impl Into<String>) -> Fault {
	Fault {
		code: Code::ValueRange,
		message: message.into(),
	}
}

/// Read a written value as its class requires, then check its bounds.
pub fn interpret(param: &Param, text: &str) -> Result<Value, Fault> {
	let value = match param.class {
		Class::Int => Value::Int(number(text)?),
		Class::Dim => {
			let (width, height) = dimensions(text)?;
			if width == 0 && height == 0 {
				return Err(syntax("at most one side may be 0, and only to mean auto"));
			}
			Value::Dim { width, height }
		}
		Class::Ratio => {
			let (width, height) = dimensions(text)?;
			if width == 0 || height == 0 {
				return Err(syntax("an aspect ratio has no zero side"));
			}
			Value::Ratio { width, height }
		}
		Class::Size => Value::Size(size(text)?),
		Class::Time => Value::Time(time(text)?),
		Class::Range => {
			let (from, to) = endpoints(text, param)?;
			if let (Some(from), Some(to)) = (from, to) {
				if from > to {
					return Err(out_of_range(format!("{from} comes after {to}")));
				}
			}
			Value::Range { from, to }
		}
		Class::Enum => {
			let written = text.to_ascii_lowercase();
			let Some(declared) = param.values.iter().find(|value| value.to_ascii_lowercase() == written) else {
				return Err(syntax(format!("'{text}' is not one of {}", param.values.join(", "))));
			};
			Value::Enum(declared.clone())
		}
		Class::Code => Value::Code(code(text)?),
		Class::Text => Value::Text(text_value(text)?),
	};

	bounds(param, &value)?;
	Ok(value)
}

/// The bounds an operation declares, applied to whichever numbers its class
/// produces.
fn bounds(param: &Param, value: &Value) -> Result<(), Fault> {
	let within = |number: u64, what: &str| -> Result<(), Fault> {
		if let Some(min) = param.min {
			if number < min {
				return Err(out_of_range(format!("{what} {number} is below {min}")));
			}
		}
		if let Some(max) = param.max {
			if number > max {
				return Err(out_of_range(format!("{what} {number} is above {max}")));
			}
		}
		Ok(())
	};

	match value {
		Value::Int(number) => within(*number, "a value of")?,
		// A zero side means auto, so it carries no length to measure.
		Value::Dim { width, height } => {
			if *width != 0 {
				within(u64::from(*width), "a width of")?;
			}
			if *height != 0 {
				within(u64::from(*height), "a height of")?;
			}
		}
		Value::Ratio { width, height } => {
			within(u64::from(*width), "a ratio side of")?;
			within(u64::from(*height), "a ratio side of")?;
		}
		Value::Size(bytes) => within(*bytes, "a size of")?,
		Value::Time(seconds) => within(*seconds, "a time of")?,
		Value::Range { from, to } => {
			if let Some(from) = from {
				within(*from, "a range start of")?;
			}
			if let Some(to) = to {
				within(*to, "a range end of")?;
			}
		}
		Value::Enum(_) | Value::Code(_) | Value::Text(_) => {}
	}

	Ok(())
}

/// `1*DIGIT`, leading zeros accepted and not printed.
fn number(text: &str) -> Result<u64, Fault> {
	if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
		return Err(syntax(format!("'{text}' is not a whole number")));
	}

	text.parse()
		.map_err(|_| out_of_range(format!("'{text}' is too large to be a number here")))
}

/// `int "x" int`.
fn dimensions(text: &str) -> Result<(u32, u32), Fault> {
	let mut parts = text.split('x');

	let (Some(width), Some(height), None) = (parts.next(), parts.next(), parts.next()) else {
		return Err(syntax(format!("'{text}' is not written as WxH")));
	};

	let side = |part: &str, name: &str| -> Result<u32, Fault> {
		database_side(part, name).ok_or_else(|| syntax(format!("'{text}' has no whole number for its {name}")))
	};

	Ok((side(width, "width")?, side(height, "height")?))
}

fn database_side(part: &str, _name: &str) -> Option<u32> {
	if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
		return None;
	}

	part.parse().ok()
}

/// `decimal ( "kb" / "mib" / "gb" )` and friends: decimal and binary units are
/// different quantities, `docs/grammar.md` §7.
///
/// `mb` is a thousand kilobytes and `mib` is a thousand and twenty-four kibibytes,
/// which is four and a half percent more. The two spellings are separate on
/// purpose: a byte budget of 1 MiB is not a budget of 1 MB, and a user who writes
/// one means it.
///
/// A fraction is allowed here and nowhere else, because there is no byte unit to
/// fall back on: without decimals, 1234 bytes has no spelling at all. What a
/// literal cannot be is a fraction of a byte, and `1.2345kb` is one.
fn size(text: &str) -> Result<u64, Fault> {
	let lower = text.to_ascii_lowercase();

	for (unit, scale) in UNITS {
		let Some(number) = lower.strip_suffix(unit) else {
			continue;
		};

		let (whole, fraction) = match number.split_once('.') {
			Some((_, "")) => return Err(syntax(format!("'{text}' ends at the decimal point"))),
			Some((whole, fraction)) => (whole, fraction),
			None => (number, ""),
		};

		let Some(whole) = number_of(whole) else {
			return Err(syntax(format!("'{text}' is not a number")));
		};

		let mut bytes = u128::from(whole)
			.checked_mul(scale)
			.ok_or_else(|| out_of_range(format!("'{text}' is larger than a byte count can hold")))?;

		if !fraction.is_empty() {
			if fraction.len() > 9 {
				return Err(syntax(format!(
					"'{text}' has more decimal places than a byte count allows"
				)));
			}

			let Some(digits) = number_of(fraction) else {
				return Err(syntax(format!("'{text}' is not a number")));
			};

			let denominator = 10u128.pow(fraction.len() as u32);
			let scaled = u128::from(digits) * scale;

			if scaled % denominator != 0 {
				return Err(syntax(format!("'{text}' is a fraction of a byte")));
			}

			bytes += scaled / denominator;
		}

		return u64::try_from(bytes)
			.map_err(|_| out_of_range(format!("'{text}' is larger than a byte count can hold")));
	}

	Err(syntax(format!(
		"'{text}' has no unit: use kb, mb or gb, or kib, mib or gib"
	)))
}

/// Decimal and binary units, longest spelling first so that `mib` is never read
/// as `ib` beside `mb`.
const UNITS: [(&str, u128); 6] = [
	("gib", 1024 * 1024 * 1024),
	("mib", 1024 * 1024),
	("kib", 1024),
	("gb", 1_000_000_000),
	("mb", 1_000_000),
	("kb", 1_000),
];

fn number_of(text: &str) -> Option<u64> {
	if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
		return None;
	}

	text.parse().ok()
}

/// A duration, `docs/grammar.md` §7: units in order with the ones between them
/// optional, or a clock.
///
/// `1h30s` needs no `0m`, because nobody says "one hour, zero minutes, thirty
/// seconds". The clock form is the other way round: every place down to seconds
/// is present, so `1:30` is ninety seconds and an hour and a half has to be
/// written `1:30:00`.
fn time(text: &str) -> Result<u64, Fault> {
	if text.contains(':') {
		return clock(text);
	}

	tagged(text)
}

fn tagged(text: &str) -> Result<u64, Fault> {
	let mut characters = text.chars().peekable();
	let mut parts: Vec<(u64, Option<char>)> = Vec::new();

	while characters.peek().is_some() {
		let mut digits = String::new();
		while let Some(character) = characters.peek() {
			if character.is_ascii_digit() {
				digits.push(*character);
				characters.next();
			} else {
				break;
			}
		}

		if digits.is_empty() {
			return Err(syntax(format!("'{text}' is not a duration")));
		}

		let unit = match characters.next() {
			Some(unit @ ('h' | 'm' | 's')) => Some(unit),
			Some(_) => return Err(syntax(format!("'{text}' has something other than h, m or s"))),
			None => None,
		};

		parts.push((number(&digits)?, unit));
	}

	if let [(seconds, None)] = parts.as_slice() {
		return Ok(*seconds);
	}

	if parts.iter().any(|(_, unit)| unit.is_none()) {
		return Err(syntax(format!("'{text}' ends with a number that has no unit")));
	}

	let mut total = 0u64;
	let mut rank = 4;

	for (number, unit) in &parts {
		let (this, seconds) = match unit {
			Some('h') => (3, number * 3600),
			Some('m') => (2, number * 60),
			Some('s') => (1, *number),
			_ => unreachable!("units were checked above"),
		};

		if this >= rank {
			return Err(syntax(format!("'{text}' repeats or reorders its units")));
		}

		rank = this;
		total += seconds;
	}

	Ok(total)
}

fn clock(text: &str) -> Result<u64, Fault> {
	let parts: Vec<&str> = text.split(':').collect();

	let field = |part: &str, name: &str, two_digits: bool| -> Result<u64, Fault> {
		if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
			return Err(syntax(format!("'{text}' has no number for its {name}")));
		}
		if two_digits && part.len() != 2 {
			return Err(syntax(format!("'{text}' writes its {name} with two digits")));
		}

		part.parse()
			.map_err(|_| out_of_range(format!("'{text}' has a {name} too large to be a number")))
	};

	let below_sixty = |value: u64, name: &str| -> Result<u64, Fault> {
		if value > 59 {
			return Err(out_of_range(format!("'{text}' has {value} for its {name}")));
		}
		Ok(value)
	};

	match parts.as_slice() {
		[minutes, seconds] => {
			let minutes = field(minutes, "minutes", false)?;
			let seconds = below_sixty(field(seconds, "seconds", true)?, "seconds")?;
			Ok(minutes * 60 + seconds)
		}
		[hours, minutes, seconds] => {
			let hours = field(hours, "hours", false)?;
			let minutes = below_sixty(field(minutes, "minutes", true)?, "minutes")?;
			let seconds = below_sixty(field(seconds, "seconds", true)?, "seconds")?;
			Ok(hours * 3600 + minutes * 60 + seconds)
		}
		_ => Err(syntax(format!(
			"'{text}' is written as M:S or H:M:S, with every place present"
		))),
	}
}

/// `scalar`, `scalar-scalar`, `scalar-`, `-scalar`, where a scalar is an integer
/// or a duration as the operation declares.
fn endpoints(text: &str, param: &Param) -> Result<(Option<u64>, Option<u64>), Fault> {
	let scalar = |part: &str| -> Result<u64, Fault> {
		match param.scalar {
			Some(crate::registry::Scalar::Time) => time(part),
			_ => number(part),
		}
	};

	match text.split_once('-') {
		None => Ok((Some(scalar(text)?), Some(scalar(text)?))),
		Some(("", to)) => Ok((None, Some(scalar(to)?))),
		Some((from, "")) => Ok((Some(scalar(from)?), None)),
		Some((from, to)) => Ok((Some(scalar(from)?), Some(scalar(to)?))),
	}
}

/// `2*3ALPHA *( "-" 1*8ALNUM )`, a BCP 47 subset with its case preserved.
fn code(text: &str) -> Result<String, Fault> {
	let mut parts = text.split('-');
	let Some(primary) = parts.next() else {
		return Err(syntax(format!("'{text}' is not a language tag")));
	};

	if !(2..=3).contains(&primary.len()) || !primary.bytes().all(|byte| byte.is_ascii_alphabetic()) {
		return Err(syntax(format!(
			"'{text}' does not begin with a two or three letter tag"
		)));
	}

	for part in parts {
		if part.is_empty() || part.len() > 8 || !part.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
			return Err(syntax(format!(
				"'{text}' has a subtag that is not 1 to 8 letters or digits"
			)));
		}
	}

	Ok(text.to_string())
}

/// Free-form preset argument, `1*64( ALPHA / DIGIT / "." / "_" / "-" )`.
fn text_value(text: &str) -> Result<String, Fault> {
	if text.is_empty() || text.chars().count() > 64 {
		return Err(syntax("a preset argument is 1 to 64 characters"));
	}

	if !text
		.bytes()
		.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
	{
		return Err(syntax(format!(
			"'{text}' has a character a preset argument cannot hold"
		)));
	}

	Ok(text.to_string())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::registry::Scalar;

	fn param(class: Class) -> Param {
		Param {
			class,
			min: None,
			max: None,
			values: Vec::new(),
			scalar: None,
		}
	}

	fn bounded(class: Class, min: u64, max: u64) -> Param {
		Param {
			min: Some(min),
			max: Some(max),
			..param(class)
		}
	}

	#[test]
	fn an_integer_is_digits_and_nothing_else() {
		let class = param(Class::Int);

		assert_eq!(interpret(&class, "85"), Ok(Value::Int(85)));
		assert_eq!(
			interpret(&class, "085"),
			Ok(Value::Int(85)),
			"leading zeros are accepted"
		);
		assert_eq!(interpret(&class, "-1").unwrap_err().code, Code::ValueSyntax);
		assert_eq!(interpret(&class, "8.5").unwrap_err().code, Code::ValueSyntax);
		assert_eq!(interpret(&class, "").unwrap_err().code, Code::ValueSyntax);
	}

	#[test]
	fn dimensions_allow_one_auto_side() {
		let class = param(Class::Dim);

		assert_eq!(
			interpret(&class, "800x600"),
			Ok(Value::Dim {
				width: 800,
				height: 600
			})
		);
		assert_eq!(interpret(&class, "0x600"), Ok(Value::Dim { width: 0, height: 600 }));
		assert_eq!(interpret(&class, "800x0"), Ok(Value::Dim { width: 800, height: 0 }));
		assert_eq!(interpret(&class, "0x0").unwrap_err().code, Code::ValueSyntax);
		assert_eq!(interpret(&class, "800").unwrap_err().code, Code::ValueSyntax);
		assert_eq!(interpret(&class, "800x600x2").unwrap_err().code, Code::ValueSyntax);
	}

	#[test]
	fn a_ratio_has_no_auto_side() {
		let class = param(Class::Ratio);

		assert_eq!(interpret(&class, "16x9"), Ok(Value::Ratio { width: 16, height: 9 }));
		assert_eq!(interpret(&class, "0x9").unwrap_err().code, Code::ValueSyntax);
	}

	#[test]
	fn sizes_are_decimal() {
		let class = param(Class::Size);

		assert_eq!(interpret(&class, "5mb"), Ok(Value::Size(5_000_000)));
		assert_eq!(interpret(&class, "2GB"), Ok(Value::Size(2_000_000_000)));
		assert_eq!(interpret(&class, "1500kb"), Ok(Value::Size(1_500_000)));
		assert_eq!(interpret(&class, "5").unwrap_err().code, Code::ValueSyntax);
		assert_eq!(interpret(&class, "5tb").unwrap_err().code, Code::ValueSyntax);
	}

	#[test]
	fn binary_and_decimal_units_are_different_quantities() {
		let class = param(Class::Size);

		assert_eq!(interpret(&class, "1kb"), Ok(Value::Size(1_000)));
		assert_eq!(interpret(&class, "1kib"), Ok(Value::Size(1_024)));
		assert_eq!(interpret(&class, "1mb"), Ok(Value::Size(1_000_000)));
		assert_eq!(interpret(&class, "1mib"), Ok(Value::Size(1_048_576)));
		assert_eq!(interpret(&class, "1gib"), Ok(Value::Size(1_073_741_824)));
		assert_eq!(interpret(&class, "1.5kib"), Ok(Value::Size(1_536)));
	}

	#[test]
	fn a_binary_fraction_must_still_be_a_whole_byte() {
		let class = param(Class::Size);

		assert_eq!(interpret(&class, "0.1kib").unwrap_err().code, Code::ValueSyntax);
	}

	#[test]
	fn a_size_may_have_a_fraction_because_it_is_exact() {
		let class = param(Class::Size);

		assert_eq!(interpret(&class, "1.5mb"), Ok(Value::Size(1_500_000)));
		assert_eq!(interpret(&class, "0.5mb"), Ok(Value::Size(500_000)));
		assert_eq!(interpret(&class, "1.234kb"), Ok(Value::Size(1_234)));
		assert_eq!(interpret(&class, "1234.5kb"), Ok(Value::Size(1_234_500)));
		assert_eq!(interpret(&class, "1.000mb"), Ok(Value::Size(1_000_000)));
	}

	#[test]
	fn a_size_that_is_not_a_whole_number_of_bytes_is_refused() {
		let class = param(Class::Size);

		assert_eq!(interpret(&class, "1.2345kb").unwrap_err().code, Code::ValueSyntax);
		assert_eq!(interpret(&class, ".5mb").unwrap_err().code, Code::ValueSyntax);
		assert_eq!(interpret(&class, "1.mb").unwrap_err().code, Code::ValueSyntax);
	}

	#[test]
	fn a_duration_keeps_its_units_in_order_and_may_skip_one() {
		let class = param(Class::Time);

		assert_eq!(interpret(&class, "90"), Ok(Value::Time(90)));
		assert_eq!(interpret(&class, "90s"), Ok(Value::Time(90)));
		assert_eq!(interpret(&class, "2m30s"), Ok(Value::Time(150)));
		assert_eq!(interpret(&class, "1h2m3s"), Ok(Value::Time(3723)));
		assert_eq!(interpret(&class, "1h2m"), Ok(Value::Time(3720)));
		assert_eq!(interpret(&class, "1h"), Ok(Value::Time(3600)));
		assert_eq!(interpret(&class, "1h30s"), Ok(Value::Time(3630)));
		assert_eq!(interpret(&class, "1h0m30s"), Ok(Value::Time(3630)));
	}

	#[test]
	fn a_duration_may_not_reorder_its_units_or_end_without_one() {
		let class = param(Class::Time);

		for written in ["1h30", "30m1h", "1s2m", "1h1h", "1m1m", "1x"] {
			assert_eq!(
				interpret(&class, written).unwrap_err().code,
				Code::ValueSyntax,
				"{written}"
			);
		}
	}

	#[test]
	fn a_clock_has_every_place_it_needs() {
		let class = param(Class::Time);

		assert_eq!(interpret(&class, "2:30"), Ok(Value::Time(150)));
		assert_eq!(interpret(&class, "1:30"), Ok(Value::Time(90)), "minutes and seconds");
		assert_eq!(interpret(&class, "1:00:30"), Ok(Value::Time(3630)));
		assert_eq!(interpret(&class, "1:30:00"), Ok(Value::Time(5400)));
		assert_eq!(interpret(&class, "100:30"), Ok(Value::Time(6030)));
	}

	#[test]
	fn a_clock_refuses_a_missing_place_or_a_field_out_of_its_range() {
		let class = param(Class::Time);

		for written in [
			"1:5",
			"1:30:5",
			"1:2:3",
			"1::30",
			":30",
			"1:60",
			"1:60:00",
			"1:30:00:00",
		] {
			let error = interpret(&class, written).unwrap_err();
			assert!(
				matches!(error.code, Code::ValueSyntax | Code::ValueRange),
				"{written} was accepted"
			);
		}
	}

	#[test]
	fn a_range_takes_either_end_or_both() {
		let class = param(Class::Range);

		assert_eq!(
			interpret(&class, "10-20"),
			Ok(Value::Range {
				from: Some(10),
				to: Some(20)
			})
		);
		assert_eq!(
			interpret(&class, "-20"),
			Ok(Value::Range {
				from: None,
				to: Some(20)
			})
		);
		assert_eq!(
			interpret(&class, "10-"),
			Ok(Value::Range {
				from: Some(10),
				to: None
			})
		);
		assert_eq!(
			interpret(&class, "10"),
			Ok(Value::Range {
				from: Some(10),
				to: Some(10)
			})
		);
	}

	#[test]
	fn a_range_of_times_reads_its_scalars_as_durations() {
		let mut class = param(Class::Range);
		class.scalar = Some(Scalar::Time);

		assert_eq!(
			interpret(&class, "1m30s-2m"),
			Ok(Value::Range {
				from: Some(90),
				to: Some(120)
			})
		);
		assert_eq!(interpret(&class, "1m1h-2m").unwrap_err().code, Code::ValueSyntax);
		assert_eq!(interpret(&class, "1h30s-2m").unwrap_err().code, Code::ValueRange);
	}

	#[test]
	fn a_range_that_ends_before_it_starts_is_out_of_range() {
		let class = param(Class::Range);

		assert_eq!(interpret(&class, "20-10").unwrap_err().code, Code::ValueRange);
	}

	#[test]
	fn an_enum_matches_case_insensitively_and_returns_the_declared_spelling() {
		let mut class = param(Class::Enum);
		class.values = vec!["90".to_string(), "180".to_string(), "270".to_string()];

		assert_eq!(interpret(&class, "90"), Ok(Value::Enum("90".to_string())));
		assert_eq!(interpret(&class, "180"), Ok(Value::Enum("180".to_string())));
		assert_eq!(interpret(&class, "45").unwrap_err().code, Code::ValueSyntax);
		assert_eq!(interpret(&class, "90deg").unwrap_err().code, Code::ValueSyntax);

		let mut words = param(Class::Enum);
		words.values = vec!["lanczos3".to_string()];
		assert_eq!(interpret(&words, "Lanczos3"), Ok(Value::Enum("lanczos3".to_string())));
	}

	#[test]
	fn a_language_tag_keeps_its_case() {
		let class = param(Class::Code);

		assert_eq!(interpret(&class, "en"), Ok(Value::Code("en".to_string())));
		assert_eq!(interpret(&class, "pt-BR"), Ok(Value::Code("pt-BR".to_string())));
		assert_eq!(interpret(&class, "e").unwrap_err().code, Code::ValueSyntax);
		assert_eq!(interpret(&class, "english").unwrap_err().code, Code::ValueSyntax);
		assert_eq!(interpret(&class, "en-").unwrap_err().code, Code::ValueSyntax);
	}

	#[test]
	fn preset_arguments_are_short_and_plain() {
		let class = param(Class::Text);

		assert_eq!(interpret(&class, "a_b-c.1"), Ok(Value::Text("a_b-c.1".to_string())));
		assert_eq!(interpret(&class, "").unwrap_err().code, Code::ValueSyntax);
		assert_eq!(interpret(&class, "a b").unwrap_err().code, Code::ValueSyntax);
		assert_eq!(interpret(&class, &"a".repeat(65)).unwrap_err().code, Code::ValueSyntax);
	}

	#[test]
	fn bounds_are_checked_after_the_class() {
		let quality = bounded(Class::Int, 1, 100);

		assert_eq!(interpret(&quality, "100"), Ok(Value::Int(100)));
		assert_eq!(interpret(&quality, "101").unwrap_err().code, Code::ValueRange);
		assert_eq!(interpret(&quality, "0").unwrap_err().code, Code::ValueRange);
		assert_eq!(interpret(&quality, "x").unwrap_err().code, Code::ValueSyntax);
	}

	#[test]
	fn an_auto_side_is_not_measured_against_the_bounds() {
		let resize = bounded(Class::Dim, 16, 16384);

		assert_eq!(interpret(&resize, "0x1080"), Ok(Value::Dim { width: 0, height: 1080 }));
		assert_eq!(interpret(&resize, "8x1080").unwrap_err().code, Code::ValueRange);
	}

	#[test]
	fn a_range_checks_both_of_its_ends() {
		let crop = bounded(Class::Range, 10, 20);

		assert_eq!(
			interpret(&crop, "10-20"),
			Ok(Value::Range {
				from: Some(10),
				to: Some(20)
			})
		);
		assert_eq!(interpret(&crop, "5-20").unwrap_err().code, Code::ValueRange);
		assert_eq!(interpret(&crop, "10-25").unwrap_err().code, Code::ValueRange);
	}
}
