//! The spelling rules shared by user-facing identifiers.
//!
//! An identifier this build mints has one canonical form: lowercase ASCII. Input
//! may use ASCII uppercase for convenience and is normalized before parsing, so a
//! handle copied from a transcript still names the same object. Non-ASCII is left
//! alone and the parser rejects it; Unicode case folding is locale-dependent and
//! would let one identifier have several spellings. Secrets are not identifiers
//! and are never normalized, `docs/dev/architecture/state.md` §2.

/// Normalize identifier input to the canonical ASCII lowercase form.
///
/// Only ASCII letters change. Non-ASCII passes through so the parser can reject it
/// rather than applying a locale-dependent fold.
pub fn normalize(raw: &str) -> String {
	raw.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
	use super::normalize;

	#[test]
	fn uppercase_ascii_is_normalized() {
		assert_eq!(normalize("01M6B4RARARX3WrTTKRGPP1783"), "01m6b4rararx3wrttkrgpp1783");
	}

	#[test]
	fn non_ascii_is_left_for_the_parser_to_reject() {
		assert_eq!(normalize("Ä"), "Ä");
	}
}
