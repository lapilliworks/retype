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
		println!("{:<10}{:<10}{:<10}{:<8}TIER", "FORMAT", "FAMILY", "LOSS", "ALPHA");
	}

	for (name, format) in registry.formats() {
		if json {
			println!(
				"{}",
				serde_json::json!({
					"kind": "format",
					"name": name,
					"family": format.family,
					"lossy": format.lossy,
					"alpha": format.alpha,
					"tier": tier_name(format.tier),
					"aliases": format.aliases,
				})
			);
		} else {
			println!(
				"{:<10}{:<10}{:<10}{:<8}{}",
				name,
				format.family,
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
		println!("{:<6}{:<14}{:<10}FORMATS", "SLOT", "OPERATION", "PARAM");
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
		println!("{:<14}TOKENS", "PRESET");
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
		println!("{:<10}STATUS", "BACKEND");
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
	let names: Vec<&String> = registry.formats().map(|(name, _)| name).collect();

	if !json {
		println!("{:<6}{:<6}{:<12}{:<10}VERSION", "FROM", "TO", "TIER", "BACKEND");
	}

	for from in &names {
		for to in &names {
			if from == to {
				continue;
			}

			let (tier, backend, version) = claim(registry, probes, from, to);

			if json {
				println!(
					"{}",
					serde_json::json!({
						"kind": "pair",
						"from": from,
						"to": to,
						"tier": tier,
						"backend": backend,
						"version": version,
					})
				);
			} else {
				println!(
					"{:<6}{:<6}{:<12}{:<10}{}",
					from,
					to,
					tier,
					backend.unwrap_or_default(),
					version.unwrap_or_default()
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
) -> (String, Option<String>, Option<String>) {
	let tier = registry
		.format(to)
		.map(|format| tier_name(format.tier).to_string())
		.unwrap_or_else(|| "unclaimed".to_string());

	let Ok(parsed) = intent::parse(to) else {
		return (tier, None, None);
	};

	let Ok(resolved) = resolve::resolve(
		&parsed,
		Origin {
			format: from,
			is_directory: false,
		},
		registry,
	) else {
		return (tier, None, None);
	};

	match plan::compile(registry, &tools::Machine, &resolved) {
		Ok(plans) => {
			let Some(first) = plans.first() else {
				return (tier, None, None);
			};

			let name = first.backend.name().to_string();
			let version = probes
				.get(name.as_str())
				.and_then(|status| status.version.as_ref())
				.map(ToString::to_string);

			(tier, Some(name), version)
		}
		// A pair nothing can carry is worth saying out loud rather than leaving
		// blank, because it is the pair a user will try. The code is bracketed so
		// it cannot be mistaken for a backend name.
		Err(problems) => {
			let code = problems
				.codes()
				.first()
				.map(|code| format!("({code})"))
				.unwrap_or_default();

			(tier, Some(code), None)
		}
	}
}

fn tier_name(tier: Tier) -> &'static str {
	match tier {
		Tier::Exact => "exact",
		Tier::Converted => "converted",
		Tier::Delegated => "delegated",
	}
}
