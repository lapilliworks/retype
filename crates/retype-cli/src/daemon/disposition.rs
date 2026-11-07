//! Shared source disposition after durable job completion.

use std::path::Path;

use retype_core::error::Result;
use retype_core::execute::Cancellation;
use retype_core::job::{Job, JobState, SourceDisposition, now_ms};
use retype_core::resolve::Resolved;
use retype_core::safety::Fingerprint;
use retype_core::undo::Store;

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
	pub store: &'a Store,
	pub has_backup: bool,
	pub requested_outputs: usize,
	pub validation_failure: Option<&'a str>,
}

/// Persist completion before disposal and its outcome before starting undo retention.
pub fn finish_done(
	shared: &Shared,
	job: &mut Job,
	completion: Completion<'_>,
	cancellation: &Cancellation,
) -> Result<()> {
	finish_done_with(shared, job, completion, cancellation, persist)
}

fn finish_done_with(
	shared: &Shared,
	job: &mut Job,
	completion: Completion<'_>,
	cancellation: &Cancellation,
	mut record: impl FnMut(&Shared, &Job) -> Result<()>,
) -> Result<()> {
	if job.state != JobState::Done {
		return Err(std::io::Error::other("source disposition requires a completed job").into());
	}
	job.source_disposition = Some(SourceDisposition::Pending);
	record(shared, job)?;
	let disposition = decide(job, &completion, cancellation);
	let settled = !matches!(&disposition, SourceDisposition::Unresolved { .. });
	match &disposition {
		SourceDisposition::Retained { reason } => eprintln!("retype: original retained: {reason}"),
		SourceDisposition::Unresolved { reason } => eprintln!("retype: source disposition unresolved: {reason}"),
		SourceDisposition::Pending | SourceDisposition::Kept | SourceDisposition::Removed => {}
	}
	job.source_disposition = Some(disposition);
	job.updated = now_ms();
	record(shared, job)?;
	if completion.has_backup && settled {
		if let Err(error) = completion.store.finish(&job.id) {
			eprintln!("retype: undo record remains pinned: {error}");
		}
	} else if completion.has_backup {
		eprintln!("retype: undo record remains pinned: the original was not verified");
	}
	Ok(())
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
	} else if !completion.has_backup {
		Some("a verified undo backup is unavailable".to_string())
	} else {
		None
	};
	if let Some(reason) = reason {
		return retained(path, fingerprint, Some(reason));
	}
	match cancellation.commit(|| {
		let _guard = completion.store.guard_disposal(&job.id, &job.outputs)?;
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
	use retype_core::undo::{Output as UndoOutput, Subject};

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
		let _lease = store.lease(&job.id).unwrap();
		let completion = Completion {
			source: Source::Live {
				path: &source,
				fingerprint: &fingerprint,
			},
			store: &store,
			has_backup: true,
			requested_outputs: 1,
			validation_failure: None,
		};
		let mut writes = 0;
		let result = finish_done_with(
			&shared,
			&mut job,
			completion,
			&Cancellation::default(),
			|shared, job| {
				writes += 1;
				if writes == 1 {
					persist(shared, job)
				} else {
					Err(std::io::Error::other("injected journal failure").into())
				}
			},
		);
		let error = result.unwrap_err();
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
		drop(_lease);
		drop(shared);
		drop(server);

		let restarted = super::super::Server::open(&state).unwrap();
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
