//! The declarative registry, `docs/config.md` and `docs/operations.md`.
//!
//! One table of tools, formats and operations, loaded from the built-in
//! `builtin.toml` and, later, overridden by user configuration. Nothing here
//! knows how to convert anything: it knows what the product is willing to
//! accept, which is what the parser needs to resolve a name and a value, and
//! what the planner needs to know which tool can honour it.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use crate::error::{Code, Diagnoses, Diagnostic, Error};
use crate::job::Tier;
use crate::tools::{Tool, Version};

/// Schema identifier, `docs/config.md` §1.
pub const CONFIG_VERSION: &str = "retype-config-1";

const FAMILIES: &[&str] = &["image", "audio", "video", "document", "archive", "folder"];

/// An argv fragment an entity contributes to a tool's `{ops}` position,
/// `docs/config.md` §8.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fragment {
	pub args: Vec<String>,
}

impl Fragment {
	/// Whether any argument names `value`, which is how an operation's parameter
	/// reaches the command line.
	pub fn uses_value(&self) -> bool {
		self.args.iter().any(|arg| arg.contains("{value}"))
	}
}

/// A format the product can read or write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Format {
	pub family: String,
	pub lossy: bool,
	pub alpha: bool,
	pub tier: Tier,
	pub aliases: Vec<String>,
	/// What this format contributes to each tool's `{ops}`, by tool name.
	pub argv: BTreeMap<String, Fragment>,
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

/// What a `range` counts, `docs/grammar.md` §7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scalar {
	Int,
	Time,
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
	/// Only meaningful for a `range`: what its ends count.
	#[serde(default)]
	pub scalar: Option<Scalar>,
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
	/// What this operation contributes to each tool's `{ops}`, by tool name.
	pub argv: BTreeMap<String, Fragment>,
	/// How the operation reads in a sentence, `docs/cli.md` §3. A template whose
	/// only hole is `{value}`, so a user-declared operation can say what it does
	/// without a build.
	pub say: Option<String>,
}

impl Format {
	pub fn fragment(&self, tool: &str) -> Option<&Fragment> {
		self.argv.get(tool)
	}
}

impl Op {
	/// An operation with no formats has not landed yet, `docs/operations.md` §6:
	/// naming it is `E_UNSUPPORTED` rather than `E_NAME_UNKNOWN`.
	pub fn is_implemented(&self) -> bool {
		!self.formats.is_empty()
	}

	/// The fragment this operation contributes to a tool, if any. A tool with
	/// no fragment cannot carry the operation, which is what backend selection
	/// asks.
	pub fn fragment(&self, tool: &str) -> Option<&Fragment> {
		self.argv.get(tool)
	}

	pub fn applies_to(&self, format: &str) -> bool {
		self.formats
			.iter()
			.any(|candidate| candidate == "*" || candidate == format)
	}
}

/// A named token sequence, `docs/config.md` §7. A preset compiles to operations,
/// so it composes with them and joins their conflict rules; it is a macro, not a
/// backend feature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preset {
	pub key: String,
	pub name: String,
	/// One optional parameter, whose class decides how a written value is read.
	pub param: Option<Param>,
	/// Tokens, with `{value}` where the parameter goes.
	pub tokens: Vec<String>,
}

/// A token resolved against the registry, before its value is interpreted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved<'a> {
	pub target: Target<'a>,
	pub value: Option<&'a str>,
}

/// What a name turned out to be. Presets share the name space with operations,
/// so one call has to answer for both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target<'a> {
	Op(&'a Op),
	Preset(&'a Preset),
}

impl<'a> Resolved<'a> {
	pub fn op(&self) -> Option<&'a Op> {
		match self.target {
			Target::Op(op) => Some(op),
			Target::Preset(_) => None,
		}
	}

	pub fn preset(&self) -> Option<&'a Preset> {
		match self.target {
			Target::Op(_) => None,
			Target::Preset(preset) => Some(preset),
		}
	}
}

#[derive(Debug, Clone)]
pub struct Registry {
	tools: BTreeMap<String, Tool>,
	presets: BTreeMap<String, Preset>,
	/// Conditions the configuration raised but did not refuse, such as a user
	/// preset replacing a shipped one, `docs/config.md` §2.
	warnings: Vec<Diagnostic>,
	/// Backend preference order, `docs/config.md` §8. The first entry that
	/// carries every operation in a plan and is present wins.
	backends: Vec<String>,
	formats: BTreeMap<String, Format>,
	ops: BTreeMap<String, Op>,
	/// Every name and alias with the key it resolves to, longest first, so that
	/// `quality85` resolves to `quality` and not to `q`, `docs/grammar.md` R2.
	names: Vec<(String, String)>,
	/// Every format name and alias with the canonical format, longest first:
	/// the list that decides where a file name's stem ends.
	format_names: Vec<(String, String)>,
	/// How long bytes are kept for undo, `docs/config.md` §9. Host-level, so
	/// there is one answer per machine rather than one per directory.
	undo: crate::undo::Policy,
	/// Whether the record is also written onto the file, `docs/config.md` §10.
	provenance: crate::provenance::Policy,
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

	/// The shipped table with a user file merged over it, `docs/config.md` §2.
	pub fn with_user(user: &str, label: &str) -> Result<Self, Diagnoses> {
		Self::load_layers(include_str!("../builtin.toml"), user, label)
	}

	/// Merge `user` over `base` and load the result.
	///
	/// The merge is on the document rather than on the loaded tables, because the
	/// rules are about keys: a table is merged key by key, an array is replaced
	/// whole, and `enabled = false` removes an entry. What a user may *change* is
	/// narrower than what they may add, so shadowing is checked before the merge.
	pub fn load_layers(base: &str, user: &str, label: &str) -> Result<Self, Diagnoses> {
		let mut base: toml::Value = match toml::from_str(base) {
			Ok(value) => value,
			Err(error) => {
				return Err(Diagnoses::one(fail(
					Code::CfgParse,
					format!("the built-in configuration: {error}"),
				)));
			}
		};

		let over: toml::Value = match toml::from_str(user) {
			Ok(value) => value,
			Err(error) => return Err(Diagnoses::one(fail(Code::CfgParse, format!("{label}: {error}")))),
		};

		let mut problems = Vec::new();
		let mut warnings = Vec::new();

		// A user file declares its version, `docs/config.md` §12: without it the
		// merge would silently inherit the built-in one and hide a stale file.
		match over.get("version").and_then(toml::Value::as_str) {
			None => problems.push(fail(
				Code::CfgVersion,
				format!("{label} declares no version; it must say version = '{CONFIG_VERSION}'"),
			)),
			Some(version) if version != CONFIG_VERSION => problems.push(fail(
				Code::CfgVersion,
				format!("{label} declares '{version}', this build reads '{CONFIG_VERSION}'"),
			)),
			Some(_) => {}
		}

		problems.extend(shadowing(&base, &over, label, &mut warnings));

		if problems.is_empty() {
			let before = base.clone();
			merge(&mut base, &over);
			prune_preference(&before, &mut base);
		}

		if !problems.is_empty() {
			return Err(Diagnoses::new(problems));
		}

		let merged = match toml::to_string(&base) {
			Ok(merged) => merged,
			Err(error) => {
				return Err(Diagnoses::one(fail(
					Code::CfgParse,
					format!("the merged configuration does not serialize: {error}"),
				)));
			}
		};

		let mut registry = Self::load_in(&merged, label)?;
		registry.warnings = warnings;
		Ok(registry)
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

		let mut tools = BTreeMap::new();
		for (name, entry) in &document.tools {
			match entry.view(name) {
				Ok(tool) => {
					problems.extend(validate_tool(&tool, label));
					tools.insert(tool.name.clone(), tool);
				}
				Err(problem) => problems.push(problem),
			}
		}

		let formats: BTreeMap<String, Format> = document
			.formats
			.iter()
			.map(|(name, entry)| (name.to_ascii_lowercase(), entry.view()))
			.collect();

		let tool_names: BTreeSet<&str> = tools.keys().map(String::as_str).collect();

		// Default order: every declared tool by name, so a configuration that says
		// nothing is still deterministic.
		let mut backends: Vec<String> = tools.keys().cloned().collect();

		if let Some(declared) = &document.backends {
			backends = declared.prefer.clone();
			problems.extend(validate_backends(&backends, &tool_names, label));
		}

		for (name, format) in &formats {
			problems.extend(validate_fragments(
				&format!("format '{name}'"),
				&format.argv,
				&tools,
				&tool_names,
				false,
				label,
			));
		}

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

			problems.extend(validate_fragments(
				&format!("operation '{key}'"),
				&op.argv,
				&tools,
				&tool_names,
				op.param.is_some(),
				label,
			));

			// The sentence is part of the declaration: a phrase that hides its
			// parameter would describe a conversion that does something else.
			if let Some(say) = &op.say {
				let holes = placeholders(say);

				if let Some(unknown) = holes.iter().find(|hole| **hole != "value") {
					problems.push(fail(
						Code::CfgValidation,
						format!(
							"{label}: operation '{key}' says '{{{unknown}}}', and only {{value}} may appear in a phrase"
						),
					));
				}

				if op.param.is_some() != holes.contains(&"value") {
					problems.push(fail(
						Code::CfgValidation,
						format!(
							"{label}: operation '{key}' says '{say}', which disagrees with whether it takes a value"
						),
					));
				}
			}

			// V3: a parameter no fragment interpolates never reaches a command.
			if op.param.is_some() && !op.argv.is_empty() && !op.argv.values().any(Fragment::uses_value) {
				problems.push(fail(
					Code::CfgValidation,
					format!("{label}: operation '{key}' declares a parameter that no fragment uses"),
				));
			}

			ops.insert(key.to_ascii_lowercase(), op);
		}

		let mut presets = BTreeMap::new();
		for (key, entry) in &document.presets {
			let preset = entry.view(key);

			if !is_name(&preset.name) {
				problems.push(fail(
					Code::CfgValidation,
					format!("{label}: preset '{key}' has a name that is not a lowercase word"),
				));
			}

			presets.insert(preset.key.clone(), preset);
		}

		// The two tables are one name space, and identical keys are the case the
		// uniqueness pass over names cannot see.
		for key in presets.keys() {
			if ops.contains_key(key) {
				problems.push(fail(
					Code::CfgNameClash,
					format!("{label}: '{key}' is declared as both an operation and a preset"),
				));
			}
		}

		let mut names = collect_names(&ops);
		names.extend(collect_preset_names(&presets));
		problems.extend(uniqueness(&names, label));
		problems.extend(singleton_groups(&ops, label));

		names.sort_by(|left, right| right.0.len().cmp(&left.0.len()).then(left.0.cmp(&right.0)));

		let mut format_names = collect_format_names(&formats);
		format_names.sort_by(|left, right| right.0.len().cmp(&left.0.len()).then(left.0.cmp(&right.0)));

		let mut undo = crate::undo::Policy::default();

		if let Some(entry) = &document.undo {
			if let Some(enabled) = entry.enabled {
				undo.enabled = enabled;
			}

			if let Some(retention) = &entry.retention {
				match duration(retention) {
					Ok(read) => undo.retention = read,
					Err(fault) => problems.push(fail(
						fault.code,
						format!("{label}: [undo] retention '{retention}': {}", fault.message),
					)),
				}
			}
		}

		let mut provenance = crate::provenance::Policy::default();

		if let Some(entry) = &document.provenance {
			if let Some(xattr) = entry.xattr {
				provenance.xattr = xattr;
			}
		}

		let registry = Self {
			tools,
			presets,
			backends,
			formats,
			ops,
			names,
			format_names,
			undo,
			provenance,
			warnings: Vec::new(),
		};

		// Presets are checked against the registry they are part of, because a
		// preset's tokens name operations and other presets.
		problems.extend(registry.preset_problems(label));

		if !problems.is_empty() {
			return Err(Diagnoses::new(problems));
		}

		Ok(registry)
	}

	/// What the configuration said in passing. Not errors: each one is a decision
	/// resolved in the user's favour and reported rather than hidden.
	pub fn warnings(&self) -> &[Diagnostic] {
		&self.warnings
	}

	/// Backend names in preference order, `docs/config.md` §8.
	pub fn backends(&self) -> &[String] {
		&self.backends
	}

	/// Undo retention, `docs/config.md` §9.
	pub fn undo(&self) -> &crate::undo::Policy {
		&self.undo
	}

	/// Whether provenance is written onto the file, `docs/config.md` §10.
	pub fn provenance(&self) -> crate::provenance::Policy {
		self.provenance
	}

	pub fn preset(&self, name: &str) -> Option<&Preset> {
		let name = name.to_ascii_lowercase();
		self.presets.get(&name)
	}

	/// Every preset, in name order.
	pub fn presets(&self) -> impl Iterator<Item = &Preset> {
		self.presets.values()
	}

	pub fn tool(&self, name: &str) -> Option<&Tool> {
		self.tools.get(name)
	}

	/// Every tool, in name order.
	pub fn tools(&self) -> impl Iterator<Item = &Tool> {
		self.tools.values()
	}

	pub fn format(&self, name: &str) -> Option<&Format> {
		self.formats.get(&name.to_ascii_lowercase())
	}

	/// The canonical spelling of a format name or alias, `docs/config.md` §4.
	/// A name is accepted in any spelling it is declared under and printed in
	/// one, which is why the grammar can write `jpeg` and the journal says `jpg`.
	pub fn canonical_format(&self, name: &str) -> Option<&str> {
		let lowered = name.to_ascii_lowercase();
		self.format_names
			.iter()
			.find(|(spelling, _)| *spelling == lowered)
			.map(|(_, canonical)| canonical.as_str())
	}

	/// Split a file name into its stem and its format, `docs/grammar.md` §3.
	///
	/// The registry is the list that decides this, the way a public suffix list
	/// decides where a domain's registrable label ends: `archive.tar.gz` has the
	/// format `tar.gz` and the stem `archive` when both `gz` and `tar.gz` are
	/// declared, and the longest declared suffix wins. A name whose suffix is not
	/// a declared format has no format, so it is not ours to convert.
	pub fn split<'a>(&'a self, name: &'a str) -> Option<(&'a str, &'a str)> {
		if name.starts_with('.') {
			return None;
		}

		let lower = name.to_ascii_lowercase();

		for (spelling, canonical) in &self.format_names {
			let Some(stem) = lower
				.strip_suffix(spelling.as_str())
				.and_then(|rest| rest.strip_suffix('.'))
			else {
				continue;
			};

			if stem.is_empty() || stem.starts_with('.') {
				continue;
			}

			return Some((&name[..stem.len()], canonical.as_str()));
		}

		None
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
			let target = self.target(name)?;
			return Some(Resolved {
				target,
				value: Some(value),
			});
		}

		let lowered = text.to_ascii_lowercase();
		for (name, key) in &self.names {
			if let Some(rest) = lowered.strip_prefix(name.as_str()) {
				let target = self.target(key)?;
				return Some(Resolved {
					target,
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

	/// The entity a name or key stands for: a preset first, since presets and
	/// operations cannot share a name, `docs/config.md` V6.
	/// V7: a preset's tokens have to name something, must not smuggle a control
	/// token, and must not form a cycle.
	fn preset_problems(&self, label: &str) -> Vec<Error> {
		let mut problems = Vec::new();

		for preset in self.presets.values() {
			let has_value = preset.tokens.iter().any(|token| token.contains("{value}"));

			if preset.param.is_some() != has_value {
				problems.push(fail(
					Code::CfgValidation,
					format!(
						"{label}: preset '{}' declares {} parameter but its tokens {} use one",
						preset.key,
						match preset.param.is_some() {
							true => "a",
							false => "no",
						},
						match has_value {
							true => "do",
							false => "do not",
						}
					),
				));
			}

			for text in &preset.tokens {
				let token = match crate::intent::parse_token(text) {
					Ok(token) => token,
					Err(error) => {
						problems.push(Error::Diagnostic(Diagnostic::global(
							Code::CfgValidation,
							format!(
								"{label}: preset '{}' has a token that does not parse: {error}",
								preset.key
							),
						)));
						continue;
					}
				};

				match self.resolve(&token.name) {
					None => problems.push(fail(
						Code::CfgValidation,
						format!(
							"{label}: preset '{}' names '{}', which is not an operation or a preset",
							preset.key, token.name
						),
					)),
					Some(found) => match found.target {
						Target::Op(op) if op.key == "ask" || op.key == "now" => problems.push(fail(
							Code::CfgValidation,
							format!(
								"{label}: preset '{}' uses '{}', which is control rather than conversion",
								preset.key, op.name
							),
						)),
						_ => {}
					},
				}
			}
		}

		for preset in self.presets.values() {
			if let Some(cycle) = self.preset_cycle(&preset.key, &mut Vec::new()) {
				problems.push(fail(
					Code::CfgValidation,
					format!("{label}: presets form a cycle: {}", cycle.join(" -> ")),
				));
			}
		}

		problems
	}

	/// The path of presets that leads back to `start`, if there is one.
	fn preset_cycle(&self, start: &str, path: &mut Vec<String>) -> Option<Vec<String>> {
		if path.iter().any(|seen| seen == start) {
			path.push(start.to_string());
			return Some(path.clone());
		}

		path.push(start.to_string());

		let Some(preset) = self.presets.get(start) else {
			path.pop();
			return None;
		};

		for text in &preset.tokens {
			let Ok(token) = crate::intent::parse_token(text) else {
				continue;
			};

			if let Some(Target::Preset(next)) = self.resolve(&token.name).map(|found| found.target) {
				if let Some(cycle) = self.preset_cycle(&next.key, path) {
					return Some(cycle);
				}
			}
		}

		path.pop();
		None
	}

	fn target(&self, key: &str) -> Option<Target<'_>> {
		if let Some(preset) = self.presets.get(key) {
			return Some(Target::Preset(preset));
		}

		self.ops.get(key).map(Target::Op)
	}

	pub fn formats(&self) -> impl Iterator<Item = (&String, &Format)> {
		self.formats.iter()
	}

	pub fn ops(&self) -> impl Iterator<Item = (&String, &Op)> {
		self.ops.iter()
	}

	/// Every name and alias, for suggestions when one is misspelled.
	pub fn names(&self) -> impl Iterator<Item = &str> {
		self.names.iter().map(|(name, _)| name.as_str())
	}
}

/// V9 and V6 for the backend list: every entry has to name something that
/// exists, and naming one twice would make the order ambiguous.
fn validate_backends(backends: &[String], tool_names: &BTreeSet<&str>, label: &str) -> Vec<Error> {
	let mut problems = Vec::new();
	let mut seen: BTreeSet<&str> = BTreeSet::new();

	for name in backends {
		if !tool_names.contains(name.as_str()) {
			problems.push(fail(
				Code::CfgValidation,
				format!("{label}: backends.prefer names '{name}', which is not a declared tool"),
			));
		}

		if !seen.insert(name.as_str()) {
			problems.push(fail(
				Code::CfgValidation,
				format!("{label}: backends.prefer names '{name}' twice"),
			));
		}
	}

	problems
}

/// A policy knob that counts time is read as the grammar's `time` class, so a
/// configuration spells a duration the same way an intent does,
/// `docs/grammar.md` §7.
fn duration(text: &str) -> std::result::Result<std::time::Duration, crate::value::Fault> {
	let param = Param {
		class: Class::Time,
		min: None,
		max: None,
		values: Vec::new(),
		scalar: None,
	};

	match crate::value::interpret(&param, text) {
		Ok(crate::value::Value::Time(seconds)) => Ok(std::time::Duration::from_secs(seconds)),
		Ok(_) => Err(crate::value::Fault {
			code: Code::ValueSyntax,
			message: "is not a duration".to_string(),
		}),
		Err(fault) => Err(fault),
	}
}

/// V1 and V9: a fragment may carry `{value}` and `{engine.<name>}`, both of which
/// belong to the argument list, and must name a tool that exists.
fn validate_fragments(
	owner: &str,
	argv: &BTreeMap<String, Fragment>,
	tools: &BTreeMap<String, Tool>,
	tool_names: &BTreeSet<&str>,
	has_param: bool,
	label: &str,
) -> Vec<Error> {
	let mut problems = Vec::new();

	for (tool, fragment) in argv {
		if !tool_names.contains(tool.as_str()) {
			problems.push(fail(
				Code::CfgValidation,
				format!("{label}: {owner} declares argv for unknown tool '{tool}'"),
			));
			continue;
		}

		let engines = &tools[tool].engines;

		for argument in &fragment.args {
			for placeholder in placeholders(argument) {
				match placeholder {
					"value" if has_param => {}
					"value" => problems.push(fail(
						Code::CfgValidation,
						format!("{label}: {owner} uses {{value}} but declares no parameter"),
					)),
					"input" | "ops" | "output" => problems.push(fail(
						Code::CfgValidation,
						format!("{label}: {owner} names {{{placeholder}}} in an argv fragment, which is a layout hole"),
					)),
					other => match other.strip_prefix("engine.") {
						Some(engine) if engines.contains_key(engine) => {}
						Some(engine) => problems.push(fail(
							Code::CfgValidation,
							format!(
								"{label}: {owner} uses {{engine.{engine}}} but tool '{tool}' declares no such engine"
							),
						)),
						None => problems.push(fail(
							Code::CfgValidation,
							format!("{label}: {owner} uses unknown placeholder '{{{other}}}'"),
						)),
					},
				}
			}
		}
	}

	problems
}

/// Every `{name}` in an argument, without its braces.
fn placeholders(argument: &str) -> Vec<&str> {
	let mut found = Vec::new();
	let mut rest = argument;

	while let Some(open) = rest.find('{') {
		let Some(close) = rest[open..].find('}') else {
			break;
		};

		found.push(&rest[open + 1..open + close]);
		rest = &rest[open + close + 1..];
	}

	found
}

/// Placeholders a layout may contain, `docs/config.md` V1. `{value}` and
/// `{engine.<name>}` belong to an `argv` fragment, which is applied where an
/// argument list is built rather than where a path goes.
const LAYOUT_PLACEHOLDERS: &[&str] = &["{input}", "{ops}", "{output}"];

fn validate_tool(tool: &Tool, label: &str) -> Vec<Error> {
	let mut problems = Vec::new();
	let name = &tool.name;

	// V16: a component is checked against probe output, so a tool that claims one
	// without a probe has claimed something nothing can verify.
	if !tool.components.is_empty() && tool.probe.is_empty() {
		problems.push(fail(
			Code::CfgValidation,
			format!("{label}: tool '{name}' declares components but no probe to read them from"),
		));
	}

	// V5: every command is exec'd, never handed to a shell, and a name may
	// appear only once across the preferred command and its aliases.
	let mut seen = BTreeSet::new();
	for (kind, command) in
		std::iter::once(("command", &tool.command)).chain(tool.aliases.iter().map(|alias| ("alias", alias)))
	{
		if !seen.insert(command.as_str()) {
			problems.push(fail(
				Code::CfgValidation,
				format!("{label}: tool '{name}' names '{command}' more than once as command or alias"),
			));
		}

		problems.extend(validate_command(tool, label, kind, command));
	}

	// V1 and V2: the layout is a template with three holes, and `{ops}` is one
	// argument because the fragment expands into that position.
	let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
	for argument in &tool.layout {
		if LAYOUT_PLACEHOLDERS.contains(&argument.as_str()) {
			*counts.entry(argument.as_str()).or_default() += 1;
		} else if argument.contains('{') || argument.contains('}') {
			problems.push(fail(
				Code::CfgValidation,
				format!(
					"{label}: tool '{name}' has placeholder '{argument}' in its layout, which takes only {LAYOUT_PLACEHOLDERS:?}"
				),
			));
		}
	}

	for placeholder in ["{input}", "{output}"] {
		if counts.get(placeholder).copied() != Some(1) {
			problems.push(fail(
				Code::CfgValidation,
				format!("{label}: tool '{name}' must name {placeholder} exactly once in its layout"),
			));
		}
	}

	if counts.get("{ops}").copied() != Some(1) {
		problems.push(fail(
			Code::CfgValidation,
			format!("{label}: tool '{name}' must name {{ops}} exactly once in its layout"),
		));
	}

	// V4, the half a declaration alone can violate: an engine with no candidate
	// can never resolve. Whether something uses it needs the argv fragments.
	for (engine, candidates) in &tool.engines {
		if candidates.is_empty() {
			problems.push(fail(
				Code::CfgValidation,
				format!("{label}: tool '{name}' declares engine '{engine}' with no candidates"),
			));
		}
	}

	problems
}

/// V5 for one command or alias.
fn validate_command(tool: &Tool, label: &str, kind: &str, command: &str) -> Vec<Error> {
	let name = &tool.name;

	if command.is_empty() || command.contains(char::is_whitespace) {
		return vec![fail(
			Code::CfgValidation,
			format!("{label}: tool '{name}' has {kind} '{command}', which is empty or contains whitespace"),
		)];
	}

	match command.chars().find(|c| SHELL_METACHARACTERS.contains(c)) {
		Some(byte) => vec![fail(
			Code::CfgValidation,
			format!("{label}: tool '{name}' has {kind} '{command}', which contains the shell metacharacter '{byte}'"),
		)],
		None => Vec::new(),
	}
}

/// V5: characters that only mean something to a shell, which would make the
/// command a program rather than a name.
const SHELL_METACHARACTERS: &[char] = &[
	';', '|', '&', '$', '<', '>', '`', '(', ')', '\\', '\'', '"', '*', '?', '[', ']', '#', '~',
];

fn collect_format_names(formats: &BTreeMap<String, Format>) -> Vec<(String, String)> {
	let mut names = Vec::new();

	for (canonical, format) in formats {
		names.push((canonical.clone(), canonical.clone()));
		for alias in &format.aliases {
			names.push((alias.to_ascii_lowercase(), canonical.clone()));
		}
	}

	names
}

/// Keys a user may change on an operation that already exists. Everything else
/// about it is the published grammar, so changing it would make two machines
/// disagree about the same intent string, `docs/config.md` §2.
const OPERATION_BEHAVIOUR: &[&str] = &["argv", "say"];

/// What a user file may not do: redefine an operation's identity, or replace a
/// shipped preset without being told.
fn shadowing(base: &toml::Value, over: &toml::Value, label: &str, warnings: &mut Vec<Diagnostic>) -> Vec<Error> {
	let mut problems = Vec::new();

	let base_ops = table(base, "ops");
	let over_ops = table(over, "ops");

	for (key, entry) in &over_ops {
		if !base_ops.contains_key(key) {
			continue;
		}

		let Some(changed) = entry.as_table() else {
			continue;
		};

		for name in changed.keys() {
			if !OPERATION_BEHAVIOUR.contains(&name.as_str()) && name != "enabled" {
				problems.push(fail(
					Code::CfgValidation,
					format!(
						"{label}: operation '{key}' may not redefine '{name}': the grammar is a published contract, so only {} may differ",
						OPERATION_BEHAVIOUR.join(" and ")
					),
				));
			}
		}
	}

	let base_presets = table(base, "presets");
	for key in table(over, "presets").keys() {
		if base_presets.contains_key(key) {
			warnings.push(Diagnostic::global(
				Code::CfgShadowed,
				format!("{label}: preset '{key}' replaces the shipped one"),
			));
		}
	}

	problems
}

fn table(value: &toml::Value, path: &str) -> toml::value::Table {
	value
		.get(path)
		.and_then(toml::Value::as_table)
		.cloned()
		.unwrap_or_default()
}

/// A tool the user removed cannot stay in the preference list. Dropping it is the
/// only reading that does not force a user who disables one tool to restate the
/// whole order, and it is narrow: a name that was never declared still fails V9.
fn prune_preference(before: &toml::Value, merged: &mut toml::Value) {
	let names = |value: &toml::Value| -> Vec<String> { table(value, "tools").keys().cloned().collect() };

	let removed: Vec<String> = names(before)
		.into_iter()
		.filter(|tool| !names(merged).contains(tool))
		.collect();

	if removed.is_empty() {
		return;
	}

	if let Some(prefer) = merged
		.get_mut("backends")
		.and_then(|backends| backends.get_mut("prefer"))
		.and_then(toml::Value::as_array_mut)
	{
		prefer.retain(|name| {
			name.as_str()
				.is_none_or(|name| !removed.iter().any(|removed| removed == name))
		});
	}
}

/// Merge `over` into `base`, key by key for tables, whole for arrays and scalars,
/// with `enabled = false` removing an entry, `docs/config.md` §2.
fn merge(base: &mut toml::Value, over: &toml::Value) {
	match (base, over) {
		(toml::Value::Table(base), toml::Value::Table(over)) => {
			for (key, value) in over {
				let disables = value
					.get("enabled")
					.and_then(toml::Value::as_bool)
					.is_some_and(|enabled| !enabled);

				if disables {
					base.remove(key);
					continue;
				}

				match base.get_mut(key) {
					Some(existing) => merge(existing, value),
					None => {
						base.insert(key.clone(), value.clone());
					}
				}
			}
		}
		// An array or a scalar replaces what was there rather than joining it.
		(base, over) => *base = over.clone(),
	}
}

fn collect_preset_names(presets: &BTreeMap<String, Preset>) -> Vec<(String, String)> {
	let mut names = Vec::new();

	for (key, preset) in presets {
		names.push((preset.name.clone(), key.clone()));
		names.push((key.clone(), key.clone()));
	}

	names
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
	tools: BTreeMap<String, ToolEntry>,
	#[serde(default)]
	backends: Option<BackendsEntry>,
	#[serde(default)]
	presets: BTreeMap<String, PresetEntry>,
	#[serde(default)]
	formats: BTreeMap<String, FormatEntry>,
	#[serde(default)]
	ops: BTreeMap<String, OpEntry>,
	#[serde(default)]
	undo: Option<UndoEntry>,
	#[serde(default)]
	provenance: Option<ProvenanceEntry>,
}

#[derive(Debug, Deserialize)]
struct ProvenanceEntry {
	#[serde(default)]
	xattr: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct UndoEntry {
	#[serde(default)]
	enabled: Option<bool>,
	#[serde(default)]
	retention: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PresetEntry {
	#[serde(default)]
	name: Option<String>,
	#[serde(default)]
	param: Option<Param>,
	tokens: Vec<String>,
}

impl PresetEntry {
	fn view(&self, key: &str) -> Preset {
		Preset {
			key: key.to_ascii_lowercase(),
			name: self.name.clone().unwrap_or_else(|| key.to_ascii_lowercase()),
			param: self.param.clone(),
			tokens: self.tokens.clone(),
		}
	}
}

#[derive(Debug, Deserialize)]
struct BackendsEntry {
	prefer: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ToolEntry {
	command: String,
	#[serde(default)]
	aliases: Vec<String>,
	#[serde(default)]
	probe: Vec<String>,
	#[serde(default)]
	version_min: Option<String>,
	#[serde(default)]
	purpose: String,
	#[serde(default)]
	components: Vec<String>,
	layout: Vec<String>,
	#[serde(default)]
	engines: BTreeMap<String, Vec<String>>,
}

impl ToolEntry {
	/// V11: a floor that does not parse would be compared as text, where `9`
	/// sorts above `10`.
	fn view(&self, name: &str) -> Result<Tool, Error> {
		let version_min = match &self.version_min {
			Some(text) => Some(Version::parse(text).ok_or_else(|| {
				fail(
					Code::CfgValidation,
					format!("tool '{name}' declares version_min '{text}', which is not a version"),
				)
			})?),
			None => None,
		};

		Ok(Tool {
			name: name.to_string(),
			command: self.command.clone(),
			aliases: self.aliases.clone(),
			probe: self.probe.clone(),
			version_min,
			purpose: self.purpose.clone(),
			components: self.components.clone(),
			layout: self.layout.clone(),
			engines: self.engines.clone(),
		})
	}
}

#[derive(Debug, Deserialize)]
struct FormatEntry {
	family: String,
	lossy: bool,
	alpha: bool,
	tier: Tier,
	#[serde(default)]
	aliases: Vec<String>,
	#[serde(default)]
	argv: BTreeMap<String, FragmentEntry>,
}

#[derive(Debug, Deserialize)]
struct FragmentEntry {
	#[serde(default)]
	args: Vec<String>,
}

impl FragmentEntry {
	fn view(&self) -> Fragment {
		Fragment {
			args: self.args.clone(),
		}
	}
}

impl FormatEntry {
	fn view(&self) -> Format {
		Format {
			family: self.family.clone(),
			lossy: self.lossy,
			alpha: self.alpha,
			tier: self.tier,
			aliases: self.aliases.clone(),
			argv: self
				.argv
				.iter()
				.map(|(tool, entry)| (tool.clone(), entry.view()))
				.collect(),
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
	#[serde(default)]
	argv: BTreeMap<String, FragmentEntry>,
	#[serde(default)]
	say: Option<String>,
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
			argv: self
				.argv
				.iter()
				.map(|(tool, entry)| (tool.clone(), entry.view()))
				.collect(),
			say: self.say.clone(),
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
	fn undo_policy_defaults_and_reads_its_knobs() {
		let registry = Registry::load(HEADER).unwrap();
		assert!(registry.undo().enabled);
		assert_eq!(
			registry.undo().retention,
			std::time::Duration::from_secs(7 * 24 * 60 * 60)
		);

		let registry = Registry::load(&format!("{HEADER}\n[undo]\nenabled = false\nretention = '720h'\n")).unwrap();
		assert!(!registry.undo().enabled);
		assert_eq!(registry.undo().retention, std::time::Duration::from_secs(720 * 60 * 60));
	}

	#[test]
	fn provenance_defaults_on_and_can_be_turned_off() {
		assert!(Registry::load(HEADER).unwrap().provenance().xattr);
		assert!(
			!Registry::load(&format!("{HEADER}\n[provenance]\nxattr = false\n"))
				.unwrap()
				.provenance()
				.xattr
		);
	}

	#[test]
	fn a_retention_that_is_not_a_duration_is_refused() {
		let codes = codes(&format!("{HEADER}\n[undo]\nretention = '7x'\n"));

		assert_eq!(codes, vec![Code::ValueSyntax]);
	}

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
		assert_eq!(resolved.op().unwrap().key, "quality");
		assert_eq!(resolved.value, Some("85"));

		let alias = registry.resolve("q80").unwrap();
		assert_eq!(alias.op().unwrap().key, "quality");
		assert_eq!(alias.value, Some("80"));
	}

	#[test]
	fn an_equals_forces_the_exact_name() {
		let registry = builtin();

		let resolved = registry.resolve("quality=85").unwrap();
		assert_eq!(resolved.op().unwrap().key, "quality");
		assert_eq!(resolved.value, Some("85"));
	}

	#[test]
	fn a_name_without_a_value_resolves_without_one() {
		let registry = builtin();

		assert_eq!(registry.resolve("nometa").unwrap().value, None);
		assert_eq!(registry.resolve("grey").unwrap().op().unwrap().key, "gray");
		assert!(registry.resolve("zzz").is_none());
	}

	#[test]
	fn a_reserved_operation_is_known_but_not_implemented() {
		let registry = builtin();
		let resolved = registry.resolve("trim1h30s").unwrap();

		assert_eq!(resolved.op().unwrap().key, "trim");
		assert!(!resolved.op().unwrap().is_implemented());
	}

	#[test]
	fn an_operation_applies_only_to_the_formats_it_names() {
		let registry = builtin();

		assert!(registry.op("quality").unwrap().applies_to("jpg"));
		assert!(!registry.op("quality").unwrap().applies_to("png"));
		assert!(registry.op("cap").unwrap().applies_to("png"));
	}

	#[test]
	fn a_name_splits_at_the_longest_declared_format() {
		let registry = builtin();

		assert_eq!(registry.split("photo.jpg"), Some(("photo", "jpg")));
		assert_eq!(
			registry.split("photo.jpeg"),
			Some(("photo", "jpg")),
			"an alias canonicalises"
		);
		assert_eq!(
			registry.split("Photo.JPG"),
			Some(("Photo", "jpg")),
			"matching ignores case"
		);
		assert_eq!(registry.split("my.file.txt"), None, "txt is not a declared format");
	}

	#[test]
	fn the_builtin_declares_the_tools_the_matrix_names() {
		let registry = builtin();
		let names: Vec<&str> = registry.tools().map(|tool| tool.name.as_str()).collect();

		assert_eq!(names, ["7zz", "ffmpeg", "magick", "pandoc"]);

		let pandoc = registry.tool("pandoc").expect("pandoc is declared");
		assert_eq!(pandoc.candidates("pdf").first().map(String::as_str), Some("typst"));
		assert!(pandoc.candidates("absent").is_empty());

		let sevenzip = registry.tool("7zz").expect("7zz is declared");
		assert_eq!(
			sevenzip.aliases,
			["7z", "7za"],
			"the 7-Zip executables are interchangeable"
		);
	}

	#[test]
	fn a_layout_without_the_three_holes_is_refused() {
		let missing_ops = with_tool("command = 'probe'\nlayout = ['{input}', '{output}']");
		let repeated_ops = with_tool("command = 'probe'\nlayout = ['{input}', '{ops}', '{ops}', '{output}']");
		let wrong_hole = with_tool("command = 'probe'\nlayout = ['{input}', '{ops}', '{output}', '{value}']");

		assert_eq!(codes(&missing_ops), [Code::CfgValidation]);
		assert_eq!(codes(&repeated_ops), [Code::CfgValidation]);
		assert_eq!(codes(&wrong_hole), [Code::CfgValidation]);
	}

	#[test]
	fn a_command_with_a_shell_metacharacter_is_refused() {
		assert_eq!(
			codes(&with_tool(&command_tool("magick; rm -rf /"))),
			[Code::CfgValidation]
		);
		assert_eq!(codes(&with_tool(&command_tool("magick $(id)"))), [Code::CfgValidation]);
		assert_eq!(codes(&with_tool(&command_tool(""))), [Code::CfgValidation]);
		assert_eq!(
			codes(&with_tool(&command_tool("magick -density 300"))),
			[Code::CfgValidation]
		);
	}

	#[test]
	fn a_command_alias_with_a_shell_metacharacter_is_refused() {
		let text = with_tool(&format!("{STANDARD_TOOL}\naliases = ['7z; rm -rf /']"));

		assert_eq!(codes(&text), [Code::CfgValidation]);
	}

	#[test]
	fn a_command_cannot_also_be_an_alias() {
		let text = with_tool(&format!("{STANDARD_TOOL}\naliases = ['probe']"));

		assert_eq!(codes(&text), [Code::CfgValidation]);
	}

	#[test]
	fn tool_aliases_keep_their_order() {
		let text = with_tool(&format!("{STANDARD_TOOL}\naliases = ['7z', '7za']"));
		let registry = Registry::load(&text).expect("loads");
		let tool = registry.tool("probe").expect("declared");

		assert_eq!(tool.aliases, ["7z", "7za"]);
		assert_eq!(tool.commands().collect::<Vec<_>>(), ["probe", "7z", "7za"]);
	}

	#[test]
	fn a_version_floor_that_does_not_parse_is_refused() {
		assert_eq!(
			codes(&with_tool(&format!("{STANDARD_TOOL}\nversion_min = 'newest'"))),
			[Code::CfgValidation]
		);
	}

	#[test]
	fn an_engine_with_no_candidate_is_refused() {
		assert_eq!(
			codes(&with_tool(&format!("{STANDARD_TOOL}\nengines = {{ pdf = [] }}"))),
			[Code::CfgValidation],
			"an engine that can never resolve is a declaration error"
		);
	}

	#[test]
	fn a_declared_engine_keeps_its_order() {
		let text = with_tool(&format!(
			"{STANDARD_TOOL}\nengines = {{ pdf = ['typst', 'weasyprint'] }}"
		));
		let registry = Registry::load(&text).expect("loads");
		let tool = registry.tool("probe").expect("declared");

		assert_eq!(tool.candidates("pdf"), ["typst", "weasyprint"]);
	}

	/// A configuration with one tool, so that a single declaration rule can be
	/// violated on its own.
	fn with_tool(body: &str) -> String {
		format!("version = 'retype-config-1'\n[tools.probe]\n{body}\n")
	}

	const STANDARD_TOOL: &str = "command = 'probe'\nlayout = ['{input}', '{ops}', '{output}']";

	fn command_tool(command: &str) -> String {
		format!("command = '{command}'\nlayout = ['{{input}}', '{{ops}}', '{{output}}']")
	}

	/// Two tools, so that a fragment always has a real tool to name.
	const TOOLS: &str = "\
version = 'retype-config-1'

[tools.magick]
command = 'magick'
layout = ['{input}', '{ops}', '{output}']

[tools.pandoc]
command = 'pandoc'
layout = ['{input}', '{ops}', '{output}']
engines = { pdf = ['weasyprint'] }

";

	fn config(extra: &str) -> Result<Registry, Vec<Code>> {
		Registry::load(&[TOOLS, extra].concat()).map_err(|problems| problems.codes())
	}

	const WEBP: &str = "\
[formats.webp]
family = 'image'
lossy = true
alpha = true
tier = 'converted'

";

	#[test]
	fn a_format_fragment_is_kept_verbatim() {
		let text = [
			WEBP,
			"[formats.webp.argv.magick]\nargs = ['-quality', '-define', 'webp:method=6']\n",
		]
		.concat();

		let registry = config(&text).expect("loads");
		let format = registry.format("webp").expect("declared");
		let fragment = format.fragment("magick").expect("declared for magick");

		assert_eq!(fragment.args, ["-quality", "-define", "webp:method=6"]);
		assert!(!fragment.uses_value(), "a format has no parameter to interpolate");
	}

	#[test]
	fn a_parameter_must_be_interpolated_by_some_fragment() {
		let unused = [
			WEBP,
			"[ops.quality]\nslot = 5\nparam = { class = 'int' }\n",
			"[ops.quality.argv.magick]\nargs = ['-quality']\n",
		]
		.concat();
		assert_eq!(config(&unused).unwrap_err(), [Code::CfgValidation]);

		let used = [
			WEBP,
			"[ops.quality]\nslot = 5\nparam = { class = 'int' }\n",
			"[ops.quality.argv.magick]\nargs = ['-quality', '{value}']\n",
		]
		.concat();
		assert!(config(&used).is_ok());
	}

	#[test]
	fn a_value_needs_a_declared_parameter() {
		let text = [
			WEBP,
			"[ops.quality]\nslot = 5\n",
			"[ops.quality.argv.magick]\nargs = ['-quality', '{value}']\n",
		]
		.concat();
		assert_eq!(config(&text).unwrap_err(), [Code::CfgValidation]);

		let format_value = [WEBP, "[formats.webp.argv.magick]\nargs = ['{value}']\n"].concat();
		assert_eq!(config(&format_value).unwrap_err(), [Code::CfgValidation]);
	}

	#[test]
	fn a_layout_hole_is_not_a_fragment_placeholder() {
		let text = [WEBP, "[formats.webp.argv.magick]\nargs = ['{input}']\n"].concat();
		let problems = config(&text).unwrap_err();
		assert!(!problems.is_empty(), "a layout hole in a fragment is refused");
		assert!(problems.iter().all(|code| *code == Code::CfgValidation), "{problems:?}");

		let ops = [WEBP, "[formats.webp.argv.magick]\nargs = ['{ops}']\n"].concat();
		assert!(config(&ops).is_err(), "{{ops}} expands only in a layout");
	}

	#[test]
	fn an_unknown_placeholder_is_refused() {
		let text = [
			WEBP,
			"[formats.webp.argv.magick]\nargs = ['-size', '{width}x{height}']\n",
		]
		.concat();

		let problems = config(&text).unwrap_err();
		assert_eq!(problems.len(), 2, "one per placeholder");
		assert!(problems.iter().all(|code| *code == Code::CfgValidation));
	}

	#[test]
	fn the_builtin_states_its_preference() {
		let registry = builtin();

		assert_eq!(registry.backends(), ["magick", "ffmpeg", "pandoc", "7zz"]);
	}

	#[test]
	fn a_backend_that_is_not_declared_is_refused() {
		let unknown = "[backends]\nprefer = ['magick', 'ghostscript']\n";
		assert_eq!(config(unknown).unwrap_err(), [Code::CfgValidation]);

		let twice = "[backends]\nprefer = ['magick', 'magick']\n";
		assert_eq!(config(twice).unwrap_err(), [Code::CfgValidation]);
	}

	#[test]
	fn a_declared_preference_is_kept_in_order() {
		let registry = config("[backends]\nprefer = ['pandoc', 'magick']\n").expect("loads");

		assert_eq!(registry.backends(), ["pandoc", "magick"]);
	}

	#[test]
	fn a_fragment_naming_an_undeclared_tool_is_refused() {
		let text = [WEBP, "[formats.webp.argv.ffmpeg]\nargs = ['-c:v', 'libwebp']\n"].concat();
		assert_eq!(config(&text).unwrap_err(), [Code::CfgValidation]);
	}

	#[test]
	fn an_engine_placeholder_needs_a_declared_engine() {
		let declared = [
			WEBP,
			"[ops.pdf]\nslot = 0\nformats = ['webp']\n",
			"[ops.pdf.argv.pandoc]\nargs = ['--pdf-engine', '{engine.pdf}']\n",
		]
		.concat();
		assert!(config(&declared).is_ok());

		let absent = [
			WEBP,
			"[ops.pdf]\nslot = 0\nformats = ['webp']\n",
			"[ops.pdf.argv.pandoc]\nargs = ['--pdf-engine', '{engine.tex}']\n",
		]
		.concat();
		assert_eq!(config(&absent).unwrap_err(), [Code::CfgValidation]);
	}

	#[test]
	fn a_placeholder_may_sit_inside_an_argument() {
		let text = [
			WEBP,
			"[ops.quality]\nslot = 5\nparam = { class = 'int' }\n",
			"[ops.quality.argv.magick]\nargs = ['-quality={value}']\n",
		]
		.concat();

		let registry = config(&text).expect("loads");
		let fragment = registry
			.op("quality")
			.and_then(|op| op.fragment("magick"))
			.expect("declared");

		assert_eq!(fragment.args, ["-quality={value}"]);
		assert!(fragment.uses_value());
	}

	#[test]
	fn a_longer_format_wins_over_a_shorter_one_that_also_matches() {
		let text = format!(
			r#"{HEADER}
[formats.gz]
family = 'archive'
lossy = false
alpha = false
tier = 'exact'

[formats.twogz]
family = 'archive'
lossy = false
alpha = false
tier = 'exact'
aliases = ['tar.gz']

[ops.quality]
slot = 50
formats = ['gz']
"#
		);
		let registry = Registry::load(&text).unwrap();

		assert_eq!(registry.split("archive.tar.gz"), Some(("archive", "twogz")));
		assert_eq!(registry.split("archive.gz"), Some(("archive", "gz")));
	}

	#[test]
	fn a_name_without_a_declared_format_has_no_format() {
		let registry = builtin();

		assert_eq!(registry.split("README"), None);
		assert_eq!(registry.split("photo.xyz"), None);
		assert_eq!(registry.split(".hidden"), None);
		assert_eq!(registry.split("jpg"), None, "a bare format is not a name");
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
	/// The built-in table with a user file merged over it, as `retype` loads it.
	fn merged(user: &str) -> Result<Registry, Vec<Code>> {
		Registry::load_layers(include_str!("../builtin.toml"), user, "retype.toml").map_err(|problems| problems.codes())
	}

	#[test]
	fn a_user_may_change_how_a_shipped_operation_runs() {
		let registry = merged(
			"\
version = 'retype-config-1'

[ops.quality.argv.magick]
args = ['-quality', '{value}', '-define', 'jpeg:dct-method=float']
",
		)
		.expect("loads");

		let op = registry.op("quality").expect("declared");
		assert_eq!(
			op.fragment("magick").expect("magick fragment").args,
			["-quality", "{value}", "-define", "jpeg:dct-method=float"]
		);
		assert_eq!(op.slot, 50, "the identity is untouched");
	}

	#[test]
	fn a_user_may_not_redefine_an_operation_identity() {
		assert_eq!(
			merged("version = 'retype-config-1'\n\n[ops.quality]\nslot = 10\n").unwrap_err(),
			[Code::CfgValidation]
		);
		assert_eq!(
			merged("version = 'retype-config-1'\n\n[ops.quality]\nformats = ['png']\n").unwrap_err(),
			[Code::CfgValidation]
		);
	}

	#[test]
	fn a_user_may_add_a_tool_and_an_operation() {
		let registry = merged(
			"\
version = 'retype-config-1'

[tools.blurrer]
command = 'blurrer'
layout = ['{input}', '{ops}', '{output}']

[ops.blur]
slot = 20
formats = ['webp']
param = { class = 'int', min = 1, max = 100 }
say = 'blur by {value}'

[ops.blur.argv.blurrer]
args = ['--blur', '{value}']
",
		)
		.expect("loads");

		assert!(registry.tool("blurrer").is_some());
		assert!(registry.op("blur").is_some());
	}

	#[test]
	fn enabled_false_removes_what_was_shipped() {
		let registry = merged(
			"\
version = 'retype-config-1'

[ops.nometa]
enabled = false

[tools.pandoc]
enabled = false
",
		)
		.expect("loads");

		assert!(registry.op("nometa").is_none());
		assert!(registry.tool("pandoc").is_none());
		assert!(
			!registry.backends().iter().any(|name| name == "pandoc"),
			"a removed tool leaves the preference list with it"
		);
	}

	#[test]
	fn an_array_is_replaced_rather_than_joined() {
		let registry =
			merged("version = 'retype-config-1'\n\n[ops.quality.argv.magick]\nargs = ['-quality', '{value}']\n")
				.expect("loads");

		let op = registry.op("quality").expect("declared");
		assert_eq!(op.fragment("magick").expect("fragment").args, ["-quality", "{value}"]);
	}

	#[test]
	fn a_user_file_must_declare_its_version() {
		assert_eq!(merged("[ops.quality]\n").unwrap_err(), [Code::CfgVersion]);
		assert_eq!(merged("version = 'retype-config-2'\n").unwrap_err(), [Code::CfgVersion]);
	}

	#[test]
	fn replacing_a_shipped_preset_is_a_warning_rather_than_a_failure() {
		let registry = merged(
			"\
version = 'retype-config-1'

[presets.thumbnail]
tokens = ['cap={value}', 'quality90']
",
		)
		.expect("loads");

		assert_eq!(registry.warnings().len(), 1);
		assert_eq!(registry.warnings()[0].code, Code::CfgShadowed);
		assert_eq!(
			registry.preset("thumbnail").expect("declared").tokens,
			["cap={value}", "quality90"]
		);
	}

	#[test]
	fn a_user_operation_may_not_take_a_shipped_preset_name() {
		assert_eq!(
			merged("version = 'retype-config-1'\n\n[ops.thumbnail]\nslot = 20\nformats = ['webp']\n").unwrap_err(),
			[Code::CfgNameClash]
		);
	}

	#[test]
	fn a_preset_joins_the_name_space() {
		let text = [
			WEBP,
			"[ops.quality]\nslot = 50\nparam = { class = 'int' }\n",
			"[presets.quality]\ntokens = ['quality80']\n",
		]
		.concat();

		let problems = config(&text).unwrap_err();
		assert!(problems.contains(&Code::CfgNameClash), "{problems:?}");
	}

	#[test]
	fn a_preset_names_only_real_tokens() {
		let unknown = [WEBP, "[presets.look]\ntokens = ['blur5']\n"].concat();
		assert_eq!(config(&unknown).unwrap_err(), [Code::CfgValidation]);
	}

	#[test]
	fn a_preset_may_not_smuggle_a_control_token() {
		let text = [WEBP, "[presets.look]\ntokens = ['ask']\n"].concat();
		assert_eq!(config(&text).unwrap_err(), [Code::CfgValidation]);
	}

	#[test]
	fn a_preset_cycle_is_refused() {
		let text = [
			WEBP,
			"[presets.alpha]\ntokens = ['beta']\n",
			"[presets.beta]\ntokens = ['alpha']\n",
		]
		.concat();

		let problems = config(&text).unwrap_err();
		assert!(!problems.is_empty(), "a cycle is refused");
		assert!(problems.iter().all(|code| *code == Code::CfgValidation), "{problems:?}");
	}

	#[test]
	fn a_preset_parameter_must_match_its_tokens() {
		let quality = "\n[ops.quality]\nslot = 50\nparam = { class = 'int' }\n";
		let unused = [
			WEBP,
			quality,
			"[presets.look]\nparam = { class = 'int' }\ntokens = ['quality80']\n",
		]
		.concat();
		assert_eq!(config(&unused).unwrap_err(), [Code::CfgValidation]);

		let invented = [WEBP, quality, "[presets.look]\ntokens = ['quality={value}']\n"].concat();
		assert_eq!(config(&invented).unwrap_err(), [Code::CfgValidation]);

		let used = [
			WEBP,
			quality,
			"[presets.look]\nparam = { class = 'int' }\ntokens = ['quality={value}']\n",
		]
		.concat();
		assert!(config(&used).is_ok());
	}

	#[test]
	fn a_phrase_must_agree_with_the_parameter() {
		let with_value = [
			WEBP,
			"[ops.blur]\nslot = 20\nformats = ['webp']\nparam = { class = 'int' }\nsay = 'blur by {value}'\n",
		]
		.concat();
		assert!(config(&with_value).is_ok());

		let hides_value = [
			WEBP,
			"[ops.blur]\nslot = 20\nformats = ['webp']\nparam = { class = 'int' }\nsay = 'blur'\n",
		]
		.concat();
		assert_eq!(config(&hides_value).unwrap_err(), [Code::CfgValidation]);

		let invents_value = [
			WEBP,
			"[ops.blur]\nslot = 20\nformats = ['webp']\nsay = 'blur by {value}'\n",
		]
		.concat();
		assert_eq!(config(&invents_value).unwrap_err(), [Code::CfgValidation]);
	}

	#[test]
	fn a_phrase_takes_only_the_value_hole() {
		let unknown = [
			WEBP,
			"[ops.blur]\nslot = 20\nformats = ['webp']\nsay = 'blur by {amount}'\n",
		]
		.concat();
		assert_eq!(config(&unknown).unwrap_err(), [Code::CfgValidation]);
	}
}
