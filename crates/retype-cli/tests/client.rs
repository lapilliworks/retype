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
		if let Ok(mut client) = client::connect(self.state_dir()) {
			let _ = client.call_without_params("shutdown");
		}
		if let Some(handle) = self.handle.take() {
			let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
			while !handle.is_finished() && std::time::Instant::now() < deadline {
				std::thread::sleep(std::time::Duration::from_millis(20));
			}
			if handle.is_finished() {
				let joined = handle.join();
				if !std::thread::panicking() {
					assert!(joined.is_ok(), "the daemon's request loop panicked");
				}
			} else if !std::thread::panicking() {
				panic!("the daemon's request loop did not stop");
			}
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
fn a_client_disconnecting_during_a_reply_does_not_stop_the_daemon() {
	use std::io::Write;
	use std::net::Shutdown;

	let daemon = Running::start();
	for _ in 0..10 {
		let mut stream = std::os::unix::net::UnixStream::connect(&daemon.socket).unwrap();
		let hello = retype_ipc::Request::new(1, "hello")
			.with_params(retype_ipc::Hello {
				ipc: retype_ipc::IPC_VERSION.to_string(),
				client: "disconnect-test".to_string(),
				features: Vec::new(),
			})
			.unwrap();
		retype_ipc::write_frame(&mut stream, &hello).unwrap();
		stream.flush().unwrap();
		stream.shutdown(Shutdown::Both).unwrap();
		drop(stream);
		let mut client = client::connect(daemon.state_dir()).unwrap();
		assert_eq!(client.status().unwrap().pid, std::process::id());
	}
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
			"dropped",
			"placeholders",
			"primed",
			"observed",
			"watches",
			"journal",
			"endpoint",
			"supervised"
		]
	);
}

#[test]
fn the_daemon_owns_watch_registration_while_it_is_reachable() {
	let daemon = Running::start();
	let dir = tempfile::tempdir().unwrap();
	let photos = dir.path().join("photos");
	std::fs::create_dir(&photos).unwrap();
	let mut client = client::connect(daemon.state_dir()).unwrap();

	let added = client
		.call(
			"watch.add",
			serde_json::json!({"path": photos.display().to_string(), "recursive": true}),
		)
		.unwrap();
	assert_eq!(added["recursive"], true);

	let listed = client.call_without_params("watch.list").unwrap();
	assert_eq!(
		listed["watches"][0]["path"],
		photos.canonicalize().unwrap().display().to_string()
	);
	assert_eq!(listed["watches"][0]["available"], true);
	assert_eq!(
		listed["watches"][0]["status"], "pending",
		"registration alone is not readiness"
	);

	let removed = client
		.call(
			"watch.remove",
			serde_json::json!({"path": photos.display().to_string()}),
		)
		.unwrap();
	assert_eq!(removed["removed"], true);
}

#[test]
fn the_daemon_answers_a_config_reload() {
	let daemon = Running::start();
	let mut client = client::connect(daemon.state_dir()).unwrap();

	let answer = client.call_without_params("config.reload").unwrap();
	assert_eq!(answer["reloaded"], true);
}
