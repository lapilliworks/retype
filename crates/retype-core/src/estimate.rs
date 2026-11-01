//! The estimator and its calibration and `docs/state.md` §7.
//!
//! A conversion costs time and produces bytes, and both are predictable enough to
//! be worth predicting: the deferral threshold asks whether a job is expensive,
//! progress needs a total, and the editor shows what a request will produce before
//! the user commits to it.
//!
//! A measurement is keyed by the pair it was taken on: one source format, one
//! target format, one backend. The pair is the unit because the two numbers a user
//! cares about are properties of it: HEIC decodes slowly and JPEG encodes
//! quickly, so `heic` to `jpg` and `png` to `jpg` are not the same job, and no
//! single per-format figure describes both. A pair nothing has measured falls back
//! to a family constant, which is honest about being a first-order guess.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Code, Diagnostic, Error, Result};

/// Schema identifier, `docs/state.md` §7.
pub const BENCH_VERSION: &str = "retype-bench-1";

/// One measured conversion, and what it cost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Measurement {
	pub source: String,
	pub target: String,
	pub backend: String,
	/// The family the pair belongs to, which is what an unmeasured pair falls back
	/// to when this record is the nearest evidence there is.
	pub family: String,
	/// Source bytes the machine converts per second on this pair.
	pub bytes_per_second: f64,
	/// Output bytes per source byte, at the dials the sample used.
	pub size_ratio: f64,
	/// How large the sample was, so a reader can tell what the rate was taken on.
	pub sample_bytes: u64,
	/// The tool's own version string, because a rate is evidence about a build.
	#[serde(default)]
	pub tool_version: String,
	/// Unix milliseconds, `docs/state.md` §3.
	#[serde(default)]
	pub measured: i64,
}

impl Measurement {
	/// The estimate this measurement implies for a file of `source_bytes`.
	fn estimate(&self, source_bytes: u64, basis: Basis) -> Estimate {
		Estimate {
			time: Duration::from_secs_f64(source_bytes as f64 / self.bytes_per_second.max(1.0)),
			bytes: (source_bytes as f64 * self.size_ratio.max(0.0)).round() as u64,
			basis,
		}
	}
}

/// What one conversion is expected to cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Estimate {
	pub time: Duration,
	pub bytes: u64,
	pub basis: Basis,
}

/// Where an estimate came from, which decides how much it is worth telling the
/// user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
	/// Measured on this machine, for this pair and backend.
	Measured,
	/// A family constant, which is a guess with a provenance.
	Family,
}

impl Basis {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Measured => "measured",
			Self::Family => "family",
		}
	}
}

/// What is being asked about: the pair, the backend, and how much there is to
/// convert.
pub struct Query<'a> {
	pub source: &'a str,
	pub target: &'a str,
	pub backend: &'a str,
	pub family: &'a str,
	pub bytes: u64,
}

/// `bench.toml`: the per-install identity and every measurement taken on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Calibration {
	#[serde(default = "version")]
	pub version: String,
	#[serde(default)]
	pub machine: String,
	#[serde(default)]
	pub throughput: Vec<Measurement>,
}

fn version() -> String {
	BENCH_VERSION.to_string()
}

impl Default for Calibration {
	fn default() -> Self {
		Self {
			version: version(),
			machine: String::new(),
			throughput: Vec::new(),
		}
	}
}

impl Calibration {
	pub fn path(state_dir: &Path) -> PathBuf {
		state_dir.join("bench.toml")
	}

	/// Read the calibration, or an empty one when nothing has been measured yet.
	pub fn open(state_dir: &Path) -> Result<Self> {
		let path = Self::path(state_dir);

		let text = match fs::read_to_string(&path) {
			Ok(text) => text,
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
			Err(error) => return Err(Error::Io(error)),
		};

		let calibration: Self = toml::from_str(&text).map_err(|error| {
			Error::Diagnostic(Diagnostic::global(
				Code::CfgParse,
				format!("{}: {error}", path.display()),
			))
		})?;

		if calibration.version != BENCH_VERSION {
			return Err(Error::Diagnostic(Diagnostic::global(
				Code::CfgVersion,
				format!(
					"{} declares '{}', this build reads '{BENCH_VERSION}'",
					path.display(),
					calibration.version
				),
			)));
		}

		Ok(calibration)
	}

	pub fn save(&self, state_dir: &Path) -> Result<()> {
		let text = toml::to_string(self).map_err(|error| {
			Error::Diagnostic(Diagnostic::global(
				Code::InternalInvariant,
				format!("the calibration does not serialize: {error}"),
			))
		})?;

		let path = Self::path(state_dir);
		let temporary = path.with_extension("toml.tmp");

		fs::write(&temporary, text)?;
		set_private(&temporary)?;

		// A reader sees the old file or the complete new one, never a half-written
		// calibration while a parked job is asking for an estimate.
		if let Err(error) = fs::rename(&temporary, &path) {
			if path.exists() {
				fs::remove_file(&path)?;
				fs::rename(&temporary, &path)?;
			} else {
				let _ = fs::remove_file(&temporary);
				return Err(error.into());
			}
		}

		Ok(())
	}

	/// Record a measurement, replacing what the same pair and backend measured
	/// before: a second run on the same machine supersedes the first rather than
	/// leaving two answers for one question.
	pub fn measure(&mut self, measurement: Measurement) {
		let same = |existing: &Measurement| {
			existing.source == measurement.source
				&& existing.target == measurement.target
				&& existing.backend == measurement.backend
		};

		match self.throughput.iter_mut().find(|existing| same(existing)) {
			Some(existing) => *existing = measurement,
			None => self.throughput.push(measurement),
		}
	}

	/// What this pair is expected to cost, or nothing when there is neither a
	/// measurement nor a constant for its family.
	pub fn estimate(&self, query: &Query<'_>) -> Option<Estimate> {
		if let Some(measured) = self.throughput.iter().find(|measurement| {
			measurement.source == query.source
				&& measurement.target == query.target
				&& measurement.backend == query.backend
		}) {
			return Some(measured.estimate(query.bytes, Basis::Measured));
		}

		constant(query.family).map(|constant| constant.estimate(query))
	}
}

/// The per-install identity, created on first use and kept, `docs/state.md` §2.
///
/// It lives in `bench.toml` because the two travel together: calibration earned on
/// one machine is not calibration for another, so a file with no machine would be
/// evidence with no subject.
pub fn machine_identity(state_dir: &Path) -> Result<String> {
	let mut calibration = Calibration::open(state_dir)?;

	if !calibration.machine.is_empty() {
		return Ok(calibration.machine);
	}

	calibration.machine = random_hex()?;
	calibration.save(state_dir)?;
	Ok(calibration.machine.clone())
}

/// What a pair the machine has never measured is assumed to cost.
///
/// Two numbers per family: source bytes per second, and output bytes per source
/// byte. Both are first-order and both were taken here, on the development machine
/// on 2026-10-30, with ImageMagick 7.1.2-34, FFmpeg 9.0.2 and pandoc 3.11 through
/// typst. A rate is rounded down and a ratio up, so an estimate that is wrong errs
/// towards "slower and larger". `retype bench` replaces these per pair, which is
/// the only calibration worth trusting.
struct Constant {
	bytes_per_second: f64,
	size_ratio: f64,
}

impl Constant {
	fn estimate(&self, query: &Query<'_>) -> Estimate {
		Estimate {
			time: Duration::from_secs_f64(query.bytes as f64 / self.bytes_per_second.max(1.0)),
			bytes: (query.bytes as f64 * self.size_ratio).round() as u64,
			basis: Basis::Family,
		}
	}
}

fn constant(family: &str) -> Option<Constant> {
	match family {
		// A 2400x1600 jpg to webp: 1.44 MB in 0.37 s, 291 KB out.
		"image" => Some(Constant {
			bytes_per_second: 3_800_000.0,
			size_ratio: 0.21,
		}),
		// Five seconds of 1280x720 mp4 re-encoded to mkv: 1.95 MB in 0.68 s, 1.86 MB out.
		"video" => Some(Constant {
			bytes_per_second: 2_800_000.0,
			size_ratio: 0.96,
		}),
		// 30 s of mp3 to flac: 240 KB in 0.20 s, 3.98 MB out. The direction dominates
		// here, so the ratio only describes audio that is being unpacked.
		"audio" => Some(Constant {
			bytes_per_second: 1_200_000.0,
			size_ratio: 4.0,
		}),
		// A page of markdown to pdf: 116 B in 0.28 s, 20 KB out. Process startup
		// dominates, so bytes are a poor unit for a document and pages would be
		// better; until `bench` measures pages, this is a guess with a provenance.
		"document" => Some(Constant {
			bytes_per_second: 2_000.0,
			size_ratio: 40.0,
		}),
		_ => None,
	}
}

#[cfg(unix)]
fn set_private(path: &Path) -> Result<()> {
	use std::os::unix::fs::PermissionsExt;
	fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
	Ok(())
}

#[cfg(not(unix))]
fn set_private(_path: &Path) -> Result<()> {
	Ok(())
}

fn random_hex() -> Result<String> {
	const HEX: &[u8; 16] = b"0123456789abcdef";

	let mut bytes = [0u8; 4];
	getrandom::fill(&mut bytes).map_err(|error| Error::Random(error.to_string()))?;

	let mut identity = String::with_capacity(8);
	for byte in bytes {
		identity.push(char::from(HEX[usize::from(byte >> 4)]));
		identity.push(char::from(HEX[usize::from(byte & 0x0f)]));
	}

	Ok(identity)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn measurement(source: &str, target: &str) -> Measurement {
		Measurement {
			source: source.to_string(),
			target: target.to_string(),
			backend: "magick".to_string(),
			family: "image".to_string(),
			bytes_per_second: 1_000_000.0,
			size_ratio: 0.5,
			sample_bytes: 1_000_000,
			tool_version: "ImageMagick 7.1.2-34".to_string(),
			measured: 1_793_380_889_377,
		}
	}

	fn query<'a>(source: &'a str, target: &'a str) -> Query<'a> {
		Query {
			source,
			target,
			backend: "magick",
			family: "image",
			bytes: 2_000_000,
		}
	}

	#[test]
	fn a_measured_pair_answers_from_the_measurement() {
		let mut calibration = Calibration::default();
		calibration.measure(measurement("heic", "jpg"));

		let estimate = calibration.estimate(&query("heic", "jpg")).unwrap();

		assert_eq!(estimate.basis, Basis::Measured);
		assert_eq!(estimate.time, Duration::from_secs(2));
		assert_eq!(estimate.bytes, 1_000_000);
	}

	#[test]
	fn an_unmeasured_pair_falls_back_to_the_family_constant() {
		let calibration = Calibration::default();

		let estimate = calibration.estimate(&query("png", "webp")).unwrap();

		assert_eq!(estimate.basis, Basis::Family);
		assert!(estimate.bytes > 0);
	}

	#[test]
	fn a_family_with_no_constant_has_no_estimate() {
		let calibration = Calibration::default();
		let mut ask = query("bin", "rom");
		ask.family = "firmware";

		assert!(calibration.estimate(&ask).is_none());
	}

	#[test]
	fn a_measurement_is_matched_on_its_backend_too() {
		let mut calibration = Calibration::default();
		calibration.measure(measurement("heic", "jpg"));

		let mut ask = query("heic", "jpg");
		ask.backend = "ffmpeg";

		assert_eq!(calibration.estimate(&ask).unwrap().basis, Basis::Family);
	}

	#[test]
	fn measuring_the_same_pair_twice_replaces_the_answer() {
		let mut calibration = Calibration::default();
		calibration.measure(measurement("heic", "jpg"));

		let mut faster = measurement("heic", "jpg");
		faster.bytes_per_second = 4_000_000.0;
		calibration.measure(faster.clone());

		assert_eq!(calibration.throughput, vec![faster]);
	}

	#[test]
	fn the_calibration_round_trips_through_its_file() {
		let dir = tempfile::tempdir().unwrap();
		let mut calibration = Calibration {
			machine: "7f3c1a2b".to_string(),
			..Calibration::default()
		};
		calibration.measure(measurement("heic", "jpg"));
		calibration.save(dir.path()).unwrap();

		let read = Calibration::open(dir.path()).unwrap();

		assert_eq!(read, calibration);
	}

	#[test]
	fn a_calibration_from_another_schema_is_refused() {
		let dir = tempfile::tempdir().unwrap();
		fs::write(
			Calibration::path(dir.path()),
			"version = 'retype-bench-9'\nmachine = '7f3c1a2b'\n",
		)
		.unwrap();

		let error = Calibration::open(dir.path()).unwrap_err();

		assert_eq!(error.code(), Code::CfgVersion);
	}

	#[test]
	fn a_missing_file_is_an_empty_calibration() {
		let dir = tempfile::tempdir().unwrap();

		assert_eq!(Calibration::open(dir.path()).unwrap(), Calibration::default());
	}

	#[test]
	fn the_identity_is_created_once_and_reused() {
		let dir = tempfile::tempdir().unwrap();

		let first = machine_identity(dir.path()).unwrap();
		let second = machine_identity(dir.path()).unwrap();

		assert_eq!(first, second);
		assert_eq!(first.len(), 8);
	}

	#[cfg(unix)]
	#[test]
	fn the_identity_file_is_not_world_readable() {
		use std::os::unix::fs::PermissionsExt;

		let dir = tempfile::tempdir().unwrap();
		machine_identity(dir.path()).unwrap();
		let mode = fs::metadata(Calibration::path(dir.path()))
			.unwrap()
			.permissions()
			.mode() & 0o777;

		assert_eq!(mode, 0o600);
	}

	#[test]
	fn taking_an_identity_keeps_what_was_measured() {
		let dir = tempfile::tempdir().unwrap();
		let mut calibration = Calibration::default();
		calibration.measure(measurement("heic", "jpg"));
		calibration.save(dir.path()).unwrap();

		machine_identity(dir.path()).unwrap();
		let read = Calibration::open(dir.path()).unwrap();

		assert_eq!(read.throughput, calibration.throughput);
		assert!(!read.machine.is_empty());
	}
}
