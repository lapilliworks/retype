//! Typed requests and answers for the daemon protocol.
//!
//! A client calls these methods rather than assembling JSON. The daemon still
//! answers with JSON on the wire; the types are the contract both sides compile
//! against. Adding a field is compatible, so every optional field has a default.

use std::path::PathBuf;

use retype_core::error::Diagnostic;
use retype_core::job::{Engine, Job, JobError, JobId, JobState, Output, Tier};
use retype_core::undo::Output as UndoOutput;
use serde::{Deserialize, Serialize};

/// One registration as `status` and `watch.list` report it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watch {
	pub path: String,
	#[serde(default)]
	pub recursive: bool,
	#[serde(default)]
	pub added: i64,
	#[serde(default)]
	pub paused: bool,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub paused_by: Option<String>,
	#[serde(default)]
	pub available: bool,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub patterns: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchList {
	#[serde(default)]
	pub watches: Vec<Watch>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchAdd {
	pub path: PathBuf,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub recursive: Option<bool>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub paused: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchRemove {
	pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchRemoved {
	pub path: PathBuf,
	pub removed: bool,
}

/// What the daemon is doing, `status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
	pub pid: u32,
	pub machine: String,
	pub uptime_ms: u64,
	#[serde(default)]
	pub jobs: usize,
	#[serde(default)]
	pub parked: usize,
	#[serde(default)]
	pub journal: String,
	#[serde(default)]
	pub dropped: u64,
	#[serde(default)]
	pub placeholders: u64,
	#[serde(default)]
	pub primed: u64,
	#[serde(default)]
	pub observed: u64,
	#[serde(default)]
	pub watches: Vec<Watch>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub endpoint: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub supervised: Option<String>,
}

/// One row of `jobs.list`. `jobs.get` remains the full record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSummary {
	pub id: JobId,
	pub state: JobState,
	pub source: PathBuf,
	#[serde(default)]
	pub created: i64,
	#[serde(default)]
	pub updated: i64,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub intent: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub parked_reason: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub error: Option<JobError>,
	#[serde(default)]
	pub outputs_done: usize,
	#[serde(default)]
	pub outputs_total: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobList {
	#[serde(default)]
	pub jobs: Vec<JobSummary>,
}

/// The journal record as `jobs.get` returns it, with its own schema marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRecord {
	pub v: String,
	pub kind: String,
	#[serde(flatten)]
	pub job: Job,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobGet {
	pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubmitMode {
	Immediate,
	Queued,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSubmit {
	pub input: PathBuf,
	pub intent: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub output: Option<PathBuf>,
	#[serde(default)]
	pub force: bool,
	#[serde(default = "SubmitMode::immediate")]
	pub mode: SubmitMode,
}

impl SubmitMode {
	fn immediate() -> Self {
		Self::Immediate
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSubmitted {
	pub id: JobId,
	pub state: JobState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobCancel {
	pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobCancelled {
	pub id: JobId,
	pub cancelled: bool,
	/// `cancelled` or `cancelling`; a running tool is stopped asynchronously.
	pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobResume {
	pub id: String,
	#[serde(default)]
	pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobResumed {
	pub id: JobId,
	pub state: JobState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResolveMode {
	Now,
	Convert,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobResolve {
	pub id: String,
	pub mode: ResolveMode,
	#[serde(default)]
	pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobResolved {
	pub id: JobId,
	pub state: JobState,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoRequest {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub id: Option<String>,
	#[serde(default)]
	pub last: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Undone {
	pub id: JobId,
	pub source: PathBuf,
	pub consume_source: bool,
	#[serde(default)]
	pub removed: Vec<PathBuf>,
}

/// A retained record as the GUI's undo list needs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoSummary {
	pub id: JobId,
	pub intent: String,
	pub created: i64,
	pub source_path: PathBuf,
	pub source_ext: String,
	pub source_size: u64,
	#[serde(default)]
	pub outputs: Vec<UndoOutput>,
	#[serde(default)]
	pub pinned: bool,
	#[serde(default)]
	pub consume_source: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoRecords {
	#[serde(default)]
	pub records: Vec<UndoSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhyRequest {
	pub path: PathBuf,
}

/// What the journal or the file says about a path, `docs/advanced/provenance.md`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhyAnswer {
	pub path: PathBuf,
	pub store: String,
	pub confidence: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub intent: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub job: Option<JobId>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub source: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub backend: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub engine: Option<Engine>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub created: Option<i64>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub machine: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub outputs: Option<Vec<Output>>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub target: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub tier: Option<Tier>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventsRequest {
	#[serde(default)]
	pub since: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobEvent {
	pub event: String,
	pub seq: u64,
	pub job: Job,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventBatch {
	#[serde(default)]
	pub events: Vec<JobEvent>,
	#[serde(default)]
	pub seq: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigReload {
	#[serde(default)]
	pub reloaded: bool,
	#[serde(default)]
	pub warnings: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShutdownRequest {
	#[serde(default)]
	pub force: bool,
	#[serde(default)]
	pub now: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShutdownResult {
	#[serde(default)]
	pub stopping: bool,
	#[serde(default)]
	pub cancelled: usize,
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	#[test]
	fn a_job_summary_is_the_shape_the_daemon_writes() {
		let summary: JobSummary = serde_json::from_value(json!({
			"id": "01m6a4rararx3wrttkrgpp1783",
			"state": "needs-attention",
			"source": "/home/u/a.heic",
			"created": 1690000000123_i64,
			"updated": 1690000000456_i64,
			"intent": "jpg",
			"parked_reason": "intent",
			"error": {"code": "E_VALUE_SYNTAX", "message": "bad value"},
			"outputs_done": 0,
			"outputs_total": 1
		}))
		.unwrap();

		assert_eq!(summary.state, JobState::NeedsAttention);
		assert_eq!(summary.source, PathBuf::from("/home/u/a.heic"));
		assert_eq!(summary.parked_reason.as_deref(), Some("intent"));
		assert_eq!(summary.outputs_total, 1);
	}

	#[test]
	fn an_undo_list_carries_the_record_fields_a_list_needs() {
		let records: UndoRecords = serde_json::from_value(json!({
			"records": [{
				"id": "01m6a4rararx3wrttkrgpp1783",
				"intent": "webp+quality80",
				"created": 1690000000123_i64,
				"source_path": "/home/u/a.heic",
				"source_ext": "heic",
				"source_size": 2431002,
				"outputs": [{"path": "/home/u/a.webp", "ext": "webp"}],
				"pinned": false,
				"consume_source": true
			}]
		}))
		.unwrap();

		assert_eq!(records.records.len(), 1);
		assert_eq!(records.records[0].outputs[0].ext, "webp");
		assert!(records.records[0].consume_source);
	}

	#[test]
	fn an_event_batch_keeps_its_sequence() {
		let batch: EventBatch = serde_json::from_value(json!({
			"events": [],
			"seq": 91
		}))
		.unwrap();

		assert_eq!(batch.seq, 91);
		assert!(batch.events.is_empty());
	}
}
