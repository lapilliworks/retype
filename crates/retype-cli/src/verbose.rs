//! How much a command says about what it is doing, `docs/cli.md` §8.
//!
//! One count, because a user asking for detail wants detail rather than a filter
//! language. `RETYPE_LOG` sets the same number, so a wrapper script can turn it on
//! without rewriting the arguments it passes.

use std::sync::atomic::{AtomicU8, Ordering};

static LEVEL: AtomicU8 = AtomicU8::new(0);

/// Take the level from `-v` and `RETYPE_LOG`; the higher of the two wins.
///
/// `--quiet` wins over both: it means nothing below errors, and a note is below
/// errors.
pub fn start(repeats: u8, quiet: bool) {
	if quiet {
		LEVEL.store(0, Ordering::Relaxed);
		return;
	}

	let configured = std::env::var_os("RETYPE_LOG").and_then(|value| parse(&value.to_string_lossy()));
	LEVEL.store(repeats.max(configured.unwrap_or_default()), Ordering::Relaxed);
}

fn parse(text: &str) -> Option<u8> {
	match text.trim().to_ascii_lowercase().as_str() {
		"" | "0" | "error" | "warn" => Some(0),
		"1" | "info" => Some(1),
		"2" | "debug" => Some(2),
		"3" | "trace" => Some(3),
		other => other.parse().ok(),
	}
}

pub fn level() -> u8 {
	LEVEL.load(Ordering::Relaxed)
}

/// One line about what the command decided, shown from `-v`.
pub fn note(message: impl AsRef<str>) {
	if level() >= 1 {
		eprintln!("retype: {}", message.as_ref());
	}
}

/// The same, for things that are only worth reading while something is wrong.
pub fn detail(message: impl AsRef<str>) {
	if level() >= 2 {
		eprintln!("retype: {}", message.as_ref());
	}
}

#[cfg(test)]
mod tests {
	use super::parse;

	#[test]
	fn a_level_is_a_number_or_a_word() {
		assert_eq!(parse("1"), Some(1));
		assert_eq!(parse("debug"), Some(2));
		assert_eq!(parse("  trace "), Some(3));
		assert_eq!(parse("error"), Some(0));
		assert_eq!(parse("nonsense"), None);
	}
}
