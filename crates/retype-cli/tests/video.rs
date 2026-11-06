//! The video and audio vertical, against the real encoder.
//!
//! Skipped when FFmpeg is missing: the rest of the suite proves the pipeline with
//! a copy tool, and this file is the one that has to touch a real codec.

#[path = "support/commands.rs"]
mod commands;
#[path = "support/tools.rs"]
mod tool_support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use commands::{CommandResult, bounded};

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const CONVERSION_TIMEOUT: Duration = Duration::from_secs(30);

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

fn checked(command: &mut Command, stage: &str, limit: Duration) -> CommandResult {
	bounded(command, limit).unwrap_or_else(|error| panic!("{stage} failed: {error}"))
}

fn have_tool(name: &str) -> bool {
	let mut version = Command::new(name);
	version.arg("-version");
	match bounded(&mut version, PROBE_TIMEOUT) {
		Ok(output) => output.status.success(),
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
		Err(error) => panic!("{name} version probe failed: {error}"),
	}
}

/// The encoders the fixture and the targets under test ask for: `libx264` for the
/// clip and the mkv target, `aac` for the clip, `libmp3lame` for the sound.
const ENCODERS: [&str; 3] = ["libx264", "aac", "libmp3lame"];

/// What this host's FFmpeg cannot do, or nothing when it can do all of it.
///
/// A distribution build is not the same FFmpeg everywhere: Fedora ships
/// `ffmpeg-free`, which has no `libx264`, so the fixture clip cannot be made and
/// the mkv, mp4 and mov targets have no encoder. A tool that is absent skips in
/// this suite rather than failing, and a tool missing an encoder is the same thing
/// for a target that names one, `docs/tools.md` §6.
fn cannot_run() -> Option<String> {
	if !have_tool("ffmpeg") {
		return Some("no ffmpeg on this machine".to_string());
	}
	if !have_tool("ffprobe") {
		return Some("no ffprobe on this machine".to_string());
	}

	let mut list = Command::new("ffmpeg");
	list.args(["-hide_banner", "-encoders"]);
	let listed = checked(&mut list, "ffmpeg encoder enumeration", PROBE_TIMEOUT);
	if !listed.status.success() {
		return Some(format!("ffmpeg -encoders failed: {}", listed.stderr));
	}

	for encoder in ENCODERS {
		let present = listed
			.stdout
			.lines()
			.any(|line| line.split_whitespace().nth(1) == Some(encoder));

		if !present {
			return Some(format!("this ffmpeg has no {encoder} encoder"));
		}
	}

	None
}

/// Skip with a reason, which is how this suite treats a host that cannot run it.
fn skip_if_cannot_run() -> bool {
	match cannot_run() {
		Some(reason) => {
			tool_support::unavailable(reason);
			true
		}
		None => false,
	}
}

fn config(dir: &Path) -> PathBuf {
	let config = dir.join("retype.toml");
	fs::write(&config, "version = 'retype-config-1'\n[notify]\nenabled = false\n").unwrap();
	config
}

/// Three seconds of test pattern with a tone, made by the encoder itself.
fn sample(dir: &Path) -> PathBuf {
	let path = dir.join("clip.mp4");
	let mut ffmpeg = Command::new("ffmpeg");
	ffmpeg
		.args(["-v", "error", "-y", "-f", "lavfi", "-i"])
		.arg("testsrc=size=320x240:rate=25:duration=3")
		.args(["-f", "lavfi", "-i", "sine=frequency=440:duration=3"])
		.args(["-c:v", "libx264", "-c:a", "aac", "-shortest"])
		.arg(&path);
	let made = checked(&mut ffmpeg, "ffmpeg sample generation", CONVERSION_TIMEOUT);
	assert!(
		made.status.success(),
		"the fixture clip could not be made: {}",
		made.stderr
	);
	path
}

/// A property of the output, asked of the encoder rather than guessed.
fn probe(path: &Path, entries: &str) -> String {
	let mut ffprobe = Command::new("ffprobe");
	ffprobe
		.args(["-v", "error", "-select_streams", "v:0", "-show_entries"])
		.arg(entries)
		.args(["-of", "csv=p=0"])
		.arg(path);
	let output = checked(&mut ffprobe, "ffprobe video properties", PROBE_TIMEOUT);
	assert!(output.status.success(), "{}", output.stderr);
	output.stdout.trim().to_string()
}

fn convert(dir: &Path, config: &Path, from: &str, intent: &str) -> CommandResult {
	let input = dir.join(from);
	let mut command = retype();
	command
		.args(["convert", input.to_str().unwrap(), intent])
		.arg("--state-dir")
		.arg(dir.join("state"))
		.env("RETYPE_CONFIG", config);
	checked(&mut command, "retype video conversion", CONVERSION_TIMEOUT)
}

struct Cleanup(PathBuf);

impl Drop for Cleanup {
	fn drop(&mut self) {
		let mut stop = retype();
		stop.args(["daemon", "stop"]).arg("--state-dir").arg(&self.0);
		match bounded(&mut stop, PROBE_TIMEOUT) {
			Ok(output) if output.status.success() => {}
			Ok(output) => eprintln!("could not stop video test daemon: {}", output.stderr),
			Err(error) => eprintln!("could not stop video test daemon: {error}"),
		}
	}
}

#[test]
fn a_container_comes_out_as_the_other_container() {
	if skip_if_cannot_run() {
		return;
	}

	let dir = tempfile::tempdir().unwrap();
	let config = config(dir.path());
	sample(dir.path());
	let _cleanup = Cleanup(dir.path().join("state"));

	let converted = convert(dir.path(), &config, "clip.mp4", "mkv");
	assert_eq!(converted.status.code(), Some(0), "{}", converted.stderr);

	let output = dir.path().join("clip.mkv");
	assert!(output.exists());

	let mut ffprobe = Command::new("ffprobe");
	ffprobe
		.args(["-v", "error", "-show_entries", "stream=codec_name", "-of", "csv=p=0"])
		.arg(&output);
	let codecs = checked(&mut ffprobe, "ffprobe codec list", PROBE_TIMEOUT);
	assert!(codecs.status.success(), "{}", codecs.stderr);
	assert!(codecs.stdout.contains("h264"), "{}", codecs.stdout);
	assert!(codecs.stdout.contains("aac"), "{}", codecs.stdout);
}

#[test]
fn a_cap_scales_without_stretching_and_mute_removes_the_sound() {
	if skip_if_cannot_run() {
		return;
	}

	let dir = tempfile::tempdir().unwrap();
	let config = config(dir.path());
	sample(dir.path());
	let _cleanup = Cleanup(dir.path().join("state"));

	let converted = convert(dir.path(), &config, "clip.mp4", "mkv+cap120+mute");
	assert_eq!(converted.status.code(), Some(0), "{}", converted.stderr);

	let output = dir.path().join("clip-cap120-mute.mkv");
	assert_eq!(
		probe(&output, "stream=width,height"),
		"120,90",
		"the aspect ratio is kept"
	);

	let mut ffprobe = Command::new("ffprobe");
	ffprobe
		.args(["-v", "error", "-show_entries", "stream=codec_type", "-of", "csv=p=0"])
		.arg(&output);
	let streams = checked(&mut ffprobe, "ffprobe stream list", PROBE_TIMEOUT);
	assert!(streams.status.success(), "{}", streams.stderr);
	assert_eq!(streams.stdout.trim(), "video", "no audio stream survives mute");
}

#[test]
fn extracting_the_sound_is_a_conversion() {
	if skip_if_cannot_run() {
		return;
	}

	let dir = tempfile::tempdir().unwrap();
	let config = config(dir.path());
	sample(dir.path());
	let _cleanup = Cleanup(dir.path().join("state"));

	let converted = convert(dir.path(), &config, "clip.mp4", "mp3+at1s");
	assert_eq!(converted.status.code(), Some(0), "{}", converted.stderr);

	let output = dir.path().join("clip-at1s.mp3");
	let mut ffprobe = Command::new("ffprobe");
	ffprobe
		.args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
		.arg(&output);
	let duration = checked(&mut ffprobe, "ffprobe audio duration", PROBE_TIMEOUT);
	assert!(duration.status.success(), "{}", duration.stderr);
	let duration: f64 = duration.stdout.trim().parse().unwrap();

	// Three seconds from one second in: the seek is exact, not keyframe-exact.
	assert!((duration - 2.0).abs() < 0.2, "duration was {duration}");
}
