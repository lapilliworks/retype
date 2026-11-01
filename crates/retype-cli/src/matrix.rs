//! The pairs an installation can carry, and the samples they run on.
//!
//! Two commands need the same three things: the matrix of pairs this build can
//! convert, a compiled plan for one of them, and a file to run it on. `retype bench`
//! measures the machine with them; `retype fidelity` earns claims with them. Neither
//! invents a format list, because nothing about a format belongs in Rust.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use retype_core::plan::{self, Plan};
use retype_core::registry::Registry;
use retype_core::resolve::{self, Source as Origin};
use retype_core::{execute, intent, tools};

use crate::error::{Error, Result};

/// Why a `--pair` could not be read: the shape was wrong, or the name is not a
/// format. The two are different answers, so they are different exit codes.
pub enum PairError {
	Usage(String),
	Format(String),
}

/// What the user asked to measure: named pairs, or a sweep of families.
pub struct Selection<'a> {
	pub pairs: &'a [String],
	pub families: &'a [String],
}

impl Selection<'_> {
	/// The pairs to run: what the user named, or every target that compiles from
	/// each family's seed format.
	pub fn pairs(&self, registry: &Registry) -> std::result::Result<Vec<(String, String)>, PairError> {
		if !self.pairs.is_empty() {
			return self.pairs.iter().map(|pair| named(registry, pair)).collect();
		}

		let families: Vec<String> = match self.families.is_empty() {
			true => registry.bench().families().map(str::to_string).collect(),
			false => self.families.to_vec(),
		};

		let mut pairs = Vec::new();

		for family in families {
			let Some(source) = registry.bench().seed(&family).map(str::to_string) else {
				return Err(PairError::Usage(format!(
					"family '{family}' has no [bench.seed] format"
				)));
			};

			for (target, _) in registry.formats() {
				if target == &source || plan_for(registry, &source, target).is_none() {
					continue;
				}

				pairs.push((source.clone(), target.clone()));
			}
		}

		Ok(pairs)
	}
}

fn named(registry: &Registry, pair: &str) -> std::result::Result<(String, String), PairError> {
	let Some((source, target)) = pair.split_once(':') else {
		return Err(PairError::Usage(format!(
			"'{pair}' is not a pair: write it as source:target, such as heic:jpg"
		)));
	};

	let canonical = |name: &str| {
		registry
			.canonical_format(name)
			.map(str::to_string)
			.ok_or_else(|| PairError::Format(format!("'{name}' is not a format this build has")))
	};

	Ok((canonical(source)?, canonical(target)?))
}

/// The plan a bare target format compiles to from `source`, which is the same plan
/// a rename would run.
pub fn plan_for(registry: &Registry, source: &str, target: &str) -> Option<Plan> {
	let parsed = intent::parse(target).ok()?;
	let resolved = resolve::resolve(
		&parsed,
		Origin {
			format: source,
			is_directory: false,
		},
		registry,
	)
	.ok()?;

	plan::compile(registry, &tools::Machine, &resolved)
		.ok()?
		.into_iter()
		.next()
}

/// The samples one run needs, made once each.
///
/// A sample is generated in its family's seed format and converted where the source
/// is another format of the same family, which is the only way a user-named pair
/// with an unusual source can be measured.
pub struct Samples<'a> {
	registry: &'a Registry,
	work: PathBuf,
	made: std::collections::BTreeMap<String, PathBuf>,
}

impl<'a> Samples<'a> {
	pub fn new(registry: &'a Registry, work: &Path) -> Result<Self> {
		fs::create_dir_all(work)?;

		Ok(Self {
			registry,
			work: work.to_path_buf(),
			made: std::collections::BTreeMap::new(),
		})
	}

	/// A sample in `format`, generated now rather than reused: a generator that has
	/// since changed would otherwise be measured through the file it produced the
	/// first time, and a benchmark that measures the wrong file is worse than a slow
	/// one.
	pub fn get(&mut self, format: &str) -> Result<PathBuf> {
		if let Some(path) = self.made.get(format) {
			return Ok(path.clone());
		}

		let path = self.make(format)?;
		self.made.insert(format.to_string(), path.clone());
		Ok(path)
	}

	/// What the sample for `format` was made from, for a claim's `fixture` field.
	pub fn describe(&self, format: &str) -> String {
		match self.registry.format(format) {
			Some(declared) if self.registry.bench().seed(&declared.family) == Some(format) => {
				format!("generated: {} sample, [bench.sample.{}]", format, declared.family)
			}
			Some(declared) => format!(
				"generated: {} sample, converted from {}",
				format,
				self.registry.bench().seed(&declared.family).unwrap_or("nothing")
			),
			None => format!("generated: {format} sample"),
		}
	}

	fn make(&mut self, format: &str) -> Result<PathBuf> {
		let path = self.work.join(format!("sample.{format}"));

		let Some(declared) = self.registry.format(format) else {
			return Err(unknown_format(format));
		};

		let family = declared.family.clone();

		let Some(seed) = self.registry.bench().seed(&family).map(str::to_string) else {
			return Err(Error::Core(retype_core::error::Error::Diagnostic(
				retype_core::error::Diagnostic::global(
					retype_core::error::Code::CfgMissing,
					format!("no [bench.seed] {family} format, so no sample can be generated"),
				),
			)));
		};

		if seed == format {
			return generate(self.registry, &family, &path);
		}

		let Some(plan) = plan_for(self.registry, &seed, format) else {
			return Err(Error::Core(retype_core::error::Error::Diagnostic(
				retype_core::error::Diagnostic::global(
					retype_core::error::Code::Unsupported,
					format!("a sample in '{format}' cannot be made from '{seed}'"),
				),
			)));
		};

		let source = self.get(&seed)?;
		execute::run(&plan, &source, &path)?;
		Ok(path)
	}
}

fn unknown_format(format: &str) -> Error {
	Error::Core(retype_core::error::Error::Diagnostic(
		retype_core::error::Diagnostic::global(
			retype_core::error::Code::FormatUnknown,
			format!("'{format}' is not a format this build has"),
		),
	))
}

/// Run the first declared sample generator that is installed.
fn generate(registry: &Registry, family: &str, path: &Path) -> Result<PathBuf> {
	let Some(generators) = registry.bench().sample(family) else {
		return Err(Error::Core(retype_core::error::Error::Diagnostic(
			retype_core::error::Diagnostic::global(
				retype_core::error::Code::CfgMissing,
				format!("no [bench.sample.{family}] generator, so no sample can be made"),
			),
		)));
	};

	// Preference order first, because it is the product's opinion about which tool
	// does the job, and then any declared generator so a user's own tool is tried.
	let preferred = registry
		.backends()
		.iter()
		.filter_map(|name| generators.get(name).map(|fragment| (name.as_str(), fragment)))
		.chain(
			generators
				.iter()
				.filter(|(name, _)| !registry.backends().iter().any(|preferred| preferred == *name))
				.map(|(name, fragment)| (name.as_str(), fragment)),
		);

	for (name, fragment) in preferred {
		let Some(tool) = registry.tool(name) else {
			continue;
		};

		let status = tools::probe(tool);
		let Some(command) = status.executable() else {
			continue;
		};

		let argv: Vec<String> = fragment
			.args
			.iter()
			.map(|argument| argument.replace("{output}", &path.to_string_lossy()))
			.collect();

		let ran = Command::new(command)
			.args(&argv)
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::piped())
			.output()?;

		if ran.status.success() && fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0) {
			return Ok(path.to_path_buf());
		}

		let stderr = String::from_utf8_lossy(&ran.stderr);
		eprintln!("retype: {name} could not generate a {family} sample: {}", stderr.trim());
	}

	Err(Error::Core(retype_core::error::Error::Diagnostic(
		retype_core::error::Diagnostic::global(
			retype_core::error::Code::ToolMissing,
			format!("no installed tool can generate a {family} sample"),
		),
	)))
}
