//! What a conversion is, and what happened to it.
//!
//! One record is a full snapshot of one job at one transition, `docs/state.md`
//! §3, so replay is last wins per id and no record depends on another. The file
//! that stores them is `journal.rs`; this module owns the vocabulary.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

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
/// keep ids unique, `docs/state.md` §2. The form is lowercase, which is what a
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

	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl FromStr for JobId {
	type Err = Error;

	fn from_str(raw: &str) -> Result<Self> {
		if raw.len() != ID_LENGTH {
			return Err(Error::JobId(raw.to_string()));
		}

		let mut value: u128 = 0;
		for (index, byte) in raw.bytes().enumerate() {
			let digit = decode(byte).ok_or_else(|| Error::JobId(raw.to_string()))?;
			// The top two bits of a 130-bit encoding must be zero, so the first
			// character can only carry the low three of its five bits.
			if index == 0 && digit > 7 {
				return Err(Error::JobId(raw.to_string()));
			}
			value = (value << 5) | u128::from(digit);
		}

		Ok(Self(raw.to_ascii_lowercase()))
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
	/// Not attempted because the job was replaced, `docs/state.md` §5.3.
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
		let metadata = std::fs::metadata(path).ok()?;

		#[cfg(unix)]
		{
			use std::os::unix::fs::MetadataExt;

			Some(Self::Unix {
				dev: metadata.dev(),
				ino: metadata.ino(),
				birth_ns: None,
			})
		}

		// A Windows file id comes from a handle, and the stable metadata API does not
		// expose one. Identity-based skipping is therefore a Unix answer today; a
		// resume on Windows runs the outputs again rather than guessing that a file
		// with the right size and name is the one it produced.
		#[cfg(not(unix))]
		{
			let _ = metadata;
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

	/// The journal line for this snapshot, `docs/state.md` §3.
	pub fn to_line(&self) -> Result<String> {
		serde_json::to_string(&Record {
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
	Pending,
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
