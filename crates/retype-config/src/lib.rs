//! User configuration discovery, loading and writing.
//!
//! The schema, the merge and every validation rule live in `retype-core`. This
//! crate owns the parts that touch the machine: where the file is, how a missing
//! file is treated, and how a validated change is written back.
//!
//! One loader serves the CLI, the daemon and the GUI. A caller names an explicit
//! path when `--config` was given; otherwise `RETYPE_CONFIG` and the platform
//! default apply.

use std::fs;
use std::path::{Path, PathBuf};

use retype_core::error::{Code, Diagnoses};
use retype_core::registry::{CONFIG_VERSION, Param, Registry};

/// The user file inside the platform's configuration directory.
pub const FILE: &str = "retype.toml";

/// A failure to locate, read, validate or write the user configuration.
#[derive(Debug, thiserror::Error)]
pub enum Error {
	/// The configuration file failed validation.
	#[error(transparent)]
	Config(#[from] Diagnoses),

	#[error(transparent)]
	Io(#[from] std::io::Error),

	/// An explicit path was named and does not exist.
	#[error("E_CFG_MISSING: {} does not exist", .0.display())]
	Missing(PathBuf),

	/// The platform has no configuration directory and no explicit path was given.
	#[error("E_CFG_MISSING: no user configuration directory on this platform; set RETYPE_CONFIG or pass --config")]
	NoDirectory,

	/// A preset cannot replace an existing name.
	#[error("E_CFG_VALIDATION: '{}' already names an operation or preset", .0)]
	Occupied(String),

	/// A preset name or token cannot be written as configuration.
	#[error("E_CFG_VALIDATION: {0}")]
	Name(String),

	/// A validated document could not be serialized or written.
	#[error("E_CFG_PARSE: cannot write the configuration: {0}")]
	Encode(String),
}

impl Error {
	pub fn code(&self) -> Code {
		match self {
			Self::Config(problems) => problems
				.iter()
				.next()
				.map(retype_core::error::Error::code)
				.unwrap_or(Code::CfgParse),
			Self::Io(error) => retype_core::error::code_for_io(error),
			Self::Missing(_) | Self::NoDirectory => Code::CfgMissing,
			Self::Occupied(_) | Self::Name(_) => Code::CfgValidation,
			Self::Encode(_) => Code::CfgParse,
		}
	}

	pub fn exit_code(&self) -> u8 {
		self.code().category().exit_code()
	}
}

/// A preset the GUI or CLI wants to keep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetInput {
	pub name: String,
	pub tokens: Vec<String>,
	pub param: Option<Param>,
}

/// What a successful save wrote and how the merged table now reads.
#[derive(Debug, Clone)]
pub struct Saved {
	pub path: PathBuf,
	pub registry: Registry,
}

/// `--config` wins, then `RETYPE_CONFIG`, then the platform's directory.
pub fn user_path(explicit: Option<&Path>) -> Option<PathBuf> {
	if let Some(path) = explicit {
		return Some(path.to_path_buf());
	}

	if let Some(configured) = std::env::var_os("RETYPE_CONFIG") {
		if !configured.is_empty() {
			return Some(PathBuf::from(configured));
		}
	}

	default_path()
}

/// The platform's user configuration path, ignoring flags and the environment.
pub fn default_path() -> Option<PathBuf> {
	config_dir().map(|base| base.join("retype").join(FILE))
}

/// Load the user file, or the built-in table when there is none.
///
/// An explicit path is required to exist: falling back to the built-in table would
/// answer a different question than the one that was asked. A missing
/// `RETYPE_CONFIG` target is treated as "no user file", as it has always been.
pub fn load(explicit: Option<&Path>) -> Result<Registry, Error> {
	let Some(path) = user_path(explicit) else {
		return Registry::builtin().map_err(Error::Config);
	};

	if explicit.is_some() && !path.exists() {
		return Err(Error::Missing(path));
	}

	load_from(&path)
}

/// Load the same file from a named path. A missing file is the built-in table.
pub fn load_from(path: &Path) -> Result<Registry, Error> {
	let text = match fs::read_to_string(path) {
		Ok(text) => text,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
			return Registry::builtin().map_err(Error::Config);
		}
		Err(error) => return Err(Error::Io(error)),
	};

	Registry::with_user(&text, &path.display().to_string()).map_err(Error::Config)
}

/// Read the user file without parsing it.
pub fn read(explicit: Option<&Path>) -> Result<Option<String>, Error> {
	let Some(path) = user_path(explicit) else {
		return Err(Error::NoDirectory);
	};

	match fs::read_to_string(&path) {
		Ok(text) => Ok(Some(text)),
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
		Err(error) => Err(Error::Io(error)),
	}
}

/// Validate and write a whole user file atomically.
pub fn write(explicit: Option<&Path>, text: &str) -> Result<PathBuf, Error> {
	let Some(path) = user_path(explicit) else {
		return Err(Error::NoDirectory);
	};

	Registry::with_user(text, &path.display().to_string()).map_err(Error::Config)?;
	write_text(&path, text)?;
	Ok(path)
}

/// Append a validated preset to the user file.
///
/// The existing text is kept as it is, so comments and unrelated entries survive.
/// The merged document is loaded before the file is replaced, and an existing
/// operation or preset name is refused rather than shadowed.
pub fn save_preset(explicit: Option<&Path>, preset: &PresetInput) -> Result<Saved, Error> {
	validate_preset(preset)?;

	let Some(path) = user_path(explicit) else {
		return Err(Error::NoDirectory);
	};

	let current = match fs::read_to_string(&path) {
		Ok(text) => text,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
		Err(error) => return Err(Error::Io(error)),
	};

	let base = match current.trim().is_empty() {
		true => format!("version = '{CONFIG_VERSION}'\n"),
		false => current,
	};

	let label = path.display().to_string();
	let registry = Registry::with_user(&base, &label).map_err(Error::Config)?;

	if registry.preset(&preset.name).is_some() || registry.op(&preset.name).is_some() {
		return Err(Error::Occupied(preset.name.clone()));
	}

	let mut next = base;
	if !next.ends_with('\n') {
		next.push('\n');
	}
	next.push('\n');
	next.push_str(&format!("[presets.{}]\n", preset.name));
	next.push_str("tokens = [");
	for (index, token) in preset.tokens.iter().enumerate() {
		if index > 0 {
			next.push_str(", ");
		}
		next.push('\'');
		next.push_str(token);
		next.push('\'');
	}
	next.push_str("]\n");

	if let Some(param) = &preset.param {
		let body = toml::to_string(param).map_err(|error| Error::Encode(error.to_string()))?;
		next.push_str(&format!("\n[presets.{}.param]\n{body}", preset.name));
	}

	let registry = Registry::with_user(&next, &label).map_err(Error::Config)?;
	write_text(&path, &next)?;

	Ok(Saved { path, registry })
}

fn validate_preset(preset: &PresetInput) -> Result<(), Error> {
	let mut bytes = preset.name.bytes();
	match bytes.next() {
		Some(byte) if byte.is_ascii_lowercase() => {}
		_ => return Err(Error::Name(format!("'{}' is not a preset name", preset.name))),
	}
	if !bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()) {
		return Err(Error::Name(format!("'{}' is not a preset name", preset.name)));
	}

	if preset.tokens.is_empty() {
		return Err(Error::Name("a preset needs at least one token".to_string()));
	}

	for token in &preset.tokens {
		if token.contains('\'') || token.contains('\n') || token.contains('\r') {
			return Err(Error::Name(format!(
				"'{token}' cannot be written in a configuration file"
			)));
		}
	}

	Ok(())
}

fn write_text(path: &Path, text: &str) -> Result<(), Error> {
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent)?;
	}

	let mut temporary = tempfile::Builder::new()
		.prefix(".retype-config.")
		.tempfile_in(path.parent().unwrap_or(Path::new(".")))?;

	use std::io::Write;
	temporary.write_all(text.as_bytes())?;
	temporary.as_file_mut().sync_all()?;

	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		temporary.as_file().set_permissions(fs::Permissions::from_mode(0o600))?;
	}

	temporary.persist(path).map_err(|error| Error::Io(error.error))?;
	Ok(())
}

#[cfg(unix)]
fn config_dir() -> Option<PathBuf> {
	std::env::var_os("XDG_CONFIG_HOME")
		.map(PathBuf::from)
		.filter(|path| path.is_absolute())
		.or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
}

#[cfg(windows)]
fn config_dir() -> Option<PathBuf> {
	std::env::var_os("LOCALAPPDATA")
		.map(PathBuf::from)
		.or_else(|| std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join("AppData/Local")))
}

#[cfg(not(any(unix, windows)))]
fn config_dir() -> Option<PathBuf> {
	None
}

#[cfg(test)]
mod tests {
	use super::*;

	fn dir() -> tempfile::TempDir {
		tempfile::tempdir().unwrap()
	}

	#[test]
	fn a_missing_user_file_is_the_built_in_table() {
		let dir = dir();
		let absent = load_from(&dir.path().join("absent.toml")).expect("loads");
		let builtin = Registry::with_user("version = 'retype-config-1'\n", "test").expect("loads");

		assert_eq!(absent.formats().count(), builtin.formats().count());
		assert_eq!(absent.backends(), builtin.backends());
		assert!(absent.warnings().is_empty());
	}

	#[test]
	fn a_user_file_reaches_the_registry() {
		let dir = dir();
		let path = dir.path().join("retype.toml");
		fs::write(
			&path,
			"version = 'retype-config-1'\n\n[ops.quality.argv.magick]\nargs = ['-quality', '{value}']\n",
		)
		.unwrap();

		let registry = load_from(&path).expect("loads");
		let op = registry.op("quality").expect("declared");

		assert!(op.fragment("magick").is_some());
	}

	#[test]
	fn a_broken_user_file_is_a_configuration_error() {
		let dir = dir();
		let path = dir.path().join("retype.toml");
		fs::write(&path, "version = 'retype-config-1'\n\n[ops.quality]\nslot = 1\n").unwrap();

		assert!(load_from(&path).is_err());
	}

	#[test]
	fn saving_a_preset_keeps_the_rest_of_the_file() {
		let dir = dir();
		let path = dir.path().join("retype.toml");
		fs::write(
			&path,
			"version = 'retype-config-1'\n\n# keep this comment\n[ops.quality.argv.magick]\nargs = ['-quality', '{value}']\n",
		)
		.unwrap();

		let saved = save_preset(
			Some(&path),
			&PresetInput {
				name: "webby".to_string(),
				tokens: vec!["webp".to_string(), "quality80".to_string()],
				param: None,
			},
		)
		.expect("saves");

		assert_eq!(saved.path, path);
		let text = fs::read_to_string(&path).unwrap();
		assert!(text.contains("# keep this comment"));
		assert!(saved.registry.preset("webby").is_some());
	}

	#[test]
	fn saving_a_preset_with_a_parameter_writes_its_table() {
		let dir = dir();
		let path = dir.path().join("retype.toml");

		let saved = save_preset(
			Some(&path),
			&PresetInput {
				name: "thumb".to_string(),
				tokens: vec!["cap={value}".to_string(), "quality85".to_string()],
				param: Some(Param {
					class: retype_core::registry::Class::Int,
					min: Some(16),
					max: Some(4096),
					values: Vec::new(),
					scalar: None,
				}),
			},
		)
		.expect("saves");

		let preset = saved.registry.preset("thumb").expect("declared");
		assert!(preset.param.is_some());
	}

	#[test]
	fn an_existing_name_is_not_shadowed() {
		let dir = dir();
		let path = dir.path().join("retype.toml");

		let error = save_preset(
			Some(&path),
			&PresetInput {
				name: "quality".to_string(),
				tokens: vec!["quality80".to_string()],
				param: None,
			},
		)
		.expect_err("refused");

		assert!(matches!(error, Error::Occupied(name) if name == "quality"));
	}

	#[test]
	fn a_preset_name_must_be_a_lowercase_word() {
		let dir = dir();
		let path = dir.path().join("retype.toml");

		let error = save_preset(
			Some(&path),
			&PresetInput {
				name: "Not Valid".to_string(),
				tokens: vec!["quality80".to_string()],
				param: None,
			},
		)
		.expect_err("refused");

		assert!(matches!(error, Error::Name(_)));
	}
}
