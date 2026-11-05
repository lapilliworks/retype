//! Deciding whether a rename runs now or waits, `docs/deferral.md`.
//!
//! The decision is made where the rename arrives, before anything is queued, so a
//! parked job never looks like a running one and a job that cannot run as asked never
//! starts. It needs three things the watcher already has: the registry, the undo
//! store and the estimator. None of them runs a tool.

use std::fs;
use std::path::Path;
use std::time::Duration;

use retype_core::deferral::{Mode, Parked, Reason, Repair};
use retype_core::error::{Code, Diagnoses};
use retype_core::estimate::{Calibration, Query};
use retype_core::job::{Cost, Job, JobError, JobId, JobState, Output, Source, now_ms};
use retype_core::plan::{self, Plan};
use retype_core::policy::Policy;
use retype_core::registry::Registry;
use retype_core::resolve::{self, Resolved, Source as Origin};
use retype_core::undo::{Room, Store};
use retype_core::{intent, output, print};

use crate::daemon::queue::Pool;
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
	/// Run with the tokens that could not be carried dropped, and already submitted,
	/// `docs/deferral.md` §6.
	Repaired,
}

/// Decide, and record the answer when it is "not now".
#[allow(clippy::too_many_arguments)]
pub fn decide(
	shared: &Shared,
	pool: &Pool,
	root: &Path,
	source: &Path,
	target: &Path,
	id: &JobId,
	batch: Option<&str>,
	policy: &Policy,
) -> Verdict {
	let (Some(source_name), Some(target_name)) = (
		source.file_name().and_then(|name| name.to_str()),
		target.file_name().and_then(|name| name.to_str()),
	) else {
		return Verdict::Ignore;
	};

	let Some((stem, format)) = shared
		.registry()
		.split(source_name)
		.map(|(stem, format)| (stem.to_string(), format.to_string()))
	else {
		return Verdict::Ignore;
	};

	let parsed = match resolve::intent_of(&shared.registry(), source_name, target_name) {
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

	let mut resolved = match resolve::resolve(
		&parsed,
		Origin {
			format: &format,
			is_directory: false,
		},
		&shared.registry(),
	) {
		Ok(resolved) => resolved,
		Err(problems) => {
			return uncarried(
				shared, pool, root, &stem, &format, target, id, batch, &parsed, requested, problems, policy, None,
			);
		}
	};

	// The directory may refuse an operation or clamp a parameter, `docs/config.md` §12.
	let refused = policy.apply(&mut resolved);

	if !refused.is_empty() {
		return uncarried(
			shared,
			pool,
			root,
			&stem,
			&format,
			target,
			id,
			batch,
			&parsed,
			requested,
			refused,
			policy,
			Some(&resolved),
		);
	}

	let plans = match plan::compile(&shared.registry(), &retype_core::tools::Machine, &resolved) {
		Ok(plans) => plans,
		Err(problems) => {
			return uncarried(
				shared,
				pool,
				root,
				&stem,
				&format,
				target,
				id,
				batch,
				&parsed,
				requested,
				problems,
				policy,
				Some(&resolved),
			);
		}
	};

	let bytes = fs::metadata(target).map(|metadata| metadata.len()).unwrap_or_default();
	let estimate = cost(shared, &format, &plans, bytes);

	// A rename is a fast gesture and a forty gigabyte video is not, so an oversized
	// source is not started unasked. It is not refused either: the request is valid,
	// and `+now` is the person saying they meant it, `docs/watch.md` §6.
	if !resolved.meta.now && !shared.registry().watch().admits(bytes) {
		return park(
			shared,
			&stem,
			root,
			&format,
			target,
			id,
			batch,
			Reason::Size,
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

	// `now` skips both cost triggers for this job; the mode and the directory skip one.
	if !resolved.meta.now && cost_defers(shared, policy, &estimate, bytes) {
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

/// A request that cannot run as asked: park it with what went wrong, both intent
/// strings, and a repair when the policy allows the guess.
#[allow(clippy::too_many_arguments)]
fn uncarried(
	shared: &Shared,
	pool: &Pool,
	root: &Path,
	stem: &str,
	format: &str,
	target: &Path,
	id: &JobId,
	batch: Option<&str>,
	parsed: &intent::Intent,
	requested: Option<String>,
	problems: Diagnoses,
	policy: &Policy,
	resolved: Option<&Resolved>,
) -> Verdict {
	let reason = reason_of(&problems);
	let corrected = corrected(&shared.registry(), parsed, format, policy);

	if !requests_confirmation(parsed, &shared.registry()) {
		if let Some(verdict) = repair(
			shared,
			pool,
			stem,
			format,
			target,
			id,
			batch,
			reason,
			requested.as_deref(),
			corrected.as_ref(),
		) {
			return verdict;
		}
	}

	park(
		shared,
		stem,
		root,
		format,
		target,
		id,
		batch,
		reason,
		Parking {
			requested,
			corrected,
			diagnosis: Some(&problems),
			resolved,
			consume: true,
			..Parking::default()
		},
	)
}

fn requests_confirmation(parsed: &intent::Intent, registry: &Registry) -> bool {
	parsed.meta.ask
		|| parsed.groups.iter().flat_map(|group| &group.tokens).any(|token| {
			registry
				.resolve(&token.name)
				.and_then(|found| found.op())
				.is_some_and(|op| op.key == "ask")
		})
}

/// Whether the estimate crosses the threshold that applies, `docs/deferral.md` §3.
fn cost_defers(shared: &Shared, directory: &Policy, estimate: &Option<Cost>, bytes: u64) -> bool {
	let Some(estimate) = estimate else {
		// An unknown cost is not a reason to ask: turning "we do not know" into "you
		// must answer" is worse than running.
		return false;
	};

	let registry = shared.registry();
	let deferral = registry.deferral();

	if deferral.mode == Mode::Now {
		return false;
	}

	let has_undo = keeps_a_record(shared, bytes);

	// A directory may suppress the ordinary confirmation, and may not remove the brake
	// on a job whose source would be destroyed with no record, `docs/deferral.md` §5.
	if has_undo && !directory.confirm {
		return false;
	}

	Duration::from_millis(estimate.ms) > deferral.threshold(has_undo)
}

/// Whether an undo record could be kept for a file of this size, decided without
/// dropping anything, `docs/dev/architecture/state.md` §5.5.
fn keeps_a_record(shared: &Shared, bytes: u64) -> bool {
	let registry = shared.registry();
	let policy = registry.undo();

	policy.enabled
		&& Store::open(&shared.state_dir)
			.room_for(bytes, policy)
			.is_ok_and(|room| matches!(room, Room::Kept { .. }))
}

/// What the whole job is expected to cost, or nothing when any output is unknown.
fn cost(shared: &Shared, format: &str, plans: &[Plan], bytes: u64) -> Option<Cost> {
	let registry = shared.registry();
	let calibration = Calibration::open(&shared.state_dir).ok()?;
	let (mut ms, mut total) = (0u64, 0u64);

	for plan in plans {
		let family = registry
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
fn corrected(registry: &Registry, parsed: &intent::Intent, format: &str, policy: &Policy) -> Option<Correction> {
	let mut text = String::new();
	let mut dropped = Vec::new();
	let mut kept_any = false;

	for (index, group) in parsed.groups.iter().enumerate() {
		let target = registry.canonical_format(&group.format)?;

		if index > 0 {
			text.push(',');
		}
		text.push_str(target);

		for token in &group.tokens {
			let found = registry.resolve(&token.name);
			let op = found.as_ref().and_then(|found| found.op());

			// An operation the directory refuses is not carried either: offering to run
			// it would be an offer the runner would refuse in turn.
			let carries = op.is_some_and(|op| {
				op.is_implemented()
					&& op.applies_to(target)
					&& !policy.deny.iter().any(|denied| *denied == op.name || *denied == op.key)
			});

			let mut written = token.name.clone();

			if let Some(value) = &token.value {
				written.push_str(&value.text);
			}

			if !carries {
				dropped.push(written);
				continue;
			}

			text.push('+');
			text.push_str(&written);
			kept_any = true;
		}
	}

	if !kept_any && text.is_empty() {
		return None;
	}

	// The reduced request has to be one that runs, or offering it would be another
	// promise the job cannot keep.
	let reduced = intent::parse(&text).ok()?;
	let mut resolved = resolve::resolve(
		&reduced,
		Origin {
			format,
			is_directory: false,
		},
		registry,
	)
	.ok()?;

	if !policy.apply(&mut resolved).is_empty() {
		return None;
	}

	// A request that produces nothing is not a repair: `png` from a `png` is the noop
	// the grammar refuses, and offering it would run a job with no outputs.
	let plans = plan::compile(registry, &retype_core::tools::Machine, &resolved).ok()?;

	if plans.is_empty() {
		return None;
	}

	Some(Correction {
		text: print::intent(&resolved, registry),
		dropped,
	})
}

/// The request as written minus the tokens that could not be carried.
struct Correction {
	/// The canonical intent that would run.
	text: String,
	/// The tokens that were left out, as they were written.
	dropped: Vec<String>,
}

/// Run the corrected request instead of parking, when the policy says to,
/// `docs/deferral.md` §3 and §6.
///
/// The guess is bounded three ways: it is opt-in, it needs an undo record so the
/// result can be dropped in one click, and the job it guesses about must be no more
/// expensive than the ordinary threshold. Anything else parks, which is the default.
#[allow(clippy::too_many_arguments)]
fn repair(
	shared: &Shared,
	pool: &Pool,
	stem: &str,
	format: &str,
	target: &Path,
	id: &JobId,
	batch: Option<&str>,
	reason: Reason,
	requested: Option<&str>,
	correction: Option<&Correction>,
) -> Option<Verdict> {
	let registry = shared.registry();
	let policy = registry.deferral();
	let correction = correction?;

	if policy.repair != Repair::Convert {
		return None;
	}

	let bytes = fs::metadata(target).map(|metadata| metadata.len()).unwrap_or_default();

	// A repair is not consent for an oversized source: it parks instead, and the
	// person answers both questions at once, `docs/watch.md` §6.
	if !shared.registry().watch().admits(bytes) {
		return None;
	}

	if !keeps_a_record(shared, bytes) {
		return None;
	}

	let cost = cost_of(shared, format, &correction.text, bytes)?;

	if Duration::from_millis(cost.ms) > policy.above {
		return None;
	}

	// The bytes go back under the name their content fits before anything runs, so
	// the file is not mislabelled while the conversion happens.
	let source = retype_core::convert::restore_source_name(target, stem, format).ok()?;
	shared.recent.note(&source, target);
	let source_metadata = fs::metadata(&source).ok();

	let job = Job {
		id: id.clone(),
		created: now_ms(),
		updated: now_ms(),
		source: Source {
			size: source_metadata.as_ref().map(|metadata| metadata.len()).unwrap_or(bytes),
			path: source.clone(),
			format: format.to_string(),
			mtime_ns: source_metadata.as_ref().map(mtime_ns).unwrap_or_default(),
		},
		intent: Some(correction.text.clone()),
		outputs: Vec::new(),
		state: JobState::Queued,
		backend: None,
		engine: None,
		machine: shared.machine.clone(),
		error: None,
		parked: Some(retype_core::job::Parked {
			reason: reason.as_str().to_string(),
			deadline: None,
			batch: batch.map(str::to_string),
			estimate: Some(cost),
			requested: requested.map(str::to_string),
			corrected: Some(correction.text.clone()),
			dropped: correction.dropped.clone(),
			consume: true,
		}),
		consume_source: true,
	};

	let work = crate::daemon::timer::work(&job, Some(&correction.text), false).expect("a corrected job has an intent");
	match pool.submit_after(work, || watcher::publish_checked(shared, &job)) {
		Ok(true) => Some(Verdict::Repaired),
		Ok(false) => Some(park_repair_after_queue_full(shared, job)),
		Err(error) => {
			eprintln!(
				"retype: repair {id} did not start because admission could not be recorded: {error}; \
				 its outcome after restart is uncertain, so check the job before retrying"
			);
			Some(Verdict::Ignore)
		}
	}
}

fn park_repair_after_queue_full(shared: &Shared, mut job: Job) -> Verdict {
	job.set_state(JobState::NeedsAttention);
	job.error = Some(JobError::new(
		Code::JobState.to_string(),
		"the queue was full; confirm the corrected request after capacity is available",
	));
	match watcher::publish_checked(shared, &job) {
		Ok(()) => Verdict::Parked,
		Err(error) => {
			eprintln!("retype: cannot record the waiting repair {}: {error}", job.id);
			Verdict::Ignore
		}
	}
}

fn mtime_ns(metadata: &fs::Metadata) -> i64 {
	metadata
		.modified()
		.ok()
		.and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
		.map(|elapsed| elapsed.as_nanos() as i64)
		.unwrap_or_default()
}

/// What a request would cost, for a repair that has to know before it guesses.
fn cost_of(shared: &Shared, format: &str, text: &str, bytes: u64) -> Option<Cost> {
	let parsed = intent::parse(text).ok()?;
	let resolved = resolve::resolve(
		&parsed,
		Origin {
			format,
			is_directory: false,
		},
		&shared.registry(),
	)
	.ok()?;
	let plans = plan::compile(&shared.registry(), &retype_core::tools::Machine, &resolved).ok()?;

	cost(shared, format, &plans, bytes)
}

/// Everything a parked record needs besides the reason.
#[derive(Default)]
struct Parking<'a> {
	requested: Option<String>,
	corrected: Option<Correction>,
	estimate: Option<Cost>,
	resolved: Option<&'a Resolved>,
	plans: &'a [Plan],
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
	let source_path = match retype_core::convert::restore_source_name(target, stem, format) {
		Ok(source) => source,
		Err(error) => {
			let asks = reason == Reason::Ask
				|| parking
					.requested
					.as_deref()
					.and_then(|text| intent::parse(text).ok())
					.is_some_and(|intent| requests_confirmation(&intent, &shared.registry()));
			if asks {
				eprintln!(
					"retype: cannot restore {} for confirmation: {error}; conversion did not start",
					target.display()
				);
				return Verdict::Ignore;
			}
			return Verdict::Run;
		}
	};

	// The restore is a rename of our own, and the watcher must not read it as the user
	// asking for the inverse conversion: `a.webp` back to `a.png` is what parking looks
	// like from outside, and without this it parks itself again, for ever.
	shared.recent.note(&source_path, target);

	let state = reason.parked();
	let deadline = match state {
		Parked::Pending => i64::try_from(shared.registry().deferral().pause.as_millis())
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

	let source_metadata = fs::metadata(&source_path).ok();

	let job = Job {
		id: id.clone(),
		created: now_ms(),
		updated: now_ms(),
		source: Source {
			size: source_metadata
				.as_ref()
				.map(|metadata| metadata.len())
				.unwrap_or_default(),
			path: source_path,
			format: format.to_string(),
			mtime_ns: source_metadata.as_ref().map(mtime_ns).unwrap_or_default(),
		},
		intent: parking
			.resolved
			.map(|resolved| print::intent(resolved, &shared.registry())),
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
			corrected: parking.corrected.as_ref().map(|correction| correction.text.clone()),
			dropped: parking
				.corrected
				.as_ref()
				.map(|correction| correction.dropped.clone())
				.unwrap_or_default(),
			consume: parking.consume,
		}),
		consume_source: true,
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

	/// Whether this machine can carry the pair the reduced request asks for.
	///
	/// The correction is an offer to run something, so it compiles with the real
	/// machine. A host with no usable image backend has nothing to offer, and the
	/// test proves nothing there.
	fn can_carry_png_to_webp(registry: &Registry) -> bool {
		let parsed = intent::parse("webp+quality80").expect("scans");
		let resolved = resolve::resolve(
			&parsed,
			Origin {
				format: "png",
				is_directory: false,
			},
			registry,
		)
		.expect("resolves");

		plan::compile(registry, &retype_core::tools::Machine, &resolved).is_ok()
	}

	/// A directory with no policy file, which narrows nothing.
	fn nothing() -> (tempfile::TempDir, Policy) {
		let dir = tempfile::tempdir().expect("a directory");
		let policy = Policy::open(dir.path()).expect("a policy");
		(dir, policy)
	}

	#[test]
	fn a_request_that_cannot_be_carried_is_reduced_to_one_that_can() {
		let registry = registry();

		if !can_carry_png_to_webp(&registry) {
			eprintln!("no backend on this machine can carry png to webp, so this test proves nothing");
			return;
		}

		let (_dir, policy) = nothing();
		let parsed = intent::parse("webp+quality80+zzz").expect("scans");

		let correction = corrected(&registry, &parsed, "png", &policy).expect("a correction");

		assert_eq!(correction.text, "webp+quality80");
		assert_eq!(correction.dropped, vec!["zzz".to_string()]);
	}

	#[test]
	fn a_request_whose_target_survives_nothing_has_no_correction() {
		let registry = registry();
		let (_dir, policy) = nothing();
		let parsed = intent::parse("png+quality80").expect("scans");

		// Dropping `quality80` leaves `png` from a `png`, which is the noop the grammar
		// refuses, so there is nothing to offer.
		assert!(corrected(&registry, &parsed, "png", &policy).is_none());
	}
}
