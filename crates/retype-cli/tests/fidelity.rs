//! `retype fidelity` earns claims, records the build, and refuses a stale `exact`
//! claim, `docs/fidelity.md` §4 and §6.
//!
//! The tool copies, so the test proves the claim machinery rather than an encoder.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

fn config(dir: &Path, sample: &Path) -> PathBuf {
	// The probe has to answer on every host: BSD `cp` has no `--version`, and a
	// tool that declares a probe but reports no version is unusable, so the
	// fixture found no backend at all on macOS. `sh` and `cmd` carry the copy and
	// can answer with a version, which the claim also records as a fingerprint.
	let (command, probe, layout, args) = match cfg!(windows) {
		true => (
			"cmd",
			"['/c', 'echo retype-fidelity 1.0']",
			"layout = ['/c', 'copy', '{input}', '{output}', '{ops}']",
			format!("['/c', 'copy', '{}', '{{output}}']", sample.display()),
		),
		false => (
			"sh",
			"['-c', 'echo retype-fidelity 1.0']",
			"layout = ['-c', 'cp \"$1\" \"$2\"', '-', '{input}', '{output}', '{ops}']",
			format!("['-c', 'cp \"$1\" \"$2\"', '-', '{}', '{{output}}']", sample.display()),
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
probe = {probe}
{layout}

[formats.png]
family = 'image'
lossy = false
alpha = true
tier = 'exact'

[formats.bmp]
family = 'image'
lossy = false
alpha = true
tier = 'exact'

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

struct Fixture {
	dir: tempfile::TempDir,
	config: PathBuf,
	state: PathBuf,
	manifest: PathBuf,
	fixtures: PathBuf,
}

impl Fixture {
	fn new() -> Self {
		let dir = tempfile::tempdir().unwrap();
		let sample = dir.path().join("pixels.png");
		fs::write(&sample, vec![b'p'; 20_000]).unwrap();

		// One fixture, copied by the backend like everything else, so the claim rests on
		// a property that really was checked rather than on no fixture at all.
		let fixtures = dir.path().join("fixtures");
		fs::create_dir(&fixtures).unwrap();
		fs::copy(
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/fidelity/alpha.png"),
			fixtures.join("alpha.png"),
		)
		.unwrap();
		fs::write(
			fixtures.join("fixtures.toml"),
			"\
version = 'retype-fixtures-1'

[[fixture]]
file = 'alpha.png'
format = 'png'
axes = ['alpha']
",
		)
		.unwrap();

		Self {
			config: config(dir.path(), &sample),
			state: dir.path().join("state"),
			manifest: dir.path().join("fidelity.toml"),
			fixtures,
			dir,
		}
	}

	fn run(&self, arguments: &[&str]) -> std::process::Output {
		let mut command = retype();
		command
			.args(["fidelity", "--pair", "png:bmp"])
			.arg("--fixtures")
			.arg(&self.fixtures)
			.arg("--state-dir")
			.arg(&self.state)
			.env("RETYPE_CONFIG", &self.config);
		command.args(arguments);
		command.output().unwrap()
	}
}

#[test]
fn a_run_earns_a_claim_and_records_the_build() {
	let fixture = Fixture::new();

	let ran = fixture.run(&["--write", fixture.manifest.to_str().unwrap()]);
	assert!(ran.status.success(), "{}", String::from_utf8_lossy(&ran.stderr));

	let written = fs::read_to_string(&fixture.manifest).unwrap();
	assert!(written.contains("version = \"retype-fidelity-1\""), "{written}");
	assert!(written.contains("source = \"png\""), "{written}");
	assert!(written.contains("target = \"bmp\""), "{written}");
	assert!(written.contains("tier = \"exact\""), "{written}");
	assert!(written.contains("fingerprint = \""), "{written}");
	assert!(written.contains("fixture = \"generated:"), "{written}");
}

#[test]
fn a_stale_exact_claim_fails_the_run() {
	let fixture = Fixture::new();

	let first = fixture.run(&["--write", fixture.manifest.to_str().unwrap()]);
	assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));

	// Another build of the same tool: the claim is no longer supported.
	let recorded = fs::read_to_string(&fixture.manifest).unwrap();
	let stale = recorded
		.split("fingerprint = \"")
		.nth(1)
		.and_then(|rest| rest.split('"').next())
		.unwrap()
		.to_string();
	fs::write(
		&fixture.manifest,
		recorded.replace(&stale, "00000000000000000000000000000000"),
	)
	.unwrap();

	let second = fixture.run(&["--manifest", fixture.manifest.to_str().unwrap()]);

	assert_eq!(second.status.code(), Some(4), "a stale exact claim is exit 4");
	assert!(
		String::from_utf8_lossy(&second.stderr).contains("exact claim"),
		"{}",
		String::from_utf8_lossy(&second.stderr)
	);
	assert!(
		String::from_utf8_lossy(&second.stdout).contains("unverified"),
		"{}",
		String::from_utf8_lossy(&second.stdout)
	);
}

#[test]
fn the_claims_table_is_rendered_between_the_markers() {
	let fixture = Fixture::new();
	let document = fixture.dir.path().join("fidelity.md");
	fs::write(
		&document,
		"# Fidelity\n\nProse above.\n\n<!-- claims:start -->\n<!-- claims:end -->\n\nProse below.\n",
	)
	.unwrap();

	let ran = fixture.run(&[
		"--write",
		fixture.manifest.to_str().unwrap(),
		"--render",
		document.to_str().unwrap(),
	]);
	assert!(ran.status.success(), "{}", String::from_utf8_lossy(&ran.stderr));

	let rendered = fs::read_to_string(&document).unwrap();
	assert!(rendered.contains("Prose above."), "{rendered}");
	assert!(rendered.contains("Prose below."), "{rendered}");
	assert!(rendered.contains("| png | bmp | exact | copy |"), "{rendered}");
}
