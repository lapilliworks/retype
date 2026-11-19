/* Debug-only process-interruption checkpoints; never authorize or relax cleanup. */

#[cfg(not(debug_assertions))]
compile_error!("the test-checkpoints feature requires debug assertions");

pub const ROOT_ENV: &str = "RETYPE_TEST_RECOVERY_ROOT";
pub const TOKEN_ENV: &str = "RETYPE_TEST_RECOVERY_TOKEN";
pub const CHECKPOINT_ENV: &str = "RETYPE_TEST_RECOVERY_CHECKPOINT";
pub const CHECKPOINT_FILE: &str = ".retype-test-checkpoint";
pub const FIXTURE_PREFIX: &str = "retype-recovery-test-";
pub const MARKER: &str = ".retype-recovery-test";

fn allows(root: &std::path::Path, token: &str, path: &std::path::Path) -> std::io::Result<bool> {
	use std::io::Read;

	if !root.is_absolute() || crate::job::JobId::parse(token).is_err() {
		return Ok(false);
	}
	let root = root.canonicalize()?;
	let temporary = std::env::temp_dir().canonicalize()?;
	let Some(name) = root.file_name().and_then(|name| name.to_str()) else {
		return Ok(false);
	};
	if root.parent() != Some(temporary.as_path()) || !name.starts_with(FIXTURE_PREFIX) {
		return Ok(false);
	}
	let mut options = std::fs::OpenOptions::new();
	options.read(true);
	let marker = crate::safety::open_guarded_file(&root.join(MARKER), options)?;
	if marker.metadata()?.len() > 128 {
		return Ok(false);
	}
	let mut recorded = String::new();
	marker.take(129).read_to_string(&mut recorded)?;
	if recorded != token {
		return Ok(false);
	}
	let path = crate::fsx::entry_path(path)?;
	Ok(["photo.HEIC", "photo.jpg", "photo.tar", "photo.zip", "photo.tar.gz"]
		.into_iter()
		.any(|name| path == root.join(name) || path == root.join("photos").join(name)))
}

/// Pause only an explicitly selected operation inside a token-marked synthetic fixture.
#[doc(hidden)]
pub fn checkpoint(name: &str, path: &std::path::Path) -> std::io::Result<()> {
	use std::io::{self, Write};
	use std::time::{Duration, Instant};

	let Some(requested) = std::env::var_os(CHECKPOINT_ENV) else {
		return Ok(());
	};
	let Some(filename) = path.file_name().and_then(|name| name.to_str()) else {
		return Ok(());
	};
	let selected = format!("{name}:{filename}");
	if requested != selected.as_str() {
		return Ok(());
	}
	let root = std::env::var_os(ROOT_ENV).ok_or_else(|| io::Error::other("test recovery root is missing"))?;
	let token = std::env::var(TOKEN_ENV).map_err(io::Error::other)?;
	let root = std::path::PathBuf::from(root);
	#[cfg(windows)]
	let _directory = crate::fsx::windows::Directory::open(&root)?;
	if !allows(&root, &token, path)? {
		return Err(io::Error::new(
			io::ErrorKind::PermissionDenied,
			"checkpoint is outside its fixture",
		));
	}
	let mut marker = std::fs::OpenOptions::new()
		.write(true)
		.create_new(true)
		.open(root.join(CHECKPOINT_FILE))?;
	marker.write_all(selected.as_bytes())?;
	drop(marker);
	let deadline = Instant::now() + Duration::from_secs(120);
	while Instant::now() < deadline {
		std::thread::sleep(Duration::from_millis(20));
	}
	Err(io::Error::new(
		io::ErrorKind::TimedOut,
		"test checkpoint was not interrupted",
	))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn checkpoints_are_limited_to_their_named_synthetic_files() {
		let root = tempfile::Builder::new().prefix(FIXTURE_PREFIX).tempdir().unwrap();
		let token = crate::job::JobId::now().unwrap().as_str().to_string();
		std::fs::write(root.path().join(MARKER), &token).unwrap();
		std::fs::create_dir(root.path().join("photos")).unwrap();
		for path in [
			root.path().join("photo.HEIC"),
			root.path().join("photos/photo.jpg"),
			root.path().join("photo.tar"),
			root.path().join("photo.zip"),
			root.path().join("photo.tar.gz"),
			root.path().join("photos/photo.tar"),
			root.path().join("photos/photo.zip"),
			root.path().join("photos/photo.tar.gz"),
		] {
			assert!(allows(root.path(), &token, &path).unwrap());
		}
		for path in [
			root.path().join("other.HEIC"),
			root.path().join("photos/other.jpg"),
			root.path().join("other.tar.gz"),
			root.path().parent().unwrap().join("photo.HEIC"),
			root.path().parent().unwrap().join("photo.tar"),
		] {
			assert!(!allows(root.path(), &token, &path).unwrap());
		}
		assert!(!allows(root.path(), "invalid", &root.path().join("photo.HEIC")).unwrap());
		let replacement = if token.ends_with('0') { '1' } else { '0' };
		let other = format!("{}{replacement}", &token[..token.len() - 1]);
		assert!(!allows(root.path(), &other, &root.path().join("photo.HEIC")).unwrap());
		let nested = root.path().join(format!("{FIXTURE_PREFIX}nested"));
		std::fs::create_dir(&nested).unwrap();
		std::fs::write(nested.join(MARKER), &token).unwrap();
		assert!(!allows(&nested, &token, &nested.join("photo.HEIC")).unwrap());
	}
}
