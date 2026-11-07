//! A daemon must survive disconnected clients without changing CLI pipe behavior.
#![cfg(unix)]

#[path = "support/commands.rs"]
mod commands;

use std::fs;
use std::io;
use std::net::Shutdown;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

struct Process(Child);

impl Drop for Process {
	fn drop(&mut self) {
		if !matches!(self.0.try_wait(), Ok(Some(_))) {
			if let Err(error) = self.0.kill() {
				if !matches!(self.0.try_wait(), Ok(Some(_))) {
					eprintln!("could not stop test process: {error}");
				}
			}
		}
		if let Err(error) = self.0.wait() {
			eprintln!("could not reap test process: {error}");
		}
	}
}

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

fn wait_for_endpoint(process: &mut Process, state: &Path) -> String {
	let deadline = Instant::now() + Duration::from_secs(8);
	loop {
		assert_eq!(process.0.try_wait().unwrap(), None, "daemon exited before readiness");
		match fs::read_to_string(state.join("endpoint")) {
			Ok(endpoint) if Path::new(endpoint.trim()).exists() => return endpoint.trim().to_string(),
			Ok(_) => {}
			Err(error) if error.kind() == io::ErrorKind::NotFound => {}
			Err(error) => panic!("cannot read daemon endpoint: {error}"),
		}
		assert!(Instant::now() < deadline, "daemon did not publish its endpoint");
		std::thread::sleep(Duration::from_millis(20));
	}
}

fn wait_for_exit(process: &mut Process) -> ExitStatus {
	let deadline = Instant::now() + Duration::from_secs(8);
	loop {
		if let Some(status) = process.0.try_wait().unwrap() {
			return status;
		}
		assert!(Instant::now() < deadline, "command did not exit");
		std::thread::sleep(Duration::from_millis(20));
	}
}

#[test]
fn a_daemon_survives_sigpipe_and_a_disconnected_client() {
	let dir = tempfile::tempdir().unwrap();
	let config = dir.path().join("retype.toml");
	fs::write(&config, "version = 'retype-config-1'\n[notify]\nenabled = false\n").unwrap();
	let mut daemon = Process(
		retype()
			.arg("daemon")
			.arg("--state-dir")
			.arg(dir.path())
			.env("RETYPE_CONFIG", &config)
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.spawn()
			.unwrap(),
	);
	let endpoint = wait_for_endpoint(&mut daemon, dir.path());
	let pid = i32::try_from(daemon.0.id()).unwrap();
	assert_eq!(unsafe { libc::kill(pid, libc::SIGPIPE) }, 0);

	let mut stream = UnixStream::connect(endpoint).unwrap();
	let hello = retype_ipc::Request::new(1, "hello")
		.with_params(retype_ipc::Hello {
			ipc: retype_ipc::IPC_VERSION.to_string(),
			client: "sigpipe-test".to_string(),
			features: Vec::new(),
		})
		.unwrap();
	retype_ipc::write_frame(&mut stream, &hello).unwrap();
	stream.shutdown(Shutdown::Both).unwrap();
	drop(stream);

	let mut status = retype();
	status
		.args(["daemon", "status", "--no-spawn", "--json", "--state-dir"])
		.arg(dir.path())
		.env("RETYPE_CONFIG", &config);
	let result = commands::bounded(&mut status, Duration::from_secs(10)).unwrap();
	assert!(result.status.success(), "{}", result.stderr);
	let response: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
	assert_eq!(response["pid"].as_u64(), Some(daemon.0.id().into()));
	assert_eq!(daemon.0.try_wait().unwrap(), None);
}

#[test]
fn a_regular_cli_command_keeps_default_sigpipe_behavior() {
	let (output, closed_reader) = UnixStream::pair().unwrap();
	drop(closed_reader);
	let mut command = Process(
		retype()
			.arg("list")
			.stdin(Stdio::null())
			.stdout(Stdio::from(OwnedFd::from(output)))
			.stderr(Stdio::null())
			.spawn()
			.unwrap(),
	);
	assert_eq!(wait_for_exit(&mut command).signal(), Some(libc::SIGPIPE));
}
