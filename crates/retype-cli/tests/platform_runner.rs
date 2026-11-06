//! The native shell runners' orchestration, using fake commands rather than codecs.
#![cfg(unix)]

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

#[path = "support/tools.rs"]
mod tool_support;

const CARGO: &str = r#"#!/bin/sh
printf '%s|%s|%s|%s|%s\n' "$*" "${RETYPE_REQUIRE_TEST_TOOLS-}" \
    "${RETYPE_REQUIRE_NATIVE_WORKFLOW-}" "${RETYPE_STATE_DIR-unset}" "${RETYPE_NO_SPAWN-unset}" >> "$MOCK_CARGO_RECORD"
case "$RETYPE_CONFIG" in "$TMPDIR"/*) ;; *) echo 'configuration is not isolated' >&2; exit 3;; esac
case "$XDG_STATE_HOME" in "$TMPDIR"/*) ;; *) echo 'state is not isolated' >&2; exit 3;; esac
grep -q '^enabled = false$' "$RETYPE_CONFIG" || exit 3
case "$1" in
    --version) echo 'cargo mock';;
    fmt|clippy) echo "mock $1";;
    test)
        case "$MOCK_MODE" in
            empty) exit 0;;
            hang) sleep 15;;
            leak) "$MOCK_RETYPE" "$TMPDIR/leak-state" >/dev/null 2>&1 & sleep 0.15;;
            skip) echo 'SKIPPED: mock missing prerequisite';;
            falsepass) echo 'test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out;';;
        esac
        echo 'test a_real_watched_heic_conversion_and_undo_preserve_the_original ... ok'
        echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s'
        if [ "$MOCK_MODE" = fail ]; then exit 101; fi;;
    *) exit 2;;
esac
"#;

const GIT: &str = r#"#!/bin/sh
case "$1" in
    rev-parse) echo '0123456789012345678901234567890123456789';;
    status) :;;
    *) exit 2;;
esac
"#;

const UNAME: &str = r#"#!/bin/sh
case "$1" in
    -s) echo "$MOCK_OS";;
    -m) echo x86_64;;
    -a) echo "$MOCK_OS mock host";;
    *) exit 2;;
esac
"#;

struct TestProcess(Child);

impl Drop for TestProcess {
	fn drop(&mut self) {
		let _ = self.0.kill();
		let _ = self.0.wait();
	}
}

struct Run {
	status: ExitStatus,
	text: String,
	invocations: String,
}

fn executable(path: &Path, text: &str) {
	fs::write(path, text).unwrap();
	fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn timeout_command() -> Option<PathBuf> {
	let path = std::env::var_os("PATH").unwrap_or_default();
	let command = retype_core::tools::find_in_path(&path, "timeout")
		.or_else(|| retype_core::tools::find_in_path(&path, "gtimeout"));
	if command.is_none() {
		tool_support::unavailable("GNU timeout or gtimeout for platform runner tests");
	}
	command
}

fn run(platform: &str, host: &str, mode: &str, limit: u64) -> Option<Run> {
	let timeout = timeout_command()?;
	let dir = tempfile::Builder::new()
		.prefix("retype-runner-test.")
		.tempdir_in("/tmp")
		.unwrap();
	let bin = dir.path().join("bin");
	fs::create_dir(&bin).unwrap();
	executable(&bin.join("cargo"), CARGO);
	executable(&bin.join("git"), GIT);
	executable(&bin.join("uname"), UNAME);
	executable(
		&bin.join("retype"),
		"#!/bin/sh\ni=0\nwhile [ \"$i\" -lt 100 ]; do sleep 0.1; i=$((i + 1)); done\n",
	);
	for tool in ["rustc", "magick", "ffmpeg", "ffprobe", "pandoc", "7zz", "typst"] {
		executable(&bin.join(tool), "#!/bin/sh\necho 'mock tool'\n");
	}
	symlink(&timeout, bin.join("timeout")).unwrap();
	symlink(&timeout, bin.join("gtimeout")).unwrap();

	let path = std::env::join_paths(
		std::iter::once(bin.clone()).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())),
	)
	.unwrap();
	let root = Path::new(env!("CARGO_MANIFEST_DIR"))
		.parent()
		.unwrap()
		.parent()
		.unwrap();
	let record = dir.path().join("cargo-invocations");
	let mut output = tempfile::tempfile().unwrap();
	let mut command = Command::new(root.join("scripts").join(platform));
	command
		.env("PATH", path)
		.env("MOCK_OS", host)
		.env("MOCK_MODE", mode)
		.env("MOCK_CARGO_RECORD", &record)
		.env("MOCK_RETYPE", bin.join("retype"))
		.env("RETYPE_TEST_REPORT_DIR", dir.path().join("reports"))
		.env("RETYPE_TEST_TEMP_DIR", dir.path())
		.env("RETYPE_TEST_TIMEOUT_SECONDS", limit.to_string())
		.env("RETYPE_STATE_DIR", dir.path().join("must-not-be-used"))
		.env("RETYPE_NO_SPAWN", "1")
		.stdout(Stdio::from(output.try_clone().unwrap()))
		.stderr(Stdio::from(output.try_clone().unwrap()));
	let mut child = TestProcess(command.spawn().unwrap());
	let deadline = Instant::now() + Duration::from_secs(30);
	let status = loop {
		if let Some(status) = child.0.try_wait().unwrap() {
			break status;
		}
		assert!(Instant::now() < deadline, "platform runner did not stop");
		std::thread::sleep(Duration::from_millis(25));
	};
	output.seek(SeekFrom::Start(0)).unwrap();
	let mut text = String::new();
	output.read_to_string(&mut text).unwrap();
	let invocations = fs::read_to_string(record).unwrap_or_default();
	Some(Run {
		status,
		text,
		invocations,
	})
}

#[test]
fn linux_runner_reports_a_failed_gate_and_runs_all_steps() {
	let Some(result) = run("test-linux.sh", "Linux", "fail", 5) else {
		return;
	};
	assert!(!result.status.success());
	assert!(result.text.contains("installer: command exit 0"), "{}", result.text);
	assert!(result.text.contains("workspace: command exit 101"), "{}", result.text);
	assert!(result.invocations.contains("fmt --all -- --check|1|1|unset|unset"));
	assert!(
		result
			.invocations
			.contains("clippy --workspace --all-targets --locked -- -D warnings|1|1|unset|unset")
	);
	assert!(
		result
			.invocations
			.contains("test --workspace --locked --no-fail-fast -- --nocapture|1|1|unset|unset")
	);
}

#[test]
fn the_runner_does_not_accept_a_skipped_test_as_success() {
	let Some(result) = run("test-linux.sh", "Linux", "skip", 5) else {
		return;
	};
	assert!(!result.status.success());
	assert!(result.text.contains("skip or ignored test"), "{}", result.text);
}

#[test]
fn the_runner_rejects_a_failed_summary_even_if_cargo_exits_zero() {
	let Some(result) = run("test-linux.sh", "Linux", "falsepass", 5) else {
		return;
	};
	assert!(!result.status.success());
	assert!(result.text.contains("failed test summary"));
}

#[test]
fn the_runner_rejects_an_empty_workspace_result() {
	let Some(result) = run("test-linux.sh", "Linux", "empty", 5) else {
		return;
	};
	assert!(!result.status.success());
	assert!(result.text.contains("strict native workflow test did not run"));
	assert!(result.text.contains("No passing test summary was recorded"));
}

#[test]
fn the_macos_wrapper_uses_the_native_runner_contract() {
	let Some(result) = run("test-macos.sh", "Darwin", "pass", 5) else {
		return;
	};
	assert!(result.status.success(), "{}", result.text);
	assert!(result.text.contains("Platform test: Darwin"));
	assert!(result.text.contains("Platform validation passed."));
}

#[test]
fn leaked_test_processes_are_stopped_and_reported() {
	let Some(result) = run("test-linux.sh", "Linux", "leak", 5) else {
		return;
	};
	assert!(!result.status.success());
	assert!(result.text.contains("Stopping leaked test process"), "{}", result.text);
}

#[test]
fn a_runner_refuses_a_different_host_os() {
	let Some(result) = run("test-macos.sh", "Linux", "pass", 5) else {
		return;
	};
	assert_eq!(result.status.code(), Some(2));
	assert!(result.text.contains("Run this script on Darwin, not Linux."));
	assert!(result.invocations.is_empty());
}

#[test]
fn a_runner_rejects_an_invalid_timeout() {
	let Some(result) = run("test-linux.sh", "Linux", "pass", 99_999) else {
		return;
	};
	assert_eq!(result.status.code(), Some(2));
	assert!(result.text.contains("between 1 and 3600"));
	assert!(result.invocations.is_empty());
}

#[test]
fn a_timed_out_workspace_is_a_failure() {
	let Some(result) = run("test-linux.sh", "Linux", "hang", 2) else {
		return;
	};
	assert!(!result.status.success());
	assert!(result.text.contains("workspace: command exit 124"), "{}", result.text);
}
