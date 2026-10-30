//! `retype watch` and `retype unwatch`, `docs/cli.md` §4.
//!
//! Registration only: the file a directory is recorded in is the daemon's, and
//! the daemon's watcher is what acts on it. Nothing here starts a process, so a
//! command that registers is a command that can be scripted.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use retype_core::watches;

use crate::error::Result;

pub fn run(state_dir: &Path, directories: &[String], recursive: bool, json: bool) -> Result<ExitCode> {
	if directories.is_empty() {
		return list(state_dir, json);
	}

	for directory in directories {
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

	Ok(ExitCode::SUCCESS)
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

/// What the directory's policy file allows, `docs/config.md` §9. A directory
/// with none converts everything it can.
fn summary(directory: &Path) -> String {
	let Ok(text) = std::fs::read_to_string(directory.join(".retype.toml")) else {
		return "all files".to_string();
	};

	match text
		.parse::<toml::Table>()
		.ok()
		.and_then(|table| table.get("patterns").and_then(toml::Value::as_array).cloned())
	{
		Some(patterns) => {
			let patterns: Vec<String> = patterns
				.iter()
				.filter_map(|pattern| pattern.as_str().map(str::to_string))
				.collect();

			match patterns.is_empty() {
				true => "all files".to_string(),
				false => patterns.join(" "),
			}
		}
		None => "all files".to_string(),
	}
}

/// A registered directory as the daemon needs it: canonical, and nothing else.
pub fn roots(state_dir: &Path) -> Result<Vec<PathBuf>> {
	Ok(watches::load(state_dir)?
		.into_iter()
		.map(|registration| registration.path)
		.collect())
}
