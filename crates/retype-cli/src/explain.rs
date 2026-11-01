//! `retype explain`, `docs/cli.md` §3.
//!
//! Answers "what does this string mean" without a file: the canonical intent, the
//! names it derives, the plan, the backend, the tier and every diagnostic. It
//! reads nothing and writes nothing, so it is safe to run anywhere, and it is the
//! one command that shows the whole chain from text to argv.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use retype_core::error::{Code, Diagnostic};
use retype_core::estimate::{Calibration, Estimate, Query};
use retype_core::intent;
use retype_core::job::Tier;
use retype_core::output::{self as names, Derived};
use retype_core::plan::{self, Arg, Plan, Presence};
use retype_core::print;
use retype_core::registry::Registry;
use retype_core::resolve::{self, Source};
use retype_core::tools::{self, Tool};
use serde_json::json;

use crate::error::Result;
use crate::output;

/// What the user asked for: a source name and the intent string, the same two
/// arguments `convert` takes, `docs/cli.md` §2 and §3.
pub struct Request<'a> {
	pub source: &'a str,
	pub intent: &'a str,
	/// Where `bench.toml` lives, so the estimate is this machine's.
	/// Absent when the machine has no state directory, in which case `explain`
	/// still answers and simply has no estimate.
	pub state_dir: Option<&'a Path>,
	pub json: bool,
	pub quiet: bool,
	/// `-o`, which replaces the one produced name. Only `convert` sets it.
	pub output: Option<&'a str>,
	/// `--force`, which suppresses an existing-target diagnosis in a dry run.
	pub force: bool,
	/// Whether this is `convert --dry-run`: check the file and its targets.
	pub dry_run: bool,
}

pub fn run(registry: &Registry, request: &Request<'_>) -> Result<ExitCode> {
	let source = match source_of(registry, request.source) {
		Ok(source) => source,
		Err(problem) => {
			return Ok(ExitCode::from(output::fail(&problem.code, &problem.message)));
		}
	};

	if request.dry_run {
		if let Err(code) = check_source(&source) {
			return Ok(ExitCode::from(code));
		}
	}

	let parsed = match intent::parse(request.intent) {
		Ok(parsed) => parsed,
		Err(problems) => return Ok(ExitCode::from(output::fail_diagnoses(&problems))),
	};

	let resolved = match resolve::resolve(
		&parsed,
		Source {
			format: &source.format,
			is_directory: source.is_directory,
		},
		registry,
	) {
		Ok(resolved) => resolved,
		Err(problems) => return Ok(ExitCode::from(output::fail_diagnoses(&problems))),
	};

	let available = tools::Machine;
	// A plan that cannot be built because a tool is missing still has an
	// explanation worth printing, so the second attempt assumes what the file
	// declares and the report says what the machine has.
	let (plans, failure) = match plan::compile(registry, &available, &resolved) {
		Ok(plans) => (plans, None),
		Err(problems) if matches!(problems.codes().as_slice(), [Code::ToolMissing] | [Code::ToolVersion]) => (
			plan::compile(registry, &Declared, &resolved).unwrap_or_default(),
			problems.codes().first().cloned(),
		),
		Err(problems) => return Ok(ExitCode::from(output::fail_diagnoses(&problems))),
	};
	let installed = failure.is_none();

	let mut derived = names::derive(&source.stem, &source.format, &resolved.groups);

	if request.output.is_some() && plans.len() != 1 {
		return Ok(ExitCode::from(output::fail(
			&Code::UsageOutputCount,
			format!("-o names one path and this intent produces {} outputs", plans.len()),
		)));
	}

	if let Some(path) = request.output {
		let group = plans[0].group;
		if let Some(named) = derived.iter_mut().find(|named| named.group == group) {
			named.path = PathBuf::from(path);
		}
	}

	if request.dry_run {
		let input = std::path::absolute(&source.written).unwrap_or_else(|_| PathBuf::from(&source.written));

		for plan in &plans {
			let path = derived
				.iter()
				.find(|named| named.group == plan.group)
				.map(|named| named.path.clone())
				.unwrap_or_default();
			let absolute = std::path::absolute(&path).unwrap_or_else(|_| path.clone());

			if absolute == input {
				return Ok(ExitCode::from(output::fail(
					&Code::UsageConflict,
					format!(
						"{} is the source; converting into it would destroy the original",
						path.display()
					),
				)));
			}

			if !request.force && path.exists() {
				return Ok(ExitCode::from(output::fail(
					&Code::IoExists,
					format!("{} already exists", path.display()),
				)));
			}
		}
	}

	if !request.json {
		for warning in &resolved.warnings {
			eprintln!("warning: {}: {}", warning.code, warning.message);
		}
	}

	let canonical = print::intent(&resolved, registry);
	let sentence = print::sentence(&resolved, registry, &source.format);
	let estimates = estimates(registry, request.state_dir, &source, &plans);

	if request.json {
		print_json(
			&canonical,
			&sentence,
			&source.display(),
			&derived,
			&plans,
			&estimates,
			failure.as_ref(),
			&resolved.warnings,
		);
	} else {
		print_human(
			&canonical,
			&sentence,
			&source.display(),
			&derived,
			&plans,
			&estimates,
			installed,
		);
	}

	Ok(match failure {
		Some(code) => ExitCode::from(crate::error::exit_code(&code)),
		None => ExitCode::SUCCESS,
	})
}

/// A dry run names a real file, so it can tell the user that the file is not
/// there or cannot be read before printing a plan for it.
fn check_source(source: &Named) -> std::result::Result<(), u8> {
	let path = source.display();
	let metadata = match std::fs::metadata(&path) {
		Ok(metadata) => metadata,
		Err(error) => {
			return Err(output::fail(
				&retype_core::error::code_for_io(&error),
				format!("{}: {error}", path),
			));
		}
	};

	if metadata.is_dir() {
		if let Err(error) = std::fs::read_dir(&path) {
			return Err(output::fail(
				&retype_core::error::code_for_io(&error),
				format!("{}: {error}", path),
			));
		}
	} else if let Err(error) = std::fs::File::open(&path) {
		return Err(output::fail(
			&retype_core::error::code_for_io(&error),
			format!("{}: {error}", path),
		));
	}

	Ok(())
}

/// The stem a bare format implies, since `explain heic jpg` names no file.
const DEFAULT_STEM: &str = "input";

/// What the source name says about itself: its stem, its format and whether the
/// suffix claims it is a directory.
struct Named {
	/// As the user wrote it, echoed rather than rewritten: `source` is what was
	/// given, while the intent and the outputs are what it means.
	written: String,
	stem: String,
	format: String,
	is_directory: bool,
}

impl Named {
	fn display(&self) -> String {
		self.written.clone()
	}
}

/// Read a name the way the grammar reads a rename: the longest declared format
/// suffix ends the stem, and a name with no dot names a directory.
fn source_of(registry: &Registry, name: &str) -> std::result::Result<Named, Diagnostic> {
	let file = name.rsplit(['/', '\\']).next().unwrap_or(name);

	if let Some((stem, format)) = registry.split(file) {
		return Ok(Named {
			written: name.to_string(),
			stem: stem.to_string(),
			format: format.to_string(),
			is_directory: false,
		});
	}

	if !file.contains('.') {
		// A bare format is a format, not a directory called `heic`: it is how a
		// user names a source when no file is in hand.
		if let Some(format) = registry.canonical_format(file) {
			return Ok(Named {
				written: name.to_string(),
				stem: DEFAULT_STEM.to_string(),
				format: format.to_string(),
				is_directory: false,
			});
		}

		return Ok(Named {
			written: name.to_string(),
			stem: name.to_string(),
			format: "folder".to_string(),
			is_directory: true,
		});
	}

	Err(Diagnostic::global(
		Code::FormatUnknown,
		format!("'{name}' ends in a suffix this build has no format for, so it is not ours to convert"),
	))
}

/// What each output is expected to cost, one entry per plan, absent when the file
/// is not there to size.
///
/// `explain` asks about a name, which usually does not exist, so no estimate is
/// printed rather than a guess: `docs/cli.md` §3. `convert --dry-run` reaches the
/// same code with a real file and therefore gets one.
fn estimates(registry: &Registry, state_dir: Option<&Path>, source: &Named, plans: &[Plan]) -> Vec<Option<Estimate>> {
	let Some(state_dir) = state_dir else {
		return vec![None; plans.len()];
	};

	let bytes = match std::fs::metadata(source.display()) {
		Ok(metadata) if metadata.is_file() => metadata.len(),
		_ => return vec![None; plans.len()],
	};

	let calibration = match Calibration::open(state_dir) {
		Ok(calibration) => calibration,
		Err(error) => {
			// A calibration that cannot be read costs an estimate and nothing else,
			// so it is reported and the explanation goes on.
			eprintln!("warning: {error}");
			Calibration::default()
		}
	};

	plans
		.iter()
		.map(|plan| {
			let family = registry
				.format(&plan.format)
				.map(|format| format.family.as_str())
				.unwrap_or_default();

			calibration.estimate(&Query {
				source: &source.format,
				target: &plan.format,
				backend: plan.backend.name(),
				family,
				bytes,
			})
		})
		.collect()
}

fn print_human(
	canonical: &str,
	sentence: &str,
	source: &str,
	derived: &[Derived],
	plans: &[Plan],
	estimates: &[Option<Estimate>],
	installed: bool,
) {
	println!("{:<10}{canonical}", "intent");
	println!(
		"{:<10}{}",
		"source",
		if source.is_empty() { "not given" } else { source }
	);
	println!("{:<10}{sentence}", "says");

	// One line per group, because a group that keeps the source is part of what the
	// string means even though it runs nothing.
	for named in derived {
		println!("{:<10}{}", "output", named.path.display());

		let Some(index) = plans.iter().position(|plan| plan.group == named.group) else {
			println!("  {:<8}kept: the source already has this format", "action");
			continue;
		};
		let plan = &plans[index];

		match installed {
			true => println!("  {:<8}{}", "backend", plan.backend.name()),
			false => println!("  {:<8}{} (not installed)", "backend", plan.backend.name()),
		}

		println!(
			"  {:<8}{}",
			"argv",
			match plan.backend.is_native() {
				true => "nothing to run: this build writes it".to_string(),
				false => render(&plan.command, plan),
			}
		);
		println!("  {:<8}{}", "tier", tier(plan));

		if let Some((engine, candidate)) = &plan.engine {
			println!("  {:<8}{engine} {candidate}", "engine");
		}

		if let Some(estimate) = estimates.get(index).copied().flatten() {
			println!(
				"  estimate {}  {}  ({})",
				print::short_duration(estimate.time),
				print::size(estimate.bytes),
				estimate.basis.as_str()
			);
		}
	}
}

#[allow(clippy::too_many_arguments)]
fn print_json(
	canonical: &str,
	sentence: &str,
	source: &str,
	derived: &[Derived],
	plans: &[Plan],
	estimates: &[Option<Estimate>],
	failure: Option<&Code>,
	warnings: &[Diagnostic],
) {
	let outputs: Vec<serde_json::Value> = derived
		.iter()
		.map(|named| {
			let Some(index) = plans.iter().position(|plan| plan.group == named.group) else {
				// A kept group is part of what the string means and runs nothing.
				return json!({
					"path": named.path.display().to_string(),
					"format": named.format,
					"kept": true,
				});
			};
			let plan = &plans[index];

			json!({
				"path": named.path.display().to_string(),
				"format": named.format,
				"kept": false,
				"backend": plan.backend.name(),
				"available": failure.is_none(),
				"command": plan.command,
				"argv": match plan.backend.is_native() {
					true => serde_json::Value::Null,
					false => json!(render(&plan.command, plan)),
				},
				"tier": tier(plan),
				"engine": plan.engine.as_ref().map(|(name, candidate)| json!({"name": name, "resolved": candidate})),
				"estimate": estimates.get(index).copied().flatten().map(|estimate| json!({
					"seconds": (estimate.time.as_secs_f64() * 10.0).round() / 10.0,
					"bytes": estimate.bytes,
					"basis": estimate.basis.as_str(),
				})),
			})
		})
		.collect();

	let diagnostics: Vec<serde_json::Value> = warnings
		.iter()
		.map(|warning| {
			json!({
				"code": warning.code,
				"category": output::category_name(&warning.code),
				"message": warning.message,
			})
		})
		.collect();

	let mut object = json!({
		"v": output::VERSION,
		"ok": failure.is_none(),
		"intent": canonical,
		"says": sentence,
		"source": if source.is_empty() { serde_json::Value::Null } else { json!(source) },
		"outputs": outputs,
	});

	if !diagnostics.is_empty() {
		object["diagnostics"] = json!(diagnostics);
	}

	if let Some(code) = failure {
		object["error"] = json!({
			"code": code,
			"category": output::category_name(code),
			"message": "the backend this plan needs is not installed",
		});
	}

	println!("{object}");
}

/// The argv as the user would type it, with the paths left as holes.
fn render(command: &str, plan: &Plan) -> String {
	let mut parts = vec![command.to_string()];

	for arg in &plan.argv {
		parts.push(match arg {
			Arg::Literal(text) => text.clone(),
			Arg::Input => "{input}".to_string(),
			Arg::Output => "{output}".to_string(),
		});
	}

	parts.join(" ")
}

fn tier(plan: &Plan) -> &'static str {
	match plan.tier {
		Tier::Exact => "exact",
		Tier::Converted => "converted",
		Tier::Delegated => "delegated",
	}
}

/// Every declared tool, present. Used only when the real machine cannot build
/// the plan, so that the explanation still describes what the file declares.
struct Declared;

impl Presence for Declared {
	fn command(&self, tool: &Tool) -> Option<String> {
		Some(tool.command.clone())
	}

	fn engine(&self, tool: &Tool, name: &str) -> Option<String> {
		tool.candidates(name).first().cloned()
	}
}
