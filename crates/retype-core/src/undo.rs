//! Undo records, `docs/state.md` §5.
//!
//! One directory per job under `undo/`, holding the source bytes and a manifest.
//! The record is the durable truth for recoverability: the journal is a cache of
//! history and progress, so losing it costs listing and never undo.
//!
//! The bytes are a hard link when the store shares a filesystem with the source and
//! a copy otherwise, and the manifest says which, because a restore has to know
//! whether moving the file is enough or whether the mode and mtime have to be put
//! back by hand.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Code, Diagnostic, Error, Result};
use crate::job::JobId;

/// Schema identifier, `docs/state.md` §5.
pub const UNDO_VERSION: &str = "retype-undo-1";

/// One produced file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Output {
	pub path: PathBuf,
	pub ext: String,
}

/// How the source bytes are held.
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

	/// Keep the source bytes and a manifest before converting, `docs/state.md` §5.
	///
	/// `bytes` is the file as it is now: after a rename that is the source content
	/// under the target's name, which is exactly what an undo has to put back.
	pub fn save(&self, subject: Subject) -> Result<Record> {
		let directory = self.directory(&subject.id);
		fs::create_dir_all(&directory)?;
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
		record.source_size = metadata.len();
		record.source_mode = mode_of(&metadata);
		record.source_mtime_ns = mtime_ns(&metadata);

		let held = directory.join(record.source_file());

		// A hard link costs nothing and survives the original name going away,
		// which is the normal case for a rename. A copy is the fallback for a store
		// on another filesystem.
		record.link = match fs::hard_link(&subject.bytes, &held) {
			Ok(()) => Link::Hard,
			Err(_) => {
				fs::copy(&subject.bytes, &held)?;
				Link::Copy
			}
		};

		write_manifest(&directory, &record)?;
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

	/// Put the source back and remove what the job produced, `docs/state.md` §5.
	pub fn restore(&self, id: &JobId) -> Result<Record> {
		let Some(record) = self.load(id)? else {
			return Err(Error::Diagnostic(Diagnostic::global(
				Code::JobState,
				format!("{id} has no undo record"),
			)));
		};

		for output in &record.outputs {
			match fs::remove_file(&output.path) {
				Ok(()) => {}
				Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
				Err(error) => return Err(Error::Io(error)),
			}
		}

		let held = self.directory(id).join(record.source_file());

		if record.source_path.exists() {
			return Err(Error::Diagnostic(Diagnostic::global(
				Code::IoExists,
				format!(
					"{} is occupied, so the source cannot be put back",
					record.source_path.display()
				),
			)));
		}

		// A rename is enough when the store shares a filesystem; otherwise the copy
		// is carried back and the original removed.
		if fs::rename(&held, &record.source_path).is_err() {
			fs::copy(&held, &record.source_path)?;
			fs::remove_file(&held)?;
		}

		restore_times(&record)?;
		self.remove(id)?;

		Ok(record)
	}

	/// Forget a record, which is what a successful undo or a finished resume does.
	pub fn remove(&self, id: &JobId) -> Result<()> {
		let directory = self.directory(id);

		match fs::remove_dir_all(&directory) {
			Ok(()) => Ok(()),
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
			Err(error) => Err(Error::Io(error)),
		}
	}

	/// The bytes held for a job, when a source has to be read again: resume needs
	/// this after a rename has taken the original name away.
	pub fn source(&self, id: &JobId) -> Result<Option<PathBuf>> {
		let Some(record) = self.load(id)? else {
			return Ok(None);
		};

		let path = self.directory(id).join(record.source_file());
		Ok(path.exists().then_some(path))
	}
}

fn write_manifest(directory: &Path, record: &Record) -> Result<()> {
	let text = toml::to_string(record).map_err(|error| {
		Error::Diagnostic(Diagnostic::global(
			Code::InternalInvariant,
			format!("an undo record does not serialize: {error}"),
		))
	})?;

	let path = directory.join("meta.toml");
	fs::write(&path, text)?;
	set_mode(&path, 0o600)?;
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
				outputs: vec![Output {
					path: output.clone(),
					ext: "jpg".to_string(),
				}],
			})
			.unwrap();

		// A rename leaves the bytes at the target and the source name gone.
		fs::remove_file(&source).unwrap();

		let restored = store.restore(&id(2)).expect("restores");

		assert_eq!(restored.id, id(2));
		assert_eq!(fs::read(&source).unwrap(), b"heic bytes");
		assert!(!output.exists(), "the output is removed");
		assert!(store.load(&id(2)).unwrap().is_none(), "the record is gone");
	}

	#[test]
	fn restoring_into_an_occupied_path_is_refused() {
		let state = state();
		let source = source(state.path());
		let store = Store::open(state.path());

		store
			.save(Subject {
				id: id(3),
				intent: "jpg".to_string(),
				machine: "machine".to_string(),
				bytes: source.clone(),
				source_path: source.clone(),
				source_ext: "heic".to_string(),
				outputs: Vec::new(),
			})
			.unwrap();

		let error = store.restore(&id(3)).unwrap_err();

		assert_eq!(error.code(), Code::IoExists);
		assert_eq!(fs::read(&source).unwrap(), b"heic bytes", "nothing was lost");
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
				outputs: Vec::new(),
			})
			.unwrap();

		fs::remove_file(&source).unwrap();
		store.restore(&id(8)).unwrap();

		assert_eq!(fs::metadata(&source).unwrap().permissions().mode() & 0o777, 0o640);
	}
}
