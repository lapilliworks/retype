//! `retype init` and `retype doctor --emit`: the two commands that hand a user a
//! configuration file.

use std::fs;
use std::path::Path;
use std::process::Command;

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

fn run(args: &[&str]) -> std::process::Output {
	retype().args(args).output().unwrap()
}

#[test]
fn init_prints_a_configuration_that_loads() {
	let printed = run(&["init"]);
	let text = String::from_utf8_lossy(&printed.stdout);

	assert_eq!(printed.status.code(), Some(0));
	assert!(text.contains("version = 'retype-config-1'"), "{text}");
	assert!(text.contains("[backends]"), "{text}");

	// The file this build hands out has to be one it accepts.
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("retype.toml");
	fs::write(&path, text.as_bytes()).unwrap();

	let listed = retype().args(["list"]).env("RETYPE_CONFIG", &path).output().unwrap();
	assert!(listed.status.success(), "{}", String::from_utf8_lossy(&listed.stderr));
}

#[test]
fn init_writes_where_it_is_told_and_refuses_to_overwrite() {
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("nested").join("retype.toml");

	let written = retype()
		.args(["init", "--output", path.to_str().unwrap()])
		.output()
		.unwrap();
	assert_eq!(
		written.status.code(),
		Some(0),
		"{}",
		String::from_utf8_lossy(&written.stderr)
	);
	assert!(path.exists());

	let again = retype()
		.args(["init", "--output", path.to_str().unwrap()])
		.output()
		.unwrap();

	assert_eq!(again.status.code(), Some(5), "an existing file is not replaced");
	assert!(String::from_utf8_lossy(&again.stderr).contains("exists"));
}

#[test]
fn doctor_emit_pins_what_the_machine_resolved() {
	let emitted = run(&["doctor", "--emit"]);
	let text = String::from_utf8_lossy(&emitted.stdout);
	let stderr = String::from_utf8_lossy(&emitted.stderr);

	assert_eq!(emitted.status.code(), Some(0), "{stderr}");
	assert!(text.contains("version = 'retype-config-1'"), "{text}");

	// Nothing installed, nothing to pin: the snippet is the header alone and the
	// claim under test has nothing to say about this host.
	if !text.contains("[tools.") {
		eprintln!("this machine resolved no tools, so this test proves nothing");
		return;
	}

	// Ordinary configuration, which means it has to survive its own validation.
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("retype.toml");
	fs::write(&path, text.as_bytes()).unwrap();

	let listed = retype().args(["list"]).env("RETYPE_CONFIG", &path).output().unwrap();
	assert!(listed.status.success(), "{}", String::from_utf8_lossy(&listed.stderr));
}

#[test]
fn doctor_write_leaves_an_existing_configuration_alone() {
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("retype.toml");
	fs::write(&path, "version = 'retype-config-1'\n\n[undo]\nretention = '2w'\n").unwrap();

	let refused = retype()
		.args(["doctor", "--write"])
		.env("RETYPE_CONFIG", &path)
		.output()
		.unwrap();

	assert_eq!(
		refused.status.code(),
		Some(7),
		"an existing configuration is a conflict"
	);
	assert_eq!(
		fs::read_to_string(&path).unwrap(),
		"version = 'retype-config-1'\n\n[undo]\nretention = '2w'\n",
		"the file is untouched"
	);
}

#[test]
fn doctor_write_creates_a_configuration_where_there_is_none() {
	let dir = tempfile::tempdir().unwrap();
	let path: &Path = &dir.path().join("state").join("retype.toml");

	let written = retype()
		.args(["doctor", "--write"])
		.env("RETYPE_CONFIG", path)
		.output()
		.unwrap();

	assert_eq!(
		written.status.code(),
		Some(0),
		"{}",
		String::from_utf8_lossy(&written.stderr)
	);
	assert!(path.exists());

	let listed = retype().args(["list"]).env("RETYPE_CONFIG", path).output().unwrap();
	assert!(listed.status.success(), "{}", String::from_utf8_lossy(&listed.stderr));
}

#[test]
fn config_names_the_file_for_one_invocation() {
	let dir = tempfile::tempdir().unwrap();
	let named = dir.path().join("named.toml");
	fs::write(
		&named,
		"version = 'retype-config-1'\n\n[formats.png]\nfamily = 'image'\nlossy = false\nalpha = true\ntier = 'exact'\n",
	)
	.unwrap();

	let listed = retype()
		.args(["list", "--config", named.to_str().unwrap()])
		.output()
		.unwrap();
	assert!(listed.status.success(), "{}", String::from_utf8_lossy(&listed.stderr));

	// A named file has to be there; the built-in table is a different answer.
	let missing = retype()
		.args(["list", "--config", dir.path().join("absent.toml").to_str().unwrap()])
		.output()
		.unwrap();

	assert_eq!(missing.status.code(), Some(3));
	assert!(String::from_utf8_lossy(&missing.stderr).contains("E_CFG_MISSING"));
}

#[test]
fn a_daemon_started_by_a_client_reads_the_same_file() {
	let dir = tempfile::tempdir().unwrap();
	let config = dir.path().join("named.toml");
	let state = dir.path().join("state");
	let input = dir.path().join("photo.png");
	fs::write(&input, b"pixels").unwrap();

	// The built-in table has no such backend, so a conversion can only happen if
	// the daemon reads the file the client named.
	let (command, layout) = match cfg!(windows) {
		true => ("cmd", "layout = ['/c', 'copy', '{input}', '{ops}', '{output}']"),
		false => ("cp", "layout = ['{input}', '{ops}', '{output}']"),
	};

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
"
		),
	)
	.unwrap();

	let converted = retype()
		.args(["convert", input.to_str().unwrap(), "webp"])
		.arg("--config")
		.arg(&config)
		.arg("--state-dir")
		.arg(&state)
		.output()
		.unwrap();

	assert_eq!(
		converted.status.code(),
		Some(0),
		"{}\n{}",
		String::from_utf8_lossy(&converted.stderr),
		fs::read_to_string(state.join("daemon.log")).unwrap_or_default()
	);
	assert_eq!(fs::read(dir.path().join("photo.webp")).unwrap(), b"pixels");

	let _ = retype()
		.args(["daemon", "stop"])
		.arg("--state-dir")
		.arg(&state)
		.output();
}
