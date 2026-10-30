//! External tool declarations and detection.
//!
//! Mirrors the support matrix in `docs/tools.md`: a capability belongs to a
//! tool version with the right components present. The declarations live in
//! `builtin.toml` beside the formats and operations, because a tool is
//! configuration like any other and a user may add or override one. Only the
//! probing stays here, since it is the one part that needs a process.
//!
//! Detection is a preflight, never a runtime decision about what a plan may do:
//! the planner reads the configuration, and this reports what the configuration
//! will find.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A tool as declared under `[tools]`, `docs/config.md` §3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tool {
	pub name: String,
	pub command: String,
	pub probe: Vec<String>,
	pub version_min: Option<Version>,
	/// Why the product wants it, shown by `retype doctor`.
	pub purpose: String,
	/// Substrings the probe output has to mention for the claimed formats to
	/// work, e.g. `heic` for a build of ImageMagick without libheif.
	pub components: Vec<String>,
	/// Argv template with `{input}`, `{ops}` and `{output}`.
	pub layout: Vec<String>,
	/// Named alternatives resolved first-present, `docs/tools.md` §4.1.
	pub engines: BTreeMap<String, Vec<String>>,
}

impl Tool {
	/// Candidates for an engine, empty when the tool declares no such engine.
	pub fn candidates(&self, engine: &str) -> &[String] {
		self.engines.get(engine).map(Vec::as_slice).unwrap_or_default()
	}
}

/// A dotted numeric version, comparable in the obvious way.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(Vec<u32>);

impl Version {
	/// First dotted numeric run in a line, e.g. `9.0.2` out of
	/// `ffmpeg version 9.0.2-4 Copyright`.
	pub fn parse(text: &str) -> Option<Self> {
		text.split_whitespace().find_map(|word| {
			let candidate: String = word.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
			let trimmed = candidate.trim_end_matches('.');
			if !trimmed.contains('.') {
				return None;
			}
			let parts: Vec<u32> = trimmed.split('.').filter_map(|p| p.parse().ok()).collect();
			(!parts.is_empty()).then_some(Self(parts))
		})
	}
}

impl fmt::Display for Version {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let parts: Vec<String> = self.0.iter().map(u32::to_string).collect();
		f.write_str(&parts.join("."))
	}
}

/// Outcome of probing one tool.
#[derive(Debug)]
pub struct ToolStatus<'a> {
	pub tool: &'a Tool,
	pub path: Option<PathBuf>,
	pub version: Option<Version>,
	/// Components named by the declaration that the probe output did not mention.
	pub missing_components: Vec<String>,
	pub error: Option<String>,
}

impl ToolStatus<'_> {
	/// Present, new enough, and carrying every component the declaration
	/// requires. The version floor is part of usability rather than a separate
	/// question, since a tool below it behaves like a missing one.
	pub fn is_usable(&self) -> bool {
		self.path.is_some()
			&& self.version.is_some()
			&& self.missing_components.is_empty()
			&& self.error.is_none()
			&& self.meets_minimum()
	}

	pub fn meets_minimum(&self) -> bool {
		match (&self.version, &self.tool.version_min) {
			(Some(version), Some(minimum)) => version >= minimum,
			(Some(_), None) => true,
			(None, _) => false,
		}
	}

	pub fn summary(&self) -> String {
		match (&self.path, &self.version) {
			(Some(_), Some(version)) => {
				let mut line = version.to_string();
				if !self.missing_components.is_empty() {
					line.push_str(&format!("  missing components: {}", self.missing_components.join(", ")));
				}
				line
			}
			(None, _) => "not found".to_string(),
			(Some(_), None) => "version unknown".to_string(),
		}
	}
}

/// Probe one tool.
pub fn probe(tool: &Tool) -> ToolStatus<'_> {
	let path = find_in_path(&std::env::var_os("PATH").unwrap_or_default(), &tool.command);

	let Some(path) = path else {
		return ToolStatus {
			tool,
			path: None,
			version: None,
			missing_components: tool.components.clone(),
			error: None,
		};
	};

	match Command::new(&path).args(&tool.probe).output() {
		Ok(output) => {
			let stdout = String::from_utf8_lossy(&output.stdout);
			let stderr = String::from_utf8_lossy(&output.stderr);
			let text = format!("{stdout}{stderr}");

			// The version line is not always first: `7z` prints a blank line
			// and a banner before it.
			let version = text.lines().find_map(Version::parse);
			let lowered = text.to_lowercase();
			let missing_components = tool
				.components
				.iter()
				.filter(|component| !lowered.contains(&component.to_lowercase()))
				.cloned()
				.collect();

			ToolStatus {
				tool,
				path: Some(path),
				version,
				missing_components,
				error: None,
			}
		}
		Err(error) => ToolStatus {
			tool,
			path: Some(path),
			version: None,
			missing_components: tool.components.clone(),
			error: Some(error.to_string()),
		},
	}
}

pub fn probe_all(tools: &[Tool]) -> Vec<ToolStatus<'_>> {
	tools.iter().map(probe).collect()
}

/// First candidate of a named engine that is on `PATH`, `docs/tools.md` §4.1.
pub fn resolve_engine<'a>(tool: &'a Tool, engine: &str) -> Option<&'a str> {
	let path_env = std::env::var_os("PATH").unwrap_or_default();
	tool.candidates(engine)
		.iter()
		.map(String::as_str)
		.find(|candidate| find_in_path(&path_env, candidate).is_some())
}

/// Look a command up in a `PATH` value. Split out so it is testable without
/// mutating the process environment, which edition 2024 makes unsafe.
pub fn find_in_path(path_env: &OsStr, command: &str) -> Option<PathBuf> {
	let names: Vec<String> = if cfg!(windows) {
		vec![format!("{command}.exe"), command.to_string()]
	} else {
		vec![command.to_string()]
	};

	std::env::split_paths(path_env)
		.filter(|dir| !dir.as_os_str().is_empty())
		.flat_map(|dir| names.iter().map(move |name| dir.join(name)))
		.find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
	if !path.is_file() {
		return false;
	}
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		path.metadata()
			.map(|meta| meta.permissions().mode() & 0o111 != 0)
			.unwrap_or(false)
	}
	#[cfg(not(unix))]
	{
		true
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn tool(command: &str, version_min: Option<&str>) -> Tool {
		Tool {
			name: command.to_string(),
			command: command.to_string(),
			probe: vec!["-version".to_string()],
			version_min: version_min.map(|version| Version::parse(version).expect("a parsable floor")),
			purpose: "testing".to_string(),
			components: Vec::new(),
			layout: vec!["{input}".to_string(), "{ops}".to_string(), "{output}".to_string()],
			engines: BTreeMap::new(),
		}
	}

	#[test]
	fn version_from_imagemagick() {
		let line = "Version: ImageMagick 7.1.2-34 Q16 x86_64 20261014 https://imagemagick.org";
		assert_eq!(Version::parse(line).unwrap().to_string(), "7.1.2");
	}

	#[test]
	fn version_from_ffmpeg() {
		let line = "ffmpeg version 9.0.2-4 Copyright (c) 2000-2026 the FFmpeg developers";
		assert_eq!(Version::parse(line).unwrap().to_string(), "9.0.2");
	}

	#[test]
	fn version_from_sevenzip() {
		assert_eq!(
			Version::parse("7-Zip 26.05 (x64) : Copyright (c) 1999-2026 Igor Pavlov : 2026-10-21")
				.unwrap()
				.to_string(),
			"26.5"
		);
	}

	#[test]
	fn version_from_pandoc() {
		assert_eq!(Version::parse("pandoc 3.11").unwrap().to_string(), "3.11");
	}

	#[test]
	fn version_absent() {
		assert!(Version::parse("no digits here").is_none());
		assert!(Version::parse("").is_none());
		assert!(Version::parse("build 2026").is_none());
	}

	#[test]
	fn version_ordering() {
		let a = Version::parse("7.1.2").unwrap();
		let b = Version::parse("7.10").unwrap();
		let c = Version::parse("9.0.2").unwrap();
		assert!(a < b);
		assert!(b < c);
	}

	#[test]
	fn a_floor_is_part_of_usability() {
		let tool = tool("magick", Some("7.1"));
		let mut status = ToolStatus {
			tool: &tool,
			path: Some(PathBuf::from("/usr/bin/magick")),
			version: Some(Version::parse("7.1.2").unwrap()),
			missing_components: Vec::new(),
			error: None,
		};
		assert!(status.is_usable());

		status.version = Some(Version::parse("6.9.11").unwrap());
		assert!(!status.meets_minimum());
		assert!(!status.is_usable());

		status.version = Some(Version::parse("7.1.2").unwrap());
		status.missing_components = vec!["heic".to_string()];
		assert!(!status.is_usable());
	}

	#[test]
	fn a_tool_without_a_floor_accepts_what_it_finds() {
		let tool = tool("pandoc", None);
		let status = ToolStatus {
			tool: &tool,
			path: Some(PathBuf::from("/usr/bin/pandoc")),
			version: Some(Version::parse("3.11").unwrap()),
			missing_components: Vec::new(),
			error: None,
		};

		assert!(status.meets_minimum());
		assert!(status.is_usable());
	}

	#[test]
	fn path_lookup_ignores_empty_entries() {
		let dir = tempfile::tempdir().unwrap();
		// Lookup tries the `.exe` suffix first on Windows, so the fixture has to
		// carry it there.
		let name = if cfg!(windows) { "probe-tool.exe" } else { "probe-tool" };
		let script = dir.path().join(name);
		std::fs::write(&script, b"#!/bin/sh\nexit 0\n").unwrap();
		make_executable(&script);

		// The separator is platform specific, so build the value the same way
		// the platform would, with an empty entry in the middle of it.
		let path_env = std::env::join_paths([Path::new(""), Path::new("/nonexistent"), dir.path()]).unwrap();

		assert!(find_in_path(&path_env, "probe-tool").is_some());
		assert!(find_in_path(&path_env, "absent-tool").is_none());
	}

	#[cfg(unix)]
	#[test]
	fn non_executable_file_is_not_found() {
		let dir = tempfile::tempdir().unwrap();
		let script = dir.path().join("probe-tool");
		std::fs::write(&script, b"not executable").unwrap();

		let path_env = std::env::join_paths([dir.path()]).unwrap();
		assert!(find_in_path(&path_env, "probe-tool").is_none());
	}

	#[cfg(unix)]
	fn make_executable(path: &Path) {
		use std::os::unix::fs::PermissionsExt;
		let mut permissions = std::fs::metadata(path).unwrap().permissions();
		permissions.set_mode(0o755);
		std::fs::set_permissions(path, permissions).unwrap();
	}

	#[cfg(not(unix))]
	fn make_executable(_path: &Path) {}
}
