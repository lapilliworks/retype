//! `retype tools`, `docs/cli.md` §5.
//!
//! The command surface is here and the trust machinery is in `retype-core`'s
//! `managed` module. A network distribution endpoint and the embedded public key
//! are release decisions, so this build accepts an offline pack and names the
//! missing piece instead of guessing at one.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Subcommand;
use retype_core::error::Code;
use retype_core::managed::{self, Artifact, Installed, InstalledArtifact, Manifest};
use retype_core::registry::Registry;
use retype_core::tools as declared;
use serde_json::json;

use crate::error::Result;
use crate::output;

pub use retype_core::tools::use_managed;

#[derive(Subcommand)]
pub enum Action {
	/// Show declared tools, installed artifacts, or what the manifest offers
	List {
		/// Only what this machine installed
		#[arg(long)]
		installed: bool,

		/// Only what the manifest offers for this platform and architecture
		#[arg(long)]
		available: bool,
	},

	/// Install a tool from a signed manifest
	Install {
		/// The tool, optionally with @variant
		tool: String,

		/// A directory or archive holding the manifest and artifacts
		#[arg(long, value_name = "PATH")]
		offline: Option<PathBuf>,
	},

	/// Update the managed tools
	Update {
		/// The tool, optionally with @variant. Absent means everything installed
		tool: Option<String>,
	},

	/// Recheck installed artifacts against the cached manifest
	Verify {
		/// The tool to check. Absent means everything installed
		tool: Option<String>,
	},
}

struct Paths {
	root: PathBuf,
	manifest: PathBuf,
	installed: PathBuf,
	sequence: PathBuf,
	cache: PathBuf,
}

impl Paths {
	fn new(state_dir: &Path) -> Self {
		let root = state_dir.join("tools");
		Self {
			manifest: root.join("manifest.toml"),
			installed: root.join("installed.toml"),
			sequence: root.join("sequence"),
			cache: root.join("cache"),
			root,
		}
	}
}

pub fn run(registry: &Registry, state_dir: &Path, action: &Action, json: bool, quiet: bool) -> Result<ExitCode> {
	let paths = Paths::new(state_dir);

	match action {
		Action::List { installed, available } => list(registry, &paths, *installed, *available, json),
		Action::Install { tool, offline } => install(registry, &paths, tool, offline.as_deref(), json, quiet),
		Action::Update { tool } => update(registry, &paths, tool.as_deref(), json, quiet),
		Action::Verify { tool } => verify(&paths, tool.as_deref(), json, quiet),
	}
}

fn list(registry: &Registry, paths: &Paths, installed: bool, available: bool, json: bool) -> Result<ExitCode> {
	if installed && available {
		return Ok(ExitCode::from(output::fail(
			&Code::UsageConflict,
			"--installed and --available answer different questions; choose one",
		)));
	}

	if available {
		let manifest = match Manifest::open(&paths.manifest) {
			Ok(manifest) => manifest,
			Err(_) => {
				return Ok(ExitCode::from(output::fail(
					&Code::IoNotFound,
					format!(
						"no manifest at {}; run `retype tools install --offline <dir>` first",
						paths.manifest.display()
					),
				)));
			}
		};

		let artifacts: Vec<&Artifact> = manifest
			.artifact
			.iter()
			.filter(|artifact| artifact.platform == managed::platform() && artifact.arch == managed::arch())
			.collect();

		if !json {
			println!("{:<12}{:<10}{:<12}{:<10}URL", "TOOL", "VARIANT", "VERSION", "ARCH");
		}

		for artifact in artifacts {
			if json {
				println!(
					"{}",
					json!({
						"kind": "available",
						"tool": artifact.tool,
						"variant": artifact.variant,
						"version": artifact.version,
						"platform": artifact.platform,
						"arch": artifact.arch,
						"url": artifact.url,
						"sha256": artifact.sha256,
						"size": artifact.size,
					})
				);
			} else {
				println!(
					"{:<12}{:<10}{:<12}{:<10}{}",
					artifact.tool, artifact.variant, artifact.version, artifact.arch, artifact.url
				);
			}
		}

		return Ok(ExitCode::SUCCESS);
	}

	if installed {
		let set = Installed::open(&paths.installed)?;

		if !json {
			println!("{:<12}{:<10}{:<12}{:<10}PATH", "TOOL", "VARIANT", "VERSION", "ARCH");
		}

		for artifact in &set.artifact {
			if json {
				println!(
					"{}",
					json!({
						"kind": "installed",
						"tool": artifact.tool,
						"variant": artifact.variant,
						"version": artifact.version,
						"platform": artifact.platform,
						"arch": artifact.arch,
						"path": artifact.path.display().to_string(),
						"sha256": artifact.sha256,
					})
				);
			} else {
				println!(
					"{:<12}{:<10}{:<12}{:<10}{}",
					artifact.tool,
					artifact.variant,
					artifact.version,
					artifact.arch,
					artifact.path.display()
				);
			}
		}

		return Ok(ExitCode::SUCCESS);
	}

	let declared: Vec<declared::Tool> = registry.tools().cloned().collect();

	if !json {
		println!(
			"{:<12}{:<10}{:<12}{:<18}SOURCE",
			"TOOL", "COMMAND", "VERSION", "FINGERPRINT"
		);
	}

	for status in declared::probe_all(&declared) {
		let path = status.path.as_ref().map(|path| path.display().to_string());
		let source = match &path {
			Some(path) if declared::is_managed(Path::new(path)) => "managed",
			Some(_) => "system",
			None => "missing",
		};

		if json {
			println!(
				"{}",
				json!({
					"kind": "tool",
					"name": status.tool.name,
					"source": source,
					"command": status.command,
					"path": path,
					"version": status.version.as_ref().map(ToString::to_string),
					"fingerprint": status.fingerprint(),
					"usable": status.is_usable(),
					"missing_components": status.missing_components,
				})
			);
		} else {
			println!(
				"{:<12}{:<10}{:<12}{:<18}{}",
				status.tool.name,
				status.command,
				status
					.version
					.as_ref()
					.map(ToString::to_string)
					.unwrap_or_else(|| status.summary()),
				status
					.fingerprint()
					.map(|value| value.chars().take(16).collect::<String>())
					.unwrap_or_else(|| "none".to_string()),
				source
			);
		}
	}

	Ok(ExitCode::SUCCESS)
}

fn install(
	registry: &Registry,
	paths: &Paths,
	written: &str,
	offline: Option<&Path>,
	json: bool,
	quiet: bool,
) -> Result<ExitCode> {
	let (tool, variant) = split(written);

	if registry.tool(tool).is_none() {
		return Ok(ExitCode::from(output::fail(
			&Code::ToolMissing,
			format!("'{tool}' is not a declared tool"),
		)));
	}

	let Some(offline) = offline else {
		// The network path needs a distribution endpoint and an embedded key; both
		// are release decisions, so the refusal names the path that works now.
		return Ok(ExitCode::from(output::fail(
			&Code::ToolMissing,
			"this build has no toolchain distribution endpoint; pass --offline <dir> with a signed manifest and artifacts",
		)));
	};

	let pack = Pack::open(offline)?;
	let bytes = fs::read(&pack.manifest)?;
	let text = String::from_utf8_lossy(&bytes).to_string();
	let manifest = Manifest::parse(&text, &pack.manifest.display().to_string())?;
	let accepted = accepted(&paths.sequence)?;

	manifest.check_freshness(accepted)?;
	check_signature(&manifest, &bytes, &pack.directory)?;

	let Some(artifact) = manifest.artifact(tool, variant) else {
		return Ok(ExitCode::from(output::fail(
			&Code::ToolMissing,
			format!(
				"'{}' has no artifact for {} {}",
				written,
				managed::platform(),
				managed::arch()
			),
		)));
	};

	let source = pack.artifact(artifact)?;
	let cached = managed::cache(artifact, &source, &paths.cache)?;
	let installed_path = managed::unpack(&cached, &paths.root, &artifact.tool)?;

	let mut set = Installed::open(&paths.installed)?;
	set.sequence = manifest.sequence;
	set.record(InstalledArtifact {
		tool: artifact.tool.clone(),
		variant: artifact.variant.clone(),
		version: artifact.version.clone(),
		platform: artifact.platform.clone(),
		arch: artifact.arch.clone(),
		sha256: artifact.sha256.clone(),
		cache: cached,
		path: installed_path.clone(),
	});
	set.save(&paths.installed)?;
	// The signature covers the exact bytes, so the manifest is copied rather than
	// reserialised: a formatting difference would invalidate a valid signature.
	fs::copy(&pack.manifest, &paths.manifest)?;
	fs::copy(pack.directory.join("manifest.sig"), paths.root.join("manifest.sig"))?;
	fs::write(&paths.sequence, manifest.sequence.to_string())?;

	if json {
		output::success(json!({
			"tool": artifact.tool,
			"variant": artifact.variant,
			"version": artifact.version,
			"path": installed_path.display().to_string(),
			"sha256": artifact.sha256,
		}));
	} else if !quiet {
		println!("{} {}  {}", artifact.tool, artifact.version, installed_path.display());
	}

	Ok(ExitCode::SUCCESS)
}

fn update(registry: &Registry, paths: &Paths, written: Option<&str>, json: bool, quiet: bool) -> Result<ExitCode> {
	let set = Installed::open(&paths.installed)?;

	let wanted = match written {
		Some(written) => vec![split(written)],
		None => set
			.artifact
			.iter()
			.map(|artifact| {
				(
					artifact.tool.as_str(),
					(!artifact.variant.is_empty()).then_some(artifact.variant.as_str()),
				)
			})
			.collect(),
	};

	if wanted.is_empty() {
		return Ok(ExitCode::from(output::fail(
			&Code::ToolMissing,
			"nothing is installed; run `retype tools install --offline <dir>` first",
		)));
	}

	let manifest = Manifest::open(&paths.manifest)?;

	for (tool, variant) in wanted {
		let Some(artifact) = manifest.artifact(tool, variant) else {
			return Ok(ExitCode::from(output::fail(
				&Code::ToolMissing,
				format!("no manifest artifact for {tool}"),
			)));
		};

		let Some(held) = set.get(tool, variant) else {
			return Ok(ExitCode::from(output::fail(
				&Code::ToolMissing,
				format!("{tool} is not installed"),
			)));
		};

		let hash = managed::sha256_file(&held.cache)?;
		if !hash.eq_ignore_ascii_case(&artifact.sha256) {
			return Ok(ExitCode::from(output::fail(
				&Code::ToolVersion,
				format!("{} no longer matches the manifest hash", held.cache.display()),
			)));
		}

		let installed_path = managed::unpack(&held.cache, &paths.root, &artifact.tool)?;

		if json {
			output::success(json!({
				"tool": artifact.tool,
				"version": artifact.version,
				"path": installed_path.display().to_string(),
			}));
		} else if !quiet {
			println!("{} {}  {}", artifact.tool, artifact.version, installed_path.display());
		}
	}

	let _ = registry;

	Ok(ExitCode::SUCCESS)
}

fn verify(paths: &Paths, written: Option<&str>, json: bool, quiet: bool) -> Result<ExitCode> {
	let set = Installed::open(&paths.installed)?;
	let manifest = Manifest::open(&paths.manifest).ok();
	let key = std::env::var("RETYPE_TOOLS_PUBLIC_KEY").ok();

	let wanted: Vec<&InstalledArtifact> = match written {
		Some(written) => {
			let (tool, variant) = split(written);
			match set.get(tool, variant) {
				Some(artifact) => vec![artifact],
				None => {
					return Ok(ExitCode::from(output::fail(
						&Code::ToolMissing,
						format!("{written} is not installed"),
					)));
				}
			}
		}
		None => set.artifact.iter().collect(),
	};

	if wanted.is_empty() {
		if !quiet {
			eprintln!("retype: nothing is installed");
		}

		return Ok(ExitCode::SUCCESS);
	}

	let mut failed = false;

	for artifact in wanted {
		let mut problems = Vec::new();

		match managed::sha256_file(&artifact.cache) {
			Ok(hash) if hash.eq_ignore_ascii_case(&artifact.sha256) => {}
			Ok(hash) => problems.push(format!("cache hashes to {hash}, expected {}", artifact.sha256)),
			Err(error) => problems.push(format!("cannot read {}: {error}", artifact.cache.display())),
		}

		if !artifact.path.exists() {
			problems.push(format!("{} is missing", artifact.path.display()));
		}

		// A cached manifest is rechecked when it is present. The signature is only
		// checked when this machine has a key; the release embeds one, and a source
		// build can supply `RETYPE_TOOLS_PUBLIC_KEY` for the same check.
		if let Some(manifest) = &manifest {
			if manifest.sequence < set.sequence {
				problems.push(format!(
					"the cached manifest sequence {} is older than the accepted {}",
					manifest.sequence, set.sequence
				));
			}

			if let Some(key) = &key {
				let signature = paths.root.join("manifest.sig");
				let bytes = fs::read(&paths.manifest)?;
				if let Err(error) = manifest.verify_signature(&bytes, &signature, key) {
					problems.push(error.to_string());
				}
			}
		}

		if json {
			println!(
				"{}",
				json!({
					"kind": "verify",
					"tool": artifact.tool,
					"variant": artifact.variant,
					"version": artifact.version,
					"ok": problems.is_empty(),
					"problems": problems,
					"signature": match key.is_some() {
						true => "checked",
						false => "unchecked: no public key in this build",
					},
				})
			);
		} else {
			if problems.is_empty() {
				println!("{} {}  ok", artifact.tool, artifact.version);
			} else {
				eprintln!("retype: {} {}:", artifact.tool, artifact.version);
				for problem in &problems {
					eprintln!("  {problem}");
				}
			}
		}

		failed |= !problems.is_empty();
	}

	if failed {
		return Ok(ExitCode::from(output::fail(
			&Code::ToolVersion,
			"one or more managed artifacts no longer match the manifest",
		)));
	}

	if key.is_none() && !quiet && !json {
		eprintln!("retype: signature unchecked: this build has no toolchain public key");
	}

	Ok(ExitCode::SUCCESS)
}

/// A tool name and its optional variant, `tool@variant`.
fn split(written: &str) -> (&str, Option<&str>) {
	match written.split_once('@') {
		Some((tool, variant)) => (tool, Some(variant)),
		None => (written, None),
	}
}

fn accepted(path: &Path) -> Result<u64> {
	match fs::read_to_string(path) {
		Ok(text) => Ok(text.trim().parse().unwrap_or_default()),
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
		Err(error) => Err(error.into()),
	}
}

fn check_signature(manifest: &Manifest, bytes: &[u8], directory: &Path) -> Result<()> {
	let Some(key) = std::env::var("RETYPE_TOOLS_PUBLIC_KEY").ok() else {
		return Err(retype_core::error::Error::Diagnostic(
			retype_core::error::Diagnostic::global(
				Code::ToolMissing,
				"this build has no toolchain public key; the release embeds one, and a source build can set RETYPE_TOOLS_PUBLIC_KEY",
			),
		)
		.into());
	};

	let signature = directory.join("manifest.sig");
	if !signature.exists() {
		return Err(
			retype_core::error::Error::Diagnostic(retype_core::error::Diagnostic::global(
				Code::ToolVersion,
				format!("{} has no manifest signature", directory.display()),
			))
			.into(),
		);
	}

	manifest.verify_signature(bytes, &signature, &key)?;
	Ok(())
}

/// The directory or archive an offline pack names.
struct Pack {
	directory: PathBuf,
	manifest: PathBuf,
	/// Held so the temporary extraction lives as long as the pack.
	_temporary: Option<tempfile::TempDir>,
}

impl Pack {
	fn open(path: &Path) -> Result<Self> {
		if path.is_dir() {
			return Ok(Self {
				directory: path.to_path_buf(),
				manifest: path.join("manifest.toml"),
				_temporary: None,
			});
		}

		let temporary = tempfile::tempdir()?;
		managed::extract(path, temporary.path())?;
		let manifest = temporary.path().join("manifest.toml");

		Ok(Self {
			directory: temporary.path().to_path_buf(),
			manifest,
			_temporary: Some(temporary),
		})
	}

	fn artifact(&self, artifact: &Artifact) -> Result<PathBuf> {
		let name = artifact
			.url
			.rsplit('/')
			.next()
			.filter(|name| !name.is_empty())
			.unwrap_or(&artifact.tool);

		let path = self.directory.join(name);
		if path.exists() {
			return Ok(path);
		}

		// A pack may name the file after the artifact rather than after the URL.
		let fallback = self.directory.join(format!(
			"{}-{}-{}-{}-{}",
			artifact.tool, artifact.variant, artifact.version, artifact.platform, artifact.arch
		));
		if fallback.exists() {
			return Ok(fallback);
		}

		Err(
			retype_core::error::Error::Diagnostic(retype_core::error::Diagnostic::global(
				Code::IoNotFound,
				format!("{} does not hold {name}", self.directory.display()),
			))
			.into(),
		)
	}
}
