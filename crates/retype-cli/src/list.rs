//! `retype list`, `docs/cli.md` §1.
//!
//! The registry is the whole answer, so nothing here touches the daemon: what is
//! declared, in what order, and which backend would carry each pair. `--tiers`
//! adds the pairwise view, because "what can this installation do" is a question
//! about a pair of formats rather than about one.
//!
//! Claims are declared, not earned. Fixture runs earn them, `docs/fidelity.md`
//! §4, and until a run has happened the honest report is the declared tier next
//! to the engine that is installed.

use std::collections::BTreeMap;
use std::process::ExitCode;

use retype_core::job::Tier;
use retype_core::registry::Registry;
use retype_core::resolve::Source as Origin;
use retype_core::tools::{self, ToolStatus};
use retype_core::{intent, plan, resolve};

use crate::error::Result;

pub fn run(registry: &Registry, tiers: bool, json: bool) -> Result<ExitCode> {
	let probes: BTreeMap<&str, ToolStatus<'_>> = registry
		.tools()
		.map(|tool| (tool.name.as_str(), tools::probe(tool)))
		.collect();

	match tiers {
		false => describe(registry, &probes, json),
		true => pairs(registry, &probes, json),
	}

	Ok(ExitCode::SUCCESS)
}

/// What is declared: formats, operations, presets and the backend order.
fn describe(registry: &Registry, probes: &BTreeMap<&str, ToolStatus<'_>>, json: bool) {
	if !json {
		println!(
			"{:<10}{:<10}{:<10}{:<10}{:<8}tier",
			"format", "family", "direction", "loss", "alpha"
		);
	}

	for (name, format) in registry.formats() {
		if json {
			println!(
				"{}",
				serde_json::json!({
					"kind": "format",
					"name": name,
					"family": format.family,
					"direction": format.direction.as_str(),
					"lossy": format.lossy,
					"alpha": format.alpha,
					"tier": tier_name(format.tier),
					"aliases": format.aliases,
				})
			);
		} else {
			println!(
				"{:<10}{:<10}{:<10}{:<10}{:<8}{}",
				name,
				format.family,
				format.direction.as_str(),
				match format.lossy {
					true => "lossy",
					false => "lossless",
				},
				match format.alpha {
					true => "alpha",
					false => "",
				},
				tier_name(format.tier),
			);
		}
	}

	let mut ops: Vec<_> = registry.ops().collect();
	ops.sort_by_key(|(_, op)| (op.slot, op.name.clone()));

	if !json {
		println!();
		println!("{:<6}{:<14}{:<10}formats", "slot", "operation", "param");
	}

	for (key, op) in ops {
		let param = op
			.param
			.as_ref()
			.map(|param| format!("{:?}", param.class).to_lowercase());

		if json {
			println!(
				"{}",
				serde_json::json!({
					"kind": "operation",
					"name": key,
					"slot": op.slot,
					"param": param,
					"formats": op.formats,
					"exclusive": op.exclusive,
					"say": op.say,
				})
			);
		} else {
			println!(
				"{:<6}{:<14}{:<10}{}",
				op.slot,
				key,
				param.unwrap_or_default(),
				op.formats.join(" ")
			);
		}
	}

	if !json {
		println!();
		println!("{:<14}tokens", "preset");
	}

	for preset in registry.presets() {
		if json {
			println!(
				"{}",
				serde_json::json!({
					"kind": "preset",
					"name": preset.name,
					"tokens": preset.tokens,
				})
			);
		} else {
			println!("{:<14}{}", preset.name, preset.tokens.join("+"));
		}
	}

	if !json {
		println!();
		println!("{:<10}status", "backend");
	}

	for name in registry.backends() {
		let status = probes.get(name.as_str());

		if json {
			println!(
				"{}",
				serde_json::json!({
					"kind": "backend",
					"name": name,
					"present": status.is_some_and(|status| status.is_usable()),
					"version": status.and_then(|status| status.version.as_ref()).map(ToString::to_string),
				})
			);
		} else {
			println!(
				"{:<10}{}",
				name,
				match status {
					Some(status) => status.summary(),
					None => "not declared".to_string(),
				}
			);
		}
	}
}

/// One line per pair: the tier it declares, and the backend that would carry it.
fn pairs(registry: &Registry, probes: &BTreeMap<&str, ToolStatus<'_>>, json: bool) {
	let names: Vec<&str> = registry.formats().map(|(name, _)| name.as_str()).collect();

	if !json {
		println!(
			"{:<8}{:<8}{:<12}{:<10}{:<12}{:<10}fingerprint",
			"from", "to", "tier", "backend", "engine", "version"
		);
	}

	for from in &names {
		for to in &names {
			if from == to {
				continue;
			}

			let (tier, backend, error, engine, version, fingerprint) = claim(registry, probes, from, to);

			if json {
				println!(
					"{}",
					serde_json::json!({
						"kind": "pair",
						"from": from,
						"to": to,
						"tier": tier,
						"backend": backend,
						"error": error,
						"engine": engine,
						"version": version,
						"fingerprint": fingerprint,
					})
				);
			} else {
				println!(
					"{:<8}{:<8}{:<12}{:<10}{:<12}{:<10}{}",
					from,
					to,
					tier,
					backend
						.or_else(|| error.map(|code| format!("({code})")))
						.unwrap_or_default(),
					engine.unwrap_or_default(),
					version.unwrap_or_default(),
					fingerprint.unwrap_or_default()
				);
			}
		}
	}
}

/// The tier a pair declares and the backend that would produce it, which is the
/// same plan a bare intent compiles to.
fn claim<'a>(
	registry: &'a Registry,
	probes: &BTreeMap<&str, ToolStatus<'a>>,
	from: &str,
	to: &str,
) -> (String, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>) {
	let unclaimed = "unclaimed".to_string();

	let Ok(parsed) = intent::parse(to) else {
		return (unclaimed, None, None, None, None, None);
	};

	// A pair refused before any backend is chosen, such as a source that cannot be
	// read, is the same answer as one no backend carries: the user is told why.
	let resolved = match resolve::resolve(
		&parsed,
		Origin {
			format: from,
			is_directory: false,
		},
		registry,
	) {
		Ok(resolved) => resolved,
		Err(problems) => return (unclaimed, None, Some(refusal(&problems)), None, None, None),
	};

	match plan::compile(registry, &tools::Machine, &resolved) {
		Ok(plans) => {
			let Some(first) = plans.first() else {
				return (unclaimed, None, None, None, None, None);
			};

			let name = first.backend.name().to_string();
			let status = probes.get(name.as_str());
			let engine = first.engine.as_ref().map(|(_, candidate)| candidate.clone());
			let version = status
				.and_then(|status| status.version.as_ref())
				.map(ToString::to_string);
			let fingerprint = status.and_then(ToolStatus::fingerprint);

			(
				tier_name(first.tier).to_string(),
				Some(name),
				None,
				engine,
				version,
				fingerprint,
			)
		}
		// A pair nothing can carry is worth saying out loud rather than leaving
		// blank, because it is the pair a user will try. The code is bracketed so
		// it cannot be mistaken for a backend name.
		Err(problems) => (unclaimed, None, Some(refusal(&problems)), None, None, None),
	}
}

/// The first code a refusal carries. The human line brackets it so it cannot be
/// read as a backend name; the JSON object carries it in its own field.
fn refusal(problems: &retype_core::error::Diagnoses) -> String {
	problems
		.codes()
		.first()
		.map(ToString::to_string)
		.unwrap_or_default()
}

fn tier_name(tier: Tier) -> &'static str {
	match tier {
		Tier::Exact => "exact",
		Tier::Converted => "converted",
		Tier::Delegated => "delegated",
	}
}
