//! Saying what a batch of jobs did.
//!
//! One notification per rename batch, because a directory full of photographs should
//! not be twelve interruptions. The message is built from the records, so it says what
//! happened rather than what was attempted, and the only action this build offers is
//! Undo, which is the one that does not need the GUI.
//!
//! Capability is a property of the platform, so there are three tiers and the daemon
//! uses the best one it has:
//!
//! | Tier | How | Where |
//! |---|---|---|
//! | Actions | `notify-send --wait --action` | Linux with a session bus and a libnotify build that has actions |
//! | Plain | `notify-send` | Linux with a session bus |
//! | Log | a line on the daemon's stderr | everywhere else, including Windows and macOS for now |
//!
//! The Windows toast with an AppUserModelID and the macOS User Notifications centre need
//! a signed bundle and a protocol handler, which is the first artifact of rather
//! than a library call; until then those platforms get the log line and nothing pretends
//! otherwise.

use std::process::{Command, Stdio};

use retype_core::job::{Job, JobId, JobState};

/// What the daemon tells the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
	pub title: String,
	pub body: String,
	/// The jobs an Undo would reverse, empty when there is nothing to undo.
	pub undo: Vec<JobId>,
}

impl Message {
	/// Describe a finished batch, or nothing when the batch did nothing worth saying.
	pub fn of(jobs: &[Job], undo_enabled: bool) -> Option<Self> {
		if jobs.is_empty() {
			return None;
		}

		let count = |state: JobState| jobs.iter().filter(|job| job.state == state).count();

		let done = count(JobState::Done) + count(JobState::Partial);
		let failed = count(JobState::Failed);
		let cancelled = count(JobState::Cancelled) + count(JobState::Superseded);
		let parked = count(JobState::Pending) + count(JobState::NeedsAttention);

		let mut parts = Vec::new();
		if done > 0 {
			parts.push(format!("{done} converted"));
		}
		if failed > 0 {
			parts.push(format!("{failed} failed"));
		}
		if cancelled > 0 {
			parts.push(format!("{cancelled} cancelled"));
		}
		if parked > 0 {
			parts.push(format!("{parked} parked"));
		}

		if parts.is_empty() {
			return None;
		}

		let body = parts.join(", ");

		// The first thing that went wrong is worth more than a count: a code and a file
		// are what the user needs to act.
		let detail = jobs.iter().find_map(|job| {
			let error = job.error.as_ref()?;
			let path = job.source.path.file_name()?.to_string_lossy().into_owned();

			Some(format!("{path}: {}", error.message))
		});

		let body = match (detail, parked) {
			(Some(detail), _) => format!("{body} ({detail})"),
			(None, parked) if parked > 0 => {
				format!("{body}; `retype jobs --interactive` decides what happens")
			}
			(None, _) => body,
		};

		let undo = match undo_enabled {
			true => jobs
				.iter()
				.filter(|job| matches!(job.state, JobState::Done | JobState::Partial))
				.map(|job| job.id.clone())
				.collect(),
			false => Vec::new(),
		};

		Some(Self {
			title: "retype".to_string(),
			body,
			undo,
		})
	}
}

/// The best way this machine can say something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
	/// A notification with an Undo button.
	Actions,
	/// A notification without buttons.
	Plain,
	/// A line on the daemon's own output.
	Log,
}

impl Channel {
	/// What this machine can do, decided once at startup.
	pub fn detect(actions: bool) -> Self {
		if !has_session() || !has_notifier() {
			return Self::Log;
		}

		match actions {
			true => Self::Actions,
			false => Self::Plain,
		}
	}

	/// Show the message, and act on what the user pressed.
	///
	/// `undo` is called only when the platform reported the action, which is why the
	/// actionable tier waits: `notify-send --wait` answers on its own stdout, and there
	/// is nothing else to listen to.
	pub fn send(&self, message: &Message, undo: impl FnOnce(&[JobId])) {
		match self {
			Self::Log => {
				eprintln!("retype: {}: {}", message.title, message.body);
			}
			Self::Plain => {
				let _ = notify_send(message, false);
			}
			Self::Actions => match message.undo.is_empty() {
				// Nothing to offer, so nothing to wait for.
				true => {
					let _ = notify_send(message, false);
				}
				false => {
					let action = notify_send(message, true);

					if action.as_deref() == Some("undo") {
						undo(&message.undo);
					}
				}
			},
		}
	}
}

/// Run `notify-send`, answering with the action the user chose, if any.
fn notify_send(message: &Message, with_action: bool) -> Option<String> {
	let mut command = Command::new("notify-send");
	command.args(["--app-name", "retype", "--expire-time", "15000"]);

	if with_action {
		command.args(["--wait", "--action", "undo=Undo"]);
	}

	let output = command
		.arg(&message.title)
		.arg(&message.body)
		.stdin(Stdio::null())
		.stderr(Stdio::null())
		.output()
		.ok()?;

	let answer = String::from_utf8_lossy(&output.stdout).trim().to_string();
	(!answer.is_empty()).then_some(answer)
}

/// Whether there is a session to appear in at all.
fn has_session() -> bool {
	["DBUS_SESSION_BUS_ADDRESS", "WAYLAND_DISPLAY", "DISPLAY"]
		.iter()
		.any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}

/// Whether a notifier command exists. `notify-send` is libnotify's own client and the
/// one every desktop session ships.
fn has_notifier() -> bool {
	retype_core::tools::find_in_path(&std::env::var_os("PATH").unwrap_or_default(), "notify-send").is_some()
}

#[cfg(test)]
mod tests {
	use super::*;
	use retype_core::job::{Job, JobError, Output, Source, Tier};

	fn job(id: usize, state: JobState) -> Job {
		let mut job = Job::new(
			Source {
				path: format!("/photos/{id}.heic").into(),
				format: "heic".to_string(),
				size: 0,
				mtime_ns: 0,
			},
			Some("jpg".to_string()),
			vec![Output::pending(
				format!("/photos/{id}.jpg").into(),
				"jpg".to_string(),
				Tier::Converted,
			)],
			"machine",
		)
		.expect("a job");

		job.id = JobId::parse(&format!("01J{:023}", id)).expect("an id");
		job.state = state;
		job
	}

	#[test]
	fn a_batch_of_conversions_is_one_line_and_an_undo() {
		let jobs = vec![job(1, JobState::Done), job(2, JobState::Done)];

		let message = Message::of(&jobs, true).expect("a message");

		assert_eq!(message.body, "2 converted");
		assert_eq!(message.undo.len(), 2);
	}

	#[test]
	fn a_failure_is_named_rather_than_counted() {
		let mut failed = job(1, JobState::Failed);
		failed.error = Some(JobError {
			code: "E_TOOL_MISSING".to_string(),
			message: "magick is not installed".to_string(),
		});

		let message = Message::of(&[job(2, JobState::Done), failed], true).expect("a message");

		assert_eq!(message.body, "1 converted, 1 failed (1.heic: magick is not installed)");
	}

	#[test]
	fn a_parked_batch_says_where_to_decide_and_offers_no_undo() {
		let message = Message::of(&[job(1, JobState::Pending)], true).expect("a message");

		assert_eq!(
			message.body,
			"1 parked; `retype jobs --interactive` decides what happens"
		);
		assert!(message.undo.is_empty());
	}

	#[test]
	fn with_undo_disabled_there_is_no_button() {
		let message = Message::of(&[job(1, JobState::Done)], false).expect("a message");

		assert!(message.undo.is_empty());
	}

	#[test]
	fn a_batch_with_nothing_to_say_says_nothing() {
		assert!(Message::of(&[], true).is_none());
	}
}
