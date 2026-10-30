//! `retype bench`, `docs/cli.md` §1.
//!
//! Measures this machine on real conversions and writes `bench.toml`, which is
//! what turns `explain`'s estimate from a family constant into a number taken
//! here.
//!
//! Nothing about the matrix is hardcoded. A family's sample is generated in the
//! format `[bench.seed]` names, by an argv `[bench.sample]` declares, and the
//! pairs measured from it are every target that compiles from that source. A
//! build that gains a format gains it here without a line of code, and a
//! configuration that prefers another tool measures through it.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Instant;

use retype_core::estimate::{Calibration, Measurement};
use retype_core::plan::{self, Plan};
use retype_core::registry::Registry;
use retype_core::resolve::{self, Source as Origin};
use retype_core::{execute, intent, print, tools};

use crate::error::{Error, Result};

pub struct Request<'a> {
	/// Pairs to measure instead of a family sweep, written `source:target`.
	pub pairs: &'a [String],
	/// Families to sweep; empty means every family `[bench.seed]` declares.
	pub families: &'a [String],
	pub json: bool,
	pub quiet: bool,
}

pub fn run(registry: &Registry, state_dir: &Path, request: &Request<'_>) -> Result<ExitCode> {
	// The work directory comes first, because the calibration and the identity live
	// beside it and neither can be written into a directory that is not there.
	let work = state_dir.join("bench");
	fs::create_dir_all(&work)?;

	let mut calibration = Calibration::open(state_dir)?;

	// The measurement is evidence about one machine, so the file names it.
	if calibration.machine.is_empty() {
		calibration.machine = retype_core::estimate::machine_identity(state_dir)?;
	}

	let pairs = match pairs(registry, request) {
		Ok(pairs) => pairs,
		Err(PairError::Usage(message)) => {
			eprintln!("retype: {message}");
			return Ok(ExitCode::from(2));
		}
		Err(PairError::Format(message)) => {
			eprintln!("retype: {message}");
			return Ok(ExitCode::from(3));
		}
	};

	if pairs.is_empty() {
		eprintln!("retype: no pair to measure: declare [bench.seed] for a family, or name one with --pair");
		return Ok(ExitCode::from(2));
	}

	// Samples are made once and reused: regenerating one per pair would measure the
	// sample generator as much as the conversion.
	let mut samples: BTreeMap<String, PathBuf> = BTreeMap::new();
	for (source, _) in &pairs {
		if samples.contains_key(source) {
			continue;
		}

		let sample = sample(registry, &work, source)?;
		samples.insert(source.clone(), sample);
	}

	if !request.json && !request.quiet {
		println!(
			"{:<6}{:<6}{:<10}{:>10}{:>8}  SAMPLE",
			"FROM", "TO", "BACKEND", "RATE", "SIZE"
		);
	}

	let mut measured = 0usize;

	for (source, target) in &pairs {
		let Some(plan) = plan_for(registry, source, target) else {
			if !request.quiet {
				eprintln!("retype: {source} to {target}: nothing can carry the pair");
			}
			continue;
		};

		let sample = samples.get(source).expect("every source has a sample").clone();

		match measure(registry, &plan, &sample, &work, source, target) {
			Ok(measurement) => {
				measured += 1;
				report(&measurement, request);
				calibration.measure(measurement);
			}
			Err(error) => eprintln!("retype: {source} to {target}: {error}"),
		}
	}

	calibration.save(state_dir)?;

	if !request.json && !request.quiet {
		println!();
		println!(
			"measured {measured} pair(s) on this machine, written to {}",
			Calibration::path(state_dir).display()
		);
	}

	Ok(match measured {
		0 => ExitCode::from(4),
		_ => ExitCode::SUCCESS,
	})
}

/// Why a `--pair` could not be read: the shape was wrong, or the name is not a
/// format. The two are different answers, so they are different exit codes.
enum PairError {
	Usage(String),
	Format(String),
}

/// The pairs to measure: what the user named, or every target that compiles from
/// each family's seed.
fn pairs(registry: &Registry, request: &Request<'_>) -> std::result::Result<Vec<(String, String)>, PairError> {
	if !request.pairs.is_empty() {
		return request.pairs.iter().map(|pair| named(registry, pair)).collect();
	}

	let families: Vec<String> = match request.families.is_empty() {
		true => registry.bench().families().map(str::to_string).collect(),
		false => request.families.to_vec(),
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

/// Read `source:target` as the user wrote it, refusing a name this build has no
/// format for rather than measuring something else.
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
fn plan_for<'a>(registry: &'a Registry, source: &str, target: &str) -> Option<Plan<'a>> {
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

/// Make a sample in `format`. A sample that cannot be generated in this format is
/// generated in its family's seed format and converted, which is the only way a
/// user-named pair with an unusual source can be measured.
///
/// The sample is made again on every run rather than reused: a generator that has
/// since changed would otherwise be measured through the file it produced the
/// first time, and a benchmark that measures the wrong file is worse than a slow
/// one.
fn sample(registry: &Registry, work: &Path, format: &str) -> Result<PathBuf> {
	let path = work.join(format!("sample.{format}"));

	let Some(family) = registry.format(format).map(|format| format.family.clone()) else {
		return Err(Error::Core(retype_core::error::Error::Diagnostic(
			retype_core::error::Diagnostic::global(
				retype_core::error::Code::FormatUnknown,
				format!("'{format}' is not a format this build has"),
			),
		)));
	};

	let Some(seed) = registry.bench().seed(&family) else {
		return Err(Error::Core(retype_core::error::Error::Diagnostic(
			retype_core::error::Diagnostic::global(
				retype_core::error::Code::CfgMissing,
				format!("no [bench.seed] {family} format, so no sample can be generated"),
			),
		)));
	};

	if seed != format {
		let Some(plan) = plan_for(registry, seed, format) else {
			return Err(Error::Core(retype_core::error::Error::Diagnostic(
				retype_core::error::Diagnostic::global(
					retype_core::error::Code::Unsupported,
					format!("a sample in '{format}' cannot be made from '{seed}'"),
				),
			)));
		};

		let source = sample(registry, work, seed)?;
		execute::run(&plan, &source, &path)?;
		return Ok(path);
	}

	generate(registry, &family, &path)
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

/// Convert the sample and record what it cost.
fn measure(
	registry: &Registry,
	plan: &Plan<'_>,
	sample: &Path,
	work: &Path,
	source: &str,
	target: &str,
) -> Result<Measurement> {
	let bytes = fs::metadata(sample)?.len();
	let output = work.join(format!("{source}-to-{target}.{target}"));

	let started = Instant::now();
	let report = execute::run(plan, sample, &output)?;
	let seconds = started.elapsed().as_secs_f64().max(0.001);

	Ok(Measurement {
		source: source.to_string(),
		target: target.to_string(),
		backend: plan.backend.name().to_string(),
		family: registry
			.format(target)
			.map(|format| format.family.clone())
			.unwrap_or_default(),
		bytes_per_second: bytes as f64 / seconds,
		size_ratio: report.bytes as f64 / bytes.max(1) as f64,
		sample_bytes: bytes,
		tool_version: version(plan),
		measured: retype_core::job::now_ms(),
	})
}

fn version(plan: &Plan<'_>) -> String {
	match plan.backend {
		plan::Backend::Tool(tool) => tools::probe(tool)
			.version
			.map(|version| version.to_string())
			.unwrap_or_default(),
	}
}

fn report(measurement: &Measurement, request: &Request<'_>) {
	if request.json {
		println!(
			"{}",
			serde_json::json!({
				"v": "retype-cli-json-1",
				"kind": "measurement",
				"source": measurement.source,
				"target": measurement.target,
				"backend": measurement.backend,
				"family": measurement.family,
				"bytes_per_second": measurement.bytes_per_second.round(),
				"size_ratio": (measurement.size_ratio * 1000.0).round() / 1000.0,
				"sample_bytes": measurement.sample_bytes,
				"tool_version": measurement.tool_version,
				"measured": measurement.measured,
			})
		);
		return;
	}

	if request.quiet {
		return;
	}

	println!(
		"{:<6}{:<6}{:<10}{:>10}{:>8}  {}",
		measurement.source,
		measurement.target,
		measurement.backend,
		format!("{:.1} mb/s", measurement.bytes_per_second / 1_000_000.0),
		format!("{:.2}x", measurement.size_ratio),
		print::size(measurement.sample_bytes),
	);
}
