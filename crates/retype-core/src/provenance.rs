//! What a file's record says, `docs/provenance.md`.
//!
//! The answer comes from the journal, which is the store that is always written.
//! A record is matched by file identity first, because a converted file is
//! renamed straight afterwards and the name in the record is then wrong; the
//! identity survives that. Identity alone is not trusted: file ids are recycled,
//! so the size has to agree as well.
//!
//! Path matching is the fallback for a file that has moved to another volume or
//! been copied, where the identity is new and nothing else is left. It is
//! weaker, and the answer says so rather than pretending the two are the same.

use std::path::{Component, Path, PathBuf};

use crate::job::{FileIdentity, Job, Output};

/// How sure the answer is, `docs/provenance.md` §8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
	/// The file is the one the record produced.
	Exact,
	/// Only the path matched: the same name, not necessarily the same bytes.
	PathOnly,
	/// Nothing matched.
	None,
}

impl Confidence {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Exact => "exact",
			Self::PathOnly => "path-only",
			Self::None => "none",
		}
	}
}

/// Which store answered, `docs/provenance.md` §6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Store {
	JournalIdentity,
	JournalPath,
	Unknown,
}

impl Store {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::JournalIdentity => "journal-identity",
			Self::JournalPath => "journal-path",
			Self::Unknown => "unknown",
		}
	}
}

/// A job and the output that answers for a path.
#[derive(Debug, Clone, Copy)]
pub struct Answer<'a> {
	pub job: Option<&'a Job>,
	pub output: Option<&'a Output>,
	pub store: Store,
	pub confidence: Confidence,
}

/// Read what the journal says about `path`.
///
/// Jobs arrive newest first, so the first match is the most recent job that
/// produced the file, which is the one whose settings still describe it.
pub fn answer<'a>(jobs: &'a [Job], path: &Path) -> Answer<'a> {
	let wanted = Wanted::of(path);
	let mut by_path: Option<(&'a Job, &'a Output)> = None;

	for job in jobs {
		for output in &job.outputs {
			if output.state != crate::job::OutputState::Done {
				continue;
			}

			if wanted.is_identity_of(output) {
				return Answer {
					job: Some(job),
					output: Some(output),
					store: Store::JournalIdentity,
					confidence: Confidence::Exact,
				};
			}

			if by_path.is_none() && wanted.is_path_of(&output.path) {
				by_path = Some((job, output));
			}
		}
	}

	match by_path {
		Some((job, output)) => Answer {
			job: Some(job),
			output: Some(output),
			store: Store::JournalPath,
			confidence: Confidence::PathOnly,
		},
		None => Answer {
			job: None,
			output: None,
			store: Store::Unknown,
			confidence: Confidence::None,
		},
	}
}

/// The path as it is asked about, in the two forms a recorded path can be
/// compared with: absolute, and resolved when it still exists.
struct Wanted {
	absolute: PathBuf,
	canonical: Option<PathBuf>,
	identity: Option<FileIdentity>,
	size: Option<u64>,
}

impl Wanted {
	fn of(path: &Path) -> Self {
		let canonical = std::fs::canonicalize(path).ok();
		let size = std::fs::metadata(path).ok().map(|meta| meta.len());

		Self {
			absolute: absolute(path),
			canonical,
			identity: FileIdentity::of(path),
			size,
		}
	}

	fn is_identity_of(&self, output: &Output) -> bool {
		let (Some(recorded), Some(current)) = (&output.identity, &self.identity) else {
			return false;
		};

		recorded.matches(current) && output.bytes.is_none_or(|bytes| Some(bytes) == self.size)
	}

	fn is_path_of(&self, recorded: &Path) -> bool {
		let candidate = absolute(recorded);

		self.absolute == candidate
			|| (self.canonical.is_some() && self.canonical == std::fs::canonicalize(recorded).ok())
	}
}

/// Make a path absolute and lexically clean, without touching the filesystem.
///
/// Recorded paths are absolute when the daemon wrote them, because watch roots
/// are canonical, and relative when `retype convert` was given a relative name.
/// Comparing them means putting both in the same frame first.
fn absolute(path: &Path) -> PathBuf {
	let joined = match path.is_absolute() {
		true => path.to_path_buf(),
		false => std::env::current_dir().unwrap_or_default().join(path),
	};

	let mut clean = PathBuf::new();

	for component in joined.components() {
		match component {
			Component::CurDir => {}
			Component::ParentDir => {
				clean.pop();
			}
			other => clean.push(other),
		}
	}

	clean
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::job::{Job, JobState, Source, Tier, now_ms};

	fn job(output: &Path, identity: Option<FileIdentity>, bytes: Option<u64>) -> Job {
		let mut output = Output::pending(output.to_path_buf(), "webp", Tier::Converted);
		output.state = crate::job::OutputState::Done;
		output.bytes = bytes;
		output.identity = identity;

		let mut job = Job::new(
			Source {
				path: PathBuf::from("/tmp/photo.png"),
				format: "png".to_string(),
				size: 6,
				mtime_ns: 0,
			},
			Some("webp".to_string()),
			vec![output],
			"machine",
		)
		.unwrap();

		job.set_state(JobState::Done);
		job
	}

	#[test]
	fn a_file_that_was_renamed_is_still_the_one_the_record_produced() {
		let dir = tempfile::tempdir().unwrap();
		let produced = dir.path().join("photo.webp");
		std::fs::write(&produced, b"pixels").unwrap();

		let jobs = [job(&produced, FileIdentity::of(&produced), Some(6))];
		let renamed = dir.path().join("holiday.webp");
		std::fs::rename(&produced, &renamed).unwrap();

		let answer = answer(&jobs, &renamed);

		assert_eq!(answer.confidence, Confidence::Exact);
		assert_eq!(answer.store, Store::JournalIdentity);
	}

	#[test]
	fn the_same_path_with_other_bytes_is_only_a_path_match() {
		let dir = tempfile::tempdir().unwrap();
		let produced = dir.path().join("photo.webp");
		std::fs::write(&produced, b"eight by").unwrap();

		// Eight bytes recorded, seven on disk: a different file at the same name.
		let jobs = [job(
			&produced,
			Some(FileIdentity::Unix {
				dev: 1,
				ino: 2,
				birth_ns: None,
			}),
			Some(8),
		)];

		let answer = answer(&jobs, &produced);

		assert_eq!(answer.confidence, Confidence::PathOnly);
		assert_eq!(answer.store, Store::JournalPath);
	}

	#[test]
	fn a_file_no_job_produced_is_unknown() {
		let dir = tempfile::tempdir().unwrap();
		let stranger = dir.path().join("stranger.webp");
		std::fs::write(&stranger, b"pixels").unwrap();

		let jobs = [job(&dir.path().join("photo.webp"), None, Some(6))];
		let answer = answer(&jobs, &stranger);

		assert_eq!(answer.confidence, Confidence::None);
		assert_eq!(answer.store, Store::Unknown);
		assert!(answer.job.is_none());
	}

	#[test]
	fn the_newest_job_that_produced_the_file_answers() {
		let dir = tempfile::tempdir().unwrap();
		let produced = dir.path().join("photo.webp");
		std::fs::write(&produced, b"pixels").unwrap();

		let older = job(&produced, FileIdentity::of(&produced), Some(6));
		let mut newer = job(&produced, FileIdentity::of(&produced), Some(6));
		newer.set_state(JobState::Done);
		newer.intent = Some("webp+quality60".to_string());

		// Newest first, which is the order `jobs` are kept in.
		let jobs = [newer, older];
		let answer = answer(&jobs, &produced);
		let job = answer.job.expect("a job");

		assert_eq!(job.intent.as_deref(), Some("webp+quality60"));
		assert!(job.created <= now_ms());
	}
}
