//! The journal: an append-only file of job snapshots.
//!
//! Rules are in `docs/dev/architecture/state.md` §4. The ones that shape this module: records are
//! never edited, a snapshot is written per transition, replay is last wins per
//! id, a torn trailing line is a write that was interrupted and is dropped,
//! anything unparseable before the end is corruption and the file is moved
//! aside, and a record from another schema version is skipped so a downgrade
//! still starts.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::job::{JOURNAL_VERSION, Job, JobId, now_ms};

/// When compaction is worth doing, `docs/dev/architecture/state.md` §4.
const MAX_LINES: usize = 10_000;

/// The journal outlives undo, because a record of a conversion from long ago is
/// more likely to mislead than to help, `docs/provenance.md` §7.
const HORIZON_FLOOR: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// What compaction did.
#[derive(Debug, Default)]
pub struct Compact {
	/// The live records that remain. Present only when the file was rewritten.
	pub kept: Option<Vec<Job>>,
	/// Records dropped by age, which is only some of what a rewrite removes.
	pub dropped: usize,
}

/// An open journal, positioned at the end.
#[derive(Debug)]
pub struct Journal {
	path: PathBuf,
	file: fs::File,
}

/// What a replay found. Every flag here is something a caller should report
/// once, rather than rediscover on each read.
#[derive(Debug, Default)]
pub struct Replay {
	/// The last snapshot of every job, in the order the ids were first seen.
	pub jobs: Vec<Job>,
	/// A trailing line that did not parse, which is an interrupted write.
	pub torn: bool,
	/// Records carrying a schema version this build does not read.
	pub skipped_versions: usize,
	/// Where the previous journal was moved after corruption was found.
	pub quarantined: Option<PathBuf>,
}

impl Journal {
	/// Open the journal, replaying whatever is already there.
	///
	/// Corruption is handled here rather than by the caller because it changes
	/// the file: the damaged journal is moved aside and a new one is started, so
	/// the next append cannot extend a file that would fail to replay again.
	pub fn open(path: &Path) -> Result<(Self, Replay)> {
		let replay = replay(path)?;

		if let Some(quarantined) = &replay.quarantined {
			fs::rename(path, quarantined)?;
		} else if replay.torn {
			// Drop the partial line before opening for append. Leaving it would make
			// it a non-trailing line as soon as the next record is written, which
			// turns an interrupted write into corruption on the following start.
			truncate_torn(path)?;
		}

		if let Some(parent) = path.parent() {
			fs::create_dir_all(parent)?;
			set_private_directory(parent)?;
		}

		let file = fs::OpenOptions::new().create(true).append(true).open(path)?;
		set_private_file(&file)?;

		Ok((
			Self {
				path: path.to_path_buf(),
				file,
			},
			replay,
		))
	}

	/// Rewrite the journal down to its live records, dropping those past the
	/// horizon.
	///
	/// It runs on startup and only when the file has earned it: ten thousand
	/// lines, or more records superseded than live. Rewriting is a rename, so a
	/// crash leaves either the old file or the new one, never a half-written
	/// journal.
	pub fn compact(&mut self, retention: Duration) -> Result<Compact> {
		let replay = replay(&self.path)?;
		let lines = self.lines()?;
		let superseded = lines.saturating_sub(replay.jobs.len());

		if lines <= MAX_LINES && superseded <= replay.jobs.len() {
			return Ok(Compact::default());
		}

		let horizon = (retention * 2).max(HORIZON_FLOOR);
		let cutoff = now_ms() - horizon.as_millis() as i64;
		let total = replay.jobs.len();

		let kept: Vec<Job> = replay.jobs.into_iter().filter(|job| job.updated >= cutoff).collect();
		let dropped = total - kept.len();

		self.rewrite(&kept)?;
		Ok(Compact {
			kept: Some(kept),
			dropped,
		})
	}

	fn lines(&self) -> Result<usize> {
		match fs::read(&self.path) {
			Ok(bytes) => Ok(bytes.iter().filter(|byte| **byte == b'\n').count()),
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
			Err(error) => Err(Error::Io(error)),
		}
	}

	/// Write `jobs` to a temporary file and rename it into place, then reopen the
	/// handle, which now points at the replacement.
	fn rewrite(&mut self, jobs: &[Job]) -> Result<()> {
		let temp = self.path.with_file_name(format!(
			"{}.compact",
			self.path.file_name().unwrap_or_default().to_string_lossy()
		));
		let mut text = String::new();

		for job in jobs {
			text.push_str(&job.to_line()?);
			text.push('\n');
		}

		let mut replacement = fs::OpenOptions::new()
			.write(true)
			.create(true)
			.truncate(true)
			.open(&temp)?;
		set_private_file(&replacement)?;
		replacement.write_all(text.as_bytes())?;
		replacement.sync_data()?;
		self.file.sync_all()?;

		crate::fsx::rename(&temp, &self.path)?;
		self.file = fs::OpenOptions::new().append(true).open(&self.path)?;
		Ok(())
	}

	pub fn path(&self) -> &Path {
		&self.path
	}

	/// Append one snapshot, syncing it when the transition is one the product
	/// promises to survive a power loss.
	pub fn append(&mut self, job: &Job) -> Result<()> {
		let mut line = job.to_line()?;
		line.push('\n');
		self.file.write_all(line.as_bytes())?;
		self.file.flush()?;

		self.file.sync_all()?;
		crate::fsx::sync_directory(self.path.parent().unwrap_or(Path::new(".")))?;

		Ok(())
	}
}

/// Remove the incomplete line at the end of the journal.
fn truncate_torn(path: &Path) -> Result<()> {
	let bytes = fs::read(path)?;
	let mut end = bytes.len();

	while end > 0 && bytes[end - 1].is_ascii_whitespace() {
		end -= 1;
	}

	let start = bytes[..end]
		.iter()
		.rposition(|byte| *byte == b'\n')
		.map_or(0, |newline| newline + 1);

	let file = fs::OpenOptions::new().write(true).open(path)?;
	file.set_len(start as u64)?;
	file.sync_all()?;
	Ok(())
}

fn replay(path: &Path) -> Result<Replay> {
	let contents = match fs::read(path) {
		Ok(contents) => contents,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Replay::default()),
		Err(error) => return Err(Error::Io(error)),
	};

	let lines: Vec<&[u8]> = contents
		.split(|byte| *byte == b'\n')
		.filter(|line| !line.iter().all(u8::is_ascii_whitespace))
		.collect();

	let mut replay = Replay::default();
	let mut positions: HashMap<JobId, usize> = HashMap::new();

	for (index, line) in lines.iter().enumerate() {
		let last = index + 1 == lines.len();

		match parse(line) {
			Ok(Some(job)) => match positions.get(&job.id) {
				Some(position) => replay.jobs[*position] = job,
				None => {
					positions.insert(job.id.clone(), replay.jobs.len());
					replay.jobs.push(job);
				}
			},
			Ok(None) => replay.skipped_versions += 1,
			Err(_) if last => replay.torn = true,
			// Damage before the end is corruption, but not a reason to throw
			// away the records after it: the file is copied aside for
			// inspection, and everything readable still replays.
			Err(_) => {
				replay.quarantined.get_or_insert_with(|| quarantine_path(path));
			}
		}
	}

	Ok(replay)
}

/// A line parses into a job, into a record from another version, or into
/// nothing at all.
fn parse(line: &[u8]) -> std::result::Result<Option<Job>, serde_json::Error> {
	#[derive(Deserialize)]
	struct Header {
		v: String,
	}

	let value: serde_json::Value = serde_json::from_slice(line)?;

	if serde_json::from_value::<Header>(value.clone()).is_ok_and(|header| header.v != JOURNAL_VERSION) {
		return Ok(None);
	}

	serde_json::from_value(value).map(Some)
}

fn quarantine_path(path: &Path) -> PathBuf {
	let name = path
		.file_name()
		.map(|name| name.to_string_lossy().into_owned())
		.unwrap_or_else(|| "jobs.jsonl".to_string());

	path.with_file_name(format!("{name}.corrupt.{}", now_ms()))
}

#[cfg(unix)]
fn set_private_directory(path: &Path) -> Result<()> {
	use std::os::unix::fs::PermissionsExt;
	fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
	Ok(())
}

#[cfg(not(unix))]
fn set_private_directory(_path: &Path) -> Result<()> {
	Ok(())
}

#[cfg(unix)]
fn set_private_file(file: &fs::File) -> Result<()> {
	use std::os::unix::fs::PermissionsExt;
	file.set_permissions(fs::Permissions::from_mode(0o600))?;
	Ok(())
}

#[cfg(not(unix))]
fn set_private_file(_file: &fs::File) -> Result<()> {
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::job::JobState;
	use crate::job::{Output, Source, Tier};

	fn path() -> (tempfile::TempDir, PathBuf) {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("state").join("jobs.jsonl");
		(dir, path)
	}

	fn job() -> Job {
		let source = Source {
			path: PathBuf::from("/home/u/Pictures/a.heic"),
			format: "heic".to_string(),
			size: 2_431_002,
			mtime_ns: 1_690_000_000_000,
		};
		let output = Output::pending(PathBuf::from("/home/u/Pictures/a.jpg"), "jpg", Tier::Converted);

		Job::new(source, Some("jpg".to_string()), vec![output], "7f3c1a2b").unwrap()
	}

	#[test]
	fn compaction_keeps_live_records_and_drops_those_past_the_horizon() {
		let (_dir, path) = path();
		let month = 30 * 24 * 60 * 60 * 1_000;

		{
			let (mut journal, _) = Journal::open(&path).unwrap();

			// Four records for one job, plus an old job of its own: enough
			// supersession to earn a rewrite, and one record old enough to drop.
			let mut old = job();
			old.updated = now_ms() - 20 * month;
			journal.append(&old).unwrap();

			let mut current = job();
			for _ in 0..4 {
				journal.append(&current).unwrap();
			}

			// Give the live job a fresh timestamp so it survives.
			current.updated = now_ms();
			journal.append(&current).unwrap();
		}

		let (mut journal, _) = Journal::open(&path).unwrap();
		let compact = journal.compact(Duration::from_secs(7 * 24 * 60 * 60)).unwrap();
		let kept = compact.kept.expect("the file earned a rewrite");

		assert_eq!(kept.len(), 1, "{kept:?}");
		assert_eq!(compact.dropped, 1);

		let (_, replay) = Journal::open(&path).unwrap();
		assert_eq!(replay.jobs.len(), 1);
		assert!(journal.path().exists());
	}

	#[test]
	fn a_small_journal_is_left_alone() {
		let (_dir, path) = path();
		let mut job = job();

		{
			let (mut journal, _) = Journal::open(&path).unwrap();
			journal.append(&job).unwrap();
			job.set_state(JobState::Done);
			journal.append(&job).unwrap();
		}

		let (mut journal, _) = Journal::open(&path).unwrap();
		let compact = journal.compact(Duration::from_secs(7 * 24 * 60 * 60)).unwrap();

		assert!(compact.kept.is_none());
		assert_eq!(journal.lines().unwrap(), 2);
	}

	#[test]
	fn a_job_replays_as_its_last_snapshot() {
		let (_dir, path) = path();
		let mut job = job();
		let id = job.id.clone();

		{
			let (mut journal, replay) = Journal::open(&path).unwrap();
			assert!(replay.jobs.is_empty());
			journal.append(&job).unwrap();
			job.set_state(JobState::Running);
			journal.append(&job).unwrap();
			job.set_state(JobState::Done);
			journal.append(&job).unwrap();
		}

		let (_journal, replay) = Journal::open(&path).unwrap();

		assert_eq!(replay.jobs.len(), 1);
		assert_eq!(replay.jobs[0].id, id);
		assert_eq!(replay.jobs[0].state, JobState::Done);
		assert!(!replay.torn);
	}

	#[test]
	fn jobs_keep_the_order_they_were_first_seen() {
		let (_dir, path) = path();
		let (mut journal, _) = Journal::open(&path).unwrap();
		let first = job();
		let second = job();

		journal.append(&first).unwrap();
		journal.append(&second).unwrap();
		let mut later = first.clone();
		later.set_state(JobState::Done);
		journal.append(&later).unwrap();

		let (_journal, replay) = Journal::open(&path).unwrap();

		assert_eq!(replay.jobs.len(), 2);
		assert_eq!(replay.jobs[0].id, first.id);
		assert_eq!(replay.jobs[1].id, second.id);
	}

	#[test]
	fn an_interrupted_write_is_dropped_from_the_end_only() {
		let (_dir, path) = path();
		let job = job();

		{
			let (mut journal, _) = Journal::open(&path).unwrap();
			journal.append(&job).unwrap();
		}
		let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
		file.write_all(b"{\"v\":\"retype-journal-1\",\"kind\":\"job\",")
			.unwrap();

		let (_journal, replay) = Journal::open(&path).unwrap();

		assert!(replay.torn);
		assert_eq!(replay.jobs.len(), 1);
		assert!(replay.quarantined.is_none(), "a torn tail is not corruption");
	}

	#[test]
	fn a_torn_tail_is_removed_before_the_next_append() {
		let (_dir, path) = path();
		let first = job();

		{
			let (mut journal, _) = Journal::open(&path).unwrap();
			journal.append(&first).unwrap();
		}

		let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
		file.write_all(b"not json").unwrap();
		drop(file);

		let second = job();

		{
			let (mut journal, replay) = Journal::open(&path).unwrap();
			assert!(replay.torn);
			assert_eq!(replay.jobs.len(), 1);
			journal.append(&second).unwrap();
		}

		let (_journal, replay) = Journal::open(&path).unwrap();

		assert!(
			!replay.torn,
			"the partial line was not left in front of the next record"
		);
		assert!(replay.quarantined.is_none(), "a torn tail is not corruption");
		assert_eq!(replay.jobs.len(), 2);
		assert_eq!(replay.jobs[0].id, first.id);
		assert_eq!(replay.jobs[1].id, second.id);
	}

	#[test]
	fn corruption_before_the_end_is_quarantined() {
		let (_dir, path) = path();
		let first = job();

		{
			let (mut journal, _) = Journal::open(&path).unwrap();
			journal.append(&first).unwrap();
		}
		let second = job();
		let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
		file.write_all(b"not json\n").unwrap();
		writeln!(file, "{}", second.to_line().unwrap()).unwrap();
		drop(file);

		{
			let (mut journal, replay) = Journal::open(&path).unwrap();
			let quarantined = replay.quarantined.clone().expect("corruption is reported once");

			assert_eq!(replay.jobs.len(), 2, "a record after the damage still replays");
			assert!(quarantined.exists());
			assert!(fs::read_to_string(&quarantined).unwrap().contains("not json"));

			journal.append(&first).unwrap();
		}

		let (_journal, replay) = Journal::open(&path).unwrap();

		assert_eq!(replay.jobs.len(), 1, "the new journal starts empty");
		assert!(replay.quarantined.is_none());
	}

	#[test]
	fn a_record_from_another_version_is_skipped() {
		let (_dir, path) = path();
		let job = job();

		{
			let (mut journal, _) = Journal::open(&path).unwrap();
			journal.append(&job).unwrap();
		}
		let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
		let mut future = job.clone();
		future.set_state(JobState::Done);
		let mut line = future.to_line().unwrap();
		line = line.replace(JOURNAL_VERSION, "retype-journal-2");
		writeln!(file, "{line}").unwrap();
		drop(file);

		let (_journal, replay) = Journal::open(&path).unwrap();

		assert_eq!(replay.skipped_versions, 1);
		assert_eq!(replay.jobs.len(), 1);
		assert_eq!(
			replay.jobs[0].state,
			JobState::Pending,
			"the older snapshot is what we can read"
		);
		assert!(replay.quarantined.is_none());
	}

	#[test]
	fn intermediate_and_partial_snapshots_survive_reopening() {
		let (_dir, path) = path();
		for state in [JobState::Running, JobState::Partial, JobState::Cancelled] {
			let mut current = job();
			current.state = state;
			let (mut journal, _) = Journal::open(&path).unwrap();
			journal.append(&current).unwrap();
			drop(journal);
			let (_, replay) = Journal::open(&path).unwrap();
			assert_eq!(
				replay.jobs.iter().find(|job| job.id == current.id).unwrap().state,
				state
			);
		}
	}

	#[cfg(unix)]
	#[test]
	fn the_journal_is_not_world_readable() {
		use std::os::unix::fs::PermissionsExt;

		let (_dir, path) = path();
		let (journal, _) = Journal::open(&path).unwrap();

		let file_mode = fs::metadata(journal.path()).unwrap().permissions().mode() & 0o777;
		let dir_mode = fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777;

		assert_eq!(file_mode, 0o600);
		assert_eq!(dir_mode, 0o700);
	}
}
