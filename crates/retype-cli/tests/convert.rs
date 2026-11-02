//! `retype convert` end to end, with a copy tool instead of a converter so the
//! test does not depend on one being installed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

/// A configuration whose only backend copies the input to the output.
fn copy_config(dir: &Path) -> PathBuf {
	let (command, layout) = match cfg!(windows) {
		true => ("cmd", "layout = ['/c', 'copy', '{input}', '{ops}', '{output}']"),
		false => ("cp", "layout = ['{input}', '{ops}', '{output}']"),
	};

	let config = dir.join("retype.toml");
	fs::write(
		&config,
		format!(
			"\
version = 'retype-config-1'

[backends]
prefer = ['copy']

[tools.copy]
command = '{command}'
{layout}

[formats.png]
family = 'image'
lossy = false
alpha = true
tier = 'exact'

[formats.webp]
family = 'image'
lossy = true
alpha = true
tier = 'converted'

[formats.bmp]
family = 'image'
lossy = false
alpha = true
tier = 'converted'
"
		),
	)
	.unwrap();

	config
}

struct Fixture {
	dir: tempfile::TempDir,
	config: PathBuf,
	state: PathBuf,
}

impl Drop for Fixture {
	/// An autostarted daemon outlives its client, so the test stops the one it
	/// caused rather than leaving a process per fixture behind.
	fn drop(&mut self) {
		let _ = retype()
			.args(["daemon", "stop"])
			.arg("--state-dir")
			.arg(&self.state)
			.env("RETYPE_CONFIG", &self.config)
			.output();
	}
}

impl Fixture {
	fn new() -> Self {
		let dir = tempfile::tempdir().unwrap();
		let config = copy_config(dir.path());
		let state = dir.path().join("state");

		Self { dir, config, state }
	}

	fn input(&self) -> PathBuf {
		let path = self.dir.path().join("photo.png");
		fs::write(&path, b"pixels").unwrap();
		path
	}

	fn convert(&self, args: &[&str]) -> std::process::Output {
		retype()
			.args(args)
			.arg("--state-dir")
			.arg(&self.state)
			.env("RETYPE_CONFIG", &self.config)
			.output()
			.unwrap()
	}
}

#[test]
fn converts_and_writes_beside_the_input() {
	let fixture = Fixture::new();
	let input = fixture.input();

	let output = fixture.convert(&["convert", input.to_str().unwrap(), "webp"]);

	assert_eq!(
		output.status.code(),
		Some(0),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	assert_eq!(fs::read(fixture.dir.path().join("photo.webp")).unwrap(), b"pixels");
	assert!(String::from_utf8_lossy(&output.stdout).contains("photo.webp"));
}

#[test]
fn the_journal_records_the_job() {
	let fixture = Fixture::new();
	let input = fixture.input();

	fixture.convert(&["convert", input.to_str().unwrap(), "webp"]);

	let journal = fs::read_to_string(fixture.state.join("jobs.jsonl")).expect("the journal exists");
	assert!(journal.contains("\"retype-journal-1\""), "{journal}");
	assert!(journal.contains("\"state\":\"done\""), "{journal}");
	assert!(journal.contains("photo.webp"), "{journal}");
}

#[test]
fn an_existing_output_is_refused_without_force() {
	let fixture = Fixture::new();
	let input = fixture.input();
	let target = fixture.dir.path().join("photo.webp");
	fs::write(&target, b"older").unwrap();

	let output = fixture.convert(&["convert", input.to_str().unwrap(), "webp"]);

	assert_eq!(output.status.code(), Some(5));
	assert_eq!(fs::read(&target).unwrap(), b"older", "the existing file is untouched");

	let forced = fixture.convert(&["convert", input.to_str().unwrap(), "webp", "--force"]);
	assert_eq!(forced.status.code(), Some(0));
	assert_eq!(fs::read(&target).unwrap(), b"pixels");
}

#[test]
fn dry_run_prints_the_plan_and_writes_nothing() {
	let fixture = Fixture::new();
	let input = fixture.input();

	let output = fixture.convert(&["convert", input.to_str().unwrap(), "webp", "--dry-run"]);
	let stdout = String::from_utf8_lossy(&output.stdout);

	assert_eq!(output.status.code(), Some(0));
	assert!(stdout.contains("photo.webp"), "{stdout}");
	assert!(stdout.contains("backend"), "{stdout}");
	assert!(!fixture.dir.path().join("photo.webp").exists());
	assert!(!fixture.state.join("jobs.jsonl").exists(), "a dry run journals nothing");
}

#[test]
fn an_output_path_needs_one_output() {
	let fixture = Fixture::new();
	let input = fixture.input();

	let output = fixture.convert(&["convert", input.to_str().unwrap(), "webp,bmp", "-o", "one.webp"]);

	assert_eq!(output.status.code(), Some(2));
	assert!(String::from_utf8_lossy(&output.stderr).contains("E_USAGE_OUTPUT_COUNT"));
}

#[test]
fn a_kept_source_is_not_an_output() {
	// `webp,png` from a png writes one file and keeps the source, so `-o` names the
	// file that is actually produced, and the source is left exactly as it was.
	let fixture = Fixture::new();
	let input = fixture.input();
	let before = fs::read(&input).unwrap();

	let target = fixture.dir.path().join("one.webp");
	let output = fixture.convert(&[
		"convert",
		input.to_str().unwrap(),
		"webp,png",
		"-o",
		target.to_str().unwrap(),
	]);

	assert_eq!(
		output.status.code(),
		Some(0),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	assert!(target.exists());
	assert_eq!(fs::read(&input).unwrap(), before, "a kept source is not rewritten");
}

#[test]
fn a_name_with_no_declared_suffix_is_refused() {
	let fixture = Fixture::new();
	let input = fixture.dir.path().join("notes.txt");
	fs::write(&input, b"words").unwrap();

	let output = fixture.convert(&["convert", input.to_str().unwrap(), "webp"]);

	assert_eq!(output.status.code(), Some(3));
	assert!(String::from_utf8_lossy(&output.stderr).contains("E_FORMAT_UNKNOWN"));
}

#[test]
fn a_submitted_job_is_visible_and_reachable_by_id() {
	let fixture = Fixture::new();
	let input = fixture.input();

	fixture.convert(&["convert", input.to_str().unwrap(), "webp"]);

	let listed = fixture.convert(&["jobs", "list"]);
	let stdout = String::from_utf8_lossy(&listed.stdout);
	assert!(stdout.contains("done"), "{stdout}");

	let id = stdout.split_whitespace().next().expect("a job id").to_string();
	let one = fixture.convert(&["jobs", "get", &id]);
	let asked = String::from_utf8_lossy(&one.stdout);

	assert_eq!(one.status.code(), Some(0), "{}", String::from_utf8_lossy(&one.stderr));
	assert!(asked.contains("\"v\":\"retype-journal-1\""), "the record keeps its own schema: {asked}");
	assert!(asked.contains("\"state\":\"done\""), "{asked}");
	assert!(asked.contains("photo.webp"), "{asked}");
}

#[test]
fn without_a_daemon_and_without_spawning_there_is_nothing_to_convert() {
	let fixture = Fixture::new();
	let input = fixture.input();

	let output = fixture.convert(&["convert", input.to_str().unwrap(), "webp", "--no-spawn"]);

	assert_eq!(output.status.code(), Some(6));
	assert!(!fixture.dir.path().join("photo.webp").exists());
}

/// A configuration whose tool takes its time, so a job stays running long enough
/// to be cancelled.
#[cfg(unix)]
fn slow_config(dir: &Path) -> PathBuf {
	let script = dir.join("slow.sh");
	fs::write(&script, "#!/bin/sh\nsleep 5\ncp \"$1\" \"$2\"\n").unwrap();

	let mut permissions = fs::metadata(&script).unwrap().permissions();
	std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
	fs::set_permissions(&script, permissions).unwrap();

	let config = dir.join("slow.toml");
	fs::write(
		&config,
		format!(
			"\
version = 'retype-config-1'

[backends]
prefer = ['slow']

[tools.slow]
command = '{}'
layout = ['{{input}}', '{{ops}}', '{{output}}']

[formats.png]
family = 'image'
lossy = false
alpha = true
tier = 'exact'

[formats.webp]
family = 'image'
lossy = true
alpha = true
tier = 'converted'
",
			script.display()
		),
	)
	.unwrap();

	config
}

/// `jobs cancel` has to reach a submitted job while it runs, which is why the
/// submitting client does not hold the daemon's connection while it waits.
#[cfg(unix)]
#[test]
fn a_submitted_job_can_be_cancelled_while_it_runs() {
	use std::time::{Duration, Instant};

	let dir = tempfile::tempdir().unwrap();
	let config = slow_config(dir.path());
	let state = dir.path().join("state");
	let input = dir.path().join("photo.png");
	fs::write(&input, b"pixels").unwrap();

	let mut convert = retype()
		.args(["convert", input.to_str().unwrap(), "webp"])
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.stdout(std::process::Stdio::piped())
		.stderr(std::process::Stdio::piped())
		.spawn()
		.unwrap();

	let deadline = Instant::now() + Duration::from_secs(20);
	let id = loop {
		let listed = retype()
			.args(["jobs", "list", "--state", "running"])
			.arg("--state-dir")
			.arg(&state)
			.env("RETYPE_CONFIG", &config)
			.output()
			.unwrap();
		let stdout = String::from_utf8_lossy(&listed.stdout);

		if let Some(id) = stdout.split_whitespace().next().filter(|id| !id.is_empty()) {
			break id.to_string();
		}

		assert!(Instant::now() < deadline, "the job never started running");
		std::thread::sleep(Duration::from_millis(50));
	};

	retype()
		.args(["jobs", "cancel", &id])
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.output()
		.unwrap();

	let finished = convert.wait().unwrap();
	let mut stderr = String::new();
	std::io::Read::read_to_string(&mut convert.stderr.unwrap(), &mut stderr).unwrap();

	assert_eq!(finished.code(), Some(7), "{stderr}");
	assert!(stderr.contains("E_JOB_CANCELLED"), "{stderr}");
	assert!(
		!dir.path().join("photo.webp").exists(),
		"a cancelled job leaves no output"
	);

	// This test does not use the fixture, so it stops the daemon it caused.
	let stopped = retype()
		.args(["daemon", "stop"])
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.output()
		.unwrap();

	assert!(
		stopped.status.success(),
		"the daemon this test started was not stopped: {}",
		String::from_utf8_lossy(&stopped.stderr)
	);
}

#[test]
fn a_missing_source_is_a_filesystem_error_before_a_job_exists() {
	let fixture = Fixture::new();
	let missing = fixture.dir.path().join("absent.png");

	let output = fixture.convert(&["convert", missing.to_str().unwrap(), "webp"]);

	assert_eq!(output.status.code(), Some(5));
	assert!(String::from_utf8_lossy(&output.stderr).contains("E_IO_NOT_FOUND"));
	assert!(
		!fixture.state.join("jobs.jsonl").exists() || {
			let text = fs::read_to_string(fixture.state.join("jobs.jsonl")).unwrap_or_default();
			!text.contains("absent.png")
		}
	);
}

#[test]
fn a_dry_run_honours_the_output_path_and_checks_the_source() {
	let fixture = Fixture::new();
	let input = fixture.input();

	let named = fixture.convert(&[
		"convert",
		input.to_str().unwrap(),
		"webp",
		"-o",
		"holiday.webp",
		"--dry-run",
	]);
	let stdout = String::from_utf8_lossy(&named.stdout);

	assert_eq!(named.status.code(), Some(0));
	assert!(stdout.contains("holiday.webp"), "{stdout}");
	assert!(!fixture.dir.path().join("holiday.webp").exists());

	let missing = fixture.convert(&["convert", "absent.png", "webp", "--dry-run"]);
	assert_eq!(missing.status.code(), Some(5));
}

#[test]
fn an_output_that_is_the_source_is_refused_even_with_force() {
	let fixture = Fixture::new();
	let input = fixture.input();

	let output = fixture.convert(&[
		"convert",
		input.to_str().unwrap(),
		"webp",
		"-o",
		input.to_str().unwrap(),
		"--force",
	]);

	assert_eq!(output.status.code(), Some(2));
	assert!(String::from_utf8_lossy(&output.stderr).contains("E_USAGE_CONFLICT"));
	assert_eq!(fs::read(&input).unwrap(), b"pixels");
}

#[test]
fn an_existing_target_stops_the_whole_conversion_before_writing() {
	let fixture = Fixture::new();
	let input = fixture.input();
	let occupied = fixture.dir.path().join("photo.bmp");
	fs::write(&occupied, b"older").unwrap();

	let output = fixture.convert(&["convert", input.to_str().unwrap(), "bmp,webp"]);

	assert_eq!(output.status.code(), Some(5));
	assert!(!fixture.dir.path().join("photo.webp").exists());
	assert_eq!(fs::read(&occupied).unwrap(), b"older");
}

#[test]
fn a_convert_can_be_undone_and_leaves_its_source() {
	let fixture = Fixture::new();
	let input = fixture.input();
	let before = fs::read(&input).unwrap();

	let converted = fixture.convert(&["convert", input.to_str().unwrap(), "webp"]);
	assert_eq!(converted.status.code(), Some(0));
	assert!(fixture.dir.path().join("photo.webp").exists());

	let undone = fixture.convert(&["undo", "--last"]);
	if !retype_core::fsx::supports_durable_cleanup() {
		assert!(!undone.status.success());
		assert_eq!(fs::read(&input).unwrap(), before);
		assert!(fixture.dir.path().join("photo.webp").exists());
		return;
	}
	assert_eq!(
		undone.status.code(),
		Some(0),
		"{}",
		String::from_utf8_lossy(&undone.stderr)
	);
	assert!(!fixture.dir.path().join("photo.webp").exists());
	assert_eq!(fs::read(&input).unwrap(), before);
}

#[test]
fn jobs_list_since_filters_older_records() {
	let fixture = Fixture::new();
	let input = fixture.input();

	assert_eq!(
		fixture
			.convert(&["convert", input.to_str().unwrap(), "webp"])
			.status
			.code(),
		Some(0)
	);

	// `jobs list` prints the shortest handle; `--since` takes the full id.
	let first = String::from_utf8_lossy(&fixture.convert(&["jobs", "list", "--json"]).stdout)
		.lines()
		.filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
		.find(|job| job["source"].as_str().is_some_and(|path| path.contains("photo.png")))
		.and_then(|job| job["id"].as_str().map(str::to_string))
		.unwrap_or_default();
	assert!(!first.is_empty());

	assert_eq!(
		fixture
			.convert(&["convert", input.to_str().unwrap(), "bmp"])
			.status
			.code(),
		Some(0)
	);

	let listed = fixture.convert(&["jobs", "list", "--since", &first]);
	let stdout = String::from_utf8_lossy(&listed.stdout);

	assert_eq!(listed.status.code(), Some(0));
	assert!(!stdout.contains(&first), "since {first}: {stdout}");
	assert_eq!(stdout.lines().count(), 1, "since {first}: {stdout}");
}

#[test]
fn a_terminal_job_cannot_be_cancelled_or_resumed() {
	let fixture = Fixture::new();
	let input = fixture.input();

	assert_eq!(
		fixture
			.convert(&["convert", input.to_str().unwrap(), "webp"])
			.status
			.code(),
		Some(0)
	);

	let id = String::from_utf8_lossy(&fixture.convert(&["jobs", "list"]).stdout)
		.split_whitespace()
		.next()
		.unwrap_or_default()
		.to_string();

	let cancelled = fixture.convert(&["jobs", "cancel", &id]);
	assert_eq!(cancelled.status.code(), Some(7));

	let resumed = fixture.convert(&["jobs", "resume", &id]);
	assert_eq!(resumed.status.code(), Some(7));
}
