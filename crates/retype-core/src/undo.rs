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
	/// Source-disposal eligibility, not proof of disposal. Rename jobs may retain their input.
	/// A `convert` undo only removes verified outputs and never replaces the live source.
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
	/// True for a rename-derived job, false for `retype convert`.
	pub consume_source: bool,
}

/// What to do when a record does not fit the quota.
///
/// `Ask` is what the product intends: keep the expired records, ask the user
/// whether to make room, and act on the answer. Asking needs notifications, which
/// land in, so this build reads `Ask` as `Evict` and says so once at
/// startup rather than pretending the question was asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overflow {
	/// Ask first; `Evict` until notifications exist.
	Ask,
	/// Drop the oldest records until the new one fits.
	Evict,
	/// Keep no record, and convert without one.
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
			Self::Policy => "the undo quota is full and on_overflow = skip",
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
		for id in dropped {
			let _lease = self.lease(&id)?;
			self.remove_locked(&id)?;
		}
		self.save_locked(subject).map(Some)
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
		let _guard = store_lock(&self.root)?;
		let _lease = self.lease(id)?;
		let Some(record) = self.load(id)? else {
			return Err(Error::Diagnostic(Diagnostic::global(
				Code::JobNotFound,
				format!("{id} has no undo record"),
			)));
		};

		/* Refuse changed outputs before touching anything, including the original. */
		for commit in &record.commits {
			if fs::symlink_metadata(&commit.output.path).is_ok() && !commit.fingerprint.matches(&commit.output.path)? {
				return Err(Error::Exists(commit.output.path.clone()));
			}
		}
		if record.consume_source {
			let held = self
				.source(id)?
				.ok_or_else(|| Error::Io(std::io::Error::other("no verified original to restore")))?;
			if fs::symlink_metadata(&record.source_path).is_ok() {
				let expected = record.source_fingerprint.as_ref().expect("verified record");
				if !expected.matches(&record.source_path)? {
					return Err(Error::Exists(record.source_path.clone()));
				}
			} else {
				let staged = crate::convert::StagedOutput::new(&record.source_path)?;
				crate::safety::copy_snapshot(&held, staged.temp())?;
				staged.commit()?;
			}
			restore_times(&record)?;
			crate::fsx::sync_file(&record.source_path)?;
			crate::fsx::sync_directory(record.source_path.parent().unwrap_or(Path::new(".")))?;
		}
		if !crate::fsx::supports_durable_cleanup() {
			return Err(Error::Io(std::io::Error::new(
				std::io::ErrorKind::Unsupported,
				"original restored or retained; output removal requires directory durability",
			)));
		}
		remove_outputs(&record)?;
		self.remove_locked(id)?;
		Ok(record)
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
			if !record.pinned && age(&record, now) > retention {
				let _lease = self.lease(&record.id)?;
				self.remove_locked(&record.id)?;
				dropped.push(record.id);
			}
		}

		Ok(dropped)
	}

	/// Whether a record for the bytes at `path` fits, and what had to go for it,
	///.
	///
	/// Expired records go first, then the oldest ones needed to make room, and
	/// never a record younger than the new one out of order: the store drops what a
	/// user is least likely to ask for. When the answer is that nothing fits, no
	/// record is dropped for a job that will not have one, because a conversion must
	/// never be blocked and the history is worth more than nothing.
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
		let mut kept: Vec<Record> = Vec::new();

		// Expiry is retention rather than overflow, so it applies whoever is asking.
		for record in self.list()? {
			if !record.pinned && age(&record, now) > policy.retention {
				dropped.push(record.id);
			} else {
				held += record.source_size;
				kept.push(record);
			}
		}

		if held.saturating_add(wanted) <= policy.quota {
			return Ok(Room::Kept { dropped });
		}

		// One record larger than the whole quota is not a history problem, so nothing
		// is evicted to make room for it.
		if wanted > policy.quota {
			return Ok(Room::NoRoom(NoRoom::TooLarge));
		}

		if policy.on_overflow == Overflow::Skip {
			return Ok(Room::NoRoom(NoRoom::Policy));
		}

		// `list` answers newest first, and the oldest record is the one a user is
		// least likely to undo.
		for record in kept.iter().rev().filter(|record| !record.pinned) {
			if held.saturating_add(wanted) <= policy.quota {
				break;
			}

			held -= record.source_size.min(held);
			dropped.push(record.id.clone());
		}

		if held.saturating_add(wanted) > policy.quota {
			return Ok(Room::NoRoom(NoRoom::Policy));
		}
		Ok(Room::Kept { dropped })
	}

	/// Write the output's identity before publication. Recovery verifies it at the final path.
	pub fn prepare_output(&self, id: &JobId, output: crate::job::Output, staged: &Path) -> Result<()> {
		let _guard = store_lock(&self.root)?;
		let mut record = self
			.load(id)?
			.ok_or_else(|| Error::Io(std::io::Error::other("undo record disappeared")))?;
		record.commits.retain(|commit| commit.output.path != output.path);
		record.commits.push(Commit {
			output,
			fingerprint: Fingerprint::of(staged)?,
		});
		write_manifest(&self.directory(id), &record)
	}

	/// Recheck the retained input and every published object immediately before disposal.
	pub fn verify_disposal(&self, id: &JobId) -> Result<()> {
		let record = self
			.load(id)?
			.ok_or_else(|| Error::Io(std::io::Error::other("recovery record disappeared")))?;
		if self.source(id)?.is_none() || record.outputs.is_empty() {
			return Err(Error::Io(std::io::Error::other(
				"no verified original or output inventory; source retained",
			)));
		}
		for output in &record.outputs {
			let commit = record
				.commits
				.iter()
				.find(|commit| commit.output.path == output.path)
				.ok_or_else(|| Error::Exists(output.path.clone()))?;
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

fn restore_times(record: &Record) -> Result<()> {
	set_mode(&record.source_path, record.source_mode)?;

	if record.source_mtime_ns == 0 {
		return Ok(());
	}

	let modified = UNIX_EPOCH + Duration::from_nanos(record.source_mtime_ns.max(0) as u64);
	let file = fs::OpenOptions::new().write(true).open(&record.source_path)?;
	file.set_times(fs::FileTimes::new().set_modified(modified))?;
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

/// A record's age, for retention decisions the caller owns.
pub fn age(record: &Record, now: SystemTime) -> Duration {
	let created = UNIX_EPOCH + Duration::from_millis(record.created.max(0) as u64);
	now.duration_since(created).unwrap_or_default()
}

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
	fn the_oldest_records_go_until_the_new_one_fits() {
		let state = state();
		let store = Store::open(state.path());
		let newest = state.path().join("photo.heic");
		fs::write(&newest, vec![b'x'; 400]).unwrap();

		let oldest = store.save(sized(1, state.path(), 400)).unwrap();
		store.finish(&oldest.id).unwrap();
		std::thread::sleep(Duration::from_millis(2));
		let middle = store.save(sized(2, state.path(), 400)).unwrap();

		// Two records of 400 in a quota of 900 leaves 100, and the new one needs 400,
		// so the oldest goes and the middle stays.
		let room = store.make_room(&newest, &policy(900)).unwrap();

		assert_eq!(
			room,
			Room::Kept {
				dropped: vec![oldest.id.clone()]
			}
		);
		assert!(store.load(&oldest.id).unwrap().is_none());
		assert!(store.load(&middle.id).unwrap().is_some());
	}

	#[test]
	fn asking_whether_a_record_fits_changes_nothing() {
		let state = state();
		let store = Store::open(state.path());
		let source = state.path().join("photo.heic");
		fs::write(&source, vec![b'x'; 400]).unwrap();
		let existing = store.save(sized(1, state.path(), 400)).unwrap();

		store.finish(&existing.id).unwrap();
		// One record of 400 in a quota of 900 leaves 500, and 600 does not fit, so the
		// record has to go. This call says which without removing it.
		let room = store.room_for(600, &policy(900)).unwrap();

		assert_eq!(
			room,
			Room::Kept {
				dropped: vec![existing.id.clone()]
			}
		);
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
