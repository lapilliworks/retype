//! `retype fidelity`, `docs/fidelity.md` §4.
//!
//! Earns a claim per pair by running the conversion, and records the build it was
//! earned with. The manifest is a release artifact: `--write` puts it where the
//! release will ship it from, `--render` fills the table in `docs/fidelity.md` §8,
//! and running against an existing manifest reports which claims the installed
//! tools still support.
//!
//! A claim that declares `exact` and is no longer supported is a failure, because a
//! promise of no loss cannot rest on evidence about another build, `docs/fidelity.md`
//! §6.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use retype_core::axis::{self, Axis, Check, Fixtures, Verdict};
use retype_core::execute;
use retype_core::fidelity::{Claim, Manifest, State, render_into};
use retype_core::job::Tier;
use retype_core::plan::{self, Plan};
use retype_core::registry::Registry;
use retype_core::tools;

use crate::error::Result;
use crate::matrix::{PairError, Samples, Selection, plan_for};

pub struct Request<'a> {
	pub pairs: &'a [String],
	pub families: &'a [String],
	/// The manifest to read and rewrite, if the run is earning claims for a release.
	pub write: Option<&'a Path>,
	/// The document whose claim table is regenerated, `docs/fidelity.md` §8.
	pub render: Option<&'a Path>,
	/// A manifest to compare against instead of writing one.
	pub manifest: Option<&'a Path>,
	/// The directory holding the property fixtures, `docs/fidelity.md` §4.
	pub fixtures: Option<&'a Path>,
	pub json: bool,
	pub quiet: bool,
}

pub fn run(registry: &Registry, state_dir: &Path, request: &Request<'_>) -> Result<ExitCode> {
	let work = state_dir.join("fidelity");
	let mut samples = Samples::new(registry, &work)?;

	let directory = request.fixtures.unwrap_or(Path::new("fixtures/fidelity"));
	let fixtures = match Fixtures::open(directory) {
		Ok(fixtures) => fixtures,
		Err(error) => {
			eprintln!("retype: {error}");
			return Ok(ExitCode::from(3));
		}
	};

	if fixtures.fixture.is_empty() && !request.quiet {
		eprintln!(
			"retype: no fixtures in {}: claims will carry no properties, and an exact pair cannot be earned",
			directory.display()
		);
	}

	let path = request.write.or(request.manifest);
	let mut manifest = match path {
		Some(path) => Manifest::open(path)?,
		None => Manifest::default(),
	};

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
		eprintln!("retype: no pair to check: declare [bench.seed] for a family, or name one with --pair");
		return Ok(ExitCode::from(2));
	}

	if !request.json && !request.quiet {
		println!(
			"{:<6}{:<6}{:<10}{:<10}{:<12}{:<18}{:<12}STATE",
			"FROM", "TO", "TIER", "BACKEND", "VERSION", "FINGERPRINT", "PROPERTIES"
		);
	}

	let mut earned = 0usize;
	let mut stale_exact = 0usize;

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

		let output = work.join(format!("{source}-to-{target}.{target}"));

		if let Err(error) = execute::run(&plan, &sample, &output) {
			eprintln!("retype: {source} to {target}: {error}");
			continue;
		}

		let checks = check_axes(&fixtures, directory, source, &plan, &work);
		let mut claim = claim_of(&plan, source, target, &samples);
		claim.axes = kept(&checks);
		claim.lost = dropped(&checks);
		let state = match manifest.get(source, target, plan.backend.name()) {
			Some(previous) if previous.is_current(&claim.version, &claim.fingerprint) => State::Earned,
			Some(_) => State::Unverified,
			None => State::New,
		};

		if state == State::Unverified && claim.tier == Tier::Exact {
			stale_exact += 1;
			eprintln!(
				"retype: {source} to {target}: the installed {} is not the build this exact claim was earned with",
				plan.backend.name()
			);
		}

 // An exact pair rests on evidence about properties: no fixture
		// is no evidence, and a lost property is a contradiction of the word.
		if claim.tier == Tier::Exact {
			if claim.axes.is_empty() {
				stale_exact += 1;
				eprintln!("retype: {source} to {target}: exact, but no fixture carries a property for {source}");
			} else if !claim.lost.is_empty() {
				stale_exact += 1;
				eprintln!(
					"retype: {source} to {target}: exact, but the conversion does not keep {}",
					names(&claim.lost).join(", ")
				);
			}
		}

		earned += 1;
		report(&claim, &checks, state, request);
		manifest.record(claim);
	}

	if let Some(path) = request.write {
		manifest.save(path)?;

		if !request.quiet {
			eprintln!("retype: wrote {} claim(s) to {}", manifest.claim.len(), path.display());
		}
	}

	if let Some(path) = request.render {
		let document = fs::read_to_string(path)?;
		fs::write(path, render_into(&document, &manifest.render())?)?;

		if !request.quiet {
			eprintln!("retype: rendered the claims table into {}", path.display());
		}
	}

	if !request.json && !request.quiet {
		println!();
		println!("{earned} pair(s) checked");
	}

	// Nothing earned and a stale exact claim are the same answer to a script: this
	// machine cannot stand behind the release's claims.
	Ok(match stale_exact > 0 || earned == 0 {
		true => ExitCode::from(4),
		false => ExitCode::SUCCESS,
	})
}

/// Run every fixture of the source format through the same plan and see what survives.
///
/// A fixture that cannot be converted is reported and skipped: the claim then carries
/// one property fewer rather than a claim that the property was kept.
fn check_axes(fixtures: &Fixtures, directory: &Path, source: &str, plan: &Plan<'_>, work: &Path) -> Vec<Check> {
	let mut checks = Vec::new();
	let outputs = work.join("checks");

	if fixtures.for_format(source).next().is_some() && fs::create_dir_all(&outputs).is_err() {
		eprintln!("retype: cannot make {}", outputs.display());
		return checks;
	}

	for fixture in fixtures.for_format(source) {
		let input = directory.join(&fixture.file);
		let output = outputs.join(format!("{}-to-{}.{}", fixture.file, plan.format, plan.format));

		if let Err(error) = execute::run(plan, &input, &output) {
			eprintln!("retype: fixture {}: {error}", fixture.file);
			continue;
		}

		let before = match axis::Probe::read(&input) {
			Ok(probe) => probe,
			Err(error) => {
				eprintln!("retype: fixture {}: {error}", fixture.file);
				continue;
			}
		};

		let after = match axis::Probe::read(&output) {
			Ok(probe) => probe,
			Err(error) => {
				eprintln!("retype: fixture {}: {error}", fixture.file);
				continue;
			}
		};

		checks.extend(axis::compare(fixture, &before, &after));
	}

	checks
}

/// The properties every fixture kept, in the order they are declared.
fn kept(checks: &[Check]) -> Vec<Axis> {
	let mut axes: Vec<Axis> = checks
		.iter()
		.filter(|check| check.verdict == Verdict::Preserved)
		.map(|check| check.axis)
		.collect();

	axes.sort();
	axes.dedup();
	axes
}

/// The properties some fixture lost.
fn dropped(checks: &[Check]) -> Vec<Axis> {
	let mut axes: Vec<Axis> = checks
		.iter()
		.filter(|check| check.verdict == Verdict::Lost)
		.map(|check| check.axis)
		.collect();

	axes.sort();
	axes.dedup();
	axes
}

fn names(axes: &[Axis]) -> Vec<&'static str> {
	axes.iter().map(|axis| axis.as_str()).collect()
}

/// The claim one successful conversion earns.
fn claim_of(plan: &Plan<'_>, source: &str, target: &str, samples: &Samples<'_>) -> Claim {
	let (version, fingerprint) = match plan.backend {
		// This build is the backend, so the claim is earned by the release rather
		// than by a tool on the machine.
		plan::Backend::Native { .. } => (env!("CARGO_PKG_VERSION").to_string(), String::new()),
		plan::Backend::Tool(tool) => {
			let status = tools::probe(tool);
			(
				status.version.as_ref().map(ToString::to_string).unwrap_or_default(),
				status.fingerprint().unwrap_or_default(),
			)
		}
	};

	Claim {
		source: source.to_string(),
		target: target.to_string(),
		backend: plan.backend.name().to_string(),
		tier: plan.tier,
		engine: plan.engine.as_ref().map(|(_, candidate)| candidate.clone()),
		version,
		fingerprint,
		fixture: samples.describe(source),
		axes: Vec::new(),
		lost: Vec::new(),
		earned: retype_core::job::now_ms(),
	}
}

fn report(claim: &Claim, checks: &[Check], state: State, request: &Request<'_>) {
	if request.json {
		println!(
			"{}",
			serde_json::json!({
				"v": "retype-cli-json-1",
				"kind": "claim",
				"source": claim.source,
				"target": claim.target,
				"tier": tier(claim.tier),
				"backend": claim.backend,
				"engine": claim.engine,
				"version": claim.version,
				"fingerprint": claim.fingerprint,
				"fixture": claim.fixture,
				"axes": names(&claim.axes),
				"lost": names(&claim.lost),
				"state": state.as_str(),
			})
		);
		return;
	}

	if request.quiet {
		return;
	}

	println!(
		"{:<6}{:<6}{:<10}{:<10}{:<12}{:<18}{:<20}{}",
		claim.source,
		claim.target,
		tier(claim.tier),
		claim.backend,
		claim.version,
		claim.fingerprint.chars().take(16).collect::<String>(),
		properties(checks),
		state.as_str(),
	);
}

/// What the fixtures said, in one cell: kept first, then what was lost.
fn properties(checks: &[Check]) -> String {
	if checks.is_empty() {
		return "-".to_string();
	}

	let kept = kept(checks);
	let lost = dropped(checks);

	match (kept.is_empty(), lost.is_empty()) {
		(false, true) => names(&kept).join(","),
		(true, false) => format!("!{}", names(&lost).join(",")),
		(false, false) => format!("{}; !{}", names(&kept).join(","), names(&lost).join(",")),
		(true, true) => "-".to_string(),
	}
}

fn tier(tier: Tier) -> &'static str {
	match tier {
		Tier::Exact => "exact",
		Tier::Converted => "converted",
		Tier::Delegated => "delegated",
	}
}
