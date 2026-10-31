//! `retype watch` and `retype unwatch`, `docs/cli.md` §4.
//!
//! Registration only: the file a directory is recorded in is the daemon's, and
//! the daemon's watcher is what acts on it. Nothing here starts a process, so a
//! command that registers is a command that can be scripted.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use retype_core::watches;

use crate::error::Result;

pub fn run(
	state_dir: &Path,
	directories: &[String],
	recursive: bool,
	json: bool,
	allow_synced: bool,
) -> Result<ExitCode> {
	if directories.is_empty() {
		return list(state_dir, json);
	}

	let mut refused = false;

	for directory in directories {
		// A directory can take itself out of use, and policy beats state: registering it
		// anyway would be undone the next time the file is read, `docs/config.md` §12.
		let policy = retype_core::policy::Policy::open(Path::new(directory))?;

		if policy.paused {
			eprintln!(
				"retype: E_WATCH_PAUSED_BY_POLICY: {} sets paused = true, so {directory} stays unwatched",
				policy.path.display()
			);
			refused = true;
			continue;
		}

		// A provider that can leave a file as a placeholder is a network filesystem with
		// a proprietary protocol: converting inside one triggers downloads, so the
		// registration is refused and the provider is named, `docs/watch.md` §5.
		let provider = match allow_synced {
			true => None,
			false => retype_core::synced::provider(Path::new(directory)),
		};

		if let Some(provider) = provider {
			eprintln!(
				"retype: E_USAGE_ARGS: {directory} is inside {provider}, which can hold placeholders instead of files; \
				 support for that is a separate stage, and [watch] allow_synced = true overrides this"
			);
			refused = true;
			continue;
		}

		let registration = watches::register(state_dir, Path::new(directory), recursive)?;

		if json {
			println!(
				"{}",
				serde_json::json!({
					"v": "retype-cli-json-1",
					"path": registration.path.display().to_string(),
					"recursive": registration.recursive,
					"added": registration.added,
				})
			);
		} else {
			println!(
				"{:<40}{}",
				registration.path.display().to_string(),
				describe(registration.recursive)
			);
		}
	}

	Ok(match refused {
		true => ExitCode::from(3),
		false => ExitCode::SUCCESS,
	})
}

pub fn unwatch(state_dir: &Path, directories: &[String], json: bool) -> Result<ExitCode> {
	if directories.is_empty() {
		eprintln!("retype: E_USAGE_ARGS: unwatch needs at least one directory");
		return Ok(ExitCode::from(2));
	}

	for directory in directories {
		let removed = watches::unregister(state_dir, Path::new(directory))?;

		if json {
			println!(
				"{}",
				serde_json::json!({
					"v": "retype-cli-json-1",
					"path": directory,
					"removed": removed,
				})
			);
		} else if !removed {
			eprintln!("retype: {} was not registered", directory);
		}
	}

	Ok(ExitCode::SUCCESS)
}

fn list(state_dir: &Path, json: bool) -> Result<ExitCode> {
	let registrations = watches::load(state_dir)?;

	for registration in &registrations {
		match json {
			true => println!(
				"{}",
				serde_json::json!({
					"path": registration.path.display().to_string(),
					"recursive": registration.recursive,
					"added": registration.added,
					"conversions": summary(&registration.path),
				})
			),
			false => println!(
				"{:<40}{:<12}{}",
				registration.path.display().to_string(),
				describe(registration.recursive),
				summary(&registration.path)
			),
		}
	}

	Ok(ExitCode::SUCCESS)
}

fn describe(recursive: bool) -> &'static str {
	match recursive {
		true => "recursive",
		false => "this directory",
	}
}

/// What the directory's policy file allows, `docs/config.md` §12. A directory with
/// none converts everything it can.
fn summary(directory: &Path) -> String {
	match retype_core::policy::Policy::open(directory) {
		Ok(policy) if policy.paused => "paused by its policy".to_string(),
		Ok(policy) if policy.patterns.is_empty() => "all files".to_string(),
		Ok(policy) => policy.patterns.join(" "),
		Err(error) => format!("unreadable policy: {error}"),
	}
}

/// A registered directory as the daemon needs it: canonical, and nothing else.
pub fn roots(state_dir: &Path) -> Result<Vec<PathBuf>> {
	Ok(watches::load(state_dir)?
		.into_iter()
		.map(|registration| registration.path)
		.collect())
}
