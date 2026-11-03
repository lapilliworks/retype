//! Submitting a job and following it to a terminal state.

use std::time::Duration;

use retype_core::job::{Job, JobState};
use retype_ipc::{JobSubmit, JobSubmitted};

use crate::error::Result;
use crate::session::{self, Context};

/// How often a submitted job is asked about while it runs.
pub const POLL: Duration = Duration::from_millis(100);

/// Whether a state will not change again.
pub fn terminal(state: &str) -> bool {
	matches!(state, "done" | "failed" | "partial" | "cancelled" | "superseded")
}

pub fn is_terminal(state: JobState) -> bool {
	matches!(
		state,
		JobState::Done | JobState::Partial | JobState::Failed | JobState::Cancelled | JobState::Superseded
	)
}

/// Submit a job on a fresh connection.
///
/// The connection ends as soon as the submission is answered: holding it for the
/// length of a conversion would monopolise the daemon's single slot, including the
/// slot a `jobs.cancel` needs.
pub fn submit(context: &Context, request: &JobSubmit) -> Result<JobSubmitted> {
	let mut client = session::connect_or_spawn(context)?;
	Ok(client.jobs_submit(request)?)
}

/// Ask about one job until it reaches a state that will not change.
pub fn wait(context: &Context, id: &str, mut observed: impl FnMut(&Job)) -> Result<Job> {
	loop {
		let job = {
			let mut client = session::connect_or_spawn(context)?;
			client.jobs_get(id)?
		};

		if is_terminal(job.state) {
			return Ok(job);
		}

		observed(&job);
		std::thread::sleep(POLL);
	}
}

/// Submit and follow, which is what `retype convert` and a GUI action both need.
pub fn submit_and_wait(context: &Context, request: &JobSubmit, observed: impl FnMut(&Job)) -> Result<Job> {
	let submitted = submit(context, request)?;
	wait(context, submitted.id.as_str(), observed)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn only_a_settled_state_is_terminal() {
		assert!(terminal("done"));
		assert!(terminal("partial"));
		assert!(terminal("failed"));
		assert!(terminal("cancelled"));
		assert!(terminal("superseded"));
		assert!(!terminal("queued"));
		assert!(!terminal("running"));
		assert!(!terminal("pending"));
		assert!(!terminal("needs-attention"));
	}

	#[test]
	fn the_typed_state_agrees_with_the_text_one() {
		for state in [
			JobState::Queued,
			JobState::Running,
			JobState::Done,
			JobState::Partial,
			JobState::Failed,
			JobState::Cancelled,
			JobState::Superseded,
			JobState::Pending,
			JobState::NeedsAttention,
		] {
			let text = match state {
				JobState::Queued => "queued",
				JobState::Running => "running",
				JobState::Done => "done",
				JobState::Partial => "partial",
				JobState::Failed => "failed",
				JobState::Cancelled => "cancelled",
				JobState::Superseded => "superseded",
				JobState::Pending => "pending",
				JobState::NeedsAttention => "needs-attention",
			};

			assert_eq!(is_terminal(state), terminal(text), "{state:?}");
		}
	}
}
