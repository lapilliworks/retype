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

use std::fs;
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use retype_core::estimate::{Calibration, Measurement};
use retype_core::plan::{self, Plan};
use retype_core::registry::Registry;
use retype_core::{execute, print, tools};

use crate::error::Result;
use crate::matrix::{PairError, Samples, Selection, plan_for};

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
	let mut samples = Samples::new(registry, &work)?;

	let mut calibration = Calibration::open(state_dir)?;

	// The measurement is evidence about one machine, so the file names it.
	if calibration.machine.is_empty() {
		calibration.machine = retype_core::estimate::machine_identity(state_dir)?;
	}

	let selection = Selection {
		pairs: request.pairs,
		families: request.families,
	};

	let pairs = match selection.pairs(registry) {
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

		let sample = match samples.get(source) {
			Ok(sample) => sample,
			Err(error) => {
				eprintln!("retype: {source} to {target}: {error}");
				continue;
			}
		};

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
		// A native backend is this build, and its version is the product's.
		plan::Backend::Native { .. } => env!("CARGO_PKG_VERSION").to_string(),
		// A repack through a tool is the tool's work whatever the rules around it are.
		plan::Backend::SevenZip { tool, .. } | plan::Backend::Tool(tool) => tools::probe(tool)
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
