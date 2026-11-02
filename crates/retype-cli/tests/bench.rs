//! `retype bench` measures the machine, and `explain` answers from what it
//! measured.
//!
//! The tool here copies instead of converting, so the test proves the calibration
//! path rather than an encoder: a measurement is written, read back, and preferred
//! over the family constant.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

/// A configuration whose only tool copies, and whose image sample is a file the
/// test writes.
fn config(dir: &Path, sample: &Path) -> PathBuf {
	let (command, layout, args) = match cfg!(windows) {
		true => (
			"cmd",
			"layout = ['/c', 'copy', '{input}', '{ops}', '{output}']",
			format!("['/c', 'copy', '{}', '{{output}}']", sample.display()),
		),
		false => (
			"cp",
			"layout = ['{input}', '{ops}', '{output}']",
			format!("['{}', '{{output}}']", sample.display()),
		),
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

[bench.seed]
image = 'png'

[bench.sample.image.copy]
args = {args}
"
		),
	)
	.unwrap();

	config
}

#[test]
fn a_measured_pair_is_written_and_then_used_by_explain() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let sample = dir.path().join("pixels.png");
	fs::write(&sample, vec![b'p'; 40_000]).unwrap();
	let config = config(dir.path(), &sample);

	let benched = retype()
		.args(["bench", "--pair", "png:webp"])
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.output()
		.unwrap();

	assert!(benched.status.success(), "{}", String::from_utf8_lossy(&benched.stderr));

	let written = fs::read_to_string(state.join("bench.toml")).unwrap();
	assert!(written.contains("version = \"retype-bench-1\""), "{written}");
	assert!(written.contains("source = \"png\""), "{written}");
	assert!(written.contains("target = \"webp\""), "{written}");
	assert!(written.contains("backend = \"copy\""), "{written}");
	assert!(
		!written.contains("machine = \"\""),
		"the calibration names the machine it was taken on: {written}"
	);

	// The estimate is now this machine's rather than the image constant.
	let explained = retype()
		.args(["explain"])
		.arg(sample.to_str().unwrap())
		.arg("webp")
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.output()
		.unwrap();

	assert!(
		explained.status.success(),
		"{}",
		String::from_utf8_lossy(&explained.stderr)
	);

	let printed = String::from_utf8_lossy(&explained.stdout);
	assert!(printed.contains("estimate"), "{printed}");
	assert!(printed.contains("(measured)"), "{printed}");
}

#[test]
fn repeated_measurements_do_not_reuse_or_overwrite_previous_artifacts() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let sample = dir.path().join("pixels.png");
	fs::write(&sample, b"sample bytes").unwrap();
	let config = config(dir.path(), &sample);
	let old_output = state.join("bench/png-to-webp.webp");
	fs::create_dir_all(old_output.parent().unwrap()).unwrap();
	fs::write(&old_output, b"an earlier artifact").unwrap();

	for _ in 0..2 {
		let ran = retype()
			.args(["bench", "--pair", "png:webp"])
			.arg("--state-dir")
			.arg(&state)
			.env("RETYPE_CONFIG", &config)
			.output()
			.unwrap();
		assert!(ran.status.success(), "{}", String::from_utf8_lossy(&ran.stderr));
		assert_eq!(fs::read(&old_output).unwrap(), b"an earlier artifact");
		assert_eq!(
			fs::read_dir(state.join("bench")).unwrap().count(),
			1,
			"scratch directories are retired"
		);
	}
}

#[test]
fn a_pair_that_is_not_a_pair_is_a_usage_error() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let sample = dir.path().join("pixels.png");
	fs::write(&sample, b"pixels").unwrap();
	let config = config(dir.path(), &sample);

	let benched = retype()
		.args(["bench", "--pair", "png"])
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.output()
		.unwrap();

	assert_eq!(benched.status.code(), Some(2), "usage is exit 2");
}
