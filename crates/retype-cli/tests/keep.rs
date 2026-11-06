//! The keep-original control and exact source-name preservation.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

#[path = "support/tools.rs"]
mod tool_support;

const HEIC_FIXTURE: &[u8] = include_bytes!("../../../fixtures/native-watch/photo.heic");

struct Daemon {
	child: Child,
	state: PathBuf,
}

impl Drop for Daemon {
	fn drop(&mut self) {
		if let Ok(mut client) = retype_cli::client::connect(&self.state) {
			let _ = client.shutdown(&retype_ipc::ShutdownRequest { now: true, force: true });
		}
		let _ = self.child.kill();
		let _ = self.child.wait();
	}
}

struct CommandResult {
	status: ExitStatus,
	stdout: String,
	stderr: String,
}

struct TestCommand(Child);

impl Drop for TestCommand {
	fn drop(&mut self) {
		let _ = self.0.kill();
		let _ = self.0.wait();
	}
}

fn bounded(command: &mut Command, limit: Duration) -> std::io::Result<CommandResult> {
	let mut stdout = tempfile::tempfile()?;
	let mut stderr = tempfile::tempfile()?;
	let mut child = TestCommand(
		command
			.stdout(Stdio::from(stdout.try_clone()?))
			.stderr(Stdio::from(stderr.try_clone()?))
			.spawn()?,
	);
	let deadline = Instant::now() + limit;
	let status = loop {
		if let Some(status) = child.0.try_wait()? {
			break status;
		}
		if Instant::now() >= deadline {
			return Err(std::io::Error::new(
				std::io::ErrorKind::TimedOut,
				"test command timed out",
			));
		}
		std::thread::sleep(Duration::from_millis(25));
	};
	fn text(file: &mut fs::File) -> std::io::Result<String> {
		let length = file.metadata()?.len();
		file.seek(SeekFrom::Start(length.saturating_sub(128 * 1024)))?;
		let mut bytes = Vec::new();
		file.read_to_end(&mut bytes)?;
		Ok(String::from_utf8_lossy(&bytes).into_owned())
	}
	Ok(CommandResult {
		status,
		stdout: text(&mut stdout)?,
		stderr: text(&mut stderr)?,
	})
}

fn archive() -> Vec<u8> {
	let mut builder = tar::Builder::new(Vec::new());
	let mut header = tar::Header::new_gnu();
	header.set_size(7);
	header.set_mode(0o644);
	header.set_cksum();
	builder.append_data(&mut header, "file.txt", &b"payload"[..]).unwrap();
	builder.into_inner().unwrap()
}

#[derive(Clone, Copy)]
enum UndoPolicy {
	Enabled,
	Disabled,
}

fn configuration(dir: &Path, undo: UndoPolicy) -> PathBuf {
	let undo_enabled = matches!(undo, UndoPolicy::Enabled);
	let config = dir.join("retype.toml");
	fs::write(
		&config,
		format!(
			"version = 'retype-config-1'\n[notify]\nenabled = false\n[deferral]\nmode = 'now'\n[undo]\nenabled = {undo_enabled}\n[formats.tar]\naliases = ['mytar']\n"
		),
	)
	.unwrap();
	config
}

fn start_daemon(state: &Path, config: &Path) -> Daemon {
	let daemon = Daemon {
		child: Command::new(env!("CARGO_BIN_EXE_retype"))
			.args(["daemon", "--detached"])
			.arg("--state-dir")
			.arg(state)
			.env("RETYPE_CONFIG", config)
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.spawn()
			.unwrap(),
		state: state.to_path_buf(),
	};
	let deadline = Instant::now() + Duration::from_secs(10);
	while retype_cli::client::connect(state).is_err() {
		assert!(Instant::now() < deadline, "the daemon did not start");
		std::thread::sleep(Duration::from_millis(25));
	}
	daemon
}

fn wait_for_watch(state: &Path) {
	let deadline = Instant::now() + Duration::from_secs(10);
	loop {
		let status = {
			let mut client = retype_cli::client::connect(state).unwrap();
			client.status().unwrap()
		};
		if status.watches.first().and_then(|watch| watch.status.as_deref()) == Some("ready") {
			return;
		}
		assert!(Instant::now() < deadline, "watch did not become ready: {status:?}");
		std::thread::sleep(Duration::from_millis(25));
	}
}

fn wait_for_job(state: &Path, expected: retype_core::job::JobState) -> retype_core::job::Job {
	let deadline = Instant::now() + Duration::from_secs(15);
	loop {
		let job = {
			let mut client = retype_cli::client::connect(state).unwrap();
			let list = client.jobs_list().unwrap();
			list.jobs.last().map(|summary| summary.id.clone())
		};
		if let Some(id) = job {
			let job = {
				let mut client = retype_cli::client::connect(state).unwrap();
				client.jobs_get(id.as_str()).unwrap()
			};
			if job.state == expected {
				return job;
			}
			assert_ne!(job.state, retype_core::job::JobState::Failed, "{job:?}");
		}
		assert!(Instant::now() < deadline, "job did not reach {expected:?}");
		std::thread::sleep(Duration::from_millis(25));
	}
}

fn check_zip(path: &Path) {
	let mut zip = zip::ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
	let mut contents = String::new();
	zip.by_name("file.txt").unwrap().read_to_string(&mut contents).unwrap();
	assert_eq!(contents, "payload");
}

struct WatchCase<'a> {
	source_name: &'a str,
	intent: &'a str,
	needs_confirmation: bool,
	undo: UndoPolicy,
}

fn watched(case: WatchCase<'_>) {
	let WatchCase {
		source_name,
		intent,
		needs_confirmation,
		undo,
	} = case;
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let photos = dir.path().join("photos");
	fs::create_dir(&photos).unwrap();
	let source = photos.join(source_name);
	let original = archive();
	fs::write(&source, &original).unwrap();
	let config = configuration(dir.path(), undo);
	let registered = Command::new(env!("CARGO_BIN_EXE_retype"))
		.arg("watch")
		.arg(&photos)
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.output()
		.unwrap();
	assert!(registered.status.success());
	let _daemon = start_daemon(&state, &config);
	wait_for_watch(&state);
	fs::rename(&source, photos.join(format!("Bundle.{intent}"))).unwrap();
	if needs_confirmation {
		let pending = wait_for_job(&state, retype_core::job::JobState::NeedsAttention);
		assert_eq!(fs::read(&source).unwrap(), original);
		let mut client = retype_cli::client::connect(&state).unwrap();
		client
			.jobs_resolve(pending.id.as_str(), retype_ipc::ResolveMode::Now, false)
			.unwrap();
	}
	let job = wait_for_job(&state, retype_core::job::JobState::Done);
	let output = photos.join("Bundle.zip");
	check_zip(&output);
	let second = photos.join("Bundle.tar.gz");
	if intent.contains("tar.gz") {
		assert!(second.is_file(), "the second requested output exists");
	}
	assert_eq!(
		fs::read(&source).unwrap(),
		original,
		"the exact source name remains visible"
	);
	if intent.contains("+keep") {
		assert!(job.intent.as_deref().unwrap().contains("+keep"));
	}
	let store = retype_core::undo::Store::open(&state);
	if matches!(undo, UndoPolicy::Disabled) {
		assert!(store.list().unwrap().is_empty());
		return;
	}
	let undo = Command::new(env!("CARGO_BIN_EXE_retype"))
		.args(["undo", job.id.as_str(), "--no-spawn"])
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.output()
		.unwrap();
	if retype_core::fsx::supports_durable_cleanup() {
		assert!(undo.status.success(), "{}", String::from_utf8_lossy(&undo.stderr));
		assert!(!output.exists());
		assert!(!second.exists());
	} else {
		assert!(!undo.status.success());
		assert!(output.exists());
	}
	assert_eq!(fs::read(source).unwrap(), original);
}

#[test]
fn watched_keep_preserves_the_original_and_undo() {
	watched(WatchCase {
		source_name: "Bundle.TAR",
		intent: "zip+keep",
		needs_confirmation: false,
		undo: UndoPolicy::Enabled,
	});
}

#[test]
fn watched_keep_applies_from_any_output_group() {
	watched(WatchCase {
		source_name: "Bundle.TAR",
		intent: "zip,tar.gz+keep",
		needs_confirmation: false,
		undo: UndoPolicy::Enabled,
	});
}

#[test]
fn explicit_source_group_preserves_original_case() {
	watched(WatchCase {
		source_name: "Bundle.TAR",
		intent: "tar,zip",
		needs_confirmation: false,
		undo: UndoPolicy::Enabled,
	});
}

#[test]
fn an_explicit_source_alias_keeps_the_exact_original_name() {
	watched(WatchCase {
		source_name: "Bundle.MYTAR",
		intent: "mytar,zip",
		needs_confirmation: false,
		undo: UndoPolicy::Enabled,
	});
}

#[test]
fn keep_and_ask_restore_the_exact_name_before_confirmation() {
	watched(WatchCase {
		source_name: "Bundle.TAR",
		intent: "zip+keep+ask",
		needs_confirmation: true,
		undo: UndoPolicy::Enabled,
	});
}

#[test]
fn keep_and_ask_preserve_a_source_format_alias() {
	watched(WatchCase {
		source_name: "Bundle.MYTAR",
		intent: "zip+keep+ask",
		needs_confirmation: true,
		undo: UndoPolicy::Enabled,
	});
}

#[test]
fn keep_is_independent_of_undo() {
	watched(WatchCase {
		source_name: "Bundle.TAR",
		intent: "zip+keep",
		needs_confirmation: false,
		undo: UndoPolicy::Disabled,
	});
}

#[test]
fn direct_keep_preserves_the_original_and_undo_leaves_later_edits_alone() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let source = dir.path().join("Bundle.TAR");
	let output = dir.path().join("Bundle.zip");
	fs::write(&source, archive()).unwrap();
	let config = configuration(dir.path(), UndoPolicy::Enabled);
	let _daemon = start_daemon(&state, &config);
	let converted = Command::new(env!("CARGO_BIN_EXE_retype"))
		.arg("convert")
		.arg(&source)
		.arg("zip+keep")
		.args(["--json", "--no-spawn"])
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.output()
		.unwrap();
	assert!(
		converted.status.success(),
		"{}",
		String::from_utf8_lossy(&converted.stderr)
	);
	let answer: serde_json::Value = serde_json::from_slice(&converted.stdout).unwrap();
	assert_eq!(answer["ok"], true);
	assert_eq!(answer["job"]["state"], "done");
	assert!(answer["job"]["intent"].as_str().unwrap().contains("+keep"));
	check_zip(&output);
	fs::write(&source, b"later edit").unwrap();
	let undo = Command::new(env!("CARGO_BIN_EXE_retype"))
		.args(["undo", "--last", "--no-spawn"])
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.output()
		.unwrap();
	if retype_core::fsx::supports_durable_cleanup() {
		assert!(undo.status.success(), "{}", String::from_utf8_lossy(&undo.stderr));
		assert!(!output.exists());
	} else {
		assert!(!undo.status.success());
		assert!(output.exists());
	}
	assert_eq!(fs::read(source).unwrap(), b"later edit");
}

#[test]
fn keep_does_not_hide_an_undo_storage_failure_from_the_cli() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let source = dir.path().join("Bundle.TAR");
	let original = archive();
	fs::write(&source, &original).unwrap();
	let config = configuration(dir.path(), UndoPolicy::Enabled);
	let _daemon = start_daemon(&state, &config);
	fs::write(state.join("undo"), b"not a directory").unwrap();
	let converted = Command::new(env!("CARGO_BIN_EXE_retype"))
		.arg("convert")
		.arg(&source)
		.arg("zip+keep")
		.args(["--json", "--no-spawn"])
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.output()
		.unwrap();
	assert!(!converted.status.success());
	let answer: serde_json::Value = serde_json::from_slice(&converted.stdout).unwrap();
	assert_eq!(answer["ok"], false);
	assert_eq!(answer["error"]["detail"]["job"]["state"], "failed");
	assert!(answer["error"]["message"].as_str().unwrap().contains("undo"));
	assert_eq!(fs::read(source).unwrap(), original);
	assert!(!dir.path().join("Bundle.zip").exists());
}

fn requires_native_workflow() -> bool {
	std::env::var_os("RETYPE_REQUIRE_NATIVE_WORKFLOW").as_deref() == Some(std::ffi::OsStr::new("1"))
}

fn missing_heic(reason: &str) {
	assert!(
		!requires_native_workflow(),
		"required native workflow prerequisite unavailable: {reason}"
	);
	tool_support::unavailable(reason);
}

fn supports_image_conversion(formats: &str) -> bool {
	fn supports(formats: &str, format: &str, permission: char) -> bool {
		formats.lines().any(|line| {
			let fields: Vec<&str> = line.split_whitespace().collect();
			fields.first().is_some_and(|name| name.trim_end_matches('*') == format)
				&& fields.get(2).is_some_and(|mode| mode.contains(permission))
		})
	}
	supports(formats, "HEIC", 'r') && supports(formats, "JPEG", 'w')
}

#[test]
fn a_read_only_heic_delegate_is_enough_for_heic_to_jpeg() {
	let formats = "  HEIC HEIC r-- High Efficiency Image Format\n  JPEG* JPEG rw- JPEG\n";
	assert!(supports_image_conversion(formats));
	assert!(!supports_image_conversion(
		"  HEIC HEIC -w- HEIC\n  JPEG* JPEG rw- JPEG\n"
	));
	assert!(!supports_image_conversion(
		"  HEIC HEIC r-- HEIC\n  JPEG* JPEG r-- JPEG\n"
	));
}

fn has_heic_decoder_and_jpeg_encoder() -> bool {
	let mut capabilities = Command::new("magick");
	capabilities.args(["-list", "format"]);
	let capabilities = match bounded(&mut capabilities, Duration::from_secs(10)) {
		Ok(output) => output,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
			missing_heic("ImageMagick 7 with HEIC decoding and JPEG encoding");
			return false;
		}
		Err(error) => panic!("cannot inspect ImageMagick: {error}"),
	};
	assert!(capabilities.status.success(), "{}", capabilities.stderr);
	let supported = supports_image_conversion(&capabilities.stdout);
	if !supported {
		missing_heic("ImageMagick HEIC decoder and JPEG encoder");
	}
	supported
}

fn assert_jpeg_dimensions(path: &Path) {
	let mut identify = Command::new("magick");
	identify.args(["identify", "-format", "%m %wx%h"]).arg(path);
	let image = bounded(&mut identify, Duration::from_secs(10)).unwrap();
	assert!(image.status.success(), "{}", image.stderr);
	assert_eq!(image.stdout, "JPEG 40x32");
}

#[test]
fn a_real_watched_heic_conversion_and_undo_preserve_the_original() {
	if !has_heic_decoder_and_jpeg_encoder() {
		return;
	}
	if requires_native_workflow() {
		let mut ffmpeg = Command::new("ffmpeg");
		ffmpeg.arg("-version");
		let result = bounded(&mut ffmpeg, Duration::from_secs(10));
		assert!(
			result.is_ok_and(|output| output.status.success()),
			"required native workflow prerequisite unavailable: FFmpeg for independent JPEG validation"
		);
	}
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let photos = dir.path().join("photos");
	fs::create_dir(&photos).unwrap();
	let source = photos.join("photo.HEIC");
	let target = photos.join("photo.jpg");
	fs::write(&source, HEIC_FIXTURE).unwrap();
	let original = HEIC_FIXTURE.to_vec();
	let config = configuration(dir.path(), UndoPolicy::Enabled);
	let registered = Command::new(env!("CARGO_BIN_EXE_retype"))
		.arg("watch")
		.arg(&photos)
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.output()
		.unwrap();
	assert!(
		registered.status.success(),
		"{}",
		String::from_utf8_lossy(&registered.stderr)
	);
	let _daemon = start_daemon(&state, &config);
	wait_for_watch(&state);
	fs::rename(&source, &target).unwrap();
	let job = wait_for_job(&state, retype_core::job::JobState::Done);

	assert_jpeg_dimensions(&target);
	let retained_before_undo = source.exists();
	if retained_before_undo {
		assert_eq!(fs::read(&source).unwrap(), original);
	}
	let mut undo = Command::new(env!("CARGO_BIN_EXE_retype"));
	undo.args(["undo", job.id.as_str(), "--no-spawn"])
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config);
	let undo = bounded(&mut undo, Duration::from_secs(10)).unwrap();
	assert_eq!(fs::read(&source).unwrap(), original);
	if undo.status.success() {
		assert!(!target.exists());
	} else {
		assert!(!retype_core::fsx::supports_durable_cleanup(), "{}", undo.stderr);
		assert!(undo.stderr.contains("directory durability"), "{}", undo.stderr);
		assert!(target.exists());
	}
	if requires_native_workflow() {
		assert!(
			undo.status.success(),
			"release workflow requires a complete native undo: {}",
			undo.stderr
		);
		assert!(
			!retained_before_undo,
			"release workflow requires normal source disposition"
		);
	}
}

#[test]
fn a_real_watched_jpeg_with_keep_retains_the_original() {
	if !has_heic_decoder_and_jpeg_encoder() {
		return;
	}
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let photos = dir.path().join("photos");
	fs::create_dir(&photos).unwrap();
	let source = photos.join("photo.HEIC");
	let request = photos.join("photo.jpg+keep");
	let output = photos.join("photo.jpg");
	fs::write(&source, HEIC_FIXTURE).unwrap();
	let config = configuration(dir.path(), UndoPolicy::Enabled);
	let registered = Command::new(env!("CARGO_BIN_EXE_retype"))
		.arg("watch")
		.arg(&photos)
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.output()
		.unwrap();
	assert!(
		registered.status.success(),
		"{}",
		String::from_utf8_lossy(&registered.stderr)
	);
	let _daemon = start_daemon(&state, &config);
	wait_for_watch(&state);
	fs::rename(&source, &request).unwrap();
	let job = wait_for_job(&state, retype_core::job::JobState::Done);

	assert_jpeg_dimensions(&output);
	assert_eq!(fs::read(&source).unwrap(), HEIC_FIXTURE);
	let mut undo = Command::new(env!("CARGO_BIN_EXE_retype"));
	undo.args(["undo", job.id.as_str(), "--no-spawn"])
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config);
	let undo = bounded(&mut undo, Duration::from_secs(10)).unwrap();
	assert_eq!(fs::read(&source).unwrap(), HEIC_FIXTURE);
	if retype_core::fsx::supports_durable_cleanup() {
		assert!(undo.status.success(), "{}", undo.stderr);
		assert!(!output.exists());
	} else {
		assert!(!undo.status.success());
		assert!(undo.stderr.contains("directory durability"), "{}", undo.stderr);
		assert!(output.exists());
	}
}

#[test]
fn explanation_preserves_an_alias_and_shows_the_kept_original() {
	let dir = tempfile::tempdir().unwrap();
	let output = Command::new(env!("CARGO_BIN_EXE_retype"))
		.args(["explain", "Photo.JPEG", "jpg,png+keep", "--json"])
		.arg("--state-dir")
		.arg(dir.path())
		.output()
		.unwrap();
	let answer: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
	let outputs = answer["outputs"].as_array().unwrap();
	assert!(
		outputs
			.iter()
			.any(|entry| entry["kept"] == true && entry["path"] == "Photo.JPEG")
	);
	assert!(outputs.iter().any(|entry| entry["path"] == "Photo.png"));
}
