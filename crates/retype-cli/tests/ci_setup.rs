//! Exercise shared local/CI preparation without installing anything on the test host.
#![cfg(unix)]

#[path = "support/commands.rs"]
mod commands;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::time::Duration;

const UNAME: &str = "#!/bin/sh\nprintf '%s\\n' \"$MOCK_HOST\"\n";
const BREW: &str = r#"#!/bin/sh
printf 'brew %s\n' "$*" >> "$MOCK_LOG"
case "$1" in
    install) [ "$MOCK_FAILURE" != brew ] || exit 31;;
    --prefix) printf '%s\n' "$MOCK_PREFIX";;
    *) exit 32;;
esac
"#;
const SUDO: &str = r#"#!/bin/sh
printf 'sudo %s\n' "$*" >> "$MOCK_LOG"
[ "$MOCK_FAILURE" != sudo ] || exit 33
exec "$@"
"#;
const APT: &str = r#"#!/bin/sh
printf 'apt-get %s\n' "$*" >> "$MOCK_LOG"
[ "$MOCK_FAILURE" != apt ] || exit 34
"#;
const ID: &str = "#!/bin/sh\nprintf '%s\\n' \"$MOCK_UID\"\n";
const DPKG: &str = r#"#!/bin/sh
printf 'dpkg-query %s\n' "$*" >> "$MOCK_LOG"
if [ "$MOCK_FAILURE" = owner ]; then
    printf 'unexpected: %s\n' "$2"
else
    printf 'ffmpeg: %s\n' "$2"
fi
"#;
const TOOL: &str = "#!/bin/sh\nexit 0\n";

struct Setup {
	status: ExitStatus,
	stdout: String,
	stderr: String,
	calls: String,
	path: String,
	prefix: String,
}

fn executable(path: &Path, contents: &str) {
	fs::write(path, contents).unwrap();
	fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn run(host: &str, failure: &str, github_actions: &str) -> Setup {
	run_as(host, failure, github_actions, "1000")
}

fn run_as(host: &str, failure: &str, github_actions: &str, uid: &str) -> Setup {
	let dir = tempfile::tempdir().unwrap();
	let bin = dir.path().join("bin");
	fs::create_dir(&bin).unwrap();
	for (name, contents) in [
		("uname", UNAME),
		("brew", BREW),
		("sudo", SUDO),
		("apt-get", APT),
		("id", ID),
		("dpkg-query", DPKG),
		("ffmpeg", TOOL),
		("ffprobe", TOOL),
	] {
		executable(&bin.join(name), contents);
	}
	let path = std::env::join_paths(
		std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())),
	)
	.unwrap();
	let log = dir.path().join("calls.log");
	let github_path = dir.path().join("github-path");
	let prefix = dir.path().join("brew");
	let root = Path::new(env!("CARGO_MANIFEST_DIR"))
		.parent()
		.unwrap()
		.parent()
		.unwrap();
	let mut command = Command::new("bash");
	command
		.arg(root.join("tests/prepare.sh"))
		.env("PATH", path)
		.env("MOCK_HOST", host)
		.env("MOCK_UID", uid)
		.env("MOCK_FAILURE", failure)
		.env("MOCK_LOG", &log)
		.env("MOCK_PREFIX", &prefix)
		.env("GITHUB_PATH", &github_path)
		.env("GITHUB_ACTIONS", github_actions);
	let result = commands::bounded(&mut command, Duration::from_secs(10)).unwrap();
	Setup {
		status: result.status,
		stdout: result.stdout,
		stderr: result.stderr,
		calls: fs::read_to_string(log).unwrap_or_default(),
		path: fs::read_to_string(github_path).unwrap_or_default(),
		prefix: prefix.display().to_string(),
	}
}

#[test]
fn linux_installs_current_debian_tools_without_homebrew() {
	let result = run("Linux", "", "true");
	assert!(result.status.success(), "{}\n{}", result.stdout, result.stderr);
	assert!(result.calls.contains("sudo apt-get update -y"));
	assert!(
		result.calls.contains(
			"sudo apt-get install -y --no-install-recommends imagemagick ffmpeg pandoc 7zip fonts-dejavu-core"
		)
	);
	assert!(!result.calls.contains("brew "));
	assert!(!result.calls.contains("weasyprint"));
	assert!(result.stdout.contains("older-pandoc-and-typst"));
	assert_eq!(result.calls.matches("dpkg-query -S ").count(), 2);
	assert!(result.path.is_empty());
}

#[test]
fn macos_installs_all_real_tools_with_homebrew() {
	let result = run("Darwin", "", "true");
	assert!(result.status.success(), "{}\n{}", result.stdout, result.stderr);
	assert!(
		result
			.calls
			.contains("brew install imagemagick ffmpeg pandoc typst sevenzip")
	);
	assert!(!result.calls.contains("sudo "));
	assert!(!result.calls.contains("dpkg-query "));
	assert_eq!(result.path, format!("{}/bin\n", result.prefix));
}

#[test]
fn setup_failures_and_an_unexpected_ffmpeg_package_fail_the_step() {
	for failure in ["sudo", "apt", "owner"] {
		let result = run("Linux", failure, "true");
		assert!(!result.status.success(), "failure {failure} was accepted");
		assert!(result.path.is_empty(), "failure {failure} published a tool path");
	}
	assert!(!run("Darwin", "brew", "true").status.success());
}

#[test]
fn a_debian_container_installs_with_apt_without_sudo() {
	let result = run_as("Linux", "", "true", "0");
	assert!(result.status.success(), "{}\n{}", result.stdout, result.stderr);
	assert!(result.calls.contains("apt-get update -y"));
	assert!(!result.calls.contains("sudo "));
	assert!(!result.calls.contains("brew "));
}

#[test]
fn local_preparation_works_without_writing_github_paths() {
	let local = run("Linux", "", "false");
	assert!(local.status.success(), "{}\n{}", local.stdout, local.stderr);
	assert!(
		local
			.calls
			.contains("apt-get install -y --no-install-recommends imagemagick ffmpeg pandoc 7zip")
	);
	assert!(local.path.is_empty());
	assert!(local.stdout.contains("tests/run.sh --check"));
}

#[test]
fn preparation_refuses_unknown_hosts_before_installing_anything() {
	let unknown = run("Unknown", "", "true");
	assert!(!unknown.status.success());
	assert!(unknown.calls.is_empty());
}
