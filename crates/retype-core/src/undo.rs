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
	/// Original evidence persisted before copying; never proof that the backup is complete.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub reservation_fingerprint: Option<Fingerprint>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub source_attributes_sha256: Option<String>,
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

enum RecoveryLifecycle<'a> {
	Unfinished,
	Retained { retention_started: i64 },
	Undoing(&'a UndoProgress),
	Legacy,
}

impl Record {
	fn lifecycle(&self) -> RecoveryLifecycle<'_> {
		match (&self.undo_progress, self.pinned, self.retention_started) {
			(Some(progress), _, _) => RecoveryLifecycle::Undoing(progress),
			(None, true, _) => RecoveryLifecycle::Unfinished,
			(None, false, Some(retention_started)) => RecoveryLifecycle::Retained { retention_started },
			(None, false, None) => RecoveryLifecycle::Legacy,
		}
	}

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
			reservation_fingerprint: None,
			source_attributes_sha256: None,
			pinned: true,
			undo_progress: None,
			consume_source: true,
		}
	}

	fn verify_source_attributes(&self, file: &fs::File) -> Result<()> {
		if let Some(expected) = &self.source_attributes_sha256 {
			if expected != &crate::xattr::fingerprint(file)? {
				return Err(std::io::Error::other("retained original failed its attribute check").into());
			}
		}
		Ok(())
	}

	fn verify_source_metadata(&self, file: &fs::File) -> Result<()> {
		let metadata = file.metadata()?;
		if mode_of(&metadata) != self.source_mode || mtime_ns(&metadata) != self.source_mtime_ns {
			return Err(std::io::Error::other("the original's permissions or modification time changed").into());
		}
		self.verify_source_attributes(file)
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
/// Admission controls the storage goal; Skip is an explicit request to retain no new backup at the limit.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
	StopAfter,
	StopBefore,
}

impl Admission {
	pub fn parse(text: &str) -> Option<Self> {
		match text {
			"stop-after" => Some(Self::StopAfter),
			"stop-before" => Some(Self::StopBefore),
			_ => None,
		}
	}
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
	pub retained: u64,
	pub reserved: u64,
	pub goal: u64,
	pub overage: u64,
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
	/// Soft goal counted in retained and reserved source bytes.
	pub quota: u64,
	pub on_overflow: Overflow,
	pub admission: Admission,
}

impl Default for Policy {
	fn default() -> Self {
		Self {
			enabled: true,
			retention: Duration::from_secs(7 * 24 * 60 * 60),
			quota: 5_000_000_000,
			on_overflow: Overflow::Ask,
			admission: Admission::StopAfter,
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
		crate::fsx::create_dir_all_durable(state)?;
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
		Ok(JobLease {
			_file: file,
			id: id.clone(),
			root: self.root.clone(),
		})
	}

	/// Transfer a job lease and its saved manifest into execution's recovery ownership.
	pub fn retain(&self, lease: JobLease, record: Option<Record>) -> Result<Recovery<'_>> {
		if lease.root != self.root || record.as_ref().is_some_and(|record| record.id != lease.id) {
			return Err(Error::Io(std::io::Error::other(
				"recovery record does not belong to the job lease",
			)));
		}
		Ok(match record {
			Some(record) => Recovery::Retained(RetainedRecovery {
				store: self,
				record: Box::new(record),
				_lease: lease,
			}),
			None => Recovery::Unavailable(lease),
		})
	}

	/// Keep the source bytes and a manifest before converting, `docs/dev/architecture/state.md` §5.
	///
	/// `bytes` is the file as it is now: after a rename that is the source content
	/// under the target's name, which is exactly what an undo has to put back.
	pub fn save(&self, subject: Subject) -> Result<Record> {
		let bytes = subject.bytes.clone();
		let expected = Fingerprint::of(&bytes)?;
		let record = {
			let _guard = store_lock(&self.root)?;
			self.reserve(subject, Some(expected))?
		};
		self.complete(record, &bytes, &crate::execute::Cancellation::default())
	}

	/// Reserve under the store lock; copy outside it so control and recovery remain responsive.
	pub fn save_with_policy(&self, subject: Subject, policy: &Policy) -> Result<Option<Record>> {
		self.save_with_policy_cancellable(subject, policy, &crate::execute::Cancellation::default())
	}

	pub fn save_with_policy_cancellable(
		&self,
		subject: Subject,
		policy: &Policy,
		cancellation: &crate::execute::Cancellation,
	) -> Result<Option<Record>> {
		if !policy.enabled {
			return Ok(None);
		}
		let bytes = subject.bytes.clone();
		let expected = Fingerprint::of_cancellable(&bytes, cancellation)?;
		let (record, dropped) = {
			let _guard = store_lock(&self.root)?;
			cancellation.commit(|| Ok(()))?;
			let room = self.room_for_locked(fs::metadata(&bytes)?.len(), policy)?;
			let Room::Kept { dropped } = room else {
				if policy.on_overflow == Overflow::Skip {
					return Ok(None);
				}
				return Err(Diagnostic::global(
					Code::UndoLimit,
					"undo storage goal reached; waiting for expiry, explicit cleanup or a larger [undo] quota",
				)
				.into());
			};
			(self.reserve(subject, Some(expected))?, dropped)
		};
		let record = self.complete(record, &bytes, cancellation)?;
		let _guard = store_lock(&self.root)?;
		for id in dropped {
			let _lease = match self.lease(&id) {
				Ok(lease) => lease,
				Err(error) if error.code() == Code::IoLocked => continue,
				Err(error) => return Err(error),
			};
			// A concurrent resume can restart retention while the new backup is copied.
			if self.load(&id)?.is_some_and(|record| {
				!record.pinned && age(&record, SystemTime::now()).is_some_and(|age| age > policy.retention)
			}) {
				self.remove_locked(&id)?;
			}
		}
		Ok(Some(record))
	}

	fn reserve(&self, subject: Subject, expected: Option<Fingerprint>) -> Result<Record> {
		crate::fsx::create_dir_all_durable(&self.root)?;
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
		record.reservation_fingerprint = expected;
		record.source_attributes_sha256 = Some(crate::xattr::fingerprint(&open_undo_backup(&subject.bytes)?)?);

		write_manifest(&directory, &record)?;
		crate::fsx::sync_directory(&self.root)?;
		crate::fsx::sync_directory(self.root.parent().expect("state directory"))?;
		Ok(record)
	}

	fn complete(
		&self,
		mut record: Record,
		bytes: &Path,
		cancellation: &crate::execute::Cancellation,
	) -> Result<Record> {
		let directory = self.directory(&record.id);
		let held = directory.join(record.source_file());
		let fingerprint = crate::safety::copy_snapshot_cancellable(bytes, &held, cancellation)?;
		let attributes = crate::xattr::fingerprint(&open_undo_backup(&held)?)?;
		if record
			.reservation_fingerprint
			.as_ref()
			.is_some_and(|expected| !expected.same_file_and_content(&fingerprint))
			|| record
				.source_attributes_sha256
				.as_ref()
				.is_some_and(|expected| expected != &attributes)
			|| record.source_size != fingerprint.bytes
		{
			return Err(Error::Io(std::io::Error::other(
				"original changed after backup reservation; recovery remains pinned",
			)));
		}
		record.source_fingerprint = Some(fingerprint);
		record.source_attributes_sha256 = Some(attributes);
		record.link = Link::Copy;
		write_manifest(&directory, &record)?;
		Ok(record)
	}

	pub fn usage(&self, policy: &Policy) -> Result<Usage> {
		let mut usage = Usage {
			goal: policy.quota,
			..Usage::default()
		};
		for record in self.list()? {
			if record.source_fingerprint.is_none() {
				usage.reserved = usage.reserved.saturating_add(record.source_size);
			} else {
				usage.retained = usage.retained.saturating_add(record.source_size);
			}
		}
		usage.overage = usage.retained.saturating_add(usage.reserved).saturating_sub(usage.goal);
		Ok(usage)
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
		self.restore_cancellable(id, &crate::execute::Cancellation::default())
	}

	pub fn restore_cancellable(&self, id: &JobId, cancellation: &crate::execute::Cancellation) -> Result<Record> {
		let lease = self.lease(id)?;
		self.restore_owned(lease, cancellation)
	}

	pub fn restore_owned(&self, lease: JobLease, cancellation: &crate::execute::Cancellation) -> Result<Record> {
		UndoStarted::open_owned(self, lease, cancellation.clone())?
			.prepare_source()?
			.cleanup(|record| {
				crate::fsx::require_durable_cleanup(&record.source_path)?;
				remove_outputs_cancellable(record, cancellation)
			})
	}

	#[cfg(test)]
	fn restore_with_cleanup(&self, id: &JobId, cleanup: impl FnOnce(&Record) -> Result<()>) -> Result<Record> {
		UndoStarted::open(self, id)?.prepare_source()?.cleanup(cleanup)
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
		cancellation: &crate::execute::Cancellation,
	) -> Result<Restoration> {
		let staged = crate::convert::StagedOutput::new(&record.source_path)?;
		let backup = crate::safety::copy_snapshot_cancellable(held, staged.temp(), cancellation)?;
		let expected = record.source_fingerprint.as_ref().expect("verified record");
		if !expected.same_content(&backup) {
			return Err(Error::Io(std::io::Error::other(
				"retained original changed during restoration",
			)));
		}
		let fingerprint = Fingerprint::of_cancellable(staged.temp(), cancellation)?;
		if !backup.same_content(&fingerprint) {
			return Err(Error::Io(std::io::Error::other(
				"staged original changed during restoration",
			)));
		}
		record.undo_progress.as_mut().expect("undo started").restore_intent = Some(fingerprint.clone());
		write_manifest(&self.directory(&record.id), record)?;
		#[cfg(test)]
		crate::durability_tests::checkpoint("undo.intent-durable")?;
		Ok(Restoration { staged, fingerprint })
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
			Ok(()) => {
				crate::fsx::sync_directory(&self.root)?;
				Ok(())
			}
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

		let fits = if policy.admission == Admission::StopAfter && policy.on_overflow != Overflow::Skip {
			held < policy.quota
		} else {
			held.saturating_add(wanted) <= policy.quota
		};
		if fits {
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
		write_manifest(&self.directory(id), &record)?;
		#[cfg(test)]
		crate::durability_tests::checkpoint("publication.intent-durable")?;
		Ok(())
	}

	/// Recheck the retained input and complete published inventory before disposal.
	pub fn verify_disposal(&self, id: &JobId, outputs: &[crate::job::Output]) -> Result<()> {
		let _guard = self.guard_disposal(id, outputs)?;
		Ok(())
	}

	/// Hold the verified backup and outputs through the source-disposition attempt.
	pub fn guard_disposal(&self, id: &JobId, outputs: &[crate::job::Output]) -> Result<DisposalGuard> {
		let record = self
			.load(id)?
			.ok_or_else(|| Error::Io(std::io::Error::other("recovery record disappeared")))?;
		let source = self.source(id)?;
		if source.is_none() || record.outputs.is_empty() || record.outputs.len() != outputs.len() {
			return Err(Error::Io(std::io::Error::other(
				"no verified original or complete output inventory; source retained",
			)));
		}
		let (backup, fingerprint) = guarded_fingerprint(source.as_ref().expect("verified source"))?;
		record.verify_source_attributes(&backup)?;
		if !record
			.source_fingerprint
			.as_ref()
			.is_some_and(|expected| expected.same_content(&fingerprint))
		{
			return Err(Error::Io(std::io::Error::other(
				"retained original changed before disposal",
			)));
		}
		let mut files = vec![backup];
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
			let (file, fingerprint) = guarded_fingerprint(&output.path)?;
			if !commit.fingerprint.same_file_and_content(&fingerprint) {
				return Err(Error::Io(std::io::Error::other(format!(
					"{} changed after publication; source retained",
					output.path.display()
				))));
			}
			files.push(file);
		}
		Ok(DisposalGuard { _files: files })
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
		if matches!(record.lifecycle(), RecoveryLifecycle::Undoing(_)) {
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
		self.source_cancellable(id, &crate::execute::Cancellation::default())
	}

	pub fn source_cancellable(
		&self,
		id: &JobId,
		cancellation: &crate::execute::Cancellation,
	) -> Result<Option<PathBuf>> {
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
		let mut file = open_undo_backup(&path)?;
		let actual = Fingerprint::of_open_file_cancellable(&path, &mut file, cancellation)?;
		record.verify_source_attributes(&file)?;
		if !expected.same_content(&actual) {
			return Err(Error::Io(std::io::Error::other(
				"retained original failed its content check",
			)));
		}
		Ok(Some(path))
	}
}

struct UndoStarted<'a> {
	store: &'a Store,
	record: Record,
	_lease: JobLease,
	cancellation: crate::execute::Cancellation,
}

impl<'a> UndoStarted<'a> {
	#[cfg(test)]
	fn open(store: &'a Store, id: &JobId) -> Result<Self> {
		Self::open_owned(store, store.lease(id)?, crate::execute::Cancellation::default())
	}

	fn open_owned(store: &'a Store, lease: JobLease, cancellation: crate::execute::Cancellation) -> Result<Self> {
		let id = &lease.id;
		let record = store.load(id)?.ok_or_else(|| {
			Error::Diagnostic(Diagnostic::global(
				Code::JobNotFound,
				format!("{id} has no undo record"),
			))
		})?;
		Self::begin(store, record, lease, cancellation)
	}

	fn begin(
		store: &'a Store,
		mut record: Record,
		lease: JobLease,
		cancellation: crate::execute::Cancellation,
	) -> Result<Self> {
		if lease.root != store.root || lease.id != record.id {
			return Err(Error::Io(std::io::Error::other(
				"undo record does not belong to its job lease",
			)));
		}
		let policy = existing_source_policy(&record);
		{
			let _guard = store_lock(&store.root)?;
			store.begin_undo(&mut record, policy)?;
		}
		#[cfg(test)]
		crate::durability_tests::checkpoint("undo.started")?;
		Ok(Self {
			store,
			record,
			cancellation,
			_lease: lease,
		})
	}

	fn prepare_source(mut self) -> Result<UndoSourceReady<'a>> {
		let record = &mut self.record;
		let policy = existing_source_policy(record);
		verify_undo_outputs(record, &self.cancellation)?;
		let source_exists = undo_source_exists(&record.source_path)?;
		let has_restore_intent = record
			.undo_progress
			.as_ref()
			.is_some_and(|progress| progress.restore_intent.is_some());
		let must_verify = policy == ExistingSource::Verify || !source_exists || has_restore_intent;
		let held = if must_verify {
			let path = self
				.store
				.source_cancellable(&record.id, &self.cancellation)?
				.ok_or_else(|| Error::Io(std::io::Error::other("no verified original to restore")))?;
			let mut file = open_undo_backup(&path)?;
			let fingerprint = Fingerprint::of_open_file_cancellable(&path, &mut file, &self.cancellation)?;
			record.verify_source_attributes(&file)?;
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
				self.cancellation.commit(|| Ok(()))?;
				self.store
					.prepare_restoration(record, path, &self.cancellation)?
					.publish()?;
			}
		}
		let mut source = open_undo_source(&record.source_path)?;
		let before_cleanup =
			Fingerprint::of_open_file_cancellable(&record.source_path, &mut source, &self.cancellation)?;
		if (must_verify && !is_undo_fingerprint(record, &before_cleanup))
			|| !before_cleanup.matches_cancellable(&record.source_path, &self.cancellation)?
		{
			return Err(Error::Exists(record.source_path.clone()));
		}
		if must_verify {
			restore_times(&source, record)?;
		}
		source.sync_all()?;
		crate::fsx::sync_directory(record.source_path.parent().unwrap_or(Path::new(".")))?;
		#[cfg(test)]
		crate::durability_tests::checkpoint("undo.source-durable")?;
		Ok(UndoSourceReady {
			undo: self,
			source,
			before_cleanup,
			held,
		})
	}
}

struct UndoSourceReady<'a> {
	undo: UndoStarted<'a>,
	source: fs::File,
	before_cleanup: Fingerprint,
	held: Option<(PathBuf, fs::File)>,
}

impl UndoSourceReady<'_> {
	fn cleanup(self, cleanup: impl FnOnce(&Record) -> Result<()>) -> Result<Record> {
		let Self {
			undo,
			source,
			before_cleanup,
			held,
		} = self;
		cleanup(&undo.record)?;
		if !before_cleanup.matches_cancellable(&undo.record.source_path, &undo.cancellation)? {
			return Err(Error::Io(std::io::Error::other(format!(
				"{} changed during undo; recovery retained",
				undo.record.source_path.display()
			))));
		}
		/* Windows cannot remove the backup while its non-delete-sharing handle is open. */
		drop(held);
		#[cfg(test)]
		crate::durability_tests::checkpoint("undo.before-forget")?;
		{
			let _guard = store_lock(&undo.store.root)?;
			undo.store.remove_locked(&undo.record.id)?;
		}
		#[cfg(test)]
		crate::durability_tests::checkpoint("undo.complete")?;
		drop(source);
		Ok(undo.record)
	}
}

/// Guards verified files but does not replace the per-job lease or durability checks.
#[must_use = "keep the verified files guarded through source disposition"]
pub struct DisposalGuard {
	_files: Vec<fs::File>,
}

fn guarded_fingerprint(path: &Path) -> Result<(fs::File, Fingerprint)> {
	guarded_fingerprint_cancellable(path, &crate::execute::Cancellation::default())
}

fn guarded_fingerprint_cancellable(
	path: &Path,
	cancellation: &crate::execute::Cancellation,
) -> Result<(fs::File, Fingerprint)> {
	let mut options = fs::OpenOptions::new();
	options.read(true);
	let mut file = crate::safety::open_guarded_file(path, options)?;
	let fingerprint = Fingerprint::of_open_file_cancellable(path, &mut file, cancellation)?;
	if !fingerprint.matches_cancellable(path, cancellation)? {
		return Err(Error::Io(std::io::Error::other(format!(
			"{} changed while being guarded; source retained",
			path.display()
		))));
	}
	Ok((file, fingerprint))
}

fn verify_undo_outputs(record: &Record, cancellation: &crate::execute::Cancellation) -> Result<()> {
	for commit in &record.commits {
		match fs::symlink_metadata(&commit.output.path) {
			Ok(_)
				if !commit
					.fingerprint
					.matches_cancellable(&commit.output.path, cancellation)? =>
			{
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
	match record.lifecycle() {
		RecoveryLifecycle::Undoing(progress) => progress.existing_source,
		RecoveryLifecycle::Unfinished => ExistingSource::Verify,
		RecoveryLifecycle::Retained { .. } | RecoveryLifecycle::Legacy if record.consume_source => {
			ExistingSource::Verify
		}
		RecoveryLifecycle::Retained { .. } | RecoveryLifecycle::Legacy => ExistingSource::LeaveUntouched,
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

struct Restoration {
	staged: crate::convert::StagedOutput,
	fingerprint: Fingerprint,
}

impl Restoration {
	fn publish(self) -> Result<()> {
		let Self { staged, fingerprint } = self;
		let target = staged.target().to_path_buf();
		if !fingerprint.matches(staged.temp())? {
			return Err(Error::Io(std::io::Error::other(
				"staged original changed before publication",
			)));
		}
		staged.commit_checked(&fingerprint)?;
		if !fingerprint.matches(&target)? {
			return Err(Error::Io(std::io::Error::other(
				"restored original changed during publication",
			)));
		}
		Ok(())
	}
}

/// Remove every output a record says the job produced.
fn remove_outputs_cancellable(record: &Record, cancellation: &crate::execute::Cancellation) -> Result<()> {
	for commit in &record.commits {
		match fs::symlink_metadata(&commit.output.path) {
			Ok(_) => {
				cancellation.commit(|| Ok(()))?;
				crate::safety::remove_owned_cancellable(&commit.output.path, &commit.fingerprint, cancellation)?;
			}
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
			Err(error) => return Err(error.into()),
		}
	}
	Ok(())
}

/// Execution owns its lease even when no recovery record could be retained.
pub enum Recovery<'a> {
	Unavailable(JobLease),
	Retained(RetainedRecovery<'a>),
}

impl Recovery<'_> {
	pub fn id(&self) -> &JobId {
		match self {
			Self::Unavailable(lease) => &lease.id,
			Self::Retained(recovery) => &recovery.record.id,
		}
	}

	pub fn retained(&self) -> Option<&RetainedRecovery<'_>> {
		match self {
			Self::Unavailable(_) => None,
			Self::Retained(recovery) => Some(recovery),
		}
	}
}

/// A manifest bound to the store and exclusive job lease that own it.
/// Finishing consumes ownership; retention cannot be finished twice through this handle.
/// ```compile_fail
/// use retype_core::undo::RetainedRecovery;
/// fn twice(recovery: RetainedRecovery<'_>) {
///     let _ = recovery.finish();
///     let _ = recovery.finish();
/// }
/// ```
pub struct RetainedRecovery<'a> {
	store: &'a Store,
	record: Box<Record>,
	_lease: JobLease,
}

impl RetainedRecovery<'_> {
	/// Manifest snapshot at lease transfer; publication and disposal use the current durable ledger.
	pub fn record_snapshot(&self) -> &Record {
		&self.record
	}

	pub fn store(&self) -> &Store {
		self.store
	}

	pub fn prepare_output(
		&self,
		output: crate::job::Output,
		staged: &Path,
		validated: Option<&Fingerprint>,
	) -> Result<()> {
		self.store
			.prepare_checked_output(&self.record.id, output, staged, validated)
	}

	pub fn guard_disposal(&self, outputs: &[crate::job::Output]) -> Result<DisposalGuard> {
		self.store.guard_disposal(&self.record.id, outputs)
	}

	/// Release redundant recovery after durable cancellation with no publication intents.
	pub fn release_unpublished(self) -> Result<()> {
		self.release_unpublished_cancellable(&crate::execute::Cancellation::default())
	}

	pub fn release_unpublished_cancellable(self, cancellation: &crate::execute::Cancellation) -> Result<()> {
		crate::fsx::require_durable_cleanup(&self.record.source_path)?;
		let record = self.store.load(&self.record.id)?.ok_or_else(|| {
			Error::Diagnostic(Diagnostic::global(
				Code::JobResumeUnavailable,
				"the recovery record is missing",
			))
		})?;
		if record.source_fingerprint.is_none() {
			let expected = record.reservation_fingerprint.as_ref().ok_or_else(|| {
				Error::Diagnostic(Diagnostic::global(
					Code::JobResumeUnavailable,
					"legacy reservation lacks original evidence; inspect it before explicit cleanup",
				))
			})?;
			if !record.pinned || !record.commits.is_empty() || record.undo_progress.is_some() {
				return Err(Error::Diagnostic(Diagnostic::global(
					Code::JobResumeUnavailable,
					"reservation has publication or restoration evidence; recovery retained",
				)));
			}
			let (source, fingerprint) = guarded_fingerprint_cancellable(&record.source_path, cancellation)?;
			if !expected.same_file_and_content(&fingerprint) {
				return Err(Error::Exists(record.source_path.clone()));
			}
			record.verify_source_metadata(&source)?;
			cancellation.commit(|| Ok(()))?;
			if !expected.matches_cancellable(&record.source_path, cancellation)? {
				return Err(Error::Exists(record.source_path.clone()));
			}
			let _guard = store_lock(&self.store.root)?;
			self.store.remove_locked(&record.id)?;
			crate::fsx::sync_directory(&self.store.root)?;
			return Ok(());
		}
		if record.source_attributes_sha256.is_none() {
			return Err(Error::Diagnostic(Diagnostic::global(
				Code::JobResumeUnavailable,
				"the original attributes have no recorded digest; recovery cannot be proven redundant",
			)));
		}
		if !record.pinned || !record.commits.is_empty() {
			return Err(Error::Diagnostic(Diagnostic::global(
				Code::JobResumeUnavailable,
				"recovery has retained outputs or publication intents; it is not proven redundant",
			)));
		}
		match fs::symlink_metadata(&record.source_path) {
			Ok(_) => {
				let (source, fingerprint) = guarded_fingerprint_cancellable(&record.source_path, cancellation)?;
				if !is_undo_fingerprint(&record, &fingerprint) {
					return Err(Error::Exists(record.source_path.clone()));
				}
				record.verify_source_metadata(&source)?;
			}
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
			Err(error) => return Err(error.into()),
		}
		let ready = UndoStarted::begin(self.store, record, self._lease, cancellation.clone())?.prepare_source()?;
		ready.undo.record.verify_source_metadata(&ready.source)?;
		/* Reuse checked restoration and guarded cleanup, but never remove an output. */
		ready.cleanup(|_| Ok(()))?;
		crate::fsx::sync_directory(&self.store.root)?;
		Ok(())
	}

	/// Release execution ownership only after the caller has durably recorded its disposition.
	pub fn finish(self) -> Result<()> {
		self.store.finish(&self.record.id)
	}
}

pub struct JobLease {
	_file: fs::File,
	id: JobId,
	root: PathBuf,
}

#[cfg(unix)]
impl Drop for JobLease {
	fn drop(&mut self) {
		unlock_flock(&self._file);
	}
}

fn leased(id: &JobId) -> Error {
	Error::Diagnostic(Diagnostic::global(
		Code::IoLocked,
		format!("{id} is still running or being recovered; cancel it and wait before undoing"),
	))
}

pub(crate) struct StoreLock {
	_process: std::sync::MutexGuard<'static, ()>,
	_file: fs::File,
}

#[cfg(unix)]
impl Drop for StoreLock {
	fn drop(&mut self) {
		unlock_flock(&self._file);
	}
}

#[cfg(unix)]
fn unlock_flock(file: &fs::File) {
	use std::os::fd::AsRawFd;

	/* A fork can retain the open description until exec, even after its owner drops. */
	loop {
		if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) } == 0 {
			return;
		}
		let error = std::io::Error::last_os_error();
		if error.kind() != std::io::ErrorKind::Interrupted {
			eprintln!("retype: cannot release undo lock: {error}");
			return;
		}
	}
}

pub(crate) fn store_lock(root: &Path) -> Result<StoreLock> {
	let process = STORE_LOCK
		.lock()
		.map_err(|_| Error::Io(std::io::Error::other("undo store lock poisoned")))?;
	let state = root.parent().expect("state directory");
	crate::fsx::create_dir_all_durable(state)?;
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
	let started = match record.lifecycle() {
		RecoveryLifecycle::Retained { retention_started } => Some(retention_started),
		RecoveryLifecycle::Legacy => None,
		RecoveryLifecycle::Unfinished | RecoveryLifecycle::Undoing(_) => record.retention_started,
	};
	started.map(|started| {
		let started = UNIX_EPOCH + Duration::from_millis(started.max(0) as u64);
		now.duration_since(started).unwrap_or_default()
	})
}

#[cfg(test)]
#[path = "undo_recovery_tests.rs"]
mod recovery_tests;

#[cfg(test)]
#[path = "undo_cancel_tests.rs"]
mod cancel_tests;

#[cfg(test)]
mod tests {
	use super::*;

	fn id(index: usize) -> JobId {
		JobId::parse(&format!("01J{:023}", index)).expect("a valid id")
	}

	#[cfg(any(target_os = "linux", target_os = "macos"))]
	#[test]
	fn undo_restores_binary_attributes_and_rejects_attribute_corruption() {
		let directory = state();
		let store = Store::open(directory.path());
		let subject = subject(1, directory.path());
		let attribute = crate::xattr::INTENT;
		let value = b"original\0binary attribute";
		::xattr::set(&subject.bytes, attribute, value).unwrap();
		let record = store.save(subject).unwrap();
		let backup = store.source(&record.id).unwrap().unwrap();
		::xattr::set(&backup, attribute, b"corrupted attribute").unwrap();
		assert!(
			store
				.source(&record.id)
				.unwrap_err()
				.to_string()
				.contains("attribute check")
		);
		::xattr::set(&backup, attribute, value).unwrap();
		fs::remove_file(&record.source_path).unwrap();
		store.restore(&record.id).unwrap();
		assert_eq!(
			::xattr::get(&record.source_path, attribute).unwrap(),
			Some(value.to_vec())
		);
		assert!(!store.directory(&record.id).exists());
	}

	#[test]
	fn recovery_ownership_keeps_the_lease_without_a_backup() {
		let directory = state();
		let store = Store::open(directory.path());
		let recovery = store.retain(store.lease(&id(1)).unwrap(), None).unwrap();
		assert!(recovery.retained().is_none());
		assert!(store.lease(&id(1)).is_err());
		drop(recovery);
		assert!(store.lease(&id(1)).is_ok());
	}

	#[test]
	fn recovery_refuses_a_manifest_or_store_from_another_lease() {
		let directory = state();
		let store = Store::open(directory.path());
		let record = store.save(subject(2, directory.path())).unwrap();
		assert!(store.retain(store.lease(&id(1)).unwrap(), Some(record)).is_err());
		let other = Store::open(&directory.path().join("other"));
		assert!(other.retain(store.lease(&id(1)).unwrap(), None).is_err());
		assert!(store.load(&id(2)).unwrap().unwrap().pinned);
	}

	#[test]
	fn legacy_retention_and_unfinished_recovery_remain_distinct() {
		let directory = state();
		let store = Store::open(directory.path());
		let mut record = store.save(subject(1, directory.path())).unwrap();
		assert!(matches!(record.lifecycle(), RecoveryLifecycle::Unfinished));
		record.pinned = false;
		assert!(matches!(record.lifecycle(), RecoveryLifecycle::Legacy));
		record.retention_started = Some(100);
		assert!(matches!(
			record.lifecycle(),
			RecoveryLifecycle::Retained { retention_started: 100 }
		));
		store.begin_undo(&mut record, ExistingSource::Verify).unwrap();
		assert!(matches!(record.lifecycle(), RecoveryLifecycle::Undoing(_)));
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

	#[cfg(unix)]
	#[test]
	fn dropping_a_job_lease_unlocks_a_duplicated_open_description() {
		use std::os::fd::AsRawFd;

		let state = state();
		let store = Store::open(state.path());
		let job_id = id(1);
		let lease = store.lease(&job_id).unwrap();
		/* A fork inherits the same open description before close-on-exec. */
		let duplicate = lease._file.try_clone().unwrap();
		assert_eq!(store.lease(&job_id).err().unwrap().code(), Code::IoLocked);

		drop(lease);
		let next = store.lease(&job_id);
		assert!(next.is_ok(), "a duplicate kept the dropped lease locked");
		let next = next.unwrap();
		assert_eq!(unsafe { libc::flock(duplicate.as_raw_fd(), libc::LOCK_UN) }, 0);
		drop(duplicate);
		assert_eq!(store.lease(&job_id).err().unwrap().code(), Code::IoLocked);
		drop(next);
		assert!(store.lease(&job_id).is_ok());
	}

	#[cfg(unix)]
	#[test]
	fn dropping_a_store_lock_unlocks_a_duplicated_open_description() {
		use std::os::fd::AsRawFd;

		let state = state();
		let store = Store::open(state.path());
		let guard = store_lock(&store.root).unwrap();
		let duplicate = guard._file.try_clone().unwrap();
		let probe = fs::OpenOptions::new()
			.read(true)
			.write(true)
			.open(state.path().join("undo.lock"))
			.unwrap();
		assert_ne!(
			unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
			0
		);
		assert_eq!(std::io::Error::last_os_error().kind(), std::io::ErrorKind::WouldBlock);

		drop(guard);
		assert_eq!(
			unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
			0,
			"a duplicate kept the dropped store lock held"
		);
		let second = fs::OpenOptions::new()
			.read(true)
			.write(true)
			.open(state.path().join("undo.lock"))
			.unwrap();
		assert_eq!(unsafe { libc::flock(duplicate.as_raw_fd(), libc::LOCK_UN) }, 0);
		drop(duplicate);
		assert_ne!(
			unsafe { libc::flock(second.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
			0
		);
		assert_eq!(std::io::Error::last_os_error().kind(), std::io::ErrorKind::WouldBlock);
		drop(probe);
		assert_eq!(
			unsafe { libc::flock(second.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
			0
		);
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
	fn stop_after_keeps_the_crossing_job_then_holds_the_next() {
		let state = state();
		let store = Store::open(state.path());
		let policy = policy(500);
		store
			.save_with_policy(sized(1, state.path(), 400), &policy)
			.unwrap()
			.unwrap();
		store
			.save_with_policy(sized(2, state.path(), 400), &policy)
			.unwrap()
			.unwrap();
		assert_eq!(store.usage(&policy).unwrap().overage, 300);
		let error = store.save_with_policy(sized(3, state.path(), 10), &policy).unwrap_err();
		assert_eq!(error.code(), Code::UndoLimit);
		assert_eq!(store.list().unwrap().len(), 2);
	}

	#[test]
	fn incomplete_backup_cleanup_requires_the_unchanged_original_and_no_publication() {
		for changed in [false, true] {
			let state = state();
			let store = Store::open(state.path());
			let subject = sized(1, state.path(), 600);
			let path = subject.bytes.clone();
			let expected = Fingerprint::of(&path).unwrap();
			let record = store.reserve(subject, Some(expected)).unwrap();
			fs::write(store.directory(&record.id).join(record.source_file()), b"partial").unwrap();
			if changed {
				fs::write(&path, b"changed original").unwrap();
			}
			let recovery = store
				.retain(store.lease(&record.id).unwrap(), Some(record.clone()))
				.unwrap();
			let Recovery::Retained(recovery) = recovery else {
				unreachable!()
			};
			let outcome = recovery.release_unpublished();
			if changed || !crate::fsx::supports_durable_cleanup() {
				assert!(outcome.is_err());
				assert!(store.load(&record.id).unwrap().unwrap().pinned);
			} else {
				outcome.unwrap();
				assert!(store.load(&record.id).unwrap().is_none());
				assert_eq!(fs::metadata(&path).unwrap().len(), 600);
			}
		}
	}

	#[test]
	fn reservation_crash_worker() {
		let Some(root) = std::env::var_os("RETYPE_RESERVATION_CRASH_ROOT") else {
			return;
		};
		let root = PathBuf::from(root);
		let phase = std::env::var("RETYPE_RESERVATION_CRASH_PHASE").unwrap();
		let store = Store::open(&root.join("state"));
		let subject = sized(1, &root, 8192);
		let expected = Fingerprint::of(&subject.bytes).unwrap();
		let source = subject.bytes.clone();
		let record = store.reserve(subject, Some(expected)).unwrap();
		let held = store.directory(&record.id).join(record.source_file());
		if phase == "copying" {
			fs::write(&held, b"partial").unwrap();
		}
		if phase == "copied" {
			crate::safety::copy_snapshot(&source, &held).unwrap();
		}
		std::process::exit(81);
	}

	#[test]
	#[cfg(unix)]
	fn process_interruption_before_during_and_after_backup_copy_releases_only_redundant_reservations() {
		for phase in ["reserved", "copying", "copied"] {
			let root = tempfile::tempdir().unwrap();
			let status = std::process::Command::new(std::env::current_exe().unwrap())
				.args(["--exact", "undo::tests::reservation_crash_worker", "--nocapture"])
				.env("RETYPE_RESERVATION_CRASH_ROOT", root.path())
				.env("RETYPE_RESERVATION_CRASH_PHASE", phase)
				.status()
				.unwrap();
			assert_eq!(status.code(), Some(81));
			let store = Store::open(&root.path().join("state"));
			let record = store.list().unwrap().pop().unwrap();
			assert!(record.pinned && record.source_fingerprint.is_none());
			let original = fs::read(&record.source_path).unwrap();
			let recovery = store
				.retain(store.lease(&record.id).unwrap(), Some(record.clone()))
				.unwrap();
			let Recovery::Retained(recovery) = recovery else {
				unreachable!()
			};
			recovery.release_unpublished().unwrap();
			assert!(store.list().unwrap().is_empty());
			assert_eq!(fs::read(&record.source_path).unwrap(), original);
		}
	}

	#[test]
	fn a_cancelled_undo_preserves_its_recovery_and_can_be_retried() {
		let state = state();
		let store = Store::open(state.path());
		let record = store.save(subject(1, state.path())).unwrap();
		let token = crate::execute::Cancellation::default();
		token.cancel();
		assert!(store.restore_cancellable(&record.id, &token).is_err());
		assert!(store.load(&record.id).unwrap().unwrap().pinned);
		assert!(store.source(&record.id).unwrap().is_some());
	}

	#[test]
	fn an_incomplete_reservation_counts_after_reopening_the_store() {
		let state = state();
		let store = Store::open(state.path());
		let policy = policy(500);
		store.reserve(sized(1, state.path(), 600), None).unwrap();
		let reopened = Store::open(state.path());
		assert_eq!(reopened.usage(&policy).unwrap().reserved, 600);
		assert!(matches!(reopened.room_for(1, &policy).unwrap(), Room::NoRoom(_)));
		assert!(reopened.sweep(Duration::ZERO).unwrap().is_empty());
	}

	#[test]
	fn concurrent_admissions_cannot_both_cross_the_goal() {
		let state = state();
		let store = Store::open(state.path());
		let policy = policy(500);
		store.save_with_policy(sized(1, state.path(), 400), &policy).unwrap();
		let first = sized(2, state.path(), 400);
		let second = sized(3, state.path(), 400);
		let gate = std::sync::Barrier::new(2);
		let accepted = std::thread::scope(|scope| {
			let run = |subject| {
				gate.wait();
				match store.save_with_policy(subject, &policy) {
					Ok(Some(_)) => true,
					Err(error) if error.code() == Code::UndoLimit => false,
					other => panic!("unexpected reservation outcome: {other:?}"),
				}
			};
			let first = scope.spawn(move || run(first));
			let second = scope.spawn(move || run(second));
			usize::from(first.join().unwrap()) + usize::from(second.join().unwrap())
		});
		assert_eq!(accepted, 1);
		assert_eq!(store.usage(&policy).unwrap().retained, 800);
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
			policy.admission = Admission::StopBefore;
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

		let mut policy = policy(900);
		policy.admission = Admission::StopBefore;
		let room = store.room_for(600, &policy).unwrap();

		assert_eq!(room, Room::NoRoom(NoRoom::Policy));
		assert!(store.load(&existing.id).unwrap().is_some(), "nothing was dropped");
	}

	#[test]
	fn stop_before_holds_a_file_larger_than_the_goal_and_evicts_nothing() {
		let state = state();
		let store = Store::open(state.path());
		let huge = state.path().join("huge.heic");
		fs::write(&huge, vec![b'x'; 2_000]).unwrap();
		let existing = store.save(sized(1, state.path(), 100)).unwrap();

		let mut policy = policy(1_000);
		policy.admission = Admission::StopBefore;
		let room = store.make_room(&huge, &policy).unwrap();

		assert_eq!(room, Room::NoRoom(NoRoom::TooLarge));
		assert!(
			store.load(&existing.id).unwrap().is_some(),
			"nothing is evicted for a record that will not exist"
		);
	}

	#[test]
	fn a_leased_expired_record_does_not_reject_an_unrelated_backup() {
		let state = state();
		let store = Store::open(state.path());
		let mut old = store.save(sized(1, state.path(), 400)).unwrap();
		backdate(&store, &mut old, 8);
		let _lease = store.lease(&old.id).unwrap();

		let saved = store
			.save_with_policy(sized(2, state.path(), 100), &policy(500))
			.unwrap()
			.unwrap();

		assert!(store.source(&saved.id).unwrap().is_some());
		assert!(
			store.load(&old.id).unwrap().is_some(),
			"leased recovery must remain untouched"
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
