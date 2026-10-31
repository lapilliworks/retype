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
	/// Alternate executable names, tried in order when `command` is absent.
	/// An absolute `command` pins the choice and ignores these.
	pub aliases: Vec<String>,
	pub probe: Vec<String>,
	pub version_min: Option<Version>,
	/// Why the product wants it, shown by `retype doctor`.
	pub purpose: String,
	/// Substrings the probe output has to mention for the claimed formats to
	/// work, e.g. `heic` for a build of ImageMagick without libheif.
	pub components: Vec<String>,
	/// The families this tool can carry, `docs/config.md` §3. Empty means it makes
	/// no claim and is eligible for anything, which is what a user-written tool
	/// gets; the shipped tools state theirs, because a matrix that offers ImageMagick
	/// for a video container is a claim that is simply false.
	pub families: Vec<String>,
	/// Argv template with `{input}`, `{ops}` and `{output}`.
	pub layout: Vec<String>,
	/// Named alternatives resolved first-present, `docs/tools.md` §5.1.
	pub engines: BTreeMap<String, Vec<String>>,
}

impl Tool {
	/// Candidates for an engine, empty when the tool declares no such engine.
	pub fn candidates(&self, engine: &str) -> &[String] {
		self.engines.get(engine).map(Vec::as_slice).unwrap_or_default()
	}

	/// Executable names to try, preferred first, `docs/config.md` §3. An
	/// absolute command is a pin, so it is the only candidate.
	pub fn commands(&self) -> impl Iterator<Item = &str> {
		let aliases: &[String] = if Path::new(&self.command).is_absolute() {
			&[]
		} else {
			&self.aliases
		};

		std::iter::once(self.command.as_str()).chain(aliases.iter().map(String::as_str))
	}
}

/// A dotted numeric version: the parts it compares by, and the text it was read
/// from, because `26.05` is not the same string as `26.5` to the person reading
/// it even though the two are the same version.
#[derive(Debug, Clone)]
pub struct Version {
	parts: Vec<u32>,
	written: String,
}

impl PartialEq for Version {
	fn eq(&self, other: &Self) -> bool {
		self.parts == other.parts
	}
}

impl Eq for Version {}

impl PartialOrd for Version {
	fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
		Some(self.cmp(other))
	}
}

impl Ord for Version {
	fn cmp(&self, other: &Self) -> std::cmp::Ordering {
		self.parts.cmp(&other.parts)
	}
}

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
			(!parts.is_empty()).then_some(Self {
				parts,
				written: trimmed.to_string(),
			})
		})
	}
}

impl fmt::Display for Version {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(&self.written)
	}
}

/// Outcome of probing one tool.
#[derive(Debug)]
pub struct ToolStatus<'a> {
	pub tool: &'a Tool,
	/// The candidate that answered, usable or not.
	pub command: String,
	pub path: Option<PathBuf>,
	pub version: Option<Version>,
	/// Components named by the declaration that the probe output did not mention.
	pub missing_components: Vec<String>,
	/// The probe's own text, kept so a build can be told from another build of the
	/// same version, `docs/tools.md` §2.3.
	pub output: String,
	pub error: Option<String>,
}

impl ToolStatus<'_> {
	/// Whether the version has to be known at all. A tool that declares neither a
	/// floor nor a probe makes no version claim, so `cp` or a local script is
	/// usable without one.
	fn needs_version(&self) -> bool {
		self.tool.version_min.is_some() || !self.tool.probe.is_empty()
	}

	/// Present, new enough, and carrying every component the declaration
	/// requires. The version floor is part of usability rather than a separate
	/// question, since a tool below it behaves like a missing one.
	pub fn is_usable(&self) -> bool {
		self.path.is_some()
			&& (!self.needs_version() || self.version.is_some())
			&& self.missing_components.is_empty()
			&& self.error.is_none()
			&& self.meets_minimum()
	}

	/// The executable a usable tool answers to, or `None` when it is unusable.
	pub fn executable(&self) -> Option<&str> {
		self.is_usable().then_some(self.command.as_str())
	}

	/// What this build is, as far as its own probe can say: a hash of the probe
	/// output, which carries the version, the configure line and the delegate list.
	///
	/// Not a security hash. It answers "is this the build a claim was earned with",
	/// and a tool that lies about its own version can lie about this too; the
	/// managed toolchain replaces it with an artifact hash, `docs/tools.md` §2.3.
	pub fn fingerprint(&self) -> Option<String> {
		(!self.output.is_empty()).then(|| fnv1a_128(&self.output))
	}

	pub fn meets_minimum(&self) -> bool {
		match &self.tool.version_min {
			None => true,
			Some(minimum) => self.version.as_ref().is_some_and(|version| version >= minimum),
		}
	}

	pub fn summary(&self) -> String {
		match (&self.path, &self.version) {
			(Some(_), Some(version)) => {
				let mut line = version.to_string();
				if self.command != self.tool.command {
					line.push_str(&format!(" via {}", self.command));
				}
				if !self.missing_components.is_empty() {
					line.push_str(&format!("  missing components: {}", self.missing_components.join(", ")));
				}
				line
			}
			(None, _) => "not found".to_string(),
			(Some(_), None) if self.needs_version() => "version unknown".to_string(),
			(Some(_), None) => "present".to_string(),
		}
	}
}

/// Probe one tool, taking the first candidate that is present and usable.
pub fn probe(tool: &Tool) -> ToolStatus<'_> {
	probe_with(tool, &std::env::var_os("PATH").unwrap_or_default())
}

fn probe_with<'a>(tool: &'a Tool, path_env: &OsStr) -> ToolStatus<'a> {
	let mut fallback = None;

	for command in tool.commands() {
		let status = probe_command(tool, command, path_env);
		if status.is_usable() {
			return status;
		}
		if fallback.is_none() && status.path.is_some() {
			fallback = Some(status);
		}
	}

	fallback.unwrap_or_else(|| not_found(tool))
}

fn probe_command<'a>(tool: &'a Tool, command: &str, path_env: &OsStr) -> ToolStatus<'a> {
	let Some(path) = find_in_path(path_env, command) else {
		return ToolStatus {
			tool,
			command: command.to_string(),
			path: None,
			version: None,
			missing_components: tool.components.clone(),
			output: String::new(),
			error: None,
		};
	};

	// A tool that declares no probe is found, not run. Running a command to see
	// whether it is usable is how a plan came to execute the very converter it
	// was planning, and a converter is entitled to do work when started,
	// `docs/tools.md` §3.
	if tool.probe.is_empty() {
		return ToolStatus {
			tool,
			command: command.to_string(),
			path: Some(path),
			version: None,
			// A component claim needs output to check it against, so a tool that
			// makes one without a probe stays unusable until it declares one.
			missing_components: tool.components.clone(),
			output: String::new(),
			error: None,
		};
	}

	match Command::new(&path).args(&tool.probe).output() {
		Ok(output) => {
			let stdout = String::from_utf8_lossy(&output.stdout);
			let stderr = String::from_utf8_lossy(&output.stderr);
			let text = format!("{stdout}{stderr}");

			// The version line is not always first: `7zz` prints a blank line
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
				command: command.to_string(),
				path: Some(path),
				version,
				missing_components,
				output: keep_output(&text),
				error: None,
			}
		}
		Err(error) => ToolStatus {
			tool,
			command: command.to_string(),
			path: Some(path),
			version: None,
			missing_components: tool.components.clone(),
			output: String::new(),
			error: Some(error.to_string()),
		},
	}
}

fn not_found(tool: &Tool) -> ToolStatus<'_> {
	ToolStatus {
		tool,
		command: tool.command.clone(),
		path: None,
		version: None,
		missing_components: tool.components.clone(),
		output: String::new(),
		error: None,
	}
}

/// The name this build looks for when a container needs a tool, `docs/tools.md` §7.
///
/// Not a guess at a command: it is the declaration in the shipped configuration, and a
/// machine with `7z` or `7za` resolves through that tool's aliases.
pub const SEVEN_ZIP: &str = "7zz";

/// How much probe output is kept. A delegate list is a few kilobytes; a tool that
/// prints more than this is not describing itself and is not worth remembering
/// past the version line.
const OUTPUT_KEEP: usize = 16 * 1024;

fn keep_output(text: &str) -> String {
	match text.len() > OUTPUT_KEEP {
		true => text.chars().take(OUTPUT_KEEP).collect(),
		false => text.to_string(),
	}
}

/// FNV-1a, 128 bits: the build identity a probe can report, `docs/tools.md` §2.3.
fn fnv1a_128(text: &str) -> String {
	const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
	const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;

	let mut hash = OFFSET;
	for byte in text.as_bytes() {
		hash ^= u128::from(*byte);
		hash = hash.wrapping_mul(PRIME);
	}

	format!("{hash:032x}")
}

/// This machine, as the planner sees it: presence and engine resolution through
/// the declared probes.
pub struct Machine;

pub fn probe_all(tools: &[Tool]) -> Vec<ToolStatus<'_>> {
	tools.iter().map(probe).collect()
}

/// First candidate of a named engine that is on `PATH`, `docs/tools.md` §5.1.
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
			aliases: Vec::new(),
			probe: vec!["-version".to_string()],
			version_min: version_min.map(|version| Version::parse(version).expect("a parsable floor")),
			purpose: "testing".to_string(),
			components: Vec::new(),
			families: Vec::new(),
			layout: vec!["{input}".to_string(), "{ops}".to_string(), "{output}".to_string()],
			engines: BTreeMap::new(),
		}
	}

	#[test]
	fn a_tool_without_a_probe_is_found_and_not_run() {
		let dir = tempfile::tempdir().unwrap();
		let marker = dir.path().join("ran");
		let script = dir.path().join("converter");
		std::fs::write(&script, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();

		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
		}

		let tool = Tool {
			name: "converter".to_string(),
			command: script.display().to_string(),
			aliases: Vec::new(),
			probe: Vec::new(),
			version_min: None,
			purpose: String::new(),
			components: Vec::new(),
			families: Vec::new(),
			layout: vec!["{input}".to_string()],
			engines: Default::default(),
		};

		let status = probe(&tool);

		assert!(status.is_usable(), "{:?}", status.summary());
		assert!(!marker.exists(), "a tool with no probe was executed at plan time");
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
		let version =
			Version::parse("7-Zip 26.05 (x64) : Copyright (c) 1999-2026 Igor Pavlov : 2026-10-21").expect("a version");

		// Reported as it was written, compared as a number: `26.05` is the release
		// name and `26.5` is what a floor means.
		assert_eq!(version.to_string(), "26.05");
		assert_eq!(version, Version::parse("26.5").unwrap());
		assert!(version >= Version::parse("23.0").unwrap());
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
			command: tool.command.clone(),
			path: Some(PathBuf::from("/usr/bin/magick")),
			version: Some(Version::parse("7.1.2").unwrap()),
			missing_components: Vec::new(),
			output: "ImageMagick 7.1.2-34 Q16 x86_64".to_string(),
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
	fn the_fingerprint_follows_the_probe_output_and_not_the_path() {
		let tool = tool("magick", Some("7.1"));
		let status = |output: &str, path: &str| ToolStatus {
			tool: &tool,
			command: tool.command.clone(),
			path: Some(PathBuf::from(path)),
			version: Version::parse("7.1.2"),
			missing_components: Vec::new(),
			output: output.to_string(),
			error: None,
		};

		let first = status("Delegates: heic webp", "/usr/bin/magick").fingerprint();
		let elsewhere = status("Delegates: heic webp", "/opt/magick").fingerprint();
		let rebuilt = status("Delegates: heic", "/usr/bin/magick").fingerprint();

		assert_eq!(first, elsewhere, "the same build at another path is the same");
		assert_ne!(first, rebuilt, "a build without a delegate is another build");
		assert_eq!(first.as_deref().map(str::len), Some(32));
	}

	#[test]
	fn a_tool_without_a_floor_accepts_what_it_finds() {
		let tool = tool("pandoc", None);
		let status = ToolStatus {
			tool: &tool,
			command: tool.command.clone(),
			path: Some(PathBuf::from("/usr/bin/pandoc")),
			version: Some(Version::parse("3.11").unwrap()),
			missing_components: Vec::new(),
			output: "pandoc 3.11".to_string(),
			error: None,
		};

		assert!(status.meets_minimum());
		assert!(status.is_usable());
	}

	#[test]
	fn a_bare_command_tries_its_aliases_in_order() {
		let mut declared = tool("7zz", None);
		declared.aliases = vec!["7z".to_string(), "7za".to_string()];

		assert_eq!(declared.commands().collect::<Vec<_>>(), ["7zz", "7z", "7za"]);
	}

	#[test]
	fn an_absolute_command_pins_the_executable() {
		let absolute = if cfg!(windows) { r"C:\tools\7zz.exe" } else { "/opt/7zz" };
		let mut declared = tool(absolute, None);
		declared.aliases = vec!["7z".to_string()];

		assert_eq!(declared.commands().collect::<Vec<_>>(), [absolute]);
	}

	#[cfg(unix)]
	#[test]
	fn probing_takes_the_first_usable_alias() {
		let dir = tempfile::tempdir().unwrap();
		let preferred = dir.path().join("probe-tool");
		let alias = dir.path().join("probe-alias");
		std::fs::write(&preferred, b"#!/bin/sh\necho 'probe-tool 1.0'\n").unwrap();
		std::fs::write(&alias, b"#!/bin/sh\necho 'probe-alias 2.0'\n").unwrap();
		make_executable(&preferred);
		make_executable(&alias);

		let mut declared = tool("probe-tool", Some("2.0"));
		declared.aliases = vec!["probe-alias".to_string()];
		let path_env = std::env::join_paths([dir.path()]).unwrap();
		let status = probe_with(&declared, &path_env);

		assert_eq!(status.command, "probe-alias");
		assert_eq!(status.path.as_deref(), Some(alias.as_path()));
		assert!(
			status.is_usable(),
			"the preferred candidate is too old, the alias is not"
		);
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
