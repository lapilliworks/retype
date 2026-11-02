//! What a conversion is, and what happened to it.
//!
//! One record is a full snapshot of one job at one transition, `docs/dev/architecture/state.md`
//! §3, so replay is last wins per id and no record depends on another. The file
//! that stores them is `journal.rs`; this module owns the vocabulary.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::identifier::normalize;

/// Schema identifier written on every record.
pub const JOURNAL_VERSION: &str = "retype-journal-1";

const CROCKFORD: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
const RANDOM_BITS: u32 = 80;
const RANDOM_MASK: u128 = (1 << RANDOM_BITS) - 1;
const ID_LENGTH: usize = 26;

/// Milliseconds since the Unix epoch, or zero if the clock is before it.
pub fn now_ms() -> i64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|elapsed| elapsed.as_millis() as i64)
		.unwrap_or_default()
}

/// A ULID: 48 bits of milliseconds then 80 bits of randomness, in Crockford
/// base32. Lexicographic order is creation order, and no registry is needed to
/// keep ids unique, `docs/dev/architecture/state.md` §2. The form is lowercase, which is what a
/// person types and what sorting and `grep` agree on.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct JobId(String);

impl JobId {
	pub fn now() -> Result<Self> {
		let timestamp = now_ms().max(0) as u64;
		let random = next_random(timestamp)?;
		Ok(Self(encode(timestamp, random)))
	}

	pub fn parse(raw: &str) -> Result<Self> {
		raw.parse()
	}

	/// The shortest handle a person may type, `docs/dev/architecture/state.md` §2.
	pub const PREFIX_MIN: usize = 4;

	/// Whether `raw` is a handle a listing could have printed: long enough, and
	/// made only of Crockford characters.
	pub fn valid_prefix(raw: &str) -> bool {
		let raw = normalize(raw);
		raw.len() >= Self::PREFIX_MIN && raw.bytes().all(|byte| decode(byte).is_some())
	}

	/// Whether this id starts with a handle. The comparison is case-insensitive
	/// because a handle may be typed in either case.
	pub fn starts_with(&self, prefix: &str) -> bool {
		self.0.starts_with(&normalize(prefix))
	}

	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl FromStr for JobId {
	type Err = Error;

	fn from_str(raw: &str) -> Result<Self> {
		let raw = normalize(raw);
		if raw.len() != ID_LENGTH {
			return Err(Error::JobId(raw));
		}

		let mut value: u128 = 0;
		for (index, byte) in raw.bytes().enumerate() {
			let digit = decode(byte).ok_or_else(|| Error::JobId(raw.clone()))?;
			// The top two bits of a 130-bit encoding must be zero, so the first
			// character can only carry the low three of its five bits.
			if index == 0 && digit > 7 {
				return Err(Error::JobId(raw.clone()));
			}
			value = (value << 5) | u128::from(digit);
		}

		Ok(Self(raw))
	}
}

impl fmt::Display for JobId {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(&self.0)
	}
}

impl fmt::Debug for JobId {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(&self.0)
	}
}

impl TryFrom<String> for JobId {
	type Error = Error;

	fn try_from(raw: String) -> Result<Self> {
		Self::from_str(&raw)
	}
}

impl From<JobId> for String {
	fn from(id: JobId) -> Self {
		id.0
	}
}

static LAST: Mutex<Option<(u64, u128)>> = Mutex::new(None);

/// Randomness for this millisecond, one greater than the previous draw so that
/// two jobs created in the same millisecond still sort in creation order.
fn next_random(timestamp: u64) -> Result<u128> {
	let mut last = LAST
		.lock()
		.map_err(|_| Error::Random("id counter is poisoned".to_string()))?;

	let random = match *last {
		Some((previous, value)) if previous == timestamp => value.wrapping_add(1) & RANDOM_MASK,
		_ => {
			let mut bytes = [0u8; 10];
			getrandom::fill(&mut bytes).map_err(|error| Error::Random(error.to_string()))?;
			let mut buffer = [0u8; 16];
			buffer[6..].copy_from_slice(&bytes);
			u128::from_be_bytes(buffer)
		}
	};

	*last = Some((timestamp, random));
	Ok(random)
}

fn encode(timestamp: u64, random: u128) -> String {
	let value = (u128::from(timestamp) << RANDOM_BITS) | (random & RANDOM_MASK);
	let mut out = [0u8; ID_LENGTH];

	for (index, slot) in out.iter_mut().enumerate() {
		let shift = 5 * (ID_LENGTH - 1 - index);
		*slot = CROCKFORD[((value >> shift) & 0x1f) as usize];
	}

	String::from_utf8(out.to_vec()).expect("the Crockford alphabet is ASCII")
}

fn decode(byte: u8) -> Option<u8> {
	let byte = byte.to_ascii_lowercase();
	CROCKFORD
		.iter()
		.position(|candidate| *candidate == byte)
		.map(|index| index as u8)
}

/// A named engine and the candidate that answered for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Engine {
	pub name: String,
	pub candidate: String,
}

/// Where the job's input came from, as recorded at creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
	pub path: PathBuf,
	pub format: String,
	pub size: u64,
	pub mtime_ns: i64,
}

/// One output of a job, and how far it got.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Output {
	pub path: PathBuf,
	pub format: String,
	pub state: OutputState,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub bytes: Option<u64>,
	pub tier: Tier,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub identity: Option<FileIdentity>,
	/// The tail of what the tool printed. The full output is in
	/// `<state>/logs/<id>.log`, which is where a long failure gets read.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub stderr: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub error: Option<JobError>,
}

impl Output {
	pub fn pending(path: PathBuf, format: impl Into<String>, tier: Tier) -> Self {
		Self {
			path,
			format: format.into(),
			state: OutputState::Pending,
			bytes: None,
			tier,
			identity: None,
			stderr: None,
			error: None,
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutputState {
	Pending,
	Running,
	Done,
	Failed,
	/// Not attempted because the job was replaced, `docs/dev/architecture/state.md` §5.3.
	Skipped,
}

/// Fidelity of an output, `docs/fidelity.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
	Exact,
	Converted,
	Delegated,
}

/// Enough of a file's identity to recognise it after a rename.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum FileIdentity {
	Unix {
		dev: u64,
		ino: u64,
		#[serde(skip_serializing_if = "Option::is_none")]
		birth_ns: Option<i64>,
	},
	Windows {
		volume: u64,
		/// Hex, because a Windows file id is 128 bits and JSON numbers are not.
		file: String,
	},
}
impl FileIdentity {
	/// The identity of a file on disk, when the platform reports one. Recorded with
	/// an output so a resume can tell an existing output from a different file that
	/// happens to have the same name.
	pub fn of(path: &std::path::Path) -> Option<Self> {
		#[cfg(unix)]
		{
			use std::os::unix::fs::MetadataExt;

			let metadata = std::fs::metadata(path).ok()?;

			Some(Self::Unix {
				dev: metadata.dev(),
				ino: metadata.ino(),
				birth_ns: None,
			})
		}

		// The stable metadata API does not expose a Windows file id, so it comes
		// from a handle, the same source the watcher uses to correlate a rename.
		#[cfg(windows)]
		{
			let (volume, file, wide) = windows_file_id(path)?;
			let width = if wide { 32 } else { 16 };

			Some(Self::Windows {
				volume,
				file: format!("{file:0width$x}"),
			})
		}

		#[cfg(not(any(unix, windows)))]
		{
			let _ = path;
			None
		}
	}

	/// Whether a path still carries this identity. Used by resume to skip an output
	/// that is already what the job produced.
	pub fn matches(&self, other: &Self) -> bool {
		match (self, other) {
			(
				Self::Unix { dev, ino, .. },
				Self::Unix {
					dev: other_dev,
					ino: other_ino,
					..
				},
			) => dev == other_dev && ino == other_ino,
			(
				Self::Windows { volume, file },
				Self::Windows {
					volume: other_volume,
					file: other_file,
				},
			) => volume == other_volume && file == other_file,
			_ => false,
		}
	}
}

/// The volume and file id Windows reports for `path`, read from a handle.
///
/// `FileIdInfo` carries the 128-bit id ReFS uses; the older
/// `BY_HANDLE_FILE_INFORMATION` is the fallback for filesystems that do not
/// answer it, with the 64-bit file index in the low half. `wide` says which,
/// because the two are written in different widths.
#[cfg(windows)]
pub(crate) fn windows_file_id(path: &std::path::Path) -> Option<(u64, u128, bool)> {
	let info = windows_file_info(path)?;
	Some((info.volume, info.file, info.wide))
}

/// Identity and link count must describe the same open file, not two pathname lookups.
#[cfg(windows)]
pub(crate) struct WindowsFileInfo {
	pub volume: u64,
	pub file: u128,
	pub wide: bool,
	pub links: u64,
}

#[cfg(windows)]
pub(crate) fn windows_file_info(path: &std::path::Path) -> Option<WindowsFileInfo> {
	use std::ffi::c_void;
	use std::mem::size_of;
	use std::os::windows::fs::OpenOptionsExt;
	use std::os::windows::io::AsRawHandle;

	use windows_sys::Win32::Foundation::HANDLE;
	use windows_sys::Win32::Storage::FileSystem::{
		BY_HANDLE_FILE_INFORMATION, FILE_ID_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo,
		GetFileInformationByHandle, GetFileInformationByHandleEx,
	};

	let file = std::fs::OpenOptions::new()
		.read(true)
		.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
		.open(path)
		.ok()?;
	let handle = file.as_raw_handle() as HANDLE;

	let mut basic: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
	if unsafe { GetFileInformationByHandle(handle, &mut basic) } == 0 {
		return None;
	}
	let links = u64::from(basic.nNumberOfLinks);
	let mut id: FILE_ID_INFO = unsafe { std::mem::zeroed() };
	let asked = unsafe {
		GetFileInformationByHandleEx(
			handle,
			FileIdInfo,
			std::ptr::from_mut(&mut id).cast::<c_void>(),
			size_of::<FILE_ID_INFO>() as u32,
		)
	};

	if asked != 0 {
		return Some(WindowsFileInfo {
			volume: id.VolumeSerialNumber,
			file: u128::from_be_bytes(id.FileId.Identifier),
			wide: true,
			links,
		});
	}

	let file = (u64::from(basic.nFileIndexHigh) << 32) | u64::from(basic.nFileIndexLow);
	Some(WindowsFileInfo {
		volume: u64::from(basic.dwVolumeSerialNumber),
		file: u128::from(file),
		wide: false,
		links,
	})
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobError {
	pub code: String,
	pub message: String,
}

impl JobError {
	pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
		Self {
			code: code.into(),
			message: message.into(),
		}
	}
}

/// A job that is parked rather than running, `docs/deferral.md` §2.
///
/// Absent from every record for a job that was never deferred or repaired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parked {
	/// One of `ask`, `cost`, `intent`, `inapplicable`, `backend`.
	pub reason: String,
	/// Unix milliseconds at which a `pending` job runs anyway.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub deadline: Option<i64>,
	/// Shared by the jobs parked in one daemon tick, so one notification can answer
	/// for all of them.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub batch: Option<String>,
	/// What the job was expected to cost, when the estimator had an answer.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub estimate: Option<Cost>,
	/// The intent exactly as the new name carried it, when the canonical form differs.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub requested: Option<String>,
	/// The intent that would run after the tokens that could not be carried are
	/// dropped, when the target format survives that.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub corrected: Option<String>,
	/// The tokens a repair dropped, in written order.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub dropped: Vec<String>,
	/// Whether the job removes its source when it commits, which is true for a job a
	/// rename asked for and false for one a client submitted.
	#[serde(default)]
	pub consume: bool,
}

/// What a job is expected to cost, `docs/dev/architecture/state.md` §7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cost {
	pub ms: u64,
	pub bytes: u64,
}

/// One job, at one moment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
	pub id: JobId,
	pub created: i64,
	pub updated: i64,
	pub source: Source,
	/// Canonical intent string, or none for a plain conversion.
	pub intent: Option<String>,
	pub outputs: Vec<Output>,
	pub state: JobState,
	pub backend: Option<String>,
	/// The engine that resolved for the backend, as `(name, candidate)`: the same
	/// intent with a different engine produces a different file, so the record
	/// has to say which one ran, `docs/config.md` §3.1.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub engine: Option<Engine>,
	pub machine: String,
	pub error: Option<JobError>,
	/// Present when the job is parked or was repaired, `docs/deferral.md` §11.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub parked: Option<Parked>,
	/// Whether the job removes its source when it commits: true for a rename,
	/// false for a client submission, `docs/dev/architecture/state.md` §5.
	#[serde(default)]
	pub consume_source: bool,
}

impl Job {
	/// A job that exists but has not been queued. The id is generated here so a
	/// deferred job is addressable before anything runs.
	pub fn new(
		source: Source,
		intent: Option<String>,
		outputs: Vec<Output>,
		machine: impl Into<String>,
	) -> Result<Self> {
		let now = now_ms();

		Ok(Self {
			id: JobId::now()?,
			created: now,
			updated: now,
			source,
			intent,
			outputs,
			state: JobState::Pending,
			backend: None,
			engine: None,
			machine: machine.into(),
			error: None,
			parked: None,
			consume_source: false,
		})
	}

	pub fn set_state(&mut self, state: JobState) {
		self.state = state;
		self.updated = now_ms();
	}

	pub fn fail(&mut self, error: JobError) {
		self.error = Some(error);
		self.set_state(JobState::Failed);
	}

	/// Whether no further transition will arrive, which is what makes a record
	/// safe to compact away once a later one exists.
	pub fn is_finished(&self) -> bool {
		matches!(
			self.state,
			JobState::Done | JobState::Partial | JobState::Failed | JobState::Cancelled | JobState::Superseded
		)
	}

	/// The journal line for this snapshot, `docs/dev/architecture/state.md` §3.
	pub fn to_line(&self) -> Result<String> {
		serde_json::to_string(&Record {
			v: JOURNAL_VERSION,
			kind: "job",
			job: self,
		})
		.map_err(|error| Error::Json(error.to_string()))
	}

	/// The same snapshot as an object, for a client asking over IPC.
	///
	/// It carries `v` and `kind` so a record read from the daemon is identified
	/// the same way as one read from the journal.
	pub fn to_record(&self) -> Result<serde_json::Value> {
		serde_json::to_value(Record {
			v: JOURNAL_VERSION,
			kind: "job",
			job: self,
		})
		.map_err(|error| Error::Json(error.to_string()))
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JobState {
	/// Parked by deferral: the job could run, and is waiting for its timer or for a
	/// person, `docs/deferral.md`.
	Pending,
	/// Submitted, or requeued at startup, and not started yet, `docs/dev/architecture/state.md` §6.
	Queued,
	Running,
	Done,
	Partial,
	Failed,
	Cancelled,
	Superseded,
	NeedsAttention,
}

#[derive(Serialize)]
struct Record<'a> {
	v: &'static str,
	kind: &'static str,
	#[serde(flatten)]
	job: &'a Job,
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::Value;

	fn source() -> Source {
		Source {
			path: PathBuf::from("/home/u/Pictures/a.heic"),
			format: "heic".to_string(),
			size: 2_431_002,
			mtime_ns: 1_690_000_000_000,
		}
	}

	fn job() -> Job {
		let outputs = vec![
			{
				let mut output = Output::pending(PathBuf::from("/home/u/Pictures/a.jpg"), "jpg", Tier::Converted);
				output.state = OutputState::Done;
				output.bytes = Some(812_345);
				output.identity = Some(FileIdentity::Unix {
					dev: 2049,
					ino: 5_243_121,
					birth_ns: Some(1_690_000_000_000),
				});
				output
			},
			{
				let mut output = Output::pending(PathBuf::from("/home/u/Pictures/a.png"), "png", Tier::Converted);
				output.state = OutputState::Failed;
				output.error = Some(JobError::new("E_TOOL_FAILED", "converter exited 1"));
				output
			},
		];

		let mut job = Job::new(source(), Some("jpg+quality85".to_string()), outputs, "7f3c1a2b").unwrap();
		job.backend = Some("magick".to_string());
		job.set_state(JobState::Partial);
		job
	}

	#[test]
	fn ids_are_crockford_and_sort_by_creation() {
		let ids: Vec<JobId> = (0..1_000).map(|_| JobId::now().unwrap()).collect();

		for id in &ids {
			assert_eq!(id.as_str().len(), 26);
			assert!(id.as_str().bytes().all(|byte| decode(byte).is_some()), "{id}");
		}
		for pair in ids.windows(2) {
			assert!(pair[0] < pair[1], "{} then {}", pair[0], pair[1]);
		}
	}

	#[test]
	fn ids_order_by_timestamp_first() {
		assert!(encode(1, u128::MAX) < encode(2, 0));
	}

	#[test]
	fn ids_are_lowercase_and_parse_in_either_case() {
		let id = JobId::now().unwrap();
		let uppercased = id.as_str().to_ascii_uppercase();
		let lowercase = id.as_str().to_ascii_lowercase();

		assert_eq!(id.as_str(), lowercase, "the canonical form is lowercase");
		assert_eq!(JobId::parse(&uppercased).unwrap(), id);
		assert_eq!(JobId::parse(&lowercase).unwrap(), id);
		assert_eq!(JobId::parse(&uppercased).unwrap().as_str(), lowercase);
	}

	#[test]
	fn malformed_ids_are_refused() {
		for raw in [
			"",
			"01J8Z",
			"01J8ZXXXXXXXXXXXXXXXXXXXX",
			"01J8ZXXXXXXXXXXXXXXXXXXXXXX",
			"01J8ZUXXXXXXXXXXXXXXXXXXXX",
			"Z1J8ZXXXXXXXXXXXXXXXXXXXX",
		] {
			assert!(JobId::parse(raw).is_err(), "{raw} parsed");
		}
	}

	#[test]
	fn the_record_matches_the_documented_shape() {
		let line = job().to_line().unwrap();
		let value: Value = serde_json::from_str(&line).unwrap();

		assert_eq!(value["v"], JOURNAL_VERSION);
		assert_eq!(value["kind"], "job");
		assert_eq!(value["state"], "partial");
		assert_eq!(value["intent"], "jpg+quality85");
		assert_eq!(value["backend"], "magick");
		assert_eq!(value["machine"], "7f3c1a2b");
		assert_eq!(value["source"]["mtime_ns"], 1_690_000_000_000_i64);
		assert_eq!(value["outputs"][0]["state"], "done");
		assert_eq!(value["outputs"][0]["tier"], "converted");
		assert_eq!(value["outputs"][0]["identity"]["kind"], "unix");
		assert_eq!(value["outputs"][1]["error"]["code"], "E_TOOL_FAILED");
		assert!(
			value["outputs"][1].get("bytes").is_none(),
			"a failed output has no size"
		);
	}

	#[test]
	fn states_are_kebab_case() {
		for (state, expected) in [
			(JobState::Pending, "pending"),
			(JobState::NeedsAttention, "needs-attention"),
			(JobState::Superseded, "superseded"),
		] {
			assert_eq!(
				serde_json::to_value(state).unwrap(),
				Value::String(expected.to_string())
			);
		}
	}

	#[test]
	fn a_parked_job_carries_why_and_until_when() {
		let mut job = job();
		job.state = JobState::Pending;
		job.parked = Some(Parked {
			reason: "cost".to_string(),
			deadline: Some(1_793_380_889_377),
			batch: Some(format!("01J{:023}", 9)),
			estimate: Some(Cost {
				ms: 2_400,
				bytes: 1_200_000,
			}),
			requested: None,
			corrected: None,
			dropped: Vec::new(),
			consume: true,
		});

		let line = job.to_line().expect("a record");

		assert!(line.contains("\"reason\":\"cost\""), "{line}");
		assert!(line.contains("\"deadline\":1793380889377"), "{line}");
		assert!(line.contains("\"estimate\":{\"ms\":2400,\"bytes\":1200000}"), "{line}");

		// The line is the record, and reading it back gives the same job.
		let read: Job = serde_json::from_str(&line).expect("a job");
		assert_eq!(read, job);
	}

	#[test]
	fn a_job_that_was_never_parked_has_no_parked_object() {
		let line = job().to_line().expect("a record");

		assert!(!line.contains("\"parked\""), "{line}");

		// And a record written before the field existed still reads.
		let old = r#"{"id":"01J00000000000000000000001","created":0,"updated":0,
			"source":{"path":"a.heic","format":"heic","size":0,"mtime_ns":0},
			"intent":"jpg","outputs":[],"state":"done","backend":null,"machine":"m","error":null}"#;
		let read: Job = serde_json::from_str(old).expect("a job from an older journal");
		assert!(read.parked.is_none());
	}

	#[test]
	fn a_finished_job_says_so() {
		let mut job = job();
		job.set_state(JobState::Running);

		assert!(!job.is_finished());
		job.set_state(JobState::Done);
		assert!(job.is_finished());
	}

	#[test]
	fn the_machine_identity_is_not_part_of_the_id() {
		let first = job();
		let second = job();

		assert_ne!(first.id, second.id);
		assert_eq!(first.machine, second.machine);
	}
}

#[cfg(test)]
mod handle_tests {
	use super::JobId;

	#[test]
	fn a_handle_is_at_least_four_crockford_characters() {
		assert!(JobId::valid_prefix("01m6"));
		assert!(JobId::valid_prefix("01M6"));
		assert!(!JobId::valid_prefix(""));
		assert!(!JobId::valid_prefix("01m"));
		assert!(!JobId::valid_prefix("01m!"));
	}

	#[test]
	fn a_handle_is_matched_in_either_case() {
		let id = JobId::parse("01m6b4rararx3wrttkrgpp1783").unwrap();

		assert!(id.starts_with("01M6"));
		assert!(!id.starts_with("01m7"));
	}
}
