//! Deciding whether a rename runs now or waits, `docs/deferral.md`.
//!
//! The decision is made where the rename arrives, before anything is queued, so a
//! parked job never looks like a running one and a job that cannot run as asked never
//! starts. It needs three things the watcher already has: the registry, the undo
//! store and the estimator. None of them runs a tool.

use std::fs;
use std::path::Path;
use std::time::Duration;

use retype_core::deferral::{Parked, Reason};
use retype_core::error::{Code, Diagnoses};
use retype_core::estimate::{Calibration, Query};
use retype_core::job::{Cost, Job, JobError, JobId, JobState, Output, Source, now_ms};
use retype_core::plan::{self, Plan};
use retype_core::registry::Registry;
use retype_core::resolve::{self, Resolved, Source as Origin};
use retype_core::undo::{Room, Store};
use retype_core::{intent, output, print};

use crate::daemon::watcher::{self, Shared};

/// What a rename should do, `docs/deferral.md` §3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
	/// Run it now.
	Run,
	/// Not ours: a rename that carries no intent, or one that changes nothing.
	Ignore,
	/// Parked, and the record says why.
	Parked,
}

/// Decide, and record the answer when it is "not now".
#[allow(clippy::too_many_arguments)]
pub fn decide(shared: &Shared, root: &Path, source: &Path, target: &Path, id: &JobId, batch: Option<&str>) -> Verdict {
	let (Some(source_name), Some(target_name)) = (
		source.file_name().and_then(|name| name.to_str()),
		target.file_name().and_then(|name| name.to_str()),
	) else {
		return Verdict::Ignore;
	};

	let Some((stem, format)) = shared
		.registry
		.split(source_name)
		.map(|(stem, format)| (stem.to_string(), format.to_string()))
	else {
		return Verdict::Ignore;
	};

	let parsed = match resolve::intent_of(&shared.registry, source_name, target_name) {
		Ok(Some(parsed)) => parsed,
		Ok(None) => return Verdict::Ignore,
		Err(problems) => {
			// The text the name carried, which is what the user has to correct.
			let written = intent::anchor(target_name, &stem).ok().flatten().map(str::to_string);

			return park(
				shared,
				&stem,
				root,
				&format,
				target,
				id,
				batch,
				Reason::Intent,
				Parking {
					requested: written,
					diagnosis: Some(&problems),
					consume: true,
					..Parking::default()
				},
			);
		}
	};

	let requested = Some(parsed.text.clone());

	let resolved = match resolve::resolve(
		&parsed,
		Origin {
			format: &format,
			is_directory: false,
		},
		&shared.registry,
	) {
		Ok(resolved) => resolved,
		Err(problems) => {
			let reason = reason_of(&problems);
			let corrected = corrected(&shared.registry, &parsed, &format);

			return park(
				shared,
				&stem,
				root,
				&format,
				target,
				id,
				batch,
				reason,
				Parking {
					requested,
					corrected,
					diagnosis: Some(&problems),
					consume: true,
					..Parking::default()
				},
			);
		}
	};

	let plans = match plan::compile(&shared.registry, &retype_core::tools::Machine, &resolved) {
		Ok(plans) => plans,
		Err(problems) => {
			return park(
				shared,
				&stem,
				root,
				&format,
				target,
				id,
				batch,
				reason_of(&problems),
				Parking {
					requested,
					diagnosis: Some(&problems),
					resolved: Some(&resolved),
					consume: true,
					..Parking::default()
				},
			);
		}
	};

	let bytes = fs::metadata(target).map(|metadata| metadata.len()).unwrap_or_default();
	let estimate = cost(shared, &format, &plans, bytes);

	// An ask is the user's own brake, and nothing else overrides it.
	if resolved.meta.ask {
		return park(
			shared,
			&stem,
			root,
			&format,
			target,
			id,
			batch,
			Reason::Ask,
			Parking {
				requested,
				estimate,
				resolved: Some(&resolved),
				plans: &plans,
				consume: true,
				..Parking::default()
			},
		);
	}

	// `now` skips both cost triggers for this job; the mode skips them for the machine.
	if !resolved.meta.now && cost_defers(shared, &estimate, bytes) {
		return park(
			shared,
			&stem,
			root,
			&format,
			target,
			id,
			batch,
			Reason::Cost,
			Parking {
				requested,
				estimate,
				resolved: Some(&resolved),
				plans: &plans,
				consume: true,
				..Parking::default()
			},
		);
	}

	Verdict::Run
}

/// Whether the estimate crosses the threshold that applies, `docs/deferral.md` §3.
fn cost_defers(shared: &Shared, estimate: &Option<Cost>, bytes: u64) -> bool {
	let Some(estimate) = estimate else {
		// An unknown cost is not a reason to ask: turning "we do not know" into "you
		// must answer" is worse than running.
		return false;
	};

	let has_undo = keeps_a_record(shared, bytes);
	shared
		.registry
		.deferral()
		.defers(Duration::from_millis(estimate.ms), has_undo)
}

/// Whether an undo record could be kept for a file of this size, decided without
/// dropping anything, `docs/state.md` §5.5.
fn keeps_a_record(shared: &Shared, bytes: u64) -> bool {
	let policy = shared.registry.undo();

	policy.enabled
		&& Store::open(&shared.state_dir)
			.room_for(bytes, policy)
			.is_ok_and(|room| matches!(room, Room::Kept { .. }))
}

/// What the whole job is expected to cost, or nothing when any output is unknown.
fn cost(shared: &Shared, format: &str, plans: &[Plan<'_>], bytes: u64) -> Option<Cost> {
	let calibration = Calibration::open(&shared.state_dir).ok()?;
	let (mut ms, mut total) = (0u64, 0u64);

	for plan in plans {
		let family = shared
			.registry
			.format(&plan.format)
			.map(|format| format.family.as_str())
			.unwrap_or_default();

		let estimate = calibration.estimate(&Query {
			source: format,
			target: &plan.format,
			backend: plan.backend.name(),
			family,
			bytes,
		})?;

		ms += u64::try_from(estimate.time.as_millis()).unwrap_or(u64::MAX);
		total += estimate.bytes;
	}

	Some(Cost { ms, bytes: total })
}

/// Which parked state a diagnosis belongs to, `docs/deferral.md` §2.
fn reason_of(problems: &Diagnoses) -> Reason {
	let codes = problems.codes();

	if codes
		.iter()
		.any(|code| matches!(code, Code::Inapplicable | Code::FormatFolder))
	{
		return Reason::Inapplicable;
	}

	if codes
		.iter()
		.any(|code| matches!(code, Code::Unsupported | Code::ToolMissing | Code::ToolVersion))
	{
		return Reason::Backend;
	}

	Reason::Intent
}

/// The request with the tokens that cannot be carried dropped, when what is left
/// still resolves, `docs/deferral.md` §6.
fn corrected(registry: &Registry, parsed: &intent::Intent, format: &str) -> Option<String> {
	let mut text = String::new();
	let mut kept_any = false;

	for (index, group) in parsed.groups.iter().enumerate() {
		let target = registry.canonical_format(&group.format)?;

		if index > 0 {
			text.push(',');
		}
		text.push_str(target);

		for token in &group.tokens {
			let carries = registry
				.resolve(&token.name)
				.and_then(|found| found.op().cloned())
				.is_some_and(|op| op.is_implemented() && op.applies_to(target));

			if !carries {
				continue;
			}

			text.push('+');
			text.push_str(&token.name);

			if let Some(value) = &token.value {
				text.push_str(&value.text);
			}

			kept_any = true;
		}
	}

	if !kept_any && text.is_empty() {
		return None;
	}

	// The reduced request has to be one that runs, or offering it would be another
	// promise the job cannot keep.
	let reduced = intent::parse(&text).ok()?;
	let resolved = resolve::resolve(
		&reduced,
		Origin {
			format,
			is_directory: false,
		},
		registry,
	)
	.ok()?;

	// A request that produces nothing is not a repair: `png` from a `png` is the noop
	// the grammar refuses, and offering it would run a job with no outputs.
	let plans = plan::compile(registry, &retype_core::tools::Machine, &resolved).ok()?;

	if plans.is_empty() {
		return None;
	}

	Some(print::intent(&resolved, registry))
}

/// Everything a parked record needs besides the reason.
#[derive(Default)]
struct Parking<'a> {
	requested: Option<String>,
	corrected: Option<String>,
	estimate: Option<Cost>,
	resolved: Option<&'a Resolved>,
	plans: &'a [Plan<'a>],
	consume: bool,
	diagnosis: Option<&'a Diagnoses>,
}

/// Write the parked record, and restore the source name first so the file is not left
/// claiming a format it does not hold, `docs/deferral.md` §9.
#[allow(clippy::too_many_arguments)]
fn park(
	shared: &Shared,
	stem: &str,
	_root: &Path,
	format: &str,
	target: &Path,
	id: &JobId,
	batch: Option<&str>,
	reason: Reason,
	parking: Parking<'_>,
) -> Verdict {
	// A name that cannot be restored would be a mislabelled file for as long as the
	// job waits, so the wait is not offered: the conversion runs and handles the
	// fallback itself.
	let Ok(source_path) = retype_core::convert::restore_source_name(target, stem, format) else {
		return Verdict::Run;
	};

	// The restore is a rename of our own, and the watcher must not read it as the user
	// asking for the inverse conversion: `a.webp` back to `a.png` is what parking looks
	// like from outside, and without this it parks itself again, for ever.
	shared.recent.note(&source_path, target);

	let state = reason.parked();
	let deadline = match state {
		Parked::Pending => i64::try_from(shared.registry.deferral().pause.as_millis())
			.ok()
			.map(|pause| now_ms() + pause),
		Parked::Attention => None,
	};

	let derived = parking
		.resolved
		.map(|resolved| output::derive(stem, format, &resolved.groups))
		.unwrap_or_default();

	let outputs: Vec<Output> = parking
		.plans
		.iter()
		.filter_map(|plan| {
			derived
				.iter()
				.find(|derived| derived.group == plan.group)
				.map(|derived| {
					Output::pending(
						target.parent().unwrap_or(Path::new("")).join(&derived.path),
						plan.format.clone(),
						plan.tier,
					)
				})
		})
		.collect();

	let job = Job {
		id: id.clone(),
		created: now_ms(),
		updated: now_ms(),
		source: Source {
			size: fs::metadata(&source_path)
				.map(|metadata| metadata.len())
				.unwrap_or_default(),
			path: source_path,
			format: format.to_string(),
			mtime_ns: 0,
		},
		intent: parking
			.resolved
			.map(|resolved| print::intent(resolved, &shared.registry)),
		outputs,
		state: match state {
			Parked::Pending => JobState::Pending,
			Parked::Attention => JobState::NeedsAttention,
		},
		backend: parking.plans.first().map(|plan| plan.backend.name().to_string()),
		engine: parking
			.plans
			.first()
			.and_then(|plan| plan.engine.clone())
			.map(|(name, candidate)| retype_core::job::Engine { name, candidate }),
		machine: shared.machine.clone(),
		error: parking.diagnosis.and_then(|problems| {
			problems.iter().next().map(|problem| JobError {
				code: problem.code().to_string(),
				message: problem.to_string(),
			})
		}),
		parked: Some(retype_core::job::Parked {
			reason: reason.as_str().to_string(),
			deadline,
			batch: batch.map(str::to_string),
			estimate: parking.estimate,
			requested: parking.requested,
			corrected: parking.corrected,
			dropped: Vec::new(),
			consume: parking.consume,
		}),
	};

	watcher::publish(shared, &job);
	Verdict::Parked
}

/// Cancel a parked job that a later rename replaced.
pub fn superseded(shared: &Shared, source: &Path) {
	let replaced = {
		let Ok(jobs) = shared.jobs.lock() else {
			return;
		};

		jobs.iter()
			.find(|job| job.source.path == source && matches!(job.state, JobState::Pending | JobState::NeedsAttention))
			.map(|job| {
				let mut superseded = job.clone();
				superseded.set_state(JobState::Superseded);
				superseded
			})
	};

	// Published after the lock is released: `publish` takes the same one.
	if let Some(job) = replaced {
		watcher::publish(shared, &job);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn registry() -> Registry {
		Registry::builtin().expect("the shipped registry loads")
	}

	#[test]
	fn a_request_that_cannot_be_carried_is_reduced_to_one_that_can() {
		let registry = registry();
		let parsed = intent::parse("webp+quality80+zzz").expect("scans");

		assert_eq!(corrected(&registry, &parsed, "png").as_deref(), Some("webp+quality80"));
	}

	#[test]
	fn a_request_whose_target_survives_nothing_has_no_correction() {
		let registry = registry();
		let parsed = intent::parse("png+quality80").expect("scans");

		// Dropping `quality80` leaves `png` from a `png`, which is the noop the grammar
		// refuses, so there is nothing to offer.
		assert_eq!(corrected(&registry, &parsed, "png"), None);
	}
}
