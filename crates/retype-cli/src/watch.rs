//! `retype watch` and `retype unwatch`, `docs/cli.md` §4.
//!
//! Registration only: the file a directory is recorded in is the daemon's, and
//! the daemon's watcher is what acts on it. Nothing here starts a process, so a
//! command that registers is a command that can be scripted.
//!
//! When a daemon is already reachable the mutation goes through it; otherwise the
//! command writes the registry itself. That is the one writer rule with an
//! exception for the case where there is no daemon to write it.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use retype_core::error::{Code, code_for_io};
use retype_core::watches;
use serde_json::{Value, json};

use crate::error::Result;
use crate::output;

#[allow(clippy::too_many_arguments)]
pub fn run(
	state_dir: &Path,
	directories: &[String],
	recursive: bool,
	json: bool,
	allow_synced: bool,
	pause: bool,
	resume: bool,
) -> Result<ExitCode> {
	if directories.is_empty() {
		if pause || resume {
			return Ok(ExitCode::from(output::fail(
				&Code::UsageArgs,
				"watch --pause and --resume need at least one directory",
			)));
		}

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

		// A directory can take itself out of use, and policy beats state: resuming it
		// anyway would be undone the next time the file is read, `docs/config.md` §12.
		let policy = retype_core::policy::Policy::open(Path::new(directory))?;

		if policy.paused && !pause {
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
		let provider = match allow_synced || pause || resume {
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

		let paused = match (pause, resume) {
			(true, _) => Some(true),
			(_, true) => Some(false),
			_ => None,
		};
		let path = Path::new(directory);

		match register(state_dir, path, recursive, paused) {
			Ok(registration) => report(&registration, json),
			Err(error) => {
				refused = refused.max(output::fail(&error.code(), error.to_string()));
			}
		}
	}

	Ok(ExitCode::from(refused))
}

/// Ask a running daemon to register, or write the registry when none is there.
fn register(
	state_dir: &Path,
	directory: &Path,
	recursive: bool,
	paused: Option<bool>,
) -> Result<watches::Registration> {
	if let Ok(mut client) = crate::client::connect(state_dir) {
		let answer = client.call(
			"watch.add",
			json!({
				"path": directory.display().to_string(),
				"recursive": recursive,
				"paused": paused,
			}),
		);

		return match answer {
			Ok(value) => Ok(registration_from(&value)),
			Err(retype_ipc::ClientError::Rpc(error)) => {
				Err(crate::error::Error::Client(retype_ipc::ClientError::Rpc(error)))
			}
			Err(other) => Err(crate::error::Error::Client(other)),
		};
	}

	match paused {
		Some(paused) => Ok(watches::set_paused(state_dir, directory, recursive, paused)?),
		None => Ok(watches::register(state_dir, directory, recursive)?),
	}
}

/// The daemon answered with the fields the caller printed anyway; the canonical
/// path is the one useful piece.
fn registration_from(answer: &Value) -> watches::Registration {
	watches::Registration {
		path: answer
			.get("path")
			.and_then(Value::as_str)
			.map(PathBuf::from)
			.unwrap_or_default(),
		recursive: answer.get("recursive").and_then(Value::as_bool).unwrap_or(true),
		added: answer.get("added").and_then(Value::as_i64).unwrap_or_default(),
		paused: answer.get("paused").and_then(Value::as_bool).unwrap_or(false),
	}
}

fn report(registration: &watches::Registration, json: bool) {
	if json {
		output::success(json!({
			"path": registration.path.display().to_string(),
			"recursive": registration.recursive,
			"added": registration.added,
			"paused": registration.paused,
		}));
	} else {
		println!(
			"{:<40}{:<12}{}",
			registration.path.display().to_string(),
			describe(registration.recursive),
			if registration.paused { "paused" } else { "" }
		);
	}
}

pub fn unwatch(state_dir: &Path, directories: &[String], json: bool) -> Result<ExitCode> {
	if directories.is_empty() {
		return Ok(ExitCode::from(output::fail(
			&Code::UsageArgs,
			"unwatch needs at least one directory",
		)));
	}

	for directory in directories {
		let removed = match remove(state_dir, Path::new(directory)) {
			Ok(removed) => removed,
			Err(error) => return Ok(ExitCode::from(output::fail(&error.code(), error.to_string()))),
		};

		if json {
			output::success(json!({
				"path": directory,
				"removed": removed,
			}));
		} else if !removed {
			eprintln!("retype: {} was not registered", directory);
		}
	}

	Ok(ExitCode::SUCCESS)
}

/// The same choice for removal: a reachable daemon owns the registry.
fn remove(state_dir: &Path, directory: &Path) -> Result<bool> {
	if let Ok(mut client) = crate::client::connect(state_dir) {
		let answer = client.call("watch.remove", json!({"path": directory.display().to_string()}))?;
		return Ok(answer.get("removed").and_then(Value::as_bool).unwrap_or(false));
	}

	Ok(watches::unregister(state_dir, directory)?)
}

fn list(state_dir: &Path, json: bool) -> Result<ExitCode> {
	let registrations = watches::load(state_dir)?;

	for registration in &registrations {
		let summary = summary(registration);

		match json {
			true => println!(
				"{}",
				json!({
					"path": registration.path.display().to_string(),
					"recursive": registration.recursive,
					"added": registration.added,
					"paused": registration.paused,
					"available": registration.path.exists(),
					"policy": summary,
				})
			),
			false => println!(
				"{:<40}{:<12}{}",
				registration.path.display().to_string(),
				describe(registration.recursive),
				summary
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
/// none converts everything it can; a missing one is marked rather than silently
/// reading as a working registration.
fn summary(registration: &watches::Registration) -> String {
	if !registration.path.exists() {
		return "missing".to_string();
	}

	if registration.paused {
		return "paused".to_string();
	}

	match retype_core::policy::Policy::open(&registration.path) {
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
