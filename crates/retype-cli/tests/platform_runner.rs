//! The native shell runners' orchestration, using fake commands rather than codecs.
#![cfg(unix)]

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

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
            stubborn|stubborn-leak)
                /bin/sh -c 'trap "" TERM; echo $$ > "$1"; while :; do sleep 0.1; done' sh "$MOCK_CHILD_PID" &
                if [ "$MOCK_MODE" = stubborn ]; then wait; else sleep 0.15; fi;;
            leak) "$MOCK_RETYPE" "$TMPDIR/leak-state" >/dev/null 2>&1 & sleep 0.15;;
            skip) echo 'SKIPPED: mock missing prerequisite';;
            falsepass) echo 'test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out;';;
        esac
        echo 'test a_real_watched_heic_conversion_and_undo_preserve_the_original ... ok'
        if [ "$MOCK_MODE" != missing-direct ]; then
            echo 'test a_real_direct_heic_conversion_and_undo_follow_watched_disposition ... ok'
        fi
        echo 'test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s'
        if [ "$MOCK_MODE" = fail ]; then exit 101; fi;;
    *) exit 2;;
esac
"#;

const TIMEOUT_SHIM: &str = r#"#!/bin/sh
grace=${1#--kill-after=}
grace=${grace%s}
seconds=${2%s}
shift 2
exec /usr/bin/perl "$MOCK_BOUNDED" "$seconds" "$grace" "$@"
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
		if self.0.try_wait().ok().flatten().is_some() {
			return;
		}
		unsafe { libc::kill(self.0.id() as i32, libc::SIGTERM) };
		let deadline = Instant::now() + Duration::from_secs(2);
		while Instant::now() < deadline {
			if self.0.try_wait().ok().flatten().is_some() {
				return;
			}
			std::thread::sleep(Duration::from_millis(25));
		}
		let _ = self.0.kill();
		let deadline = Instant::now() + Duration::from_secs(2);
		while Instant::now() < deadline {
			if self.0.try_wait().ok().flatten().is_some() {
				return;
			}
			std::thread::sleep(Duration::from_millis(25));
		}
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

fn capture(command: &mut Command, limit: Duration) -> (ExitStatus, String) {
	let mut output = tempfile::tempfile().unwrap();
	command
		.stdout(Stdio::from(output.try_clone().unwrap()))
		.stderr(Stdio::from(output.try_clone().unwrap()));
	let mut child = TestProcess(command.spawn().unwrap());
	let deadline = Instant::now() + limit;
	let status = loop {
		if let Some(status) = child.0.try_wait().unwrap() {
			break status;
		}
		assert!(Instant::now() < deadline, "test command did not stop");
		std::thread::sleep(Duration::from_millis(25));
	};
	output.seek(SeekFrom::Start(0)).unwrap();
	let mut text = String::new();
	output.read_to_string(&mut text).unwrap();
	(status, text)
}

struct ChildMarker(PathBuf);

impl ChildMarker {
	fn running(&self) -> Result<Option<(i32, String)>, String> {
		let contents = match fs::read_to_string(&self.0) {
			Ok(contents) => contents,
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
			Err(error) => return Err(error.to_string()),
		};
		let pid = contents.trim().parse::<i32>().map_err(|error| error.to_string())?;
		let inspection = Command::new("ps")
			.args(["-ww", "-p", &pid.to_string(), "-o", "stat=,command="])
			.output()
			.map_err(|error| error.to_string())?;
		if !inspection.status.success() && !inspection.stderr.is_empty() {
			return Err(String::from_utf8_lossy(&inspection.stderr).into_owned());
		}
		let description = String::from_utf8_lossy(&inspection.stdout).into_owned();
		let state = description.split_whitespace().next().unwrap_or_default();
		if state.is_empty() || state.starts_with('Z') || !description.contains(&*self.0.to_string_lossy()) {
			return Ok(None);
		}
		Ok(Some((pid, description)))
	}

	fn assert_stopped(&self) {
		assert!(self.0.exists(), "the test child did not start");
		let running = self.running().expect("cannot inspect the test child");
		assert!(running.is_none(), "test child was left running: {running:?}");
	}
}

impl Drop for ChildMarker {
	fn drop(&mut self) {
		match self.running() {
			Ok(Some((pid, _))) => {
				/* Stop only a process whose command line contains this test's marker. */
				unsafe { libc::kill(pid, libc::SIGKILL) };
			}
			Ok(None) => {}
			Err(error) => eprintln!("could not inspect the test child during cleanup: {error}"),
		}
	}
}

fn run(platform: &str, host: &str, mode: &str, limit: u64) -> Run {
	let dir = tempfile::Builder::new().prefix("rt.").tempdir_in("/tmp").unwrap();
	let scratch = fs::canonicalize(dir.path()).unwrap().join("retype-test.XXXXXXXX");
	assert!(
		scratch.as_os_str().len() <= 56,
		"test scratch path is too long: {scratch:?}"
	);
	let bin = dir.path().join("bin");
	fs::create_dir(&bin).unwrap();
	executable(&bin.join("cargo"), CARGO);
	executable(&bin.join("git"), GIT);
	executable(&bin.join("uname"), UNAME);
	executable(&bin.join("getconf"), "#!/bin/sh\necho 'glibc 2.40'\n");
	executable(
		&bin.join("retype"),
		"#!/bin/sh\ni=0\nwhile [ \"$i\" -lt 100 ]; do sleep 0.1; i=$((i + 1)); done\n",
	);
	for tool in ["rustc", "magick", "ffmpeg", "ffprobe", "pandoc", "7zz", "typst"] {
		executable(&bin.join(tool), "#!/bin/sh\necho 'mock tool'\n");
	}
	if platform == "test-linux.sh" {
		let path = std::env::var_os("PATH").unwrap_or_default();
		let timeout = if cfg!(target_os = "linux") {
			retype_core::tools::find_in_path(&path, "timeout")
		} else {
			None
		};
		if let Some(timeout) = timeout {
			symlink(timeout, bin.join("timeout")).unwrap();
		} else {
			executable(&bin.join("timeout"), TIMEOUT_SHIM);
		}
	} else {
		for tool in ["timeout", "gtimeout"] {
			executable(
				&bin.join(tool),
				"#!/bin/sh\necho 'unexpected GNU timeout' >&2\nexit 99\n",
			);
		}
	}

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
	let child_pid = ChildMarker(dir.path().join("stubborn-child.pid"));
	let mut command = Command::new(root.join("scripts").join(platform));
	command
		.env("PATH", path)
		.env("MOCK_OS", host)
		.env("MOCK_MODE", mode)
		.env("MOCK_CARGO_RECORD", &record)
		.env("MOCK_RETYPE", bin.join("retype"))
		.env("MOCK_CHILD_PID", &child_pid.0)
		.env("MOCK_BOUNDED", root.join("scripts/run-bounded.pl"))
		.env("RETYPE_TEST_REPORT_DIR", dir.path().join("reports"))
		.env("RETYPE_TEST_TEMP_DIR", dir.path())
		.env("RETYPE_TEST_TIMEOUT_SECONDS", limit.to_string())
		.env("RETYPE_STATE_DIR", dir.path().join("must-not-be-used"))
		.env("RETYPE_NO_SPAWN", "1");
	let (status, text) = capture(&mut command, Duration::from_secs(30));
	if matches!(mode, "stubborn" | "stubborn-leak") {
		child_pid.assert_stopped();
	}
	let invocations = fs::read_to_string(record).unwrap_or_default();
	Run {
		status,
		text,
		invocations,
	}
}

#[test]
fn linux_runner_reports_a_failed_gate_and_runs_all_steps() {
	let result = run("test-linux.sh", "Linux", "fail", 5);
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
	let result = run("test-linux.sh", "Linux", "skip", 5);
	assert!(!result.status.success());
	assert!(result.text.contains("skip or ignored test"), "{}", result.text);
}

#[test]
fn the_runner_rejects_a_failed_summary_even_if_cargo_exits_zero() {
	let result = run("test-linux.sh", "Linux", "falsepass", 5);
	assert!(!result.status.success());
	assert!(result.text.contains("failed test summary"));
}

#[test]
fn the_runner_rejects_a_missing_direct_native_workflow() {
	let result = run("test-linux.sh", "Linux", "missing-direct", 5);
	assert!(!result.status.success());
	assert!(
		result
			.text
			.contains("a_real_direct_heic_conversion_and_undo_follow_watched_disposition")
	);
}

#[test]
fn the_runner_rejects_an_empty_workspace_result() {
	let result = run("test-linux.sh", "Linux", "empty", 5);
	assert!(!result.status.success());
	assert!(result.text.contains("strict native workflow test did not run"));
	assert!(result.text.contains("No passing test summary was recorded"));
}

#[test]
fn the_macos_wrapper_uses_the_native_runner_contract() {
	let result = run("test-macos.sh", "Darwin", "pass", 5);
	assert!(result.status.success(), "{}", result.text);
	assert!(result.text.contains("Platform test: Darwin"));
	assert!(result.text.contains("Platform validation passed."));
}

#[test]
fn the_macos_runner_times_out_a_stubborn_process_tree() {
	let mut unrelated = TestProcess(Command::new("sleep").arg("30").process_group(0).spawn().unwrap());
	let result = run("test-macos.sh", "Darwin", "stubborn", 3);
	assert!(!result.status.success());
	assert!(result.text.contains("workspace: command exit 124"), "{}", result.text);
	assert!(
		unrelated.0.try_wait().unwrap().is_none(),
		"an unrelated process was stopped"
	);
}

#[test]
fn the_macos_runner_reports_subprocesses_left_after_a_command_exits() {
	let result = run("test-macos.sh", "Darwin", "stubborn-leak", 5);
	assert!(!result.status.success());
	assert!(
		result.text.contains("Command left subprocesses running"),
		"{}",
		result.text
	);
	assert!(result.text.contains("workspace: command exit 125"), "{}", result.text);
}

#[test]
fn the_macos_timeout_helper_preserves_arguments_and_exit_status() {
	let root = Path::new(env!("CARGO_MANIFEST_DIR"))
		.parent()
		.unwrap()
		.parent()
		.unwrap();
	let mut command = Command::new("/usr/bin/perl");
	command
		.arg(root.join("scripts/run-bounded.pl"))
		.args(["2", "1", "/bin/sh", "-c", "printf '%s\\n' \"$1\"; exit 42", "sh"])
		.arg("literal ;$* & spaces");
	let (status, text) = capture(&mut command, Duration::from_secs(8));
	assert_eq!(status.code(), Some(42), "{text}");
	assert_eq!(text, "literal ;$* & spaces\n");

	let mut missing = Command::new("/usr/bin/perl");
	missing
		.arg(root.join("scripts/run-bounded.pl"))
		.args(["2", "1", "/no/such/retype-test-command"]);
	let (status, text) = capture(&mut missing, Duration::from_secs(8));
	assert_eq!(status.code(), Some(127), "{text}");
	assert!(text.contains("Cannot execute"), "{text}");

	let mut signalled = Command::new("/usr/bin/perl");
	signalled
		.arg(root.join("scripts/run-bounded.pl"))
		.args(["2", "1", "/bin/sh", "-c", "kill -TERM $$"]);
	let (status, text) = capture(&mut signalled, Duration::from_secs(8));
	assert_eq!(status.code(), Some(143), "{text}");
}

#[test]
fn interrupting_the_macos_timeout_helper_stops_its_process_tree() {
	let root = Path::new(env!("CARGO_MANIFEST_DIR"))
		.parent()
		.unwrap()
		.parent()
		.unwrap();
	let dir = tempfile::tempdir().unwrap();
	let marker = ChildMarker(dir.path().join("child.pid"));
	let mut command = Command::new("/usr/bin/perl");
	command
		.arg(root.join("scripts/run-bounded.pl"))
		.args([
			"30",
			"1",
			"/bin/sh",
			"-c",
			"trap '' TERM; echo $$ > \"$1\"; while :; do sleep 0.1; done",
			"sh",
		])
		.arg(&marker.0)
		.stdout(Stdio::null())
		.stderr(Stdio::null());
	let mut process = TestProcess(command.spawn().unwrap());
	let deadline = Instant::now() + Duration::from_secs(5);
	while !marker.0.exists() {
		assert!(Instant::now() < deadline, "the test child did not start");
		std::thread::sleep(Duration::from_millis(25));
	}
	assert_eq!(unsafe { libc::kill(process.0.id() as i32, libc::SIGTERM) }, 0);
	let deadline = Instant::now() + Duration::from_secs(8);
	let status = loop {
		if let Some(status) = process.0.try_wait().unwrap() {
			break status;
		}
		assert!(Instant::now() < deadline, "the interrupted helper did not stop");
		std::thread::sleep(Duration::from_millis(25));
	};
	assert_eq!(status.code(), Some(143));
	marker.assert_stopped();
}

#[test]
fn leaked_test_processes_are_stopped_and_reported() {
	let result = run("test-linux.sh", "Linux", "leak", 5);
	assert!(!result.status.success());
	assert!(
		result.text.contains("Stopping leaked test process")
			|| result.text.contains("Command left subprocesses running"),
		"{}",
		result.text
	);
}

#[test]
fn a_runner_refuses_a_different_host_os() {
	let result = run("test-macos.sh", "Linux", "pass", 5);
	assert_eq!(result.status.code(), Some(2));
	assert!(result.text.contains("Run this script on Darwin, not Linux."));
	assert!(result.invocations.is_empty());
}

#[test]
fn a_runner_rejects_an_invalid_timeout() {
	let result = run("test-linux.sh", "Linux", "pass", 99_999);
	assert_eq!(result.status.code(), Some(2));
	assert!(result.text.contains("between 1 and 3600"));
	assert!(result.invocations.is_empty());
}

#[test]
fn a_timed_out_workspace_is_a_failure() {
	let result = run("test-linux.sh", "Linux", "hang", 2);
	assert!(!result.status.success());
	assert!(result.text.contains("workspace: command exit 124"), "{}", result.text);
}
