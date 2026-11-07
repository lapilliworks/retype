//! Undo records, `docs/dev/architecture/state.md` §5.
//!
//! One directory per job under `undo/`, holding the source bytes and a manifest.
//! The record is the durable truth for recoverability: the journal is a cache of
//! history and progress, so losing it costs listing and never undo.
//!
//! New records hold independent snapshots with source and output content fingerprints.
//! Publication intents are persisted before output rename; recovery verifies final objects.
//! Unfinished records stay pinned against expiry and quota eviction.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Code, Diagnostic, Error, Result};
use crate::job::JobId;
use crate::safety::Fingerprint;

static STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commit {
	pub output: crate::job::Output,
	pub fingerprint: Fingerprint,
	/// Older or unvalidated publication intents never authorize source disposal.
	#[serde(default)]
	pub validated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ExistingSource {
	Verify,
	LeaveUntouched,
}

/// A pinned undo records the source policy and any restoration before publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoProgress {
	existing_source: ExistingSource,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	restore_intent: Option<Fingerprint>,
}

/// Schema identifier, `docs/dev/architecture/state.md` §5.
pub const UNDO_VERSION: &str = "retype-undo-1";

/// One produced file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Output {
	pub path: PathBuf,
	pub ext: String,
}

/// How the source bytes are held. `Hard` is retained only for reading legacy manifests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Link {
	/// A hard link, so the bytes are the same inode and the store costs nothing.
	Hard,
	/// A copy, because the store is on another filesystem.
	Copy,
}

/// What was converted, and where the bytes came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
	pub version: String,
	pub id: JobId,
	pub intent: String,
	pub created: i64,
	/// When minimum retention began; absent while unfinished or for older records.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub retention_started: Option<i64>,
	pub machine: String,
	pub source_path: PathBuf,
	pub source_ext: String,
	pub source_size: u64,
	/// Zero on platforms without a mode to restore.
	pub source_mode: u32,
	/// Zero when the platform did not report one.
	pub source_mtime_ns: i64,
	pub link: Link,
	pub outputs: Vec<Output>,
	#[serde(default)]
	pub commits: Vec<Commit>,
	#[serde(default)]
	pub source_fingerprint: Option<Fingerprint>,
	#[serde(default)]
	pub pinned: bool,
	/// Present while an undo has started but has not removed its recovery record.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub undo_progress: Option<UndoProgress>,
	/// Whether undo may need to restore the source; not proof that disposal occurred.
	/// Explicit keep requests leave an independently retained live source untouched.
	#[serde(default = "consumed_default")]
	pub consume_source: bool,
}

fn consumed_default() -> bool {
	true
}

impl Record {
	fn new(id: JobId, intent: String, machine: String, source_path: PathBuf, source_ext: &str) -> Self {
		Self {
			version: UNDO_VERSION.to_string(),
			id,
			intent,
			created: crate::job::now_ms(),
			retention_started: None,
			machine,
			source_path,
			source_ext: source_ext.to_string(),
			source_size: 0,
			source_mode: 0,
			source_mtime_ns: 0,
			link: Link::Copy,
			outputs: Vec::new(),
			commits: Vec::new(),
			source_fingerprint: None,
			pinned: true,
			undo_progress: None,
			consume_source: true,
		}
	}

	/// Where the source bytes live inside the record.
	pub fn source_file(&self) -> String {
		format!("source.{}", self.source_ext)
	}
}

/// What a record is written about: one conversion, described by the job that asked
/// for it.
#[derive(Debug, Clone)]
pub struct Subject {
	pub id: JobId,
	pub intent: String,
	pub machine: String,
	/// The file as it is now. After a rename it holds the source bytes under the
	/// target's name, which is what an undo has to put back.
	pub bytes: PathBuf,
	pub source_path: PathBuf,
	pub source_ext: String,
	pub outputs: Vec<Output>,
	/// Whether undo may need to restore the source at its original path.
	pub consume_source: bool,
}

/// What to do when a record does not fit the quota.
///
/// No overflow policy authorizes automatic removal before retention ends.
/// Interactive overflow decisions and soft-goal admission are not yet implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overflow {
	/// Request an overflow decision when that action is available.
	Ask,
	/// Remove expired records, but not unexpired records.
	Evict,
	/// Keep no new record when the quota is full.
	Skip,
}

impl Overflow {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Ask => "ask",
			Self::Evict => "evict",
			Self::Skip => "skip",
		}
	}

	pub fn parse(text: &str) -> Option<Self> {
		match text {
			"ask" => Some(Self::Ask),
			"evict" => Some(Self::Evict),
			"skip" => Some(Self::Skip),
			_ => None,
		}
	}
}

/// How long a record is kept, and whether records are kept at all,
/// `docs/config.md` §9.
///
/// Retention and quota are host-level knobs rather than per-directory ones: a
/// `.retype.toml` committed to a repository must not decide every contributor's
/// disk budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
	pub enabled: bool,
	/// Age after which a record is dropped, and so after which its job can no
	/// longer be resumed.
	pub retention: Duration,
	/// Upper bound for the store, counted in source bytes.
	pub quota: u64,
	pub on_overflow: Overflow,
}

impl Default for Policy {
	fn default() -> Self {
		Self {
			enabled: true,
			retention: Duration::from_secs(7 * 24 * 60 * 60),
			quota: 5_000_000_000,
			on_overflow: Overflow::Ask,
		}
	}
}

/// Whether one more record fits, and what had to go for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Room {
	/// There is room, after dropping what had to go.
	Kept { dropped: Vec<JobId> },
	/// A record cannot be kept, so the conversion runs without one.
	NoRoom(NoRoom),
}

/// Why a record cannot be kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoRoom {
	/// The policy keeps no record when it does not fit.
	Policy,
	/// One record is larger than the whole quota.
	TooLarge,
}

impl NoRoom {
	pub fn reason(self) -> &'static str {
		match self {
			Self::Policy => "the undo quota is full; unexpired records cannot be evicted automatically",
			Self::TooLarge => "the file is larger than the undo quota",
		}
	}
}

/// The store, rooted at `<state>/undo`.
pub struct Store {
	root: PathBuf,
}

impl Store {
	pub fn open(state_dir: &Path) -> Self {
		Self {
			root: state_dir.join("undo"),
		}
	}

	pub fn directory(&self, id: &JobId) -> PathBuf {
		self.root.join(id.as_str())
	}

	/// An OS lease prevents undo or eviction from racing a publication or source disposal.
	pub fn lease(&self, id: &JobId) -> Result<JobLease> {
		let state = self.root.parent().expect("state directory");
		fs::create_dir_all(state)?;
		let mut options = fs::OpenOptions::new();
		options.read(true).write(true).create(true);
		#[cfg(unix)]
		{
			use std::os::unix::fs::OpenOptionsExt;
			options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
		}
		let file = options.open(state.join(format!("undo-job.{}.lock", id.as_str())))?;
		#[cfg(unix)]
		{
			use std::os::fd::AsRawFd;
			if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
				let error = std::io::Error::last_os_error();
				if error.kind() != std::io::ErrorKind::WouldBlock {
					return Err(error.into());
				}
				return Err(leased(id));
			}
		}
		#[cfg(windows)]
		{
			use std::os::windows::io::AsRawHandle;
			use windows_sys::Win32::Storage::FileSystem::{
				LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
			};
			let mut overlapped = unsafe { std::mem::zeroed() };
			if unsafe {
				LockFileEx(
					file.as_raw_handle() as _,
					LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
					0,
					1,
					0,
					&mut overlapped,
				)
			} == 0
			{
				let error = std::io::Error::last_os_error();
				if error.raw_os_error() != Some(33) {
					return Err(error.into());
				}
				return Err(leased(id));
			}
		}
		Ok(JobLease { _file: file })
	}

	/// Keep the source bytes and a manifest before converting, `docs/dev/architecture/state.md` §5.
	///
	/// `bytes` is the file as it is now: after a rename that is the source content
	/// under the target's name, which is exactly what an undo has to put back.
	pub fn save(&self, subject: Subject) -> Result<Record> {
		let _guard = store_lock(&self.root)?;
		self.save_locked(subject)
	}

	/// Reserve quota and create the pinned record under the same store lock.
	pub fn save_with_policy(&self, subject: Subject, policy: &Policy) -> Result<Option<Record>> {
		let _guard = store_lock(&self.root)?;
		if !policy.enabled {
			return Ok(None);
		}
		let room = self.room_for_locked(fs::metadata(&subject.bytes)?.len(), policy)?;
		let Room::Kept { dropped } = room else {
			return Ok(None);
		};

		/* Reserve every record before any is removed, then create the new backup before
		 * dropping the old ones. A failed save must not leave the store with neither. */
		let mut leases = Vec::with_capacity(dropped.len());
		for id in &dropped {
			leases.push(self.lease(id)?);
		}

		let record = self.save_locked(subject)?;
		for id in dropped {
			self.remove_locked(&id)?;
		}

		drop(leases);
		Ok(Some(record))
	}

	fn save_locked(&self, subject: Subject) -> Result<Record> {
		fs::create_dir_all(&self.root)?;
		set_mode(&self.root, 0o700)?;
		let directory = self.directory(&subject.id);
		fs::create_dir(&directory)?;
		set_mode(&directory, 0o700)?;

		let metadata = fs::metadata(&subject.bytes)?;
		let mut record = Record::new(
			subject.id.clone(),
			subject.intent,
			subject.machine,
			subject.source_path,
			&subject.source_ext,
		);
		record.outputs = subject.outputs;
		record.consume_source = subject.consume_source;
		record.source_size = metadata.len();
		record.source_mode = mode_of(&metadata);
		record.source_mtime_ns = mtime_ns(&metadata);

		let held = directory.join(record.source_file());

		record.source_fingerprint = Some(crate::safety::copy_snapshot(&subject.bytes, &held)?);
		record.link = Link::Copy;
		write_manifest(&directory, &record)?;
		crate::fsx::sync_directory(&self.root)?;
		crate::fsx::sync_directory(self.root.parent().expect("state directory"))?;
		Ok(record)
	}

	pub fn load(&self, id: &JobId) -> Result<Option<Record>> {
		let path = self.directory(id).join("meta.toml");

		let text = match fs::read_to_string(&path) {
			Ok(text) => text,
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
			Err(error) => return Err(Error::Io(error)),
		};

		let record: Record = toml::from_str(&text).map_err(|error| {
			Error::Diagnostic(Diagnostic::global(
				Code::CfgParse,
				format!("{}: {error}", path.display()),
			))
		})?;

		if record.version != UNDO_VERSION {
			return Err(Error::Diagnostic(Diagnostic::global(
				Code::CfgVersion,
				format!(
					"{} declares '{}', this build reads '{UNDO_VERSION}'",
					path.display(),
					record.version
				),
			)));
		}

		Ok(Some(record))
	}

	/// Every record, newest first.
	pub fn list(&self) -> Result<Vec<Record>> {
		let entries = match fs::read_dir(&self.root) {
			Ok(entries) => entries,
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
			Err(error) => return Err(Error::Io(error)),
		};

		let mut records = Vec::new();

		for entry in entries {
			let name = entry?.file_name();
			let Some(name) = name.to_str() else {
				continue;
			};

			let Ok(id) = JobId::parse(name) else {
				continue;
			};

			if let Some(record) = self.load(&id)? {
				records.push(record);
			}
		}

		records.sort_by_key(|record| std::cmp::Reverse(record.created));
		Ok(records)
	}

	/// Put the source back and remove what the job produced, `docs/dev/architecture/state.md` §5.
	pub fn restore(&self, id: &JobId) -> Result<Record> {
		self.restore_with_cleanup(id, |record| {
			if !crate::fsx::supports_durable_cleanup() {
				return Err(Error::Io(std::io::Error::new(
					std::io::ErrorKind::Unsupported,
					"original restored or retained; output removal requires directory durability",
				)));
			}
			remove_outputs(record)
		})
	}

	fn restore_with_cleanup(&self, id: &JobId, cleanup: impl FnOnce(&Record) -> Result<()>) -> Result<Record> {
		let _guard = store_lock(&self.root)?;
		let _lease = self.lease(id)?;
		let Some(mut record) = self.load(id)? else {
			return Err(Error::Diagnostic(Diagnostic::global(
				Code::JobNotFound,
				format!("{id} has no undo record"),
			)));
		};

		let policy = existing_source_policy(&record);
		self.begin_undo(&mut record, policy)?;
		verify_undo_outputs(&record)?;
		let source_exists = undo_source_exists(&record.source_path)?;
		let has_restore_intent = record
			.undo_progress
			.as_ref()
			.is_some_and(|progress| progress.restore_intent.is_some());
		let must_verify = policy == ExistingSource::Verify || !source_exists || has_restore_intent;
		let held = if must_verify {
			let path = self
				.source(id)?
				.ok_or_else(|| Error::Io(std::io::Error::other("no verified original to restore")))?;
			let mut file = open_undo_backup(&path)?;
			let fingerprint = Fingerprint::of_open_file(&path, &mut file)?;
			let expected = record.source_fingerprint.as_ref().expect("verified record");
			if !expected.same_content(&fingerprint) {
				return Err(Error::Io(std::io::Error::other(
					"retained original changed before undo",
				)));
			}
			Some((path, file))
		} else {
			None
		};

		if let Some((path, _file)) = &held {
			if !source_exists {
				let (staged, fingerprint) = self.prepare_restoration(&mut record, path)?;
				publish_restoration(staged, &fingerprint)?;
			}
		}
		let mut source = open_undo_source(&record.source_path)?;
		let source_before_cleanup = Fingerprint::of_open_file(&record.source_path, &mut source)?;
		if (must_verify && !is_undo_fingerprint(&record, &source_before_cleanup))
			|| !source_before_cleanup.matches(&record.source_path)?
		{
			return Err(Error::Exists(record.source_path.clone()));
		}
		if must_verify {
			restore_times(&source, &record)?;
		}
		source.sync_all()?;
		crate::fsx::sync_directory(record.source_path.parent().unwrap_or(Path::new(".")))?;
		cleanup(&record)?;
		if !source_before_cleanup.matches(&record.source_path)? {
			return Err(Error::Io(std::io::Error::other(format!(
				"{} changed during undo; recovery retained",
				record.source_path.display()
			))));
		}
		/* Windows cannot remove the backup while its non-delete-sharing handle is open. */
		drop(held);
		self.remove_locked(id)?;
		drop(source);
		Ok(record)
	}

	fn begin_undo(&self, record: &mut Record, policy: ExistingSource) -> Result<()> {
		let needs_write = record.undo_progress.is_none() || !record.pinned;
		if record.undo_progress.is_none() {
			record.undo_progress = Some(UndoProgress {
				existing_source: policy,
				restore_intent: None,
			});
		}
		record.pinned = true;
		if needs_write {
			write_manifest(&self.directory(&record.id), record)?;
		}
		Ok(())
	}

	fn prepare_restoration(
		&self,
		record: &mut Record,
		held: &Path,
	) -> Result<(crate::convert::StagedOutput, Fingerprint)> {
		let staged = crate::convert::StagedOutput::new(&record.source_path)?;
		let backup = crate::safety::copy_snapshot(held, staged.temp())?;
		let expected = record.source_fingerprint.as_ref().expect("verified record");
		if !expected.same_content(&backup) {
			return Err(Error::Io(std::io::Error::other(
				"retained original changed during restoration",
			)));
		}
		let fingerprint = Fingerprint::of(staged.temp())?;
		if !backup.same_content(&fingerprint) {
			return Err(Error::Io(std::io::Error::other(
				"staged original changed during restoration",
			)));
		}
		record.undo_progress.as_mut().expect("undo started").restore_intent = Some(fingerprint.clone());
		write_manifest(&self.directory(&record.id), record)?;
		Ok((staged, fingerprint))
	}

	/// Forget a record after successful undo or explicit history removal.
	pub fn remove(&self, id: &JobId) -> Result<()> {
		let _guard = store_lock(&self.root)?;
		let _lease = self.lease(id)?;
		self.remove_locked(id)
	}

	fn remove_locked(&self, id: &JobId) -> Result<()> {
		let directory = self.directory(id);

		match fs::remove_dir_all(&directory) {
			Ok(()) => Ok(()),
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
			Err(error) => Err(Error::Io(error)),
		}
	}

	/// Drop every record older than `retention`, and report what went. Called at
	/// startup, `docs/dev/architecture/state.md` §6, and never during a conversion.
	pub fn sweep(&self, retention: Duration) -> Result<Vec<JobId>> {
		let _guard = store_lock(&self.root)?;
		let now = SystemTime::now();
		let mut dropped = Vec::new();

		for record in self.list()? {
			if !record.pinned && age(&record, now).is_some_and(|age| age > retention) {
				let _lease = self.lease(&record.id)?;
				self.remove_locked(&record.id)?;
				dropped.push(record.id);
			}
		}

		Ok(dropped)
	}

	/// Whether a record for the bytes at `path` fits, and what had to go for it.
	///
	/// Only expired, unpinned records can be dropped automatically. If that is not
	/// enough, no record is dropped for a job that will not receive a backup.
	pub fn make_room(&self, path: &Path, policy: &Policy) -> Result<Room> {
		let _guard = store_lock(&self.root)?;
		let room = self.room_for_locked(fs::metadata(path)?.len(), policy)?;

		if let Room::Kept { dropped } = &room {
			for id in dropped {
				let _lease = self.lease(id)?;
				self.remove_locked(id)?;
			}
		}

		Ok(room)
	}

	/// The same answer without changing anything, which is what a decision about a job
	/// needs: whether a record could be kept if it came to that, `docs/deferral.md`
	/// §3.
	pub fn room_for(&self, wanted: u64, policy: &Policy) -> Result<Room> {
		let _guard = store_lock(&self.root)?;
		self.room_for_locked(wanted, policy)
	}

	fn room_for_locked(&self, wanted: u64, policy: &Policy) -> Result<Room> {
		let now = SystemTime::now();
		let mut dropped = Vec::new();
		let mut held: u64 = 0;

		for record in self.list()? {
			if !record.pinned && age(&record, now).is_some_and(|age| age > policy.retention) {
				dropped.push(record.id);
			} else {
				held = held.saturating_add(record.source_size);
			}
		}

		if held.saturating_add(wanted) <= policy.quota {
			return Ok(Room::Kept { dropped });
		}

		if wanted > policy.quota {
			return Ok(Room::NoRoom(NoRoom::TooLarge));
		}

		Ok(Room::NoRoom(NoRoom::Policy))
	}

	/// Write the output's identity before publication. Recovery verifies it at the final path.
	pub fn prepare_output(&self, id: &JobId, output: crate::job::Output, staged: &Path) -> Result<()> {
		self.prepare_checked_output(id, output, staged, None)
	}

	/// Bind a publication intent to the file and bytes that passed validation, when available.
	pub fn prepare_checked_output(
		&self,
		id: &JobId,
		output: crate::job::Output,
		staged: &Path,
		validated: Option<&Fingerprint>,
	) -> Result<()> {
		let _guard = store_lock(&self.root)?;
		let fingerprint = Fingerprint::of(staged)?;
		if validated.is_some_and(|expected| !expected.same_file_and_content(&fingerprint)) {
			return Err(Error::Io(std::io::Error::other(format!(
				"{} changed after output validation; original retained",
				staged.display()
			))));
		}
		let mut record = self
			.load(id)?
			.ok_or_else(|| Error::Io(std::io::Error::other("undo record disappeared")))?;
		record.commits.retain(|commit| commit.output.path != output.path);
		record.commits.push(Commit {
			output,
			fingerprint,
			validated: validated.is_some(),
		});
		write_manifest(&self.directory(id), &record)
	}

	/// Recheck the retained input and complete published inventory before disposal.
	pub fn verify_disposal(&self, id: &JobId, outputs: &[crate::job::Output]) -> Result<()> {
		let record = self
			.load(id)?
			.ok_or_else(|| Error::Io(std::io::Error::other("recovery record disappeared")))?;
		if self.source(id)?.is_none() || record.outputs.is_empty() || record.outputs.len() != outputs.len() {
			return Err(Error::Io(std::io::Error::other(
				"no verified original or complete output inventory; source retained",
			)));
		}
		let mut seen = std::collections::BTreeSet::new();
		for output in outputs {
			let planned = record.outputs.iter().find(|planned| planned.path == output.path);
			if output.state != crate::job::OutputState::Done
				|| !seen.insert(&output.path)
				|| planned.is_none_or(|planned| planned.ext != output.format)
			{
				return Err(Error::Io(std::io::Error::other(format!(
					"{} does not match the intended output inventory; source retained",
					output.path.display()
				))));
			}
			let commit = record
				.commits
				.iter()
				.find(|commit| commit.output.path == output.path)
				.ok_or_else(|| Error::Exists(output.path.clone()))?;
			if !commit.validated
				|| commit.output.state != crate::job::OutputState::Done
				|| commit.output.format != output.format
				|| commit.output.bytes != output.bytes
			{
				return Err(Error::Io(std::io::Error::other(format!(
					"{} has no recorded output validation; source retained",
					output.path.display()
				))));
			}
			if !commit.fingerprint.matches(&output.path)? {
				return Err(Error::Io(std::io::Error::other(format!(
					"{} changed after publication; source retained",
					output.path.display()
				))));
			}
		}
		Ok(())
	}

	pub fn pin(&self, id: &JobId) -> Result<()> {
		self.set_pinned(id, true)
	}

	pub fn finish(&self, id: &JobId) -> Result<()> {
		self.set_pinned(id, false)
	}

	fn set_pinned(&self, id: &JobId, pinned: bool) -> Result<()> {
		let _guard = store_lock(&self.root)?;
		let mut record = self
			.load(id)?
			.ok_or_else(|| Error::Io(std::io::Error::other("undo record disappeared")))?;
		if record.undo_progress.is_some() {
			return Err(Error::Io(std::io::Error::other(
				"undo is incomplete; finish it before resuming or unpinning the record",
			)));
		}
		if !pinned && (record.pinned || record.retention_started.is_none()) {
			record.retention_started = Some(crate::job::now_ms());
		}
		record.pinned = pinned;
		write_manifest(&self.directory(id), &record)
	}

	/// The bytes held for a job, when a source has to be read again: resume needs
	/// this after a rename has taken the original name away.
	pub fn source(&self, id: &JobId) -> Result<Option<PathBuf>> {
		let Some(record) = self.load(id)? else {
			return Ok(None);
		};

		let path = self.directory(id).join(record.source_file());
		let Some(expected) = &record.source_fingerprint else {
			return Ok(None);
		};
		if !path.try_exists()? {
			return Ok(None);
		}
		let actual = Fingerprint::of(&path)?;
		if !expected.same_content(&actual) {
			return Err(Error::Io(std::io::Error::other(
				"retained original failed its content check",
			)));
		}
		Ok(Some(path))
	}
}

fn verify_undo_outputs(record: &Record) -> Result<()> {
	for commit in &record.commits {
		match fs::symlink_metadata(&commit.output.path) {
			Ok(_) if !commit.fingerprint.matches(&commit.output.path)? => {
				return Err(Error::Exists(commit.output.path.clone()));
			}
			Ok(_) => {}
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
			Err(error) => return Err(error.into()),
		}
	}
	Ok(())
}

fn undo_source_exists(path: &Path) -> Result<bool> {
	match fs::symlink_metadata(path) {
		Ok(metadata) if metadata.file_type().is_file() => Ok(true),
		Ok(_) => Err(Error::Exists(path.to_path_buf())),
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
		Err(error) => Err(error.into()),
	}
}

fn existing_source_policy(record: &Record) -> ExistingSource {
	match &record.undo_progress {
		Some(progress) => progress.existing_source,
		None if record.consume_source || record.pinned => ExistingSource::Verify,
		None => ExistingSource::LeaveUntouched,
	}
}

fn is_undo_fingerprint(record: &Record, actual: &Fingerprint) -> bool {
	record
		.source_fingerprint
		.as_ref()
		.is_some_and(|original| original.same_file_and_content(actual))
		|| record
			.undo_progress
			.as_ref()
			.and_then(|progress| progress.restore_intent.as_ref())
			.is_some_and(|restored| restored.same_file_and_content(actual))
}

fn open_undo_backup(path: &Path) -> std::io::Result<fs::File> {
	let mut options = fs::OpenOptions::new();
	options.read(true);
	crate::safety::open_guarded_file(path, options)
}

fn open_undo_source(path: &Path) -> std::io::Result<fs::File> {
	let mut options = fs::OpenOptions::new();
	options.read(true);
	#[cfg(not(unix))]
	options.write(true);
	crate::safety::open_guarded_file(path, options)
}

fn publish_restoration(staged: crate::convert::StagedOutput, fingerprint: &Fingerprint) -> Result<()> {
	let target = staged.target().to_path_buf();
	if !fingerprint.matches(staged.temp())? {
		return Err(Error::Io(std::io::Error::other(
			"staged original changed before publication",
		)));
	}
	staged.commit_checked(fingerprint)?;
	if !fingerprint.matches(&target)? {
		return Err(Error::Io(std::io::Error::other(
			"restored original changed during publication",
		)));
	}
	Ok(())
}

/// Remove every output a record says the job produced.
fn remove_outputs(record: &Record) -> Result<()> {
	for commit in &record.commits {
		match fs::symlink_metadata(&commit.output.path) {
			Ok(_) => crate::safety::remove_owned(&commit.output.path, &commit.fingerprint)?,
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
			Err(error) => return Err(error.into()),
		}
	}
	Ok(())
}

pub struct JobLease {
	_file: fs::File,
}

fn leased(id: &JobId) -> Error {
	Error::Diagnostic(Diagnostic::global(
		Code::IoLocked,
		format!("{id} is still running or being recovered; cancel it and wait before undoing"),
	))
}

struct StoreLock {
	_process: std::sync::MutexGuard<'static, ()>,
	_file: fs::File,
}

fn store_lock(root: &Path) -> Result<StoreLock> {
	let process = STORE_LOCK
		.lock()
		.map_err(|_| Error::Io(std::io::Error::other("undo store lock poisoned")))?;
	let state = root.parent().expect("state directory");
	fs::create_dir_all(state)?;
	let mut options = fs::OpenOptions::new();
	options.read(true).write(true).create(true);
	#[cfg(unix)]
	{
		use std::os::unix::fs::OpenOptionsExt;
		options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
	}
	let file = options.open(state.join("undo.lock"))?;
	#[cfg(unix)]
	{
		use std::os::fd::AsRawFd;
		loop {
			if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
				break;
			}
			let error = std::io::Error::last_os_error();
			if error.kind() != std::io::ErrorKind::Interrupted {
				return Err(error.into());
			}
		}
	}
	#[cfg(windows)]
	{
		use std::os::windows::io::AsRawHandle;
		use windows_sys::Win32::Storage::FileSystem::{LOCKFILE_EXCLUSIVE_LOCK, LockFileEx};
		let mut overlapped = unsafe { std::mem::zeroed() };
		if unsafe {
			LockFileEx(
				file.as_raw_handle() as _,
				LOCKFILE_EXCLUSIVE_LOCK,
				0,
				1,
				0,
				&mut overlapped,
			)
		} == 0
		{
			return Err(std::io::Error::last_os_error().into());
		}
	}
	Ok(StoreLock {
		_process: process,
		_file: file,
	})
}

fn write_manifest(directory: &Path, record: &Record) -> Result<()> {
	let text = toml::to_string(record).map_err(|error| {
		Error::Diagnostic(Diagnostic::global(
			Code::InternalInvariant,
			format!("an undo record does not serialize: {error}"),
		))
	})?;

	let path = directory.join("meta.toml");
	crate::convert::write_atomically(&path, |temporary| {
		use std::io::Write;
		let mut file = fs::OpenOptions::new().write(true).create_new(true).open(temporary)?;
		set_mode(temporary, 0o600)?;
		file.write_all(text.as_bytes())?;
		file.sync_all()?;
		Ok(())
	})?;
	Ok(())
}

fn restore_times(file: &fs::File, record: &Record) -> Result<()> {
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		if record.source_mode != 0 {
			file.set_permissions(fs::Permissions::from_mode(record.source_mode))?;
		}
	}
	if record.source_mtime_ns != 0 {
		let modified = UNIX_EPOCH + Duration::from_nanos(record.source_mtime_ns.max(0) as u64);
		file.set_times(fs::FileTimes::new().set_modified(modified))?;
	}
	Ok(())
}

fn mtime_ns(metadata: &fs::Metadata) -> i64 {
	metadata
		.modified()
		.ok()
		.and_then(|time| time.duration_since(UNIX_EPOCH).ok())
		.map(|since| since.as_nanos() as i64)
		.unwrap_or_default()
}

#[cfg(unix)]
fn mode_of(metadata: &fs::Metadata) -> u32 {
	use std::os::unix::fs::PermissionsExt;
	metadata.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn mode_of(_metadata: &fs::Metadata) -> u32 {
	0
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
	use std::os::unix::fs::PermissionsExt;

	if mode == 0 {
		return Ok(());
	}

	fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
	Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
	Ok(())
}

/// Time since retention started, or `None` when the start is unknown.
pub fn age(record: &Record, now: SystemTime) -> Option<Duration> {
	record.retention_started.map(|started| {
		let started = UNIX_EPOCH + Duration::from_millis(started.max(0) as u64);
		now.duration_since(started).unwrap_or_default()
	})
}

#[cfg(test)]
#[path = "undo_recovery_tests.rs"]
mod recovery_tests;

#[cfg(test)]
mod tests {
	use super::*;

	fn id(index: usize) -> JobId {
		JobId::parse(&format!("01J{:023}", index)).expect("a valid id")
	}

	fn state() -> tempfile::TempDir {
		tempfile::tempdir().unwrap()
	}

	fn source(dir: &Path) -> PathBuf {
		let path = dir.join("photo.heic");
		fs::write(&path, b"heic bytes").unwrap();
		path
	}

	fn subject(index: usize, dir: &Path) -> Subject {
		let bytes = dir.join(format!("photo{index}.heic"));
		fs::write(&bytes, b"heic bytes").unwrap();

		Subject {
			id: id(index),
			intent: "jpg+quality85".to_string(),
			machine: "machine".to_string(),
			bytes: bytes.clone(),
			source_path: bytes,
			source_ext: "heic".to_string(),
			consume_source: true,
			outputs: Vec::new(),
		}
	}

	/// Backdate a record the way only time can, by rewriting its manifest.
	fn backdate(store: &Store, record: &mut Record, days: i64) {
		record.created = crate::job::now_ms() - days * 24 * 60 * 60 * 1_000;
		record.retention_started = Some(record.created);
		record.pinned = false;
		fs::write(
			store.directory(&record.id).join("meta.toml"),
			toml::to_string(&record).unwrap(),
		)
		.unwrap();
	}

	/// A subject whose bytes are `size` long, so a quota test does not need files
	/// of that size to be plausible about anything.
	fn sized(index: usize, dir: &Path, size: usize) -> Subject {
		let bytes = dir.join(format!("photo{index}.heic"));
		fs::write(&bytes, vec![b'x'; size]).unwrap();

		Subject {
			id: id(index),
			intent: "jpg".to_string(),
			machine: "machine".to_string(),
			bytes: bytes.clone(),
			source_path: bytes,
			source_ext: "heic".to_string(),
			consume_source: true,
			outputs: Vec::new(),
		}
	}

	fn policy(quota: u64) -> Policy {
		Policy {
			quota,
			..Policy::default()
		}
	}

	fn kept_record(store: &Store, directory: &Path) -> (Record, PathBuf) {
		let output = directory.join("photo.jpg");
		fs::write(&output, b"converted bytes").unwrap();
		let mut subject = subject(1, directory);
		subject.consume_source = false;
		subject.outputs.push(Output {
			path: output.clone(),
			ext: "jpg".to_string(),
		});
		let record = store.save(subject).unwrap();
		let produced = crate::job::Output::pending(output.clone(), "jpg", crate::job::Tier::Converted);
		store.prepare_output(&record.id, produced, &output).unwrap();
		(record, output)
	}

	#[test]
	fn undo_of_a_pinned_keep_record_refuses_a_changed_original() {
		let state = state();
		let store = Store::open(state.path());
		let (record, output) = kept_record(&store, state.path());
		fs::write(&record.source_path, b"later changes").unwrap();

		assert!(store.restore(&record.id).is_err());
		assert_eq!(fs::read(&record.source_path).unwrap(), b"later changes");
		assert!(output.exists());
		assert!(store.load(&record.id).unwrap().unwrap().pinned);
		assert!(store.source(&record.id).unwrap().is_some());
	}

	#[test]
	fn undo_restores_a_missing_kept_original_before_removing_outputs() {
		let state = state();
		let store = Store::open(state.path());
		let (record, output) = kept_record(&store, state.path());
		store.finish(&record.id).unwrap();
		fs::remove_file(&record.source_path).unwrap();

		let result = store.restore(&record.id);
		assert_eq!(fs::read(&record.source_path).unwrap(), b"heic bytes");
		if crate::fsx::supports_durable_cleanup() {
			result.unwrap();
			assert!(!output.exists());
			assert!(store.load(&record.id).unwrap().is_none());
		} else {
			assert!(result.is_err());
			assert!(output.exists());
			assert!(store.source(&record.id).unwrap().is_some());
		}
	}

	#[test]
	fn undo_can_retry_after_the_original_was_restored_but_output_removal_failed() {
		let root = state();
		let input = root.path().join("input");
		let output_directory = root.path().join("output");
		fs::create_dir(&input).unwrap();
		fs::create_dir(&output_directory).unwrap();
		let store = Store::open(&root.path().join("state"));
		let output = output_directory.join("photo.jpg");
		fs::write(&output, b"converted bytes").unwrap();
		let mut subject = subject(1, &input);
		subject.outputs.push(Output {
			path: output.clone(),
			ext: "jpg".to_string(),
		});
		let record = store.save(subject).unwrap();
		let produced = crate::job::Output::pending(output.clone(), "jpg", crate::job::Tier::Converted);
		store.prepare_output(&record.id, produced, &output).unwrap();
		store.finish(&record.id).unwrap();
		fs::remove_file(&record.source_path).unwrap();

		let first = store.restore_with_cleanup(&record.id, |_| {
			Err(std::io::Error::other("injected output-removal failure").into())
		});
		assert!(
			first
				.unwrap_err()
				.to_string()
				.contains("injected output-removal failure")
		);
		assert_eq!(fs::read(&record.source_path).unwrap(), b"heic bytes");
		assert!(output.exists());
		let interrupted = store.load(&record.id).unwrap().unwrap();
		assert!(interrupted.pinned);
		assert!(interrupted.undo_progress.as_ref().unwrap().restore_intent.is_some());
		assert!(store.finish(&record.id).is_err());
		assert!(store.pin(&record.id).is_err());
		assert!(store.sweep(Duration::ZERO).unwrap().is_empty());

		let retry = store.restore(&record.id);
		assert_eq!(fs::read(&record.source_path).unwrap(), b"heic bytes");
		if crate::fsx::supports_durable_cleanup() {
			retry.unwrap();
			assert!(!output.exists());
			assert!(store.load(&record.id).unwrap().is_none());
		} else {
			assert!(retry.unwrap_err().to_string().contains("directory durability"));
			assert!(output.exists());
			assert!(store.load(&record.id).unwrap().unwrap().pinned);
		}
	}

	#[test]
	fn disposal_requires_validation_in_the_publication_record() {
		let state = state();
		let store = Store::open(state.path());
		let output = state.path().join("photo.jpg");
		fs::write(&output, b"output").unwrap();
		let mut subject = subject(1, state.path());
		subject.outputs.push(Output {
			path: output.clone(),
			ext: "jpg".to_string(),
		});
		let record = store.save(subject).unwrap();
		let mut produced = crate::job::Output::pending(output.clone(), "jpg", crate::job::Tier::Converted);
		produced.state = crate::job::OutputState::Done;
		store.prepare_output(&record.id, produced.clone(), &output).unwrap();
		assert!(!store.load(&record.id).unwrap().unwrap().commits[0].validated);
		assert!(
			store
				.verify_disposal(&record.id, std::slice::from_ref(&produced))
				.is_err()
		);

		let meta = store.directory(&record.id).join("meta.toml");
		let text = fs::read_to_string(&meta).unwrap();
		assert!(text.contains("validated = false\n"));
		fs::write(&meta, text.replace("validated = false\n", "")).unwrap();
		assert!(!store.load(&record.id).unwrap().unwrap().commits[0].validated);
		assert!(
			store
				.verify_disposal(&record.id, std::slice::from_ref(&produced))
				.is_err()
		);

		let checked = Fingerprint::of(&output).unwrap();
		store
			.prepare_checked_output(&record.id, produced.clone(), &output, Some(&checked))
			.unwrap();
		assert!(store.load(&record.id).unwrap().unwrap().commits[0].validated);
		store
			.verify_disposal(&record.id, std::slice::from_ref(&produced))
			.unwrap();
		assert!(store.verify_disposal(&record.id, &[]).is_err());
		let mut wrong = produced.clone();
		wrong.format = "png".to_string();
		assert!(store.verify_disposal(&record.id, &[wrong]).is_err());
		let mut unfinished = produced.clone();
		unfinished.state = crate::job::OutputState::Failed;
		assert!(store.verify_disposal(&record.id, &[unfinished]).is_err());
		assert!(
			store
				.verify_disposal(&record.id, &[produced.clone(), produced])
				.is_err()
		);
	}

	#[test]
	fn a_record_that_fits_is_kept() {
		let state = state();
		let store = Store::open(state.path());
		let source = state.path().join("photo.heic");
		fs::write(&source, vec![b'x'; 100]).unwrap();
		store.save(sized(1, state.path(), 100)).unwrap();

		let room = store.make_room(&source, &policy(1_000)).unwrap();

		assert_eq!(room, Room::Kept { dropped: Vec::new() });
	}

	#[test]
	fn overflow_never_evicts_an_unexpired_record() {
		let state = state();
		let store = Store::open(state.path());
		let source = state.path().join("photo.heic");
		fs::write(&source, vec![b'x'; 400]).unwrap();
		let existing = store.save(sized(1, state.path(), 400)).unwrap();
		store.finish(&existing.id).unwrap();

		for overflow in [Overflow::Ask, Overflow::Evict, Overflow::Skip] {
			let mut policy = policy(700);
			policy.on_overflow = overflow;
			assert_eq!(store.make_room(&source, &policy).unwrap(), Room::NoRoom(NoRoom::Policy));
			assert!(
				store.load(&existing.id).unwrap().is_some(),
				"{overflow:?} evicted an unexpired record"
			);
		}
	}

	#[test]
	fn asking_whether_a_record_fits_changes_nothing() {
		let state = state();
		let store = Store::open(state.path());
		let existing = store.save(sized(1, state.path(), 400)).unwrap();
		store.finish(&existing.id).unwrap();

		let room = store.room_for(600, &policy(900)).unwrap();

		assert_eq!(room, Room::NoRoom(NoRoom::Policy));
		assert!(store.load(&existing.id).unwrap().is_some(), "nothing was dropped");
	}

	#[test]
	fn a_file_larger_than_the_whole_quota_is_not_kept_and_evicts_nothing() {
		let state = state();
		let store = Store::open(state.path());
		let huge = state.path().join("huge.heic");
		fs::write(&huge, vec![b'x'; 2_000]).unwrap();
		let existing = store.save(sized(1, state.path(), 100)).unwrap();

		let room = store.make_room(&huge, &policy(1_000)).unwrap();

		assert_eq!(room, Room::NoRoom(NoRoom::TooLarge));
		assert!(
			store.load(&existing.id).unwrap().is_some(),
			"nothing is evicted for a record that will not exist"
		);
	}

	#[test]
	fn a_failed_new_record_does_not_evict_an_old_one() {
		let state = state();
		let store = Store::open(state.path());
		let mut old = store.save(sized(1, state.path(), 400)).unwrap();
		backdate(&store, &mut old, 8);

		let new = sized(2, state.path(), 400);
		// Force the new record creation to fail after room has been selected.
		fs::create_dir_all(store.directory(&new.id)).unwrap();
		let result = store.save_with_policy(new, &policy(500));

		assert!(result.is_err());
		assert!(
			store.load(&old.id).unwrap().is_some(),
			"the old backup must survive a failed new record"
		);
	}

	#[test]
	fn a_skip_policy_keeps_no_record_and_evicts_nothing() {
		let state = state();
		let store = Store::open(state.path());
		let source = state.path().join("photo.heic");
		fs::write(&source, vec![b'x'; 400]).unwrap();
		let existing = store.save(sized(1, state.path(), 400)).unwrap();

		let mut policy = policy(700);
		policy.on_overflow = Overflow::Skip;

		let room = store.make_room(&source, &policy).unwrap();

		assert_eq!(room, Room::NoRoom(NoRoom::Policy));
		assert!(store.load(&existing.id).unwrap().is_some(), "nothing is evicted");
	}

	#[test]
	fn expired_records_go_before_anything_is_evicted() {
		let state = state();
		let store = Store::open(state.path());
		let source = state.path().join("photo.heic");
		fs::write(&source, vec![b'x'; 400]).unwrap();

		let mut old = store.save(sized(1, state.path(), 400)).unwrap();
		backdate(&store, &mut old, 8);
		let recent = store.save(sized(2, state.path(), 400)).unwrap();

		let room = store.make_room(&source, &policy(900)).unwrap();

		assert_eq!(
			room,
			Room::Kept {
				dropped: vec![old.id.clone()]
			}
		);
		assert!(store.load(&old.id).unwrap().is_none());
		assert!(
			store.load(&recent.id).unwrap().is_some(),
			"a record inside retention stays"
		);
	}

	#[test]
	fn retention_starts_when_a_long_running_job_finishes() {
		let state = state();
		let store = Store::open(state.path());
		let mut record = store.save(subject(1, state.path())).unwrap();
		record.created -= 8 * 24 * 60 * 60 * 1_000;
		write_manifest(&store.directory(&record.id), &record).unwrap();

		store.finish(&record.id).unwrap();
		let finished = store.load(&record.id).unwrap().unwrap();
		assert!(!finished.pinned);
		assert!(
			finished
				.retention_started
				.is_some_and(|started| started > finished.created)
		);
		assert!(store.sweep(Duration::from_secs(7 * 24 * 60 * 60)).unwrap().is_empty());
		assert!(store.load(&record.id).unwrap().is_some());
	}

	#[test]
	fn finishing_a_recovered_job_restarts_retention() {
		let state = state();
		let store = Store::open(state.path());
		let mut record = store.save(subject(1, state.path())).unwrap();
		backdate(&store, &mut record, 8);
		let previous_start = record.retention_started.unwrap();

		store.pin(&record.id).unwrap();
		store.finish(&record.id).unwrap();

		let finished = store.load(&record.id).unwrap().unwrap();
		assert!(
			finished
				.retention_started
				.is_some_and(|started| started > previous_start)
		);
		assert!(store.sweep(Duration::from_secs(7 * 24 * 60 * 60)).unwrap().is_empty());
	}

	#[test]
	fn an_unknown_retention_start_is_not_assumed_expired() {
		let state = state();
		let store = Store::open(state.path());
		let mut record = store.save(subject(1, state.path())).unwrap();
		record.created -= 8 * 24 * 60 * 60 * 1_000;
		record.pinned = false;
		write_manifest(&store.directory(&record.id), &record).unwrap();

		assert!(store.sweep(Duration::from_secs(7 * 24 * 60 * 60)).unwrap().is_empty());
		assert_eq!(
			store
				.make_room(&record.source_path, &policy(record.source_size))
				.unwrap(),
			Room::NoRoom(NoRoom::Policy)
		);
		assert!(store.load(&record.id).unwrap().is_some());
	}

	#[test]
	fn sweeping_drops_what_is_past_retention_and_keeps_the_rest() {
		let state = state();
		let store = Store::open(state.path());

		let mut old = store.save(subject(1, state.path())).unwrap();
		backdate(&store, &mut old, 8);
		let recent = store.save(subject(2, state.path())).unwrap();

		let dropped = store.sweep(Duration::from_secs(7 * 24 * 60 * 60)).unwrap();

		assert_eq!(dropped, vec![old.id.clone()]);
		assert!(store.load(&old.id).unwrap().is_none());
		assert!(store.load(&recent.id).unwrap().is_some());
	}

	#[test]
	fn saving_keeps_the_bytes_and_the_manifest() {
		let state = state();
		let source = source(state.path());
		let store = Store::open(state.path());

		let record = store
			.save(Subject {
				id: id(1),
				intent: "jpg+quality85".to_string(),
				machine: "machine".to_string(),
				bytes: source.clone(),
				source_path: source.clone(),
				source_ext: "heic".to_string(),
				consume_source: true,
				outputs: vec![Output {
					path: state.path().join("photo.jpg"),
					ext: "jpg".to_string(),
				}],
			})
			.expect("saves");

		assert_eq!(record.source_size, 10);
		assert_eq!(fs::read(store.source(&id(1)).unwrap().unwrap()).unwrap(), b"heic bytes");
		assert_eq!(store.load(&id(1)).unwrap().unwrap(), record);
	}

	#[test]
	fn undo_puts_the_source_back_and_removes_the_output() {
		let state = state();
		let source = source(state.path());
		let output = state.path().join("photo.jpg");
		fs::write(&output, b"jpeg bytes").unwrap();

		let store = Store::open(state.path());
		store
			.save(Subject {
				id: id(2),
				intent: "jpg".to_string(),
				machine: "machine".to_string(),
				bytes: source.clone(),
				source_path: source.clone(),
				source_ext: "heic".to_string(),
				consume_source: true,
				outputs: vec![Output {
					path: output.clone(),
					ext: "jpg".to_string(),
				}],
			})
			.unwrap();

		let mut committed = crate::job::Output::pending(output.clone(), "jpg", crate::job::Tier::Converted);
		committed.state = crate::job::OutputState::Done;
		store.prepare_output(&id(2), committed, &output).unwrap();
		// A rename leaves the bytes at the target and the source name gone.
		fs::remove_file(&source).unwrap();

		let outcome = store.restore(&id(2));
		if !crate::fsx::supports_durable_cleanup() {
			assert!(outcome.is_err(), "unsupported durability must refuse output removal");
			assert_eq!(fs::read(&source).unwrap(), b"heic bytes");
			assert!(output.exists());
			assert!(store.source(&id(2)).unwrap().is_some());
			return;
		}
		let restored = outcome.expect("restores");

		assert_eq!(restored.id, id(2));
		assert_eq!(fs::read(&source).unwrap(), b"heic bytes");
		assert!(!output.exists(), "the output is removed");
		assert!(store.load(&id(2)).unwrap().is_none(), "the record is gone");
	}

	#[test]
	fn restoring_into_an_occupied_path_is_refused_before_outputs_are_touched() {
		let state = state();
		let source = source(state.path());
		let output = state.path().join("photo.jpg");
		fs::write(&output, b"jpeg bytes").unwrap();
		let store = Store::open(state.path());

		store
			.save(Subject {
				id: id(3),
				intent: "jpg".to_string(),
				machine: "machine".to_string(),
				bytes: source.clone(),
				source_path: source.clone(),
				source_ext: "heic".to_string(),
				consume_source: true,
				outputs: vec![Output {
					path: output.clone(),
					ext: "jpg".to_string(),
				}],
			})
			.unwrap();

		fs::write(&source, b"another user's file").unwrap();
		let error = store.restore(&id(3)).unwrap_err();

		assert_eq!(error.code(), Code::IoExists);
		assert_eq!(fs::read(&source).unwrap(), b"another user's file", "nothing was lost");
		assert_eq!(fs::read(&output).unwrap(), b"jpeg bytes", "the output was not removed");
		assert!(store.load(&id(3)).unwrap().is_some(), "the record is kept for a retry");
	}

	#[test]
	fn an_unknown_id_has_no_record() {
		let state = state();
		let store = Store::open(state.path());

		assert!(store.load(&id(4)).unwrap().is_none());
		assert!(store.restore(&id(4)).is_err());
		assert!(store.list().unwrap().is_empty());
	}

	#[test]
	fn listing_answers_newest_first() {
		let state = state();
		let source = source(state.path());
		let store = Store::open(state.path());

		for index in [5, 6, 7] {
			store
				.save(Subject {
					id: id(index),
					intent: "jpg".to_string(),
					machine: "machine".to_string(),
					bytes: source.clone(),
					source_path: source.clone(),
					source_ext: "heic".to_string(),
					consume_source: true,
					outputs: Vec::new(),
				})
				.unwrap();

			// `created` comes from the clock, so make the order unambiguous.
			std::thread::sleep(Duration::from_millis(2));
		}

		let listed: Vec<JobId> = store.list().unwrap().into_iter().map(|record| record.id).collect();

		assert_eq!(listed, [id(7), id(6), id(5)]);
	}

	#[cfg(unix)]
	#[test]
	fn the_mode_comes_back_with_the_file() {
		use std::os::unix::fs::PermissionsExt;

		let state = state();
		let source = source(state.path());
		fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).unwrap();

		let store = Store::open(state.path());
		store
			.save(Subject {
				id: id(8),
				intent: "jpg".to_string(),
				machine: "machine".to_string(),
				bytes: source.clone(),
				source_path: source.clone(),
				source_ext: "heic".to_string(),
				consume_source: true,
				outputs: Vec::new(),
			})
			.unwrap();

		fs::remove_file(&source).unwrap();
		store.restore(&id(8)).unwrap();

		assert_eq!(fs::metadata(&source).unwrap().permissions().mode() & 0o777, 0o640);
	}
}
