//! The property fixtures, `docs/fidelity.md` §4.
//!
//! A tier says what a conversion promises; an axis says what was checked. These tests
//! are the check itself: the fixtures carry the properties they declare, a pair that
//! keeps them says so, a pair that does not says which one it lost, and a pair declared
//! `exact` cannot be earned without either.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use retype_core::axis::{Axis, Fixtures, Probe, Verdict, compare};

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

/// A guard for the daemon that `convert` starts when there is none: this test asks for
/// one conversion and should not leave a process behind, `tests/documents.rs`.
struct Autostarted {
	state: PathBuf,
}

impl Drop for Autostarted {
	fn drop(&mut self) {
		let _ = retype()
			.args(["daemon", "stop", "--force"])
			.arg("--state-dir")
			.arg(&self.state)
			.output();
	}
}

fn fixtures() -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/fidelity")
}

/// Skip when this machine cannot check a fixture property, naming what is missing.
fn skip_if_no_decoder() -> bool {
	match Probe::read(&fixtures().join("alpha.png")) {
		Ok(_) => false,
		Err(error) => {
			eprintln!("{error}, so this test proves nothing");
			true
		}
	}
}

/// The fixtures are evidence, so the declaration beside them has to be true of the bytes.
#[test]
fn every_fixture_carries_the_property_it_declares() {
	let directory = fixtures();
	if skip_if_no_decoder() {
		return;
	}

	let declared = Fixtures::open(&directory).expect("the fixture declaration");

	assert!(!declared.fixture.is_empty(), "no fixtures were declared");

	for fixture in &declared.fixture {
		let probe = Probe::read(&directory.join(&fixture.file)).expect("a readable fixture");

		for axis in &fixture.axes {
			let carried = match axis {
				Axis::Alpha => !probe.opaque,
				Axis::Animation => probe.scenes >= 2,
				Axis::Depth => probe.depth >= 16,
				Axis::Icc => probe.icc,
				Axis::Orientation => probe.orientation >= 2,
				Axis::Palette => probe.colors <= 8,
			};

			assert!(carried, "{} does not carry {}: {probe:?}", fixture.file, axis.as_str());
		}
	}
}

/// A property the target cannot carry is lost, and the check says so rather than
/// passing because the source never had it.
#[test]
fn a_transparent_fixture_told_the_truth_about_jpeg() {
	let directory = fixtures();
	if skip_if_no_decoder() {
		return;
	}

	let alpha = Probe::read(&directory.join("alpha.png")).expect("a readable fixture");

	let work = tempfile::tempdir().unwrap();
	let flat = work.path().join("flat.jpg");
	let state = work.path().join("state");
	let _daemon = Autostarted { state: state.clone() };

	let ran = retype()
		.arg("convert")
		.arg(directory.join("alpha.png"))
		.arg("jpg")
		.arg("--output")
		.arg(&flat)
		.arg("--state-dir")
		.arg(&state)
		.output()
		.unwrap();

	assert!(ran.status.success(), "{}", String::from_utf8_lossy(&ran.stderr));

	let output = Probe::read(&flat).expect("a readable output");
	let fixture = Fixtures::open(&directory).unwrap();
	let declared = fixture
		.for_format("png")
		.find(|fixture| fixture.file == "alpha.png")
		.expect("the alpha fixture");
	let checks = compare(declared, &alpha, &output);

	assert_eq!(checks[0].axis, Axis::Alpha);
	assert_eq!(checks[0].verdict, Verdict::Lost);
}

fn config(path: &Path, target_tier: &str) {
	fs::write(
		path,
		format!(
			"\
version = 'retype-config-1'

[formats.png]
family = 'image'
lossy = false
alpha = true
tier = 'exact'

[formats.tiff]
family = 'image'
lossy = false
alpha = true
tier = '{target_tier}'
"
		),
	)
	.unwrap();
}

fn fidelity(config: &Path, fixtures: &Path, state: &Path) -> Output {
	retype()
		.args(["fidelity", "--pair", "png:tiff"])
		.arg("--fixtures")
		.arg(fixtures)
		.arg("--write")
		.arg(state.join("fidelity.toml"))
		.env("RETYPE_CONFIG", config)
		.arg("--state-dir")
		.arg(state)
		.output()
		.unwrap()
}

/// No fixture, no evidence: an `exact` pair cannot be earned by a conversion that
/// proved nothing about what it kept.
#[test]
fn an_exact_pair_without_a_fixture_is_a_failure() {
	let work = tempfile::tempdir().unwrap();
	if skip_if_no_decoder() {
		return;
	}

	let empty = work.path().join("no-fixtures");
	fs::create_dir(&empty).unwrap();
	let config_path = work.path().join("retype.toml");
	config(&config_path, "exact");

	let ran = fidelity(&config_path, &empty, &work.path().join("state"));
	let stderr = String::from_utf8_lossy(&ran.stderr);

	assert_eq!(ran.status.code(), Some(4), "{stderr}");
	assert!(stderr.contains("no fixture carries a property for png"), "{stderr}");
}

/// With the fixtures, the same pair earns a claim that names what it kept.
#[test]
fn an_exact_pair_with_fixtures_records_what_it_kept() {
	let work = tempfile::tempdir().unwrap();
	if skip_if_no_decoder() {
		return;
	}

	let config_path = work.path().join("retype.toml");
	config(&config_path, "exact");
	let state = work.path().join("state");

	let ran = fidelity(&config_path, &fixtures(), &state);
	let stdout = String::from_utf8_lossy(&ran.stdout);
	let stderr = String::from_utf8_lossy(&ran.stderr);

	assert!(ran.status.success(), "stdout: {stdout} stderr: {stderr}");
	assert!(stdout.contains("alpha"), "{stdout}");

	// The manifest is the release artifact, so the properties have to survive a round
	// trip through it, not just the run.
	let manifest = retype_core::fidelity::Manifest::open(&state.join("fidelity.toml")).expect("a manifest");
	let claim = manifest.get("png", "tiff", "magick").expect("the claim");

	assert!(claim.axes.contains(&Axis::Alpha), "{:?}", claim.axes);
	assert!(claim.lost.is_empty(), "{:?}", claim.lost);
}
