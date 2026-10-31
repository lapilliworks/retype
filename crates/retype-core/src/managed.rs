//! The managed toolchain: the signed list of artifacts and what is installed
//! from it, `docs/tools.md` §2 and `docs/cli.md` §5.
//!
//! The manifest is the authority for what may be installed. A file downloaded or
//! copied from an offline pack is accepted only after the manifest signature, the
//! artifact hash and the artifact's identity all agree; a failed check is an
//! environment failure, never a fallback to another tool.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Code, Diagnostic, Error, Result};

/// Schema identifier for the distribution list, `docs/tools.md` §2.1.
pub const TOOLS_VERSION: &str = "retype-tools-1";

/// One artifact the manifest offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
	pub tool: String,
	#[serde(default)]
	pub variant: String,
	pub version: String,
	pub platform: String,
	pub arch: String,
	pub url: String,
	#[serde(default)]
	pub size: u64,
	pub sha256: String,
	#[serde(default)]
	pub components: Vec<String>,
	#[serde(default)]
	pub license: String,
	#[serde(default)]
	pub source: String,
	#[serde(default)]
	pub source_sha256: String,
}

/// The signed distribution list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
	pub version: String,
	pub sequence: u64,
	pub expires: String,
	#[serde(default)]
	pub artifact: Vec<Artifact>,
}

impl Manifest {
	pub fn parse(text: &str, label: &str) -> Result<Self> {
		let manifest: Self = toml::from_str(text)
			.map_err(|error| Error::Diagnostic(Diagnostic::global(Code::CfgParse, format!("{label}: {error}"))))?;

		if manifest.version != TOOLS_VERSION {
			return Err(Error::Diagnostic(Diagnostic::global(
				Code::CfgVersion,
				format!(
					"{label} declares '{}', this build reads '{TOOLS_VERSION}'",
					manifest.version
				),
			)));
		}

		Ok(manifest)
	}

	pub fn open(path: &Path) -> Result<Self> {
		let text = fs::read_to_string(path)?;
		Self::parse(&text, &path.display().to_string())
	}

	pub fn save(&self, path: &Path) -> Result<()> {
		let text = toml::to_string(self).map_err(|error| {
			Error::Diagnostic(Diagnostic::global(
				Code::InternalUnexpected,
				format!("the manifest does not serialize: {error}"),
			))
		})?;

		if let Some(parent) = path.parent() {
			fs::create_dir_all(parent)?;
		}

		fs::write(path, text)?;
		Ok(())
	}

	/// The artifact for one tool, optional variant and this machine, newest first
	/// as the manifest wrote them.
	pub fn artifact(&self, tool: &str, variant: Option<&str>) -> Option<&Artifact> {
		self.artifact.iter().find(|artifact| {
			artifact.tool == tool
				&& variant.is_none_or(|wanted| artifact.variant == wanted)
				&& artifact.platform == platform()
				&& artifact.arch == arch()
		})
	}

	/// A manifest older than the newest accepted one, or one whose expiry has
	/// passed, is not a list a user asked to roll back to.
	pub fn check_freshness(&self, accepted: u64) -> Result<()> {
		if self.sequence < accepted {
			return Err(Error::Diagnostic(Diagnostic::global(
				Code::ToolVersion,
				format!(
					"the manifest sequence {} is older than the accepted {}",
					self.sequence, accepted
				),
			)));
		}

		let today = today();
		if !self.expires.is_empty() && self.expires.as_str() < today.as_str() {
			return Err(Error::Diagnostic(Diagnostic::global(
				Code::ToolVersion,
				format!("the manifest expired on {}", self.expires),
			)));
		}

		Ok(())
	}

	/// Verify the detached signature that covers the exact manifest bytes.
	pub fn verify_signature(&self, bytes: &[u8], signature: &Path, key_hex: &str) -> Result<()> {
		let key = hex::decode(key_hex).map_err(|_| {
			Error::Diagnostic(Diagnostic::global(
				Code::ToolVersion,
				"the toolchain public key is not hexadecimal",
			))
		})?;
		let key: [u8; 32] = key.try_into().map_err(|_| {
			Error::Diagnostic(Diagnostic::global(
				Code::ToolVersion,
				"the toolchain public key is not 32 bytes",
			))
		})?;

		let verifying = VerifyingKey::from_bytes(&key).map_err(|error| {
			Error::Diagnostic(Diagnostic::global(
				Code::ToolVersion,
				format!("the toolchain public key is invalid: {error}"),
			))
		})?;

		let raw = fs::read(signature)?;
		let raw = match std::str::from_utf8(&raw) {
			Ok(text) if text.trim().len() == 128 => hex::decode(text.trim()).unwrap_or_default(),
			_ => raw,
		};

		let signature = Signature::from_slice(&raw).map_err(|error| {
			Error::Diagnostic(Diagnostic::global(
				Code::ToolVersion,
				format!("the manifest signature is invalid: {error}"),
			))
		})?;

		verifying.verify_strict(bytes, &signature).map_err(|_| {
			Error::Diagnostic(Diagnostic::global(
				Code::ToolVersion,
				"the manifest signature does not match the manifest",
			))
		})
	}
}

/// What this machine installed, with the hash it was accepted with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installed {
	pub version: String,
	pub sequence: u64,
	#[serde(default)]
	pub artifact: Vec<InstalledArtifact>,
}

impl Default for Installed {
	fn default() -> Self {
		Self {
			version: TOOLS_VERSION.to_string(),
			sequence: 0,
			artifact: Vec::new(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledArtifact {
	pub tool: String,
	#[serde(default)]
	pub variant: String,
	pub version: String,
	pub platform: String,
	pub arch: String,
	pub sha256: String,
	/// The artifact kept for verification.
	pub cache: PathBuf,
	/// The executable the managed directory resolves.
	pub path: PathBuf,
}

impl Installed {
	pub fn open(path: &Path) -> Result<Self> {
		let text = match fs::read_to_string(path) {
			Ok(text) => text,
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
			Err(error) => return Err(Error::Io(error)),
		};

		let installed: Self = toml::from_str(&text).map_err(|error| {
			Error::Diagnostic(Diagnostic::global(
				Code::CfgParse,
				format!("{}: {error}", path.display()),
			))
		})?;

		if installed.version != TOOLS_VERSION {
			return Err(Error::Diagnostic(Diagnostic::global(
				Code::CfgVersion,
				format!(
					"{} declares '{}', this build reads '{TOOLS_VERSION}'",
					path.display(),
					installed.version
				),
			)));
		}

		Ok(installed)
	}

	pub fn save(&self, path: &Path) -> Result<()> {
		let text = toml::to_string(self).map_err(|error| {
			Error::Diagnostic(Diagnostic::global(
				Code::InternalUnexpected,
				format!("the installed set does not serialize: {error}"),
			))
		})?;

		if let Some(parent) = path.parent() {
			fs::create_dir_all(parent)?;
		}

		fs::write(path, text)?;
		Ok(())
	}

	pub fn record(&mut self, artifact: InstalledArtifact) {
		self.artifact.retain(|held| {
			!(held.tool == artifact.tool
				&& held.variant == artifact.variant
				&& held.platform == artifact.platform
				&& held.arch == artifact.arch)
		});
		self.artifact.push(artifact);
	}

	pub fn get(&self, tool: &str, variant: Option<&str>) -> Option<&InstalledArtifact> {
		self.artifact.iter().find(|artifact| {
			artifact.tool == tool
				&& variant.is_none_or(|wanted| artifact.variant == wanted)
				&& artifact.platform == platform()
				&& artifact.arch == arch()
		})
	}
}

pub fn platform() -> &'static str {
	std::env::consts::OS
}

pub fn arch() -> &'static str {
	std::env::consts::ARCH
}

/// The SHA-256 of a file, lowercase hex.
pub fn sha256_file(path: &Path) -> Result<String> {
	let mut file = File::open(path)?;
	let mut hasher = Sha256::new();
	let mut buffer = [0u8; 64 * 1024];

	loop {
		let read = file.read(&mut buffer)?;
		if read == 0 {
			break;
		}
		hasher.update(&buffer[..read]);
	}

	Ok(hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
}

pub fn is_sha256(text: &str) -> bool {
	text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Copy an artifact into the verification cache after checking its size and hash.
pub fn cache(artifact: &Artifact, from: &Path, directory: &Path) -> Result<PathBuf> {
	let bytes = fs::metadata(from)?.len();
	if artifact.size != 0 && bytes != artifact.size {
		return Err(Error::Diagnostic(Diagnostic::global(
			Code::ToolVersion,
			format!(
				"{} is {} bytes, the manifest says {}",
				from.display(),
				bytes,
				artifact.size
			),
		)));
	}

	let hash = sha256_file(from)?;
	if !hash.eq_ignore_ascii_case(&artifact.sha256) {
		return Err(Error::Diagnostic(Diagnostic::global(
			Code::ToolVersion,
			format!(
				"{} hashes to {hash}, the manifest says {}",
				from.display(),
				artifact.sha256
			),
		)));
	}

	fs::create_dir_all(directory)?;
	let name = from.file_name().and_then(|name| name.to_str()).unwrap_or("artifact");
	let held = directory.join(format!("{hash}-{name}"));
	fs::copy(from, &held)?;
	Ok(held)
}

/// Unpack an artifact into the managed directory. A plain executable is copied to
/// `bin/<name>` instead, because some artifacts are a single binary.
pub fn extract(archive: &Path, destination: &Path) -> Result<()> {
	fs::create_dir_all(destination)?;

	let name = archive
		.file_name()
		.and_then(|name| name.to_str())
		.unwrap_or_default()
		.to_ascii_lowercase();

	if name.ends_with(".zip") {
		unzip(archive, destination)
	} else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
		untar(archive, destination, true)
	} else if name.ends_with(".tar") {
		untar(archive, destination, false)
	} else {
		Err(Error::Diagnostic(Diagnostic::global(
			Code::ToolFailed,
			format!("{} is not an archive this build can unpack", archive.display()),
		)))
	}
}

pub fn unpack(archive: &Path, destination: &Path, command: &str) -> Result<PathBuf> {
	fs::create_dir_all(destination)?;
	let bin = destination.join("bin");
	fs::create_dir_all(&bin)?;

	let name = archive
		.file_name()
		.and_then(|name| name.to_str())
		.unwrap_or_default()
		.to_ascii_lowercase();

	if name.ends_with(".zip") || name.ends_with(".tar.gz") || name.ends_with(".tgz") || name.ends_with(".tar") {
		extract(archive, destination)?;
	} else {
		let target = bin.join(executable_name(command));
		fs::copy(archive, &target)?;
		executable(&target)?;
		return Ok(target);
	}

	// The archive layout is the distribution's, but the tool is what this build
	// names: an executable at the root is moved to `bin/` so both layouts work.
	let direct = bin.join(executable_name(command));
	if direct.exists() {
		return Ok(direct);
	}

	let root = destination.join(executable_name(command));
	if root.exists() {
		fs::rename(&root, &direct)?;
		return Ok(direct);
	}

	Ok(direct)
}

fn executable_name(command: &str) -> String {
	if cfg!(windows) && !command.ends_with(".exe") {
		format!("{command}.exe")
	} else {
		command.to_string()
	}
}

fn unzip(archive: &Path, destination: &Path) -> Result<()> {
	let file = File::open(archive)?;
	let mut zip = zip::ZipArchive::new(file).map_err(|error| {
		Error::Diagnostic(Diagnostic::global(
			Code::ToolFailed,
			format!("{} is not a zip archive: {error}", archive.display()),
		))
	})?;

	for index in 0..zip.len() {
		let mut entry = zip
			.by_index(index)
			.map_err(|error| Error::Diagnostic(Diagnostic::global(Code::ToolFailed, error.to_string())))?;
		let Some(relative) = entry.enclosed_name() else {
			return Err(Error::Diagnostic(Diagnostic::global(
				Code::ToolFailed,
				format!("{} has an entry that would escape the archive", archive.display()),
			)));
		};

		let target = destination.join(relative);
		if entry.is_dir() {
			fs::create_dir_all(&target)?;
			continue;
		}

		if let Some(parent) = target.parent() {
			fs::create_dir_all(parent)?;
		}
		let mut out = File::create(&target)?;
		std::io::copy(&mut entry, &mut out)?;

		#[cfg(unix)]
		if let Some(mode) = entry.unix_mode() {
			use std::os::unix::fs::PermissionsExt;
			fs::set_permissions(&target, fs::Permissions::from_mode(mode))?;
		}
	}

	Ok(())
}

fn untar(archive: &Path, destination: &Path, gzip: bool) -> Result<()> {
	let file = File::open(archive)?;

	if gzip {
		let decoder = flate2::read::GzDecoder::new(file);
		let mut tar = tar::Archive::new(decoder);
		tar.unpack(destination)?;
	} else {
		let mut tar = tar::Archive::new(file);
		tar.unpack(destination)?;
	}

	Ok(())
}

fn executable(path: &Path) -> Result<()> {
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
	}

	Ok(())
}

/// `YYYY-MM-DD`, for the manifest's expiry.
pub fn today() -> String {
	let seconds = crate::job::now_ms() / 1000;
	let days = seconds.div_euclid(86_400);
	let (year, month, day) = civil_from_days(days);
	format!("{year:04}-{month:02}-{day:02}")
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
	let z = days + 719_468;
	let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
	let day_of_era = z - era * 146_097;
	let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
	let year = year_of_era + era * 400;
	let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
	let month_position = (5 * day_of_year + 2) / 153;
	let day = (day_of_year - (153 * month_position + 2) / 5 + 1) as u32;
	let month = if month_position < 10 {
		month_position + 3
	} else {
		month_position - 9
	} as u32;

	(year, month, day)
}

/// Hex helpers, kept local so a manifest does not pull a general-purpose codec in.
mod hex {
	pub fn decode(text: &str) -> std::result::Result<Vec<u8>, ()> {
		let text: String = text.chars().filter(|c| !c.is_whitespace()).collect();
		if text.len() % 2 != 0 {
			return Err(());
		}

		(0..text.len())
			.step_by(2)
			.map(|index| u8::from_str_radix(&text[index..index + 2], 16).map_err(|_| ()))
			.collect()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_manifest_names_the_schema_it_reads() {
		let manifest = Manifest::parse(
			"version = 'retype-tools-1'\nsequence = 4\nexpires = '2999-01-01'\n\n[[artifact]]\ntool = 'ffmpeg'\nvariant = 'gpl'\nversion = '9.0.2'\nplatform = 'linux'\narch = 'x86_64'\nurl = 'https://example.invalid/ffmpeg.zip'\nsha256 = '00'\n",
			"test",
		)
		.expect("parses");

		assert_eq!(manifest.sequence, 4);
		assert_eq!(manifest.artifact[0].tool, "ffmpeg");
	}

	#[test]
	fn a_manifest_from_another_build_is_refused() {
		let error =
			Manifest::parse("version = 'retype-tools-9'\nsequence = 1\nexpires = ''\n", "test").expect_err("refuses");

		assert!(error.to_string().contains(TOOLS_VERSION));
	}

	#[test]
	fn the_hash_is_the_one_the_manifest_will_compare() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("a");
		fs::write(&path, b"hello").unwrap();

		assert_eq!(
			sha256_file(&path).unwrap(),
			"2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
		);
	}

	#[test]
	fn a_cached_artifact_must_match_its_hash() {
		let dir = tempfile::tempdir().unwrap();
		let from = dir.path().join("artifact");
		fs::write(&from, b"hello").unwrap();
		let artifact = Artifact {
			tool: "ffmpeg".to_string(),
			variant: String::new(),
			version: "1".to_string(),
			platform: platform().to_string(),
			arch: arch().to_string(),
			url: "x".to_string(),
			size: 5,
			sha256: sha256_file(&from).unwrap(),
			components: Vec::new(),
			license: String::new(),
			source: String::new(),
			source_sha256: String::new(),
		};

		let cached = cache(&artifact, &from, &dir.path().join("cache")).unwrap();
		assert!(cached.exists());

		let wrong = Artifact {
			sha256: "0".repeat(64),
			..artifact
		};
		assert!(cache(&wrong, &from, &dir.path().join("cache")).is_err());
	}

	#[test]
	fn a_signature_covers_the_exact_bytes() {
		use ed25519_dalek::{Signer, SigningKey};

		let key = SigningKey::from_bytes(&[7u8; 32]);
		let bytes = b"version = 'retype-tools-1'";
		let signature = key.sign(bytes);
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("manifest.sig");
		fs::write(&path, signature.to_bytes()).unwrap();

		let manifest = Manifest::parse(
			"version = 'retype-tools-1'\nsequence = 1\nexpires = '2999-01-01'\n",
			"test",
		)
		.unwrap();
		let key_hex: String = key
			.verifying_key()
			.to_bytes()
			.iter()
			.map(|b| format!("{b:02x}"))
			.collect();

		manifest.verify_signature(bytes, &path, &key_hex).expect("verifies");
		assert!(manifest.verify_signature(b"other", &path, &key_hex).is_err());
	}
}
