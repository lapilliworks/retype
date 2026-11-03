//! When a job is parked instead of run, `docs/deferral.md`.
//!
//! This module holds the policy and the vocabulary: the two states, the six
//! reasons, and the thresholds. What decides a job and what runs it lives in the
//! daemon, because it needs the estimator and the undo store.

use std::time::Duration;

/// Whether a parked job is waiting for its timer or for a person,
/// `docs/deferral.md` §1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parked {
	/// It can run as asked; the timer or a client ends the wait.
	Pending,
	/// Only a person can end the wait: either it cannot run as asked, or it may run
	/// only with explicit consent.
	Attention,
}

impl Parked {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Pending => "pending",
			Self::Attention => "needs-attention",
		}
	}
}

/// Why a job is parked, `docs/deferral.md` §2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
	/// The name carried `+ask`.
	Ask,
	/// The estimate is above the threshold that applies.
	Cost,
	/// The source is above the automatic size limit.
	Size,
	/// The intent in the new name does not parse.
	Intent,
	/// An operation does not apply to the target format.
	Inapplicable,
	/// No usable backend carries the pair.
	Backend,
}

impl Reason {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Ask => "ask",
			Self::Cost => "cost",
			Self::Size => "size",
			Self::Intent => "intent",
			Self::Inapplicable => "inapplicable",
			Self::Backend => "backend",
		}
	}

	pub fn parse(text: &str) -> Option<Self> {
		match text {
			"ask" => Some(Self::Ask),
			"cost" => Some(Self::Cost),
			"size" => Some(Self::Size),
			"intent" => Some(Self::Intent),
			"inapplicable" => Some(Self::Inapplicable),
			"backend" => Some(Self::Backend),
			_ => None,
		}
	}

	/// Which state a job with this reason is in: the first two are only a delay, the
	/// rest need a decision.
	pub fn parked(self) -> Parked {
		match self {
			Self::Ask | Self::Cost => Parked::Pending,
			Self::Size | Self::Intent | Self::Inapplicable | Self::Backend => Parked::Attention,
		}
	}

	/// Whether the job may run exactly as asked, once a person says so.
	///
	/// A source over the size limit is a valid request that needs consent, unlike a
	/// request that cannot be carried out at all.
	pub fn can_run_as_asked(self) -> bool {
		matches!(self, Self::Size)
	}
}

/// What a job that cannot run as asked does, `docs/deferral.md` §4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repair {
	/// Park it and say what went wrong. The default: a result the user did not ask
	/// for is worse than no result.
	Ask,
	/// Run it without the tokens that could not be carried, and report what was
	/// dropped. Needs an undo record, and is bounded by the ordinary threshold.
	Convert,
}

impl Repair {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Ask => "ask",
			Self::Convert => "convert",
		}
	}

	pub fn parse(text: &str) -> Option<Self> {
		match text {
			"ask" => Some(Self::Ask),
			"convert" => Some(Self::Convert),
			_ => None,
		}
	}
}

/// `mode` in `[deferral]`, `docs/config.md` §10.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
	/// Defer by `docs/deferral.md` §3.
	Ask,
	/// Skip the two cost triggers on this machine.
	Now,
}

impl Mode {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Ask => "ask",
			Self::Now => "now",
		}
	}

	pub fn parse(text: &str) -> Option<Self> {
		match text {
			"ask" => Some(Self::Ask),
			"now" => Some(Self::Now),
			_ => None,
		}
	}
}

/// The host-level policy, `docs/config.md` §10.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
	pub mode: Mode,
	/// Estimated duration above which a watched rename is parked while an undo record
	/// can be kept.
	pub above: Duration,
	/// The same, for a job whose source cannot be kept: a lower bar.
	pub above_without_undo: Duration,
	/// How long a parked job waits before it runs anyway.
	pub pause: Duration,
	/// What a job that cannot run as asked does.
	pub repair: Repair,
}

impl Default for Policy {
	fn default() -> Self {
		Self {
			mode: Mode::Ask,
			above: Duration::from_secs(10),
			above_without_undo: Duration::from_secs(2),
			pause: Duration::from_secs(60 * 60),
			repair: Repair::Ask,
		}
	}
}

impl Policy {
	/// The threshold that applies to a job, given whether its source can be kept.
	pub fn threshold(&self, has_undo: bool) -> Duration {
		match has_undo {
			true => self.above,
			false => self.above_without_undo,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_defaults_are_the_ones_the_document_states() {
		let policy = Policy::default();

		assert_eq!(policy.mode, Mode::Ask);
		assert_eq!(policy.above, Duration::from_secs(10));
		assert_eq!(policy.above_without_undo, Duration::from_secs(2));
		assert_eq!(policy.pause, Duration::from_secs(3_600));
		assert_eq!(policy.repair, Repair::Ask);
	}

	#[test]
	fn a_job_without_a_record_has_the_lower_bar() {
		let policy = Policy::default();

		assert_eq!(policy.threshold(true), Duration::from_secs(10));
		assert_eq!(policy.threshold(false), Duration::from_secs(2));
	}

	#[test]
	fn a_reason_knows_which_state_it_parks_in() {
		assert_eq!(Reason::Ask.parked(), Parked::Pending);
		assert_eq!(Reason::Cost.parked(), Parked::Pending);
		assert_eq!(Reason::Size.parked(), Parked::Attention);
		assert_eq!(Reason::Intent.parked(), Parked::Attention);
		assert_eq!(Reason::Inapplicable.parked(), Parked::Attention);
		assert_eq!(Reason::Backend.parked(), Parked::Attention);

		assert_eq!(Parked::Pending.as_str(), "pending");
		assert_eq!(Parked::Attention.as_str(), "needs-attention");
		assert!(Reason::Size.can_run_as_asked());
		assert!(!Reason::Intent.can_run_as_asked());
	}

	#[test]
	fn a_reason_parses_and_prints_the_same_way() {
		for text in ["ask", "cost", "size", "intent", "inapplicable", "backend"] {
			assert_eq!(Reason::parse(text).map(Reason::as_str), Some(text));
		}

		assert!(Reason::parse("later").is_none());
	}

	#[test]
	fn the_tokens_parse_and_print_the_same_way() {
		for text in ["ask", "now"] {
			assert_eq!(Mode::parse(text).map(Mode::as_str), Some(text));
		}

		for text in ["ask", "convert"] {
			assert_eq!(Repair::parse(text).map(Repair::as_str), Some(text));
		}

		assert!(Mode::parse("later").is_none());
		assert!(Repair::parse("guess").is_none());
	}
}
