//! The video and audio vertical, against the real encoder.
//!
//! Skipped when FFmpeg is missing: the rest of the suite proves the pipeline with
//! a copy tool, and this file is the one that has to touch a real codec.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

fn have_ffmpeg() -> bool {
	Command::new("ffmpeg")
		.arg("-version")
		.stdout(std::process::Stdio::null())
		.stderr(std::process::Stdio::null())
		.status()
		.map(|status| status.success())
		.unwrap_or(false)
}

fn config(dir: &Path) -> PathBuf {
	let config = dir.join("retype.toml");
	fs::write(&config, "version = 'retype-config-1'\n").unwrap();
	config
}

/// Three seconds of test pattern with a tone, made by the encoder itself.
fn sample(dir: &Path) -> PathBuf {
	let path = dir.join("clip.mp4");
	let made = Command::new("ffmpeg")
		.args(["-v", "error", "-y", "-f", "lavfi", "-i"])
		.arg("testsrc=size=320x240:rate=25:duration=3")
		.args(["-f", "lavfi", "-i", "sine=frequency=440:duration=3"])
		.args(["-c:v", "libx264", "-c:a", "aac", "-shortest"])
		.arg(&path)
		.status()
		.unwrap();

	assert!(made.success(), "the fixture clip could not be made");
	path
}

/// A property of the output, asked of the encoder rather than guessed.
fn probe(path: &Path, entries: &str) -> String {
	let output = Command::new("ffprobe")
		.args(["-v", "error", "-select_streams", "v:0", "-show_entries"])
		.arg(entries)
		.args(["-of", "csv=p=0"])
		.arg(path)
		.output()
		.unwrap();

	String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn convert(dir: &Path, config: &Path, from: &str, intent: &str) -> std::process::Output {
	let input = dir.join(from);

	retype()
		.args(["convert", input.to_str().unwrap(), intent])
		.arg("--state-dir")
		.arg(dir.join("state"))
		.env("RETYPE_CONFIG", config)
		.output()
		.unwrap()
}

struct Cleanup(PathBuf);

impl Drop for Cleanup {
	fn drop(&mut self) {
		let _ = retype()
			.args(["daemon", "stop"])
			.arg("--state-dir")
			.arg(&self.0)
			.output();
	}
}

#[test]
fn a_container_comes_out_as_the_other_container() {
	if !have_ffmpeg() {
		return;
	}

	let dir = tempfile::tempdir().unwrap();
	let config = config(dir.path());
	sample(dir.path());
	let _cleanup = Cleanup(dir.path().join("state"));

	let converted = convert(dir.path(), &config, "clip.mp4", "mkv");
	assert_eq!(
		converted.status.code(),
		Some(0),
		"{}",
		String::from_utf8_lossy(&converted.stderr)
	);

	let output = dir.path().join("clip.mkv");
	assert!(output.exists());

	let codecs = Command::new("ffprobe")
		.args(["-v", "error", "-show_entries", "stream=codec_name", "-of", "csv=p=0"])
		.arg(&output)
		.output()
		.unwrap();
	let codecs = String::from_utf8_lossy(&codecs.stdout).to_string();

	assert!(codecs.contains("h264"), "{codecs}");
	assert!(codecs.contains("aac"), "{codecs}");
}

#[test]
fn a_cap_scales_without_stretching_and_mute_removes_the_sound() {
	if !have_ffmpeg() {
		return;
	}

	let dir = tempfile::tempdir().unwrap();
	let config = config(dir.path());
	sample(dir.path());
	let _cleanup = Cleanup(dir.path().join("state"));

	let converted = convert(dir.path(), &config, "clip.mp4", "mkv+cap120+mute");
	assert_eq!(
		converted.status.code(),
		Some(0),
		"{}",
		String::from_utf8_lossy(&converted.stderr)
	);

	let output = dir.path().join("clip-cap120-mute.mkv");
	assert_eq!(
		probe(&output, "stream=width,height"),
		"120,90",
		"the aspect ratio is kept"
	);

	let streams = Command::new("ffprobe")
		.args(["-v", "error", "-show_entries", "stream=codec_type", "-of", "csv=p=0"])
		.arg(&output)
		.output()
		.unwrap();
	let streams = String::from_utf8_lossy(&streams.stdout).to_string();

	assert_eq!(streams.trim(), "video", "no audio stream survives mute");
}

#[test]
fn extracting_the_sound_is_a_conversion() {
	if !have_ffmpeg() {
		return;
	}

	let dir = tempfile::tempdir().unwrap();
	let config = config(dir.path());
	sample(dir.path());
	let _cleanup = Cleanup(dir.path().join("state"));

	let converted = convert(dir.path(), &config, "clip.mp4", "mp3+at1s");
	assert_eq!(
		converted.status.code(),
		Some(0),
		"{}",
		String::from_utf8_lossy(&converted.stderr)
	);

	let output = dir.path().join("clip-at1s.mp3");
	let duration = Command::new("ffprobe")
		.args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
		.arg(&output)
		.output()
		.unwrap();
	let duration: f64 = String::from_utf8_lossy(&duration.stdout).trim().parse().unwrap();

	// Three seconds from one second in: the seek is exact, not keyframe-exact.
	assert!((duration - 2.0).abs() < 0.2, "duration was {duration}");
}
