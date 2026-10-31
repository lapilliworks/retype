//! The client against a daemon in this process, which is the only way to test
//! the handshake without a spawn.

#![cfg(unix)]

use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use retype_cli::daemon::{Server, serve};
use retype_cli::{client, state};

struct Running {
	dir: tempfile::TempDir,
	socket: PathBuf,
	handle: Option<JoinHandle<()>>,
}

impl Running {
	fn start() -> Self {
		let dir = tempfile::tempdir().unwrap();
		let socket = dir.path().join("daemon.sock");
		let listener = UnixListener::bind(&socket).unwrap();

		state::publish_endpoint(dir.path(), socket.to_str().unwrap()).unwrap();

		let mut server = Server::open(dir.path()).unwrap();
		server.set_endpoint(socket.to_str().unwrap());
		let handle = std::thread::spawn(move || serve(listener, &mut server).unwrap());

		Self {
			dir,
			socket,
			handle: Some(handle),
		}
	}

	fn state_dir(&self) -> &Path {
		self.dir.path()
	}
}

impl Drop for Running {
	fn drop(&mut self) {
		if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&self.socket) {
			use retype_ipc::{Request, write_frame};
			let request = Request::new(1, "hello").with_params(retype_ipc::Hello {
				ipc: retype_ipc::IPC_VERSION.to_string(),
				client: "test".to_string(),
				features: Vec::new(),
			});
			if let Ok(request) = request {
				let _ = write_frame(&mut stream, &request);
				let _ = write_frame(&mut stream, &Request::new(2, "shutdown"));
			}
		}
		if let Some(handle) = self.handle.take() {
			let _ = handle.join();
		}
	}
}

#[test]
fn a_client_greets_and_reads_status_through_the_published_endpoint() {
	let daemon = Running::start();

	let mut client = client::connect(daemon.state_dir()).unwrap();
	let status = client.call_without_params("status").unwrap();

	assert_eq!(status["pid"], std::process::id());
	assert_eq!(status["jobs"], 0);
	assert_eq!(status["endpoint"], daemon.socket.to_str().unwrap());
}

#[test]
fn shutting_down_through_the_client_stops_the_daemon() {
	let daemon = Running::start();
	let mut client = client::connect(daemon.state_dir()).unwrap();

	let reply = client.call_without_params("shutdown").unwrap();

	assert_eq!(reply["stopping"], true);
}

#[test]
fn a_client_without_an_endpoint_reports_the_daemon_missing() {
	let dir = tempfile::tempdir().unwrap();

	match client::connect(dir.path()) {
		Err(retype_cli::error::Error::DaemonUnavailable) => {}
		other => panic!("expected the daemon to be missing, got {other:?}"),
	}
}

#[test]
fn the_status_lines_are_what_a_human_needs() {
	let daemon = Running::start();
	let mut client = client::connect(daemon.state_dir()).unwrap();
	let status = client.call_without_params("status").unwrap();

	let described: Vec<String> = client::describe_status(&status)
		.into_iter()
		.map(|(name, _)| name)
		.collect();

	assert_eq!(
		described,
		vec![
			"pid",
			"machine",
			"uptime",
			"jobs",
			"parked",
			"journal",
			"endpoint",
			"supervised"
		]
	);
}
