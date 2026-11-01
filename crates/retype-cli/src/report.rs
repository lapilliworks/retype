//! `retype doctor --report`.
//!
//! One file a user can read and send: the build, the machine, the configuration it
//! was set up with, the tools that answered, and enough of the journal and the logs
//! to show what happened. Nothing is uploaded and nothing is collected behind the
//! user's back; the file exists so that a bug report can be specific without asking
//! for six separate answers.
//!
//! The warning at the top is not decoration. A configuration holds absolute paths
//! and a log can hold anything a converter chose to print.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use retype_core::registry::Registry;
use retype_core::{tools, undo, watches};

use crate::error::Result;

/// How much of the journal and of each log the report carries. A support report is
/// read by a person; the point is the shape of the failure, not the archive.
const JOURNAL_TAIL: usize = 40;
const LOGS: usize = 3;
const LOG_BYTES: usize = 4 * 1024;

/// Write the report and answer with where it went.
pub fn write(registry: &Registry, state_dir: &Path, output: Option<&Path>) -> Result<PathBuf> {
	let path = match output {
		Some(path) => path.to_path_buf(),
		None => PathBuf::from(format!("retype-report-{}.txt", stamp())),
	};

	let text = render(registry, state_dir)?;
	fs::write(&path, text)?;
	Ok(path)
}

/// The whole report, as the file gets it.
pub fn render(registry: &Registry, state_dir: &Path) -> Result<String> {
	let mut text = String::new();

	head(&mut text, state_dir);
	build(&mut text);
	machine(&mut text, state_dir);
	configuration(&mut text);
	probed_tools(&mut text, registry);
	watches_section(&mut text, state_dir);
	state_section(&mut text, state_dir);
	journal_tail(&mut text, state_dir);
	log_tails(&mut text, state_dir);

	Ok(text)
}

fn head(text: &mut String, state_dir: &Path) {
	let _ = writeln!(text, "retype support report");
	let _ = writeln!(text, "generated {}", stamp());
	let _ = writeln!(text);
	let _ = writeln!(
		text,
		"This file was generated on this machine. It contains the configuration in use,"
	);
	let _ = writeln!(
		text,
		"the last journal records and the last few job logs. Read it before sending it."
	);
	let _ = writeln!(text);
	let _ = writeln!(text, "state     {}", state_dir.display());
	let _ = writeln!(text);
}

fn build(text: &mut String) {
	let _ = writeln!(text, "## build");
	let _ = writeln!(text);
	let _ = writeln!(text, "retype    {}", env!("CARGO_PKG_VERSION"));

	for (name, version) in [
		("grammar", retype_core::intent::GRAMMAR_VERSION),
		("config", retype_core::registry::CONFIG_VERSION),
		("ipc", retype_ipc::IPC_VERSION),
		("journal", retype_core::job::JOURNAL_VERSION),
		("undo", retype_core::undo::UNDO_VERSION),
		("watches", retype_core::watches::WATCHES_VERSION),
		("bench", retype_core::estimate::BENCH_VERSION),
		("fidelity", retype_core::fidelity::FIDELITY_VERSION),
		("fixtures", retype_core::axis::FIXTURES_VERSION),
	] {
		let _ = writeln!(text, "{name:<10}{version}");
	}

	let _ = writeln!(text);
}

fn machine(text: &mut String, state_dir: &Path) {
	let _ = writeln!(text, "## machine");
	let _ = writeln!(text);
	let _ = writeln!(text, "os        {}", std::env::consts::OS);
	let _ = writeln!(text, "arch      {}", std::env::consts::ARCH);
	let _ = writeln!(
		text,
		"parallel  {}",
		std::thread::available_parallelism()
			.map(|count| count.get())
			.unwrap_or(0)
	);

	match retype_core::estimate::Calibration::open(state_dir) {
		Ok(calibration) if !calibration.machine.is_empty() => {
			let _ = writeln!(
				text,
				"machine   {} ({} measured pair(s))",
				calibration.machine,
				calibration.throughput.len()
			);
		}
		Ok(_) => {
			let _ = writeln!(text, "machine   not created yet: no daemon has run here");
		}
		Err(error) => {
			let _ = writeln!(text, "machine   unreadable: {error}");
		}
	}

	if let Ok(path) = fs::canonicalize(state_dir) {
		let _ = writeln!(text, "resolved  {}", path.display());
	}

	let _ = writeln!(text);
}

fn configuration(text: &mut String) {
	let _ = writeln!(text, "## configuration");
	let _ = writeln!(text);

	match crate::config::user_path() {
		Some(path) => {
			let _ = writeln!(text, "{}", path.display());

			match fs::read_to_string(&path) {
				Ok(contents) => {
					let _ = writeln!(text);
					let _ = writeln!(text, "{contents}");
				}
				Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
					let _ = writeln!(text, "absent: the built-in table is in use");
					let _ = writeln!(text);
				}
				Err(error) => {
					let _ = writeln!(text, "unreadable: {error}");
					let _ = writeln!(text);
				}
			}
		}
		None => {
			let _ = writeln!(text, "no configuration directory on this platform");
			let _ = writeln!(text);
		}
	}
}

fn probed_tools(text: &mut String, registry: &Registry) {
	let _ = writeln!(text, "## tools");
	let _ = writeln!(text);

	let declared: Vec<tools::Tool> = registry.tools().cloned().collect();

	let _ = writeln!(text, "{:<12}{:<28}{:<18}PURPOSE", "TOOL", "STATUS", "FINGERPRINT");

	for status in tools::probe_all(&declared) {
		let fingerprint = status
			.fingerprint()
			.map(|fingerprint| fingerprint.chars().take(16).collect::<String>())
			.unwrap_or_else(|| "none".to_string());

		let _ = writeln!(
			text,
			"{:<12}{:<28}{:<18}{}",
			status.tool.name,
			status.summary(),
			fingerprint,
			status.tool.purpose
		);
	}

	for tool in registry.tools() {
		for engine in tool.engines.keys() {
			let resolved = tools::resolve_engine(tool, engine).unwrap_or("no engine");
			let _ = writeln!(text, "{:<12}engine: {resolved}", format!("{} {engine}", tool.name));
		}
	}

	let _ = writeln!(text);
}

fn watches_section(text: &mut String, state_dir: &Path) {
	let _ = writeln!(text, "## watches");
	let _ = writeln!(text);

	match watches::load(state_dir) {
		Ok(registrations) if registrations.is_empty() => {
			let _ = writeln!(text, "none");
		}
		Ok(registrations) => {
			for registration in registrations {
				let _ = writeln!(
					text,
					"{}{}",
					registration.path.display(),
					match registration.recursive {
						true => "",
						false => "  (not recursive)",
					}
				);
			}
		}
		Err(error) => {
			let _ = writeln!(text, "unreadable: {error}");
		}
	}

	let _ = writeln!(text);
}

fn state_section(text: &mut String, state_dir: &Path) {
	let _ = writeln!(text, "## state");
	let _ = writeln!(text);

	for name in ["jobs.jsonl", "bench.toml", "watches.toml", "endpoint", "lock"] {
		let path = state_dir.join(name);

		let described = match fs::metadata(&path) {
			Ok(metadata) => format!("{} bytes", metadata.len()),
			Err(_) => "absent".to_string(),
		};

		let _ = writeln!(text, "{name:<14}{described}");
	}

	let logs = log_files(state_dir);

	match logs {
		entries if entries.is_empty() => {
			let _ = writeln!(text, "logs          none");
		}
		entries => {
			let bytes: u64 = entries
				.iter()
				.filter_map(|path| fs::metadata(path).ok())
				.map(|metadata| metadata.len())
				.sum();

			let _ = writeln!(text, "logs          {} file(s), {} bytes", entries.len(), bytes);
		}
	}

	match undo::Store::open(state_dir).list() {
		Ok(records) => {
			let bytes: u64 = records.iter().map(|record| record.source_size).sum();
			let _ = writeln!(text, "undo          {} record(s), {} bytes", records.len(), bytes);
		}
		Err(error) => {
			let _ = writeln!(text, "undo          unreadable: {error}");
		}
	}

	let _ = writeln!(text);
}

fn journal_tail(text: &mut String, state_dir: &Path) {
	let path = state_dir.join("jobs.jsonl");

	let _ = writeln!(text, "## journal");
	let _ = writeln!(text);

	let contents = match fs::read_to_string(&path) {
		Ok(contents) => contents,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
			let _ = writeln!(text, "no journal");
			let _ = writeln!(text);
			return;
		}
		Err(error) => {
			let _ = writeln!(text, "unreadable: {error}");
			let _ = writeln!(text);
			return;
		}
	};

	let records: Vec<&str> = contents
		.lines()
		.filter(|line| serde_json::from_str::<serde_json::Value>(line).is_ok())
		.collect();

	let _ = writeln!(
		text,
		"{} record(s), the last {} below",
		records.len(),
		records.len().min(JOURNAL_TAIL)
	);
	let _ = writeln!(text);

	for line in records.iter().skip(records.len().saturating_sub(JOURNAL_TAIL)) {
		let _ = writeln!(text, "{line}");
	}

	let _ = writeln!(text);
}

fn log_tails(text: &mut String, state_dir: &Path) {
	let files = log_files(state_dir);

	let _ = writeln!(text, "## logs");
	let _ = writeln!(text);

	if files.is_empty() {
		let _ = writeln!(text, "none");
		let _ = writeln!(text);
		return;
	}

	for path in files.iter().skip(files.len().saturating_sub(LOGS)) {
		let name = path
			.file_name()
			.map(|name| name.to_string_lossy().into_owned())
			.unwrap_or_default();

		let _ = writeln!(text, "### {name}");

		match fs::read(path) {
			Ok(bytes) => {
				let start = bytes.len().saturating_sub(LOG_BYTES);
				let _ = writeln!(text, "{}", String::from_utf8_lossy(&bytes[start..]).trim_end());
			}
			Err(error) => {
				let _ = writeln!(text, "unreadable: {error}");
			}
		}

		let _ = writeln!(text);
	}
}

/// The log files, oldest first, so the last few are the recent ones.
fn log_files(state_dir: &Path) -> Vec<PathBuf> {
	let directory = state_dir.join("logs");

	let mut files: Vec<(SystemTime, PathBuf)> = match fs::read_dir(&directory) {
		Ok(entries) => entries
			.filter_map(|entry| entry.ok())
			.filter_map(|entry| {
				let path = entry.path();
				let modified = entry.metadata().ok()?.modified().ok()?;
				path.is_file().then_some((modified, path))
			})
			.collect(),
		Err(_) => Vec::new(),
	};

	files.sort();
	files.into_iter().map(|(_, path)| path).collect()
}

/// Unix seconds as `YYYYMMDDTHHMMSSZ`, for a file name and a header.
fn stamp() -> String {
	let seconds = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|elapsed| elapsed.as_secs())
		.unwrap_or_default();

	let (year, month, day) = civil_from_days((seconds / 86_400) as i64);
	let rest = seconds % 86_400;

	format!(
		"{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
		rest / 3_600,
		rest % 3_600 / 60,
		rest % 60
	)
}

/// Days since 1970 as a civil date, by Howard Hinnant's count-days algorithm:
/// shifting the epoch to March makes the leap day the last day of the year, which
/// removes every special case.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
	let days = days + 719_468;
	let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
	let day_of_era = days - era * 146_097;
	let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
	let year = year_of_era + era * 400;
	let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
	let month_position = (5 * day_of_year + 2) / 153;

	let day = (day_of_year - (153 * month_position + 2) / 5 + 1) as u32;
	let month = match month_position < 10 {
		true => month_position + 3,
		false => month_position - 9,
	} as u32;

	(year + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_stamp_is_a_date_and_a_time_in_utc() {
		assert_eq!(civil_from_days(0), (1970, 1, 1));
		assert_eq!(civil_from_days(19_723), (2024, 1, 1));
		assert_eq!(civil_from_days(20_724), (2026, 9, 28));
		assert_eq!(civil_from_days(20_758), (2026, 11, 1));

		let stamp = stamp();
		assert_eq!(stamp.len(), 16, "{stamp}");
		assert!(stamp.ends_with('Z'), "{stamp}");
		assert_eq!(&stamp[8..9], "T", "{stamp}");
	}
}
