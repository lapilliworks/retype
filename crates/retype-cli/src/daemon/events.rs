//! The event ring, `docs/dev/architecture/ipc.md` §7.
//!
//! Every published job snapshot becomes one event, numbered from one. The last
//! thousand or so are kept so a client that has been away can catch up by asking
//! for what followed the last number it saw, which is what the single-connection
//! protocol allows: a client polls `events` with `since` rather than holding a
//! stream open.
//!
//! Events are advisory. `jobs.list` and `jobs.get` are the authoritative answers,
//! and a client that falls out of the window reloads with them.

use std::collections::VecDeque;

use retype_core::job::Job;
use serde_json::{Value, json};

/// How many events are kept for replay, `docs/dev/architecture/ipc.md` §7.
pub const RETAINED: usize = 1024;

/// The ring and the sequence it hands out.
#[derive(Debug, Default)]
pub struct Events {
	/// The sequence the next event will carry, so the first event is one.
	next: u64,
	retained: VecDeque<Value>,
}

impl Events {
	pub fn push(&mut self, job: &Job) -> u64 {
		self.next += 1;

		self.retained
			.push_back(json!({"event": "job", "seq": self.next, "job": job}));

		while self.retained.len() > RETAINED {
			self.retained.pop_front();
		}

		self.next
	}

	/// Everything that followed `since`, and the newest sequence, so a client knows
	/// where it now stands.
	///
	/// `since = 0` asks for everything retained. A client that has fallen out of the
	/// window gets nothing, which is what `E_IPC_RESYNC` is made of: a gap cannot be
	/// filled, so the client reloads with `jobs.list`, `docs/dev/architecture/ipc.md` §7.
	pub fn since(&self, since: u64) -> Option<(Vec<Value>, u64)> {
		let oldest = self
			.retained
			.front()
			.and_then(|event| event.get("seq").and_then(Value::as_u64));

		if let Some(oldest) = oldest {
			if since.saturating_add(1) < oldest {
				return None;
			}
		}

		let mut events = Vec::new();
		let mut bytes = 0;
		let mut newest = self.next;
		for event in self
			.retained
			.iter()
			.filter(|event| event["seq"].as_u64().is_some_and(|seq| seq > since))
		{
			let size = serde_json::to_vec(event).expect("event values serialize").len() + 1;
			if bytes + size > retype_ipc::MAX_FRAME - 1024 {
				if events.is_empty() {
					return None;
				}
				break;
			}
			bytes += size;
			newest = event["seq"].as_u64().expect("numbered event");
			events.push(event.clone());
		}
		Some((events, newest))
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use retype_core::job::{JobId, JobState, Source};

	fn job(index: usize) -> Job {
		Job {
			id: JobId::parse(&format!("01J{:023}", index)).unwrap(),
			created: 0,
			updated: 0,
			source: Source {
				path: std::path::PathBuf::from("a.heic"),
				format: "heic".to_string(),
				size: 0,
				mtime_ns: 0,
				fingerprint: None,
			},
			intent: Some("jpg".to_string()),
			outputs: Vec::new(),
			state: JobState::Done,
			backend: None,
			engine: None,
			machine: "machine".to_string(),
			error: None,
			parked: None,
			consume_source: false,
			source_disposition: None,
			admission: None,
		}
	}

	#[test]
	fn events_are_numbered_from_one_and_replayed_after_a_sequence() {
		let mut events = Events::default();
		events.push(&job(1));
		events.push(&job(2));

		let (all, newest) = events.since(0).unwrap();
		assert_eq!(all.len(), 2);
		assert_eq!(all[0]["seq"], 1);
		assert_eq!(newest, 2);

		let (after_first, _) = events.since(1).unwrap();
		assert_eq!(after_first.len(), 1);
		assert_eq!(after_first[0]["seq"], 2);

		let (nothing, newest) = events.since(2).unwrap();
		assert!(nothing.is_empty());
		assert_eq!(newest, 2);
	}

	#[test]
	fn a_client_ahead_of_the_daemon_gets_nothing_rather_than_an_error() {
		let mut events = Events::default();
		events.push(&job(1));

		assert!(events.since(9).unwrap().0.is_empty());
	}

	#[test]
	fn the_oldest_events_fall_out_and_a_gap_asks_for_a_resync() {
		let mut events = Events::default();
		for index in 1..=RETAINED + 4 {
			events.push(&job(index));
		}

		// The window now starts at 5, so a client that has seen nothing, or has seen
		// only up to three, has lost events it can never be handed.
		assert!(events.since(0).is_none());
		assert!(events.since(3).is_none());
		let (cursor_events, cursor) = events.since(u64::MAX).unwrap();
		assert!(cursor_events.is_empty());
		assert_eq!(
			cursor as usize,
			RETAINED + 4,
			"state refresh must recover a cursor after a gap"
		);

		// Four is the last sequence the window can answer for: five is next.
		let (everything, _) = events.since(4).expect("the window starts at five");
		assert_eq!(everything.len(), RETAINED);

		let (replayed, newest) = events.since(5).unwrap();
		assert_eq!(replayed.len(), RETAINED - 1);
		assert_eq!(newest as usize, RETAINED + 4);
	}

	#[test]
	fn large_events_are_replayed_in_bounded_batches_without_skipping() {
		let mut events = Events::default();
		let mut large = job(1);
		large.intent = Some("x".repeat(2048));
		for _ in 0..RETAINED {
			events.push(&large);
		}
		let (first, cursor) = events.since(0).unwrap();
		assert!(cursor < RETAINED as u64);
		assert_eq!(first.len(), cursor as usize);
		let frame = serde_json::to_vec(&json!({"events": first, "seq": cursor})).unwrap();
		assert!(frame.len() < retype_ipc::MAX_FRAME);
		let (second, next) = events.since(cursor).unwrap();
		assert_eq!(second[0]["seq"], cursor + 1);
		assert!(next > cursor);
	}

	#[test]
	fn an_event_carries_the_job_it_describes() {
		let mut events = Events::default();
		events.push(&job(7));

		let (all, _) = events.since(0).unwrap();
		assert_eq!(all[0]["event"], "job");
		assert_eq!(all[0]["job"]["intent"], "jpg");
		assert_eq!(all[0]["job"]["state"], "done");
	}
}
