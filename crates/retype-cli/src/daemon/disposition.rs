//! Shared source disposition after durable job completion.

use std::path::Path;

use retype_core::error::Result;
use retype_core::execute::Cancellation;
use retype_core::job::{Job, JobState, SourceDisposition, now_ms};
use retype_core::resolve::Resolved;
use retype_core::safety::Fingerprint;
use retype_core::undo::Recovery;

use super::watcher::{Shared, persist};

pub fn consumes_source(resolved: &Resolved) -> bool {
	!resolved.meta.keep && !resolved.groups.iter().any(|group| group.keep_source)
}

pub enum Source<'a> {
	Live {
		path: &'a Path,
		fingerprint: &'a Fingerprint,
	},
	Displaced {
		target: &'a Path,
	},
}

pub struct Completion<'a> {
	pub source: Source<'a>,
	pub recovery: Recovery<'a>,
	pub requested_outputs: usize,
	pub validation_failure: Option<&'a str>,
}

#[derive(Debug)]
pub struct CompletionFailure {
	pub job: Box<Job>,
	pub error: retype_core::error::Error,
}

struct DurableCompletion {
	job: Job,
}

/// The owned snapshot whose source-disposition outcome has been durably recorded.
#[derive(Debug)]
pub struct DispositionRecorded {
	job: Job,
}

impl DispositionRecorded {
	pub fn into_job(self) -> Job {
		self.job
	}
}

impl DurableCompletion {
	fn record(
		shared: &Shared,
		mut job: Job,
		record: &mut impl FnMut(&Shared, &Job) -> Result<()>,
	) -> std::result::Result<Self, CompletionFailure> {
		if job.state != JobState::Done {
			return Err(CompletionFailure {
				job: Box::new(job),
				error: std::io::Error::other("source disposition requires a completed job").into(),
			});
		}
		job.source_disposition = Some(SourceDisposition::Pending);
		match record(shared, &job) {
			Ok(()) => Ok(Self { job }),
			Err(error) => Err(CompletionFailure {
				job: Box::new(job),
				error,
			}),
		}
	}

	fn settle(
		self,
		shared: &Shared,
		completion: Completion<'_>,
		cancellation: &Cancellation,
		record: &mut impl FnMut(&Shared, &Job) -> Result<()>,
	) -> std::result::Result<DispositionRecorded, CompletionFailure> {
		let mut job = self.job;
		let disposition = decide(&job, &completion, cancellation);
		let settled = !matches!(&disposition, SourceDisposition::Unresolved { .. });
		match &disposition {
			SourceDisposition::Retained { reason } => eprintln!("retype: original retained: {reason}"),
			SourceDisposition::Unresolved { reason } => eprintln!("retype: source disposition unresolved: {reason}"),
			SourceDisposition::Pending | SourceDisposition::Kept | SourceDisposition::Removed => {}
		}
		job.source_disposition = Some(disposition);
		job.updated = now_ms();
		if let Err(error) = record(shared, &job) {
			return Err(CompletionFailure {
				job: Box::new(job),
				error,
			});
		}
		if let Recovery::Retained(recovery) = completion.recovery {
			if settled {
				if let Err(error) = recovery.finish() {
					eprintln!("retype: undo record remains pinned: {error}");
				}
			} else {
				eprintln!("retype: undo record remains pinned: the original was not verified");
			}
		}
		Ok(DispositionRecorded { job })
	}
}

/// Transfer completion ownership through the durable checkpoint and source-disposition outcome.
pub fn finish_done(
	shared: &Shared,
	job: Job,
	completion: Completion<'_>,
	cancellation: &Cancellation,
) -> std::result::Result<DispositionRecorded, CompletionFailure> {
	finish_done_with(shared, job, completion, cancellation, persist)
}

fn finish_done_with(
	shared: &Shared,
	job: Job,
	completion: Completion<'_>,
	cancellation: &Cancellation,
	mut record: impl FnMut(&Shared, &Job) -> Result<()>,
) -> std::result::Result<DispositionRecorded, CompletionFailure> {
	if completion.recovery.id() != &job.id {
		return Err(CompletionFailure {
			job: Box::new(job),
			error: std::io::Error::other("completion belongs to another recovery job").into(),
		});
	}
	DurableCompletion::record(shared, job, &mut record)?.settle(shared, completion, cancellation, &mut record)
}

fn decide(job: &Job, completion: &Completion<'_>, cancellation: &Cancellation) -> SourceDisposition {
	let (path, fingerprint) = match &completion.source {
		Source::Live { path, fingerprint } => (*path, *fingerprint),
		Source::Displaced { target } => {
			return SourceDisposition::Unresolved {
				reason: format!(
					"source name is occupied; check {} and the recovery record",
					target.display()
				),
			};
		}
	};
	if !job.consume_source {
		return retained(path, fingerprint, None);
	}
	let reason = if completion.requested_outputs == 0 || job.outputs.len() != completion.requested_outputs {
		Some("the requested output inventory is incomplete".to_string())
	} else if let Some(reason) = completion.validation_failure {
		Some(reason.to_string())
	} else if completion.recovery.retained().is_none() {
		Some("a verified undo backup is unavailable".to_string())
	} else {
		None
	};
	if let Some(reason) = reason {
		return retained(path, fingerprint, Some(reason));
	}
	match cancellation.commit(|| {
		let recovery = completion.recovery.retained().expect("retained recovery checked above");
		let _guard = recovery.guard_disposal(&job.outputs)?;
		retype_core::safety::remove_owned(path, fingerprint)
	}) {
		Ok(()) => SourceDisposition::Removed,
		Err(error) => retained(path, fingerprint, Some(error.to_string())),
	}
}

fn retained(path: &Path, fingerprint: &Fingerprint, reason: Option<String>) -> SourceDisposition {
	match fingerprint.matches(path) {
		Ok(true) => match reason {
			Some(reason) => SourceDisposition::Retained { reason },
			None => SourceDisposition::Kept,
		},
		Ok(false) => SourceDisposition::Unresolved {
			reason: format!("{} changed or was replaced during conversion", path.display()),
		},
		Err(error) => SourceDisposition::Unresolved {
			reason: format!("cannot verify {} after conversion: {error}", path.display()),
		},
	}
}

#[cfg(all(test, unix))]
mod tests {
	use std::fs;

	use retype_core::job::{Output, OutputState, Source as JobSource, Tier};
	use retype_core::undo::{Output as UndoOutput, Store, Subject};

	use super::*;

	#[test]
	fn a_failed_outcome_checkpoint_reports_removed_source_and_keeps_recovery_pinned() {
		let dir = tempfile::tempdir().unwrap();
		let state = dir.path().join("state");
		let server = super::super::Server::open(&state).unwrap();
		server.pool().stop();
		let shared = server.shared();
		let source = dir.path().join("source.tar");
		let output = dir.path().join("output.zip");
		fs::write(&source, b"original bytes").unwrap();
		fs::write(&output, b"verified output").unwrap();
		let fingerprint = Fingerprint::of(&source).unwrap();
		let mut produced = Output::pending(output.clone(), "zip", Tier::Converted);
		produced.state = OutputState::Done;
		let mut job = Job::new(
			JobSource {
				path: source.clone(),
				format: "tar".to_string(),
				size: fingerprint.bytes,
				mtime_ns: 0,
				fingerprint: None,
			},
			Some("zip".to_string()),
			vec![produced.clone()],
			"test",
		)
		.unwrap();
		job.state = JobState::Done;
		job.consume_source = true;
		let store = Store::open(&state);
		store
			.save(Subject {
				id: job.id.clone(),
				intent: "zip".to_string(),
				machine: "test".to_string(),
				bytes: source.clone(),
				source_path: source.clone(),
				source_ext: "tar".to_string(),
				outputs: vec![UndoOutput {
					path: output.clone(),
					ext: "zip".to_string(),
				}],
				consume_source: true,
			})
			.unwrap();
		let output_fingerprint = Fingerprint::of(&output).unwrap();
		store
			.prepare_checked_output(&job.id, produced, &output, Some(&output_fingerprint))
			.unwrap();
		let lease = store.lease(&job.id).unwrap();
		let recovery = store.retain(lease, store.load(&job.id).unwrap()).unwrap();
		let completion = Completion {
			source: Source::Live {
				path: &source,
				fingerprint: &fingerprint,
			},
			recovery,
			requested_outputs: 1,
			validation_failure: None,
		};
		let mut writes = 0;
		let result = finish_done_with(&shared, job, completion, &Cancellation::default(), |shared, job| {
			writes += 1;
			if writes == 1 {
				persist(shared, job)
			} else {
				Err(std::io::Error::other("injected journal failure").into())
			}
		});
		let failure = result.unwrap_err();
		let job = failure.job;
		let error = failure.error;
		assert!(!source.exists());
		assert!(output.exists());
		assert!(store.load(&job.id).unwrap().unwrap().pinned);
		assert!(store.source(&job.id).unwrap().is_some());
		assert_eq!(job.source_disposition, Some(SourceDisposition::Removed));
		super::super::watcher::report_unrecorded_completion(&shared, &job, &error);
		let jobs = shared.jobs.lock().unwrap();
		let reported = jobs.iter().find(|candidate| candidate.id == job.id).unwrap();
		assert_eq!(reported.state, JobState::Partial);
		let message = &reported.error.as_ref().unwrap().message;
		assert!(message.contains("source was removed"), "{message}");
		assert!(message.contains("not durable"), "{message}");
		drop(jobs);
		drop(shared);
		drop(server);

		let restarted = super::super::Server::open(&state).unwrap();
		restarted.recoveries().wait();
		restarted.pool().stop();
		let shared = restarted.shared();
		let jobs = shared.jobs.lock().unwrap();
		let recovered = jobs.iter().find(|candidate| candidate.id == job.id).unwrap();
		assert!(matches!(
			recovered.source_disposition,
			Some(SourceDisposition::Unresolved { .. })
		));
		assert!(store.load(&job.id).unwrap().unwrap().pinned);
		assert!(store.source(&job.id).unwrap().is_some());
	}
}
