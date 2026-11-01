//! `retype watch` and `retype unwatch`, `docs/cli.md` §4.

use std::fs;
use std::path::Path;
use std::process::Command;

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

fn run(state: &Path, args: &[&str]) -> std::process::Output {
	retype().args(args).arg("--state-dir").arg(state).output().unwrap()
}

#[test]
fn registering_a_directory_records_it() {
	let dir = tempfile::tempdir().unwrap();
	let photos = dir.path().join("photos");
	fs::create_dir(&photos).unwrap();
	let state = dir.path().join("state");

	let output = run(&state, &["watch", photos.to_str().unwrap()]);

	assert_eq!(
		output.status.code(),
		Some(0),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);

	let listing = run(&state, &["watch"]);
	let stdout = String::from_utf8_lossy(&listing.stdout);
	assert!(stdout.contains("photos"), "{stdout}");
	assert!(stdout.contains("recursive"), "{stdout}");

	let file = fs::read_to_string(state.join("watches.toml")).unwrap();
	assert!(file.contains("retype-watches-1"), "{file}");
}

#[test]
fn registering_twice_is_not_an_error_and_keeps_one_entry() {
	let dir = tempfile::tempdir().unwrap();
	let photos = dir.path().join("photos");
	fs::create_dir(&photos).unwrap();
	let state = dir.path().join("state");

	run(&state, &["watch", photos.to_str().unwrap()]);
	let second = run(&state, &["watch", photos.to_str().unwrap()]);

	assert_eq!(second.status.code(), Some(0));

	let file = fs::read_to_string(state.join("watches.toml")).unwrap();
	assert_eq!(file.matches("[[watch]]").count(), 1, "{file}");
}

#[test]
fn a_non_recursive_registration_says_so() {
	let dir = tempfile::tempdir().unwrap();
	let photos = dir.path().join("photos");
	fs::create_dir(&photos).unwrap();
	let state = dir.path().join("state");

	run(&state, &["watch", photos.to_str().unwrap(), "--non-recursive"]);

	let listing = run(&state, &["watch"]);
	assert!(String::from_utf8_lossy(&listing.stdout).contains("non-recursive"));
}

#[test]
fn unwatch_removes_and_is_idempotent() {
	let dir = tempfile::tempdir().unwrap();
	let photos = dir.path().join("photos");
	fs::create_dir(&photos).unwrap();
	let state = dir.path().join("state");

	run(&state, &["watch", photos.to_str().unwrap()]);
	let removed = run(&state, &["unwatch", photos.to_str().unwrap()]);
	assert_eq!(removed.status.code(), Some(0));

	let again = run(&state, &["unwatch", photos.to_str().unwrap()]);
	assert_eq!(again.status.code(), Some(0), "removing something absent succeeds");
	assert!(String::from_utf8_lossy(&again.stderr).contains("was not registered"));

	let listing = run(&state, &["watch"]);
	assert!(listing.stdout.is_empty(), "nothing is registered");
}

#[test]
fn unwatch_needs_a_directory() {
	let dir = tempfile::tempdir().unwrap();

	let output = run(&dir.path().join("state"), &["unwatch"]);

	assert_eq!(output.status.code(), Some(2));
}

#[test]
fn a_directory_that_does_not_exist_is_refused() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");

	let output = run(&state, &["watch", dir.path().join("absent").to_str().unwrap()]);

	assert_eq!(output.status.code(), Some(5), "a missing path is a filesystem answer");
}

#[test]
fn a_policy_file_is_summarised_in_the_listing() {
	let dir = tempfile::tempdir().unwrap();
	let photos = dir.path().join("photos");
	fs::create_dir(&photos).unwrap();
	fs::write(
		photos.join(".retype.toml"),
		"version = 'retype-config-1'\npatterns = ['*.jpg', '*.heic']\n",
	)
	.unwrap();
	let state = dir.path().join("state");

	run(&state, &["watch", photos.to_str().unwrap()]);
	let listing = run(&state, &["watch"]);

	assert!(String::from_utf8_lossy(&listing.stdout).contains("*.jpg *.heic"));
}

/// A directory inside a provider that can hold placeholders is refused by name, and
/// `allow_synced` is the explicit way past it, `docs/watch.md` §5.
///
/// The prefix rule is the macOS one, which is the same code on every platform: what is
/// under `~/Library/CloudStorage` is a provider's mirror wherever the string is
/// assembled.
#[test]
fn a_cloud_mirror_is_refused_unless_allowed() {
	let dir = tempfile::tempdir().unwrap();
	let home = dir.path().join("home");
	let photos = home.join("Library/CloudStorage/OneDrive-Personal/Photos");
	fs::create_dir_all(&photos).unwrap();
	let state = dir.path().join("state");

	let refused = retype()
		.args(["watch", photos.to_str().unwrap()])
		.arg("--state-dir")
		.arg(&state)
		.env("HOME", &home)
		.env("USERPROFILE", &home)
		.env_remove("RETYPE_CONFIG")
		.output()
		.unwrap();

	assert_eq!(refused.status.code(), Some(3), "{refused:?}");
	let stderr = String::from_utf8_lossy(&refused.stderr);
	assert!(stderr.contains("OneDrive-Personal"), "{stderr}");
	assert!(
		!state.join("watches.toml").exists(),
		"a refused registration writes nothing"
	);

	// The same directory, with the key that says the user has checked their provider.
	let config = dir.path().join("retype.toml");
	fs::write(&config, "version = 'retype-config-1'\n\n[watch]\nallow_synced = true\n").unwrap();

	let allowed = retype()
		.args(["watch", photos.to_str().unwrap()])
		.arg("--state-dir")
		.arg(&state)
		.env("HOME", &home)
		.env("USERPROFILE", &home)
		.env("RETYPE_CONFIG", &config)
		.output()
		.unwrap();

	assert_eq!(
		allowed.status.code(),
		Some(0),
		"{}",
		String::from_utf8_lossy(&allowed.stderr)
	);
	assert!(state.join("watches.toml").exists(), "the registration was written");
}

#[test]
fn unwatching_a_path_that_does_not_exist_succeeds() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");

	let output = retype()
		.args(["unwatch"])
		.arg(dir.path().join("gone"))
		.args(["--state-dir", state.to_str().unwrap()])
		.output()
		.unwrap();

	assert_eq!(
		output.status.code(),
		Some(0),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
}

#[test]
fn pause_and_resume_are_state_that_the_listing_shows() {
	let dir = tempfile::tempdir().unwrap();
	let photos = dir.path().join("photos");
	fs::create_dir(&photos).unwrap();
	let state = dir.path().join("state");

	let paused = run(&state, &["watch", photos.to_str().unwrap(), "--pause"]);
	assert_eq!(
		paused.status.code(),
		Some(0),
		"{}",
		String::from_utf8_lossy(&paused.stderr)
	);
	assert!(String::from_utf8_lossy(&run(&state, &["watch"]).stdout).contains("paused"));

	let resumed = run(&state, &["watch", photos.to_str().unwrap(), "--resume"]);
	assert_eq!(
		resumed.status.code(),
		Some(0),
		"{}",
		String::from_utf8_lossy(&resumed.stderr)
	);
	let listing = String::from_utf8_lossy(&run(&state, &["watch"]).stdout).to_string();
	assert!(!listing.contains("paused"), "{listing}");
}

#[test]
fn a_policy_pause_refuses_resume() {
	let dir = tempfile::tempdir().unwrap();
	let photos = dir.path().join("photos");
	fs::create_dir(&photos).unwrap();
	fs::write(
		photos.join(".retype.toml"),
		"version = 'retype-config-1'\npaused = true\n",
	)
	.unwrap();
	let state = dir.path().join("state");

	let resumed = run(&state, &["watch", photos.to_str().unwrap(), "--resume"]);

	assert_eq!(resumed.status.code(), Some(3));
	assert!(String::from_utf8_lossy(&resumed.stderr).contains("paused = true"));
}

#[test]
fn a_vanished_registration_can_still_be_removed() {
	let dir = tempfile::tempdir().unwrap();
	let photos = dir.path().join("photos");
	fs::create_dir(&photos).unwrap();
	let state = dir.path().join("state");

	run(&state, &["watch", photos.to_str().unwrap()]);
	fs::remove_dir(&photos).unwrap();

	assert!(String::from_utf8_lossy(&run(&state, &["watch"]).stdout).contains("missing"));

	let removed = run(&state, &["unwatch", photos.to_str().unwrap()]);
	assert_eq!(
		removed.status.code(),
		Some(0),
		"{}",
		String::from_utf8_lossy(&removed.stderr)
	);
	assert!(run(&state, &["watch"]).stdout.is_empty());
}
