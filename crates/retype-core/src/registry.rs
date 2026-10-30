//! The operation registry, `docs/config.md` and `docs/operations.md`.
//!
//! A declarative table of formats and operations, loaded from the built-in
//! `builtin.toml` and, later, overridden by user configuration. Nothing here
//! knows how to convert anything: it knows what the product is willing to
//! accept, which is what the parser needs to resolve a name and a value.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::error::{Code, Diagnoses, Diagnostic, Error};
use crate::job::Tier;

/// Schema identifier, `docs/config.md` §1.
pub const CONFIG_VERSION: &str = "retype-config-1";

const FAMILIES: &[&str] = &["image", "audio", "video", "document", "archive", "folder"];

/// A format the product can read or write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Format {
	pub family: String,
	pub lossy: bool,
	pub alpha: bool,
	pub tier: Tier,
	pub aliases: Vec<String>,
}

/// The value class of an operation parameter, `docs/grammar.md` §7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
	Int,
	Dim,
	Ratio,
	Size,
	Time,
	Range,
	Enum,
	Code,
	Text,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Param {
	pub class: Class,
	#[serde(default)]
	pub min: Option<u64>,
	#[serde(default)]
	pub max: Option<u64>,
	#[serde(default)]
	pub values: Vec<String>,
}

/// An operation, which is what a token name resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Op {
	/// Permanent identity. Never reused for a different meaning.
	pub key: String,
	/// Primary spelling, printed canonically.
	pub name: String,
	pub aliases: Vec<String>,
	pub slot: u16,
	pub exclusive: Vec<String>,
	/// Formats this applies to, or `*` for every format.
	pub formats: Vec<String>,
	pub param: Option<Param>,
}

impl Op {
	/// An operation with no formats has not landed yet, `docs/operations.md` §6:
	/// naming it is `E_UNSUPPORTED` rather than `E_NAME_UNKNOWN`.
	pub fn is_implemented(&self) -> bool {
		!self.formats.is_empty()
	}

	pub fn applies_to(&self, format: &str) -> bool {
		self.formats
			.iter()
			.any(|candidate| candidate == "*" || candidate == format)
	}
}

/// A token resolved against the registry, before its value is interpreted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved<'a> {
	pub op: &'a Op,
	pub value: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct Registry {
	formats: BTreeMap<String, Format>,
	ops: BTreeMap<String, Op>,
	/// Every name and alias with the key it resolves to, longest first, so that
	/// `quality85` resolves to `quality` and not to `q`, `docs/grammar.md` R2.
	names: Vec<(String, String)>,
}

impl Registry {
	/// The table the product ships. A failure here is a build defect, not a
	/// user error, which is why it is a `Result` that callers surface as
	/// internal rather than mask.
	pub fn builtin() -> Result<Self, Diagnoses> {
		Self::load_in(include_str!("../builtin.toml"), "the built-in configuration")
	}

	pub fn load(text: &str) -> Result<Self, Diagnoses> {
		Self::load_in(text, "the configuration")
	}

	fn load_in(text: &str, label: &str) -> Result<Self, Diagnoses> {
		let document: Document = match toml::from_str(text) {
			Ok(document) => document,
			Err(error) => return Err(Diagnoses::one(fail(Code::CfgParse, format!("{label}: {error}")))),
		};

		let mut problems = Vec::new();

		if document.version != CONFIG_VERSION {
			problems.push(fail(
				Code::CfgVersion,
				format!(
					"{label} declares '{}', this build reads '{CONFIG_VERSION}'",
					document.version
				),
			));
		}

		let formats: BTreeMap<String, Format> = document
			.formats
			.iter()
			.map(|(name, entry)| (name.to_ascii_lowercase(), entry.view()))
			.collect();

		for (name, format) in &formats {
			if !FAMILIES.contains(&format.family.as_str()) {
				problems.push(fail(
					Code::CfgValidation,
					format!("{label}: format '{name}' declares unknown family '{}'", format.family),
				));
			}
		}

		let mut ops = BTreeMap::new();
		for (key, entry) in &document.ops {
			let op = entry.view(key);

			if !is_name(&op.name) || !op.aliases.iter().all(|alias| is_name(alias)) {
				problems.push(fail(
					Code::CfgValidation,
					format!("{label}: operation '{key}' has a name or alias that is not a lowercase word"),
				));
			}

			for format in &op.formats {
				if format != "*" && !formats.contains_key(format) {
					problems.push(fail(
						Code::CfgValidation,
						format!("{label}: operation '{key}' applies to unknown format '{format}'"),
					));
				}
			}

			if let Some(param) = &op.param {
				if let (Some(min), Some(max)) = (param.min, param.max) {
					if min > max {
						problems.push(fail(
							Code::CfgValidation,
							format!("{label}: operation '{key}' declares min {min} above max {max}"),
						));
					}
				}
				if param.class == Class::Enum && param.values.is_empty() && op.is_implemented() {
					problems.push(fail(
						Code::CfgValidation,
						format!("{label}: operation '{key}' is an enum with no values"),
					));
				}
			}

			ops.insert(key.to_ascii_lowercase(), op);
		}

		let mut names = collect_names(&ops);
		problems.extend(uniqueness(&names, label));
		problems.extend(singleton_groups(&ops, label));

		names.sort_by(|left, right| right.0.len().cmp(&left.0.len()).then(left.0.cmp(&right.0)));

		if !problems.is_empty() {
			return Err(Diagnoses::new(problems));
		}

		Ok(Self { formats, ops, names })
	}

	pub fn format(&self, name: &str) -> Option<&Format> {
		self.formats.get(&name.to_ascii_lowercase())
	}

	pub fn op(&self, name: &str) -> Option<&Op> {
		let name = name.to_ascii_lowercase();
		self.ops.get(&name).or_else(|| {
			self.names
				.iter()
				.find(|(candidate, _)| *candidate == name)
				.and_then(|(_, key)| self.ops.get(key))
		})
	}

	/// Resolve a written token: an exact name before `=`, otherwise the longest
	/// registered prefix, `docs/grammar.md` R2 and R6.
	pub fn resolve<'a>(&'a self, text: &'a str) -> Option<Resolved<'a>> {
		if let Some((name, value)) = text.split_once('=') {
			return self.op(name).map(|op| Resolved { op, value: Some(value) });
		}

		let lowered = text.to_ascii_lowercase();
		for (name, key) in &self.names {
			if let Some(rest) = lowered.strip_prefix(name.as_str()) {
				let op = self.ops.get(key)?;
				return Some(Resolved {
					op,
					value: if rest.is_empty() {
						None
					} else {
						Some(&text[text.len() - rest.len()..])
					},
				});
			}
		}

		None
	}

	pub fn formats(&self) -> impl Iterator<Item = (&String, &Format)> {
		self.formats.iter()
	}

	pub fn ops(&self) -> impl Iterator<Item = (&String, &Op)> {
		self.ops.iter()
	}
}

fn collect_names(ops: &BTreeMap<String, Op>) -> Vec<(String, String)> {
	let mut names = Vec::new();

	for (key, op) in ops {
		names.push((op.name.clone(), key.clone()));
		for alias in &op.aliases {
			names.push((alias.clone(), key.clone()));
		}
		names.push((key.clone(), key.clone()));
	}

	names
}

/// R1: names, aliases and keys occupy one name space.
fn uniqueness(names: &[(String, String)], label: &str) -> Vec<Error> {
	let mut problems = Vec::new();
	let mut seen: BTreeMap<&str, &str> = BTreeMap::new();

	for (name, key) in names {
		match seen.get(name.as_str()) {
			Some(existing) if *existing != key.as_str() => problems.push(fail(
				Code::CfgNameClash,
				format!("{label}: '{name}' is declared by both '{existing}' and '{key}'"),
			)),
			Some(_) => {}
			None => {
				seen.insert(name, key);
			}
		}
	}

	problems
}

/// V8: an exclusive group with one member cannot conflict, which is a sign the
/// author meant to add another member.
fn singleton_groups(ops: &BTreeMap<String, Op>, label: &str) -> Vec<Error> {
	let mut members: BTreeMap<&str, usize> = BTreeMap::new();
	for op in ops.values() {
		for group in &op.exclusive {
			*members.entry(group.as_str()).or_default() += 1;
		}
	}

	members
		.into_iter()
		.filter(|(_, count)| *count < 2)
		.map(|(group, _)| {
			fail(
				Code::CfgValidation,
				format!("{label}: exclusive group '{group}' has a single member"),
			)
		})
		.collect()
}

fn fail(code: Code, message: String) -> Error {
	Error::Diagnostic(Diagnostic::global(code, message))
}

fn is_name(text: &str) -> bool {
	let mut bytes = text.bytes();
	match bytes.next() {
		Some(byte) if byte.is_ascii_lowercase() => {}
		_ => return false,
	}
	bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

#[derive(Debug, Deserialize)]
struct Document {
	version: String,
	#[serde(default)]
	formats: BTreeMap<String, FormatEntry>,
	#[serde(default)]
	ops: BTreeMap<String, OpEntry>,
}

#[derive(Debug, Deserialize)]
struct FormatEntry {
	family: String,
	lossy: bool,
	alpha: bool,
	tier: Tier,
	#[serde(default)]
	aliases: Vec<String>,
}

impl FormatEntry {
	fn view(&self) -> Format {
		Format {
			family: self.family.clone(),
			lossy: self.lossy,
			alpha: self.alpha,
			tier: self.tier,
			aliases: self.aliases.clone(),
		}
	}
}

#[derive(Debug, Deserialize)]
struct OpEntry {
	#[serde(default)]
	name: Option<String>,
	#[serde(default)]
	aliases: Vec<String>,
	slot: u16,
	#[serde(default)]
	exclusive: Vec<String>,
	#[serde(default)]
	formats: Vec<String>,
	#[serde(default)]
	param: Option<Param>,
}

impl OpEntry {
	fn view(&self, key: &str) -> Op {
		Op {
			key: key.to_ascii_lowercase(),
			name: self.name.clone().unwrap_or_else(|| key.to_ascii_lowercase()),
			aliases: self.aliases.clone(),
			slot: self.slot,
			exclusive: self.exclusive.clone(),
			formats: self.formats.clone(),
			param: self.param.clone(),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn builtin() -> Registry {
		Registry::builtin().expect("the built-in registry must load")
	}

	fn codes(text: &str) -> Vec<Code> {
		Registry::load(text).unwrap_err().codes()
	}

	const HEADER: &str = "version = 'retype-config-1'\n";

	#[test]
	fn the_builtin_registry_loads_and_names_every_format_it_uses() {
		let registry = builtin();

		assert!(registry.format("jpg").is_some());
		assert_eq!(registry.formats().count(), 9);
		assert_eq!(registry.ops().count(), 30);

		for (key, op) in registry.ops() {
			for format in &op.formats {
				assert!(
					format == "*" || registry.format(format).is_some(),
					"'{key}' applies to '{format}', which is not declared"
				);
			}
		}
	}

	#[test]
	fn the_longest_name_wins_over_a_shorter_one_that_also_matches() {
		let registry = builtin();

		let resolved = registry.resolve("quality85").unwrap();
		assert_eq!(resolved.op.key, "quality");
		assert_eq!(resolved.value, Some("85"));

		let alias = registry.resolve("q80").unwrap();
		assert_eq!(alias.op.key, "quality");
		assert_eq!(alias.value, Some("80"));
	}

	#[test]
	fn an_equals_forces_the_exact_name() {
		let registry = builtin();

		let resolved = registry.resolve("quality=85").unwrap();
		assert_eq!(resolved.op.key, "quality");
		assert_eq!(resolved.value, Some("85"));
	}

	#[test]
	fn a_name_without_a_value_resolves_without_one() {
		let registry = builtin();

		assert_eq!(registry.resolve("nometa").unwrap().value, None);
		assert_eq!(registry.resolve("grey").unwrap().op.key, "gray");
		assert!(registry.resolve("zzz").is_none());
	}

	#[test]
	fn a_reserved_operation_is_known_but_not_implemented() {
		let registry = builtin();
		let resolved = registry.resolve("trim1h30s").unwrap();

		assert_eq!(resolved.op.key, "trim");
		assert!(!resolved.op.is_implemented());
	}

	#[test]
	fn an_operation_applies_only_to_the_formats_it_names() {
		let registry = builtin();

		assert!(registry.op("quality").unwrap().applies_to("jpg"));
		assert!(!registry.op("quality").unwrap().applies_to("png"));
		assert!(registry.op("cap").unwrap().applies_to("png"));
	}

	#[test]
	fn a_wrong_schema_version_is_refused() {
		assert_eq!(codes("version = 'retype-config-2'\n"), vec![Code::CfgVersion]);
	}

	#[test]
	fn two_operations_cannot_share_a_name_or_alias() {
		let text = format!(
			r#"{HEADER}
[formats.png]
family = 'image'
lossy = false
alpha = true
tier = 'converted'

[ops.alpha]
slot = 10
formats = ['png']

[ops.beta]
aliases = ['alpha']
slot = 10
formats = ['png']
"#
		);

		assert_eq!(codes(&text), vec![Code::CfgNameClash]);
	}

	#[test]
	fn a_parameter_with_min_above_max_is_refused() {
		let text = format!(
			r#"{HEADER}
[formats.png]
family = 'image'
lossy = false
alpha = true
tier = 'converted'

[ops.quality]
slot = 50
formats = ['png']
param = {{ class = 'int', min = 90, max = 10 }}
"#
		);

		assert_eq!(codes(&text), vec![Code::CfgValidation]);
	}

	#[test]
	fn an_operation_cannot_apply_to_an_undeclared_format() {
		let text = format!(
			r#"{HEADER}
[ops.quality]
slot = 50
formats = ['png']
"#
		);

		assert_eq!(codes(&text), vec![Code::CfgValidation]);
	}

	#[test]
	fn an_exclusive_group_of_one_is_refused() {
		let text = format!(
			r#"{HEADER}
[formats.png]
family = 'image'
lossy = false
alpha = true
tier = 'converted'

[ops.rotate]
slot = 20
exclusive = ['orient']
formats = ['png']
"#
		);

		assert_eq!(codes(&text), vec![Code::CfgValidation]);
	}

	#[test]
	fn an_unknown_family_is_refused() {
		let text = format!(
			r#"{HEADER}
[formats.png]
family = 'picture'
lossy = false
alpha = true
tier = 'converted'
"#
		);

		assert_eq!(codes(&text), vec![Code::CfgValidation]);
	}

	#[test]
	fn malformed_toml_is_a_parse_failure() {
		assert_eq!(codes("version = "), vec![Code::CfgParse]);
	}

	#[test]
	fn every_builtin_flag_matches_the_operations_document() {
		let registry = builtin();
		let quality = registry.op("quality").unwrap();

		assert_eq!(quality.slot, 50);
		assert_eq!(quality.exclusive, vec!["encode-target".to_string()]);
		assert_eq!(quality.param.as_ref().unwrap().class, Class::Int);
		assert_eq!(quality.param.as_ref().unwrap().min, Some(1));
		assert_eq!(quality.param.as_ref().unwrap().max, Some(100));
	}
}
