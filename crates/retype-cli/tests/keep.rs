//! The keep-original control and exact source-name preservation.

use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[path = "support/commands.rs"]
mod commands;
#[path = "support/tools.rs"]
mod tool_support;

use commands::bounded;

const HEIC_FIXTURE: &[u8] = include_bytes!("../../../fixtures/native-watch/photo.heic");
const ORIENTED_HEIC_FIXTURE: &[u8] = include_bytes!("../../../fixtures/native-watch/oriented-8bit.heic");
const ORIENTED_PIXEL_CHANNELS: &str = concat!(
	"%[fx:round(255*p{16,16}.r)] %[fx:round(255*p{16,16}.g)] %[fx:round(255*p{16,16}.b)] ",
	"%[fx:round(255*p{48,16}.r)] %[fx:round(255*p{48,16}.g)] %[fx:round(255*p{48,16}.b)] ",
	"%[fx:round(255*p{16,80}.r)] %[fx:round(255*p{16,80}.g)] %[fx:round(255*p{16,80}.b)] ",
	"%[fx:round(255*p{48,80}.r)] %[fx:round(255*p{48,80}.g)] %[fx:round(255*p{48,80}.b)]",
);

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
	start_daemon_with(state, config, None, None)
}

fn start_daemon_in(state: &Path, config: &Path, directory: Option<&Path>) -> Daemon {
	start_daemon_with(state, config, directory, None)
}

fn start_daemon_with(state: &Path, config: &Path, directory: Option<&Path>, path: Option<&std::ffi::OsStr>) -> Daemon {
	let mut command = Command::new(env!("CARGO_BIN_EXE_retype"));
	command
		.args(["daemon", "--detached"])
		.arg("--state-dir")
		.arg(state)
		.env("RETYPE_CONFIG", config)
		.stdout(Stdio::null())
		.stderr(Stdio::null());
	if let Some(directory) = directory {
		command.current_dir(directory);
	}
	if let Some(path) = path {
		command.env("PATH", path);
	}
	let daemon = Daemon {
		child: command.spawn().unwrap(),
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

fn has_jpeg_markers(path: &Path) -> io::Result<bool> {
	let mut file = fs::File::open(path)?;
	if file.metadata()?.len() < 4 {
		return Ok(false);
	}
	let mut start = [0; 3];
	let mut end = [0; 2];
	file.read_exact(&mut start)?;
	file.seek(SeekFrom::End(-2))?;
	file.read_exact(&mut end)?;
	Ok(start == [0xff, 0xd8, 0xff] && end == [0xff, 0xd9])
}

#[test]
fn the_jpeg_prerequisite_probe_checks_start_and_end_markers() {
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("output.jpg");
	fs::write(&path, [0xff, 0xd8, 0xff, 0xd9]).unwrap();
	assert!(has_jpeg_markers(&path).unwrap());
	fs::write(&path, [0xff, 0xd8, 0xff]).unwrap();
	assert!(!has_jpeg_markers(&path).unwrap());
	fs::write(&path, b"not jpeg").unwrap();
	assert!(!has_jpeg_markers(&path).unwrap());
}

fn probe_heic_conversion(program: &Path) -> io::Result<(bool, String)> {
	let dir = tempfile::tempdir()?;
	let input = dir.path().join("photo.heic");
	let output = dir.path().join("photo.jpg");
	fs::write(&input, HEIC_FIXTURE)?;
	let mut magick = Command::new(program);
	magick.arg(&input).arg(&output);
	let result = bounded(&mut magick, Duration::from_secs(10))?;
	if !result.status.success() {
		return Ok((false, result.stderr));
	}
	match has_jpeg_markers(&output) {
		Ok(true) => Ok((true, result.stderr)),
		Ok(false) => Ok((false, "output is not a JPEG".to_string())),
		Err(error) if error.kind() == io::ErrorKind::NotFound => Ok((false, "no JPEG was produced".to_string())),
		Err(error) => Err(error),
	}
}

#[test]
#[cfg(unix)]
fn the_heic_prerequisite_probes_conversion_instead_of_format_enumeration() {
	use std::os::unix::fs::PermissionsExt;

	let dir = tempfile::tempdir().unwrap();
	let program = dir.path().join("magick");
	for (script, expected) in [
		(
			r#"#!/bin/sh
[ "$#" -eq 2 ] && [ -s "$1" ] || exit 1
printf '\377\330\377\331' > "$2"
"#,
			true,
		),
		("#!/bin/sh\nexit 0\n", false),
		("#!/bin/sh\nprintf invalid > \"$2\"\n", false),
	] {
		fs::write(&program, script).unwrap();
		fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
		let (supported, _) = probe_heic_conversion(&program).unwrap();
		assert_eq!(supported, expected);
	}
}

fn has_heic_decoder_and_jpeg_encoder() -> bool {
	let path = std::env::var_os("PATH").unwrap_or_default();
	let Some(magick) = retype_core::tools::find_in_path(&path, "magick") else {
		missing_heic("ImageMagick 7 with HEIC decoding and JPEG encoding");
		return false;
	};
	let (supported, detail) = probe_heic_conversion(&magick)
		.unwrap_or_else(|error| panic!("cannot test ImageMagick HEIC conversion: {error}"));
	if !supported {
		missing_heic(&format!(
			"ImageMagick could not convert the HEIC fixture to JPEG: {detail}"
		));
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

fn assert_oriented_jpeg(path: &Path) {
	let mut identify = Command::new("magick");
	identify.args(["identify", "-format", "%m %wx%h"]).arg(path);
	let image = bounded(&mut identify, Duration::from_secs(10)).unwrap();
	assert!(image.status.success(), "{}", image.stderr);
	assert_eq!(image.stdout, "JPEG 64x96");

	let mut inspect = Command::new("magick");
	inspect.arg(path).args(["-format", ORIENTED_PIXEL_CHANNELS, "info:"]);
	let pixels = bounded(&mut inspect, Duration::from_secs(10)).unwrap();
	assert!(pixels.status.success(), "{}", pixels.stderr);
	let channels: Vec<u8> = pixels
		.stdout
		.split_whitespace()
		.map(|value| value.parse().expect("an 8-bit color channel"))
		.collect();
	let [
		green_r,
		green_g,
		green_b,
		red_r,
		red_g,
		red_b,
		yellow_r,
		yellow_g,
		yellow_b,
		blue_r,
		blue_g,
		blue_b,
	] = channels.as_slice()
	else {
		panic!("expected four RGB pixels, got: {}", pixels.stdout);
	};
	assert!(
		*green_r < 60 && *green_g > 90 && *green_b < 60,
		"green quadrant: {channels:?}"
	);
	assert!(*red_r > 180 && *red_g < 80 && *red_b < 80, "red quadrant: {channels:?}");
	assert!(
		*yellow_r > 180 && *yellow_g > 180 && *yellow_b < 80,
		"yellow quadrant: {channels:?}"
	);
	assert!(
		*blue_r < 80 && *blue_g < 80 && *blue_b > 180,
		"blue quadrant: {channels:?}"
	);
}

#[test]
fn a_real_oriented_heic_conversion_preserves_display_orientation() {
	if !has_heic_decoder_and_jpeg_encoder() {
		return;
	}
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let source = dir.path().join("oriented.heic");
	let output = dir.path().join("oriented.jpg");
	fs::write(&source, ORIENTED_HEIC_FIXTURE).unwrap();
	let config = configuration(dir.path(), UndoPolicy::Enabled);
	let _daemon = start_daemon(&state, &config);
	let mut convert = Command::new(env!("CARGO_BIN_EXE_retype"));
	convert
		.arg("convert")
		.arg(&source)
		.args(["jpg+keep", "--json", "--no-spawn", "--state-dir"])
		.arg(&state)
		.env("RETYPE_CONFIG", &config);
	let converted = bounded(&mut convert, Duration::from_secs(20)).unwrap();
	assert!(converted.status.success(), "{}", converted.stderr);
	let answer: serde_json::Value = serde_json::from_str(&converted.stdout).unwrap();
	assert_eq!(answer["job"]["state"], "done");
	assert_eq!(fs::read(&source).unwrap(), ORIENTED_HEIC_FIXTURE);
	assert_oriented_jpeg(&output);
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
fn relative_conversion_paths_use_the_clients_working_directory() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let daemon_directory = dir.path().join("daemon");
	let client_directory = dir.path().join("client");
	fs::create_dir(&daemon_directory).unwrap();
	fs::create_dir(&client_directory).unwrap();
	let daemon_source = daemon_directory.join("photo.tar");
	let client_source = client_directory.join("photo.tar");
	let daemon_output = daemon_directory.join("result.zip");
	let client_output = client_directory.join("result.zip");
	fs::write(&daemon_source, archive()).unwrap();
	let mut client_archive = archive();
	let payload = client_archive.windows(7).position(|bytes| bytes == b"payload").unwrap();
	client_archive[payload..payload + 7].copy_from_slice(b"client!");
	fs::write(&client_source, &client_archive).unwrap();
	fs::write(&daemon_output, b"unrelated destination").unwrap();
	let config = configuration(dir.path(), UndoPolicy::Enabled);
	let _daemon = start_daemon_in(&state, &config, Some(&daemon_directory));
	let mut convert = Command::new(env!("CARGO_BIN_EXE_retype"));
	convert
		.args(["convert", "photo.tar", "zip", "-o", "result.zip", "--force", "--json"])
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.current_dir(&client_directory);
	let converted = bounded(&mut convert, Duration::from_secs(20)).unwrap();
	assert!(converted.status.success(), "{}", converted.stderr);
	let answer: serde_json::Value = serde_json::from_str(&converted.stdout).unwrap();
	assert_eq!(
		Path::new(answer["job"]["source"]["path"].as_str().unwrap()),
		client_source
	);
	assert_eq!(fs::read(&daemon_source).unwrap(), archive());
	assert_eq!(fs::read(&daemon_output).unwrap(), b"unrelated destination");
	let mut zip = zip::ZipArchive::new(fs::File::open(&client_output).unwrap()).unwrap();
	let mut content = Vec::new();
	zip.by_name("file.txt").unwrap().read_to_end(&mut content).unwrap();
	assert_eq!(content, b"client!");
	assert_eq!(client_source.exists(), !retype_core::fsx::supports_durable_cleanup());
}

#[test]
fn relative_watch_paths_use_the_clients_working_directory() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let daemon_directory = dir.path().join("daemon");
	let client_directory = dir.path().join("client");
	fs::create_dir_all(daemon_directory.join("photos")).unwrap();
	fs::create_dir_all(client_directory.join("photos")).unwrap();
	let config = configuration(dir.path(), UndoPolicy::Enabled);
	let _daemon = start_daemon_in(&state, &config, Some(&daemon_directory));
	let mut watch = Command::new(env!("CARGO_BIN_EXE_retype"));
	watch
		.args(["watch", "photos", "--state-dir"])
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.current_dir(&client_directory);
	let watched = bounded(&mut watch, Duration::from_secs(10)).unwrap();
	assert!(watched.status.success(), "{}", watched.stderr);
	let registrations = retype_core::watches::load(&state).unwrap();
	assert_eq!(registrations.len(), 1);
	assert_eq!(
		registrations[0].path,
		fs::canonicalize(client_directory.join("photos")).unwrap()
	);

	let mut unwatch = Command::new(env!("CARGO_BIN_EXE_retype"));
	unwatch
		.args(["unwatch", "photos", "--state-dir"])
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.current_dir(&client_directory);
	let unwatched = bounded(&mut unwatch, Duration::from_secs(10)).unwrap();
	assert!(unwatched.status.success(), "{}", unwatched.stderr);
	assert!(retype_core::watches::load(&state).unwrap().is_empty());
}

#[test]
fn a_real_direct_heic_conversion_and_undo_follow_watched_disposition() {
	if !has_heic_decoder_and_jpeg_encoder() {
		return;
	}
	let mut ffmpeg = Command::new("ffmpeg");
	ffmpeg.arg("-version");
	let has_ffmpeg = bounded(&mut ffmpeg, Duration::from_secs(10)).is_ok_and(|output| output.status.success());
	if requires_native_workflow() {
		assert!(has_ffmpeg, "strict JPEG workflow requires FFmpeg");
	}
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let source = dir.path().join("photo.HEIC");
	let output = dir.path().join("photo.jpg");
	fs::write(&source, HEIC_FIXTURE).unwrap();
	let config = configuration(dir.path(), UndoPolicy::Enabled);
	let _daemon = start_daemon(&state, &config);
	let mut convert = Command::new(env!("CARGO_BIN_EXE_retype"));
	convert
		.arg("convert")
		.arg(&source)
		.args(["jpg", "--json", "--no-spawn", "--state-dir"])
		.arg(&state)
		.env("RETYPE_CONFIG", &config);
	let converted = bounded(&mut convert, Duration::from_secs(20)).unwrap();
	assert!(converted.status.success(), "{}", converted.stderr);
	let answer: serde_json::Value = serde_json::from_str(&converted.stdout).unwrap();
	assert_eq!(answer["job"]["state"], "done");
	let id = answer["job"]["id"].as_str().unwrap();
	assert_jpeg_dimensions(&output);
	let should_remove = has_ffmpeg && retype_core::fsx::supports_durable_cleanup();
	if should_remove {
		assert_eq!(answer["job"]["source_disposition"]["state"], "removed");
		assert!(!source.exists());
	} else {
		assert_eq!(answer["job"]["source_disposition"]["state"], "retained");
		assert_eq!(fs::read(&source).unwrap(), HEIC_FIXTURE);
	}
	let mut undo = Command::new(env!("CARGO_BIN_EXE_retype"));
	undo.args(["undo", id, "--no-spawn", "--state-dir"])
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
	if requires_native_workflow() {
		assert!(should_remove, "strict CLI workflow requires normal source disposition");
		assert!(
			undo.status.success(),
			"strict CLI workflow requires complete native undo"
		);
	}
}

#[test]
#[cfg(unix)]
fn an_unresolved_direct_source_is_a_failed_cli_result_with_recovery() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let source = dir.path().join("photo.png");
	let output = dir.path().join("photo.webp");
	let script = dir.path().join("converter.sh");
	fs::write(&source, b"original bytes").unwrap();
	fs::write(
		&script,
		format!("cp \"$1\" \"$2\"\nprintf 'changed source' > '{}'\n", source.display()),
	)
	.unwrap();
	let config = dir.path().join("retype.toml");
	fs::write(
		&config,
		format!(
			"version = 'retype-config-1'\n[notify]\nenabled = false\n\
			[backends]\nprefer = ['copy']\n[tools.copy]\ncommand = 'sh'\n\
			layout = ['{}', '{{input}}', '{{ops}}', '{{output}}']\n\
			[formats.png]\nfamily = 'image'\nlossy = false\nalpha = true\ntier = 'exact'\n\
			[formats.webp]\nfamily = 'image'\nlossy = true\nalpha = true\ntier = 'converted'\n",
			script.display()
		),
	)
	.unwrap();
	let _daemon = start_daemon(&state, &config);
	let mut convert = Command::new(env!("CARGO_BIN_EXE_retype"));
	convert
		.arg("convert")
		.arg(&source)
		.args(["webp", "--json", "--no-spawn", "--state-dir"])
		.arg(&state)
		.env("RETYPE_CONFIG", &config);
	let result = bounded(&mut convert, Duration::from_secs(20)).unwrap();
	assert!(!result.status.success());
	let answer: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
	assert_eq!(answer["ok"], false);
	assert_eq!(answer["error"]["code"], "E_IO_FAILED");
	assert_eq!(
		answer["error"]["detail"]["job"]["source_disposition"]["state"],
		"unresolved"
	);
	assert_eq!(fs::read(&source).unwrap(), b"changed source");
	assert_eq!(fs::read(&output).unwrap(), b"original bytes");
	let id = retype_core::job::JobId::parse(answer["error"]["detail"]["job"]["id"].as_str().unwrap()).unwrap();
	let store = retype_core::undo::Store::open(&state);
	assert!(store.load(&id).unwrap().unwrap().pinned);
	assert_eq!(
		fs::read(store.source(&id).unwrap().unwrap()).unwrap(),
		b"original bytes"
	);
	assert!(store.restore(&id).is_err());
	assert!(output.exists());
}

#[test]
#[cfg(unix)]
fn a_missing_jpeg_validator_retains_and_reports_the_original() {
	if !has_heic_decoder_and_jpeg_encoder() {
		return;
	}
	let dir = tempfile::tempdir().unwrap();
	let bin = dir.path().join("bin");
	fs::create_dir(&bin).unwrap();
	let path = std::env::var_os("PATH").unwrap_or_default();
	let magick = retype_core::tools::find_in_path(&path, "magick").unwrap();
	std::os::unix::fs::symlink(magick, bin.join("magick")).unwrap();
	let state = dir.path().join("state");
	let source = dir.path().join("photo.HEIC");
	let output = dir.path().join("photo.jpg");
	fs::write(&source, HEIC_FIXTURE).unwrap();
	let config = configuration(dir.path(), UndoPolicy::Enabled);
	let _daemon = start_daemon_with(&state, &config, None, Some(bin.as_os_str()));
	let mut convert = Command::new(env!("CARGO_BIN_EXE_retype"));
	convert
		.arg("convert")
		.arg(&source)
		.args(["jpg", "--no-spawn", "--state-dir"])
		.arg(&state)
		.env("RETYPE_CONFIG", &config);
	let converted = bounded(&mut convert, Duration::from_secs(20)).unwrap();
	assert!(converted.status.success(), "{}", converted.stderr);
	assert!(converted.stderr.contains("FFmpeg is missing"), "{}", converted.stderr);
	assert_eq!(fs::read(&source).unwrap(), HEIC_FIXTURE);
	assert_jpeg_dimensions(&output);
	let mut undo = Command::new(env!("CARGO_BIN_EXE_retype"));
	undo.args(["undo", "--last", "--no-spawn", "--state-dir"])
		.arg(&state)
		.env("RETYPE_CONFIG", &config);
	let undo = bounded(&mut undo, Duration::from_secs(10)).unwrap();
	assert!(undo.status.success(), "{}", undo.stderr);
	assert_eq!(fs::read(source).unwrap(), HEIC_FIXTURE);
	assert!(!output.exists());
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
