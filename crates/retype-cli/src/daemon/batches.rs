//! Which jobs belong to one rename batch.
//!
//! A batch is the set of renames one debounce window produced. Its jobs run at
//! different speeds, so the notification waits for the last of them: one interruption
//! for a directory of photographs rather than one per file.
//!
//! This is memory, not state. A notification is about what this process just did, and a
//! daemon that restarts has nothing to say about the jobs it did not run, so there is
//! nothing here worth keeping in a journal.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use retype_core::job::JobId;

#[derive(Default)]
pub struct Batches {
	inner: Mutex<BTreeMap<String, Batch>>,
}

#[derive(Default)]
struct Batch {
	jobs: BTreeSet<JobId>,
	finished: BTreeSet<JobId>,
}

impl Batches {
	/// Record that `id` belongs to `batch`, before anything about it is published.
	pub fn open(&self, batch: &str, id: &JobId) {
		if let Ok(mut inner) = self.inner.lock() {
			inner.entry(batch.to_string()).or_default().jobs.insert(id.clone());
		}
	}

	/// Forget a job that never became one, so a batch cannot wait for it.
	pub fn forget(&self, batch: &str, id: &JobId) {
		if let Ok(mut inner) = self.inner.lock() {
			if let Some(entry) = inner.get_mut(batch) {
				entry.jobs.remove(id);

				if entry.jobs.is_empty() {
					inner.remove(batch);
				}
			}
		}
	}

	/// Mark a job finished, and answer with its batch when it was the last one.
	pub fn finish(&self, id: &JobId) -> Option<Vec<JobId>> {
		let mut inner = self.inner.lock().ok()?;
		let name = inner
			.iter()
			.find(|(_, batch)| batch.jobs.contains(id))
			.map(|(name, _)| name.clone())?;

		let batch = inner.get_mut(&name)?;
		batch.finished.insert(id.clone());

		if batch.finished.len() < batch.jobs.len() {
			return None;
		}

		let jobs = batch.jobs.iter().cloned().collect();
		inner.remove(&name);
		Some(jobs)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn id(index: usize) -> JobId {
		JobId::parse(&format!("01J{:023}", index)).expect("an id")
	}

	#[test]
	fn a_batch_is_answered_when_its_last_job_finishes() {
		let batches = Batches::default();
		batches.open("b1", &id(1));
		batches.open("b1", &id(2));

		assert_eq!(batches.finish(&id(1)), None);
		assert_eq!(batches.finish(&id(2)), Some(vec![id(1), id(2)]));

		// And once only: the batch is gone with its answer.
		assert_eq!(batches.finish(&id(2)), None);
	}

	#[test]
	fn a_batch_whose_only_job_was_forgotten_never_answers() {
		let batches = Batches::default();
		batches.open("b1", &id(1));
		batches.forget("b1", &id(1));

		assert_eq!(batches.finish(&id(1)), None);
	}

	#[test]
	fn a_job_nobody_asked_about_is_not_in_a_batch() {
		let batches = Batches::default();

		assert_eq!(batches.finish(&id(9)), None);
	}
}
