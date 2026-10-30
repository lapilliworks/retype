//! `retype explain`, `docs/cli.md` §3.
//!
//! Answers "what does this string mean" without a file: the canonical intent, the
//! names it derives, the plan, the backend, the tier and every diagnostic. It
//! reads nothing and writes nothing, so it is safe to run anywhere, and it is the
//! one command that shows the whole chain from text to argv.

use std::process::ExitCode;

use retype_core::error::{Code, Diagnostic};
use retype_core::intent;
use retype_core::job::Tier;
use retype_core::output::{self, Derived};
use retype_core::plan::{self, Arg, Backend, Plan, Presence};
use retype_core::print;
use retype_core::registry::Registry;
use retype_core::resolve::{self, Source};
use retype_core::tools::{self, Tool};
use serde_json::json;

use crate::error::Result;

/// What the user asked for: a source name and the intent string, the same two
/// arguments `convert` takes, `docs/cli.md` §2 and §3.
pub struct Request<'a> {
	pub source: &'a str,
	pub intent: &'a str,
	pub json: bool,
	pub quiet: bool,
}

pub fn run(registry: &Registry, request: &Request<'_>) -> Result<ExitCode> {
	let source = match source_of(registry, request.source) {
		Ok(source) => source,
		Err(problem) => {
			eprintln!("retype: {}: {}", problem.code, problem.message);
			return Ok(ExitCode::from(3));
		}
	};

	let parsed = match intent::parse(request.intent) {
		Ok(parsed) => parsed,
		Err(problems) => {
			eprintln!("retype: {problems}");
			return Ok(ExitCode::from(3));
		}
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
		Err(problems) => {
			eprintln!("retype: {problems}");
			return Ok(ExitCode::from(3));
		}
	};

	let available = tools::Machine;
	// A plan that cannot be built because a tool is missing still has an
	// explanation worth printing, so the second attempt assumes what the file
	// declares and the report says what the machine has.
	let (plans, installed) = match plan::compile(registry, &available, &resolved) {
		Ok(plans) => (plans, true),
		Err(problems) if problems.codes() == [Code::ToolMissing] => {
			(plan::compile(registry, &Declared, &resolved).unwrap_or_default(), false)
		}
		Err(problems) => {
			eprintln!("retype: {problems}");
			return Ok(ExitCode::from(3));
		}
	};

	for warning in &resolved.warnings {
		eprintln!("warning: {}: {}", warning.code, warning.message);
	}

	let canonical = print::intent(&resolved, registry);
	let derived = output::derive(&source.stem, &source.format, &resolved.groups);

	if request.json {
		print_json(&canonical, &source.display(), &derived, &plans, installed);
	} else if !request.quiet {
		print_human(&canonical, &source.display(), &derived, &plans, installed);
	}

	Ok(if installed {
		ExitCode::SUCCESS
	} else {
		eprintln!("retype: the backend this plan needs is not installed");
		ExitCode::from(4)
	})
}

/// What the source name says about itself: its stem, its format and whether the
/// suffix claims it is a directory.
struct Named {
	stem: String,
	format: String,
	is_directory: bool,
}

impl Named {
	fn display(&self) -> String {
		match self.is_directory {
			true => self.stem.clone(),
			false => format!("{}.{}", self.stem, self.format),
		}
	}
}

/// Read a name the way the grammar reads a rename: the longest declared format
/// suffix ends the stem, and a name with no dot names a directory.
fn source_of(registry: &Registry, name: &str) -> std::result::Result<Named, Diagnostic> {
	let file = name.rsplit(['/', '\\']).next().unwrap_or(name);

	if let Some((stem, format)) = registry.split(file) {
		return Ok(Named {
			stem: stem.to_string(),
			format: format.to_string(),
			is_directory: false,
		});
	}

	if !file.contains('.') {
		return Ok(Named {
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

fn print_human(canonical: &str, source: &str, derived: &[Derived], plans: &[Plan<'_>], installed: bool) {
	println!("{:<10}{canonical}", "intent");
	println!(
		"{:<10}{}",
		"source",
		if source.is_empty() { "not given" } else { source }
	);

	for (index, plan) in plans.iter().enumerate() {
		let name = derived
			.iter()
			.find(|derived| derived.group == index)
			.map(|derived| derived.path.display().to_string())
			.unwrap_or_default();

		println!("{:<10}{name}", "output");

		let Backend::Tool(tool) = plan.backend;

		match installed {
			true => println!("  {:<8}{}", "backend", plan.backend.name()),
			false => println!("  {:<8}{} (not installed)", "backend", plan.backend.name()),
		}

		println!("  {:<8}{}", "argv", render(tool.command.as_str(), plan));
		println!("  {:<8}{}", "tier", tier(plan));

		if let Some((engine, candidate)) = &plan.engine {
			println!("  {:<8}{engine} {candidate}", "engine");
		}
	}
}

fn print_json(canonical: &str, source: &str, derived: &[Derived], plans: &[Plan<'_>], installed: bool) {
	let outputs: Vec<serde_json::Value> = plans
		.iter()
		.enumerate()
		.map(|(index, plan)| {
			let Backend::Tool(tool) = plan.backend;

			json!({
				"path": derived.iter().find(|derived| derived.group == index).map(|derived| derived.path.display().to_string()),
				"format": plan.format,
				"backend": plan.backend.name(),
				"available": installed,
				"command": tool.command,
				"argv": render(tool.command.as_str(), plan),
				"tier": tier(plan),
				"engine": plan.engine.as_ref().map(|(name, candidate)| json!({"name": name, "resolved": candidate})),
			})
		})
		.collect();

	println!(
		"{}",
		json!({
			"v": "retype-cli-json-1",
			"intent": canonical,
			"source": if source.is_empty() { serde_json::Value::Null } else { json!(source) },
			"outputs": outputs,
		})
	);
}

/// The argv as the user would type it, with the paths left as holes.
fn render(command: &str, plan: &Plan<'_>) -> String {
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

fn tier(plan: &Plan<'_>) -> &'static str {
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
	fn is_available(&self, _tool: &Tool) -> bool {
		true
	}

	fn engine(&self, tool: &Tool, name: &str) -> Option<String> {
		tool.candidates(name).first().cloned()
	}
}
