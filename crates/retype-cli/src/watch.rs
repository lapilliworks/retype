//! `retype watch` and `retype unwatch`, `docs/cli.md` §4.
//!
//! Registration only: the file a directory is recorded in is the daemon's, and
//! the daemon's watcher is what acts on it. Nothing here starts a process, so a
//! command that registers is a command that can be scripted.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use retype_core::error::{Code, code_for_io};
use retype_core::watches;

use crate::error::Result;
use crate::output;

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

	let mut refused = 0u8;

	for directory in directories {
		// The unit is a directory, so a file path is a usage error rather than a
		// registration the watcher has to discover it cannot use.
		match std::fs::metadata(directory) {
			Ok(metadata) if !metadata.is_dir() => {
				refused = refused.max(output::fail(
					&Code::UsageArgs,
					format!("{directory} is not a directory"),
				));
				continue;
			}
			Ok(_) => {}
			Err(error) => {
				refused = refused.max(output::fail(&code_for_io(&error), format!("{directory}: {error}")));
				continue;
			}
		}

		// A directory can take itself out of use, and policy beats state: registering it
		// anyway would be undone the next time the file is read, `docs/config.md` §12.
		let policy = retype_core::policy::Policy::open(Path::new(directory))?;

		if policy.paused {
			refused = refused.max(output::fail(
				&Code::WatchPausedByPolicy,
				format!(
					"{} sets paused = true, so {directory} stays unwatched",
					policy.path.display()
				),
			));
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
			// The refusal is a diagnosis about this filesystem rather than a bad
			// argument, so it keeps exit 3 even though the code is the invocation
			// one `docs/watch.md` §5 names.
			let _ = output::fail(
				&Code::UsageArgs,
				format!(
					"{directory} is inside {provider}, which can hold placeholders instead of files; \
					 support for that is a separate stage, and [watch] allow_synced = true overrides this"
				),
			);
			refused = refused.max(3);
			continue;
		}

		let registration = watches::register(state_dir, Path::new(directory), recursive)?;

		if json {
			output::success(serde_json::json!({
				"path": registration.path.display().to_string(),
				"recursive": registration.recursive,
				"added": registration.added,
			}));
		} else {
			println!(
				"{:<40}{}",
				registration.path.display().to_string(),
				describe(registration.recursive)
			);
		}
	}

	Ok(ExitCode::from(refused))
}

pub fn unwatch(state_dir: &Path, directories: &[String], json: bool) -> Result<ExitCode> {
	if directories.is_empty() {
		return Ok(ExitCode::from(output::fail(
			&Code::UsageArgs,
			"unwatch needs at least one directory",
		)));
	}

	for directory in directories {
		let removed = match watches::unregister(state_dir, Path::new(directory)) {
			Ok(removed) => removed,
			Err(error) => return Ok(ExitCode::from(output::fail(&error.code(), error.to_string()))),
		};

		if json {
			output::success(serde_json::json!({
				"path": directory,
				"removed": removed,
			}));
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
					"policy": summary(&registration.path),
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
