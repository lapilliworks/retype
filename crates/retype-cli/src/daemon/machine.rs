//! Per-install identity, `docs/state.md` §2.
//!
//! It lives in `bench.toml` beside the calibration, because the two travel
//! together: calibration earned on one machine is not calibration for another.
//! The estimator lands with real parsing; until then this reads and writes the
//! one key it owns.

use std::fs;
use std::path::Path;

use crate::error::{Error, Result};

const KEY: &str = "machine";

pub fn load_or_create(state_dir: &Path) -> Result<String> {
	let path = state_dir.join("bench.toml");

	if let Ok(text) = fs::read_to_string(&path) {
		if let Some(identity) = find(&text) {
			return Ok(identity);
		}
	}

	let identity = random_hex()?;
	let mut text = fs::read_to_string(&path).unwrap_or_default();

	if !text.is_empty() && !text.ends_with('\n') {
		text.push('\n');
	}
	text.push_str(&format!("{KEY} = \"{identity}\"\n"));

	fs::write(&path, text)?;
	set_private(&path)?;
	Ok(identity)
}

fn find(text: &str) -> Option<String> {
	for line in text.lines() {
		let Some((key, value)) = line.split_once('=') else {
			continue;
		};
		if key.trim() != KEY {
			continue;
		}

		let value = value.trim().trim_matches('"');
		if value.len() == 8 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
			return Some(value.to_ascii_lowercase());
		}
	}

	None
}

#[cfg(unix)]
fn set_private(path: &Path) -> Result<()> {
	use std::os::unix::fs::PermissionsExt;
	fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
	Ok(())
}

#[cfg(not(unix))]
fn set_private(_path: &Path) -> Result<()> {
	Ok(())
}

fn random_hex() -> Result<String> {
	let mut bytes = [0u8; 4];
	getrandom::fill(&mut bytes).map_err(|error| Error::Core(retype_core::error::Error::Random(error.to_string())))?;
	Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_identity_is_created_once_and_reused() {
		let dir = tempfile::tempdir().unwrap();

		let first = load_or_create(dir.path()).unwrap();
		let second = load_or_create(dir.path()).unwrap();

		assert_eq!(first, second);
		assert_eq!(first.len(), 8);
		assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
	}

	#[cfg(unix)]
	#[test]
	fn the_identity_file_is_not_world_readable() {
		use std::os::unix::fs::PermissionsExt;

		let dir = tempfile::tempdir().unwrap();
		load_or_create(dir.path()).unwrap();
		let mode = fs::metadata(dir.path().join("bench.toml"))
			.unwrap()
			.permissions()
			.mode() & 0o777;

		assert_eq!(mode, 0o600);
	}

	#[test]
	fn an_existing_bench_file_is_appended_to_not_replaced() {
		let dir = tempfile::tempdir().unwrap();
		fs::write(dir.path().join("bench.toml"), "[video]\nmegapixels_per_second = 12.0\n").unwrap();

		let identity = load_or_create(dir.path()).unwrap();
		let text = fs::read_to_string(dir.path().join("bench.toml")).unwrap();

		assert!(text.contains("megapixels_per_second"), "calibration survived: {text}");
		assert!(text.contains(&format!("machine = \"{identity}\"")));
	}
}
