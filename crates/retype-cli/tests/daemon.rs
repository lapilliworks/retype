//! The daemon as a client sees it: a socket, a handshake, and answers.

#![cfg(unix)]

use std::io::BufReader;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use retype_cli::daemon::{Server, serve};
use retype_core::error::Code;
use retype_ipc::{Hello, IPC_VERSION, Request, Response, read_frame, write_frame};

struct Daemon {
	dir: tempfile::TempDir,
	socket: PathBuf,
	handle: Option<JoinHandle<()>>,
}

impl Daemon {
	fn start() -> Self {
		let dir = tempfile::tempdir().unwrap();
		let socket = dir.path().join("daemon.sock");
		let listener = UnixListener::bind(&socket).unwrap();
		let mut server = Server::open(dir.path()).unwrap();
		let handle = std::thread::spawn(move || serve(listener, &mut server).unwrap());

		Self {
			dir,
			socket,
			handle: Some(handle),
		}
	}

	fn connect(&self) -> Client {
		Client::new(&self.socket)
	}
}

impl Drop for Daemon {
	fn drop(&mut self) {
		// Stop the accept loop even when the test failed before asking.
		if let Ok(mut stream) = UnixStream::connect(&self.socket) {
			let _ = write_frame(&mut stream, &greeted(99, "shutdown"));
		}
		if let Some(handle) = self.handle.take() {
			let _ = handle.join();
		}
		let _ = &self.dir;
	}
}

struct Client {
	stream: UnixStream,
	reader: BufReader<UnixStream>,
}

impl Client {
	fn new(socket: &Path) -> Self {
		let stream = UnixStream::connect(socket).unwrap();
		let reader = BufReader::new(stream.try_clone().unwrap());
		Self { stream, reader }
	}

	fn call(&mut self, request: &Request) -> Response {
		write_frame(&mut self.stream, request).unwrap();
		let frame = read_frame(&mut self.reader).unwrap().unwrap();
		serde_json::from_value(frame).unwrap()
	}

	fn hello(&mut self) -> Response {
		self.call(&greeted(1, "hello"))
	}
}

fn greeted(id: u64, method: &str) -> Request {
	match method {
		"hello" => Request::new(id, "hello")
			.with_params(Hello {
				ipc: IPC_VERSION.to_string(),
				client: "retype-cli/0.1.0".to_string(),
				features: vec!["events".to_string()],
			})
			.unwrap(),
		_ => Request::new(id, method),
	}
}

#[test]
fn a_client_greets_then_reads_status_and_stops_the_daemon() {
	let daemon = Daemon::start();
	let mut client = daemon.connect();

	let hello = client.hello();
	assert!(hello.ok, "{hello:?}");
	assert_eq!(hello.result.as_ref().unwrap()["ipc"], IPC_VERSION);
	assert_eq!(hello.result.as_ref().unwrap()["pid"], std::process::id());

	let status = client.call(&Request::new(2, "status"));
	assert_eq!(status.result.as_ref().unwrap()["jobs"], 0);
	assert!(status.result.as_ref().unwrap()["machine"].as_str().unwrap().len() == 8);

	let jobs = client.call(&Request::new(3, "jobs.list"));
	assert_eq!(jobs.result.as_ref().unwrap()["jobs"].as_array().unwrap().len(), 0);

	let shutdown = client.call(&Request::new(4, "shutdown"));
	assert_eq!(shutdown.result.as_ref().unwrap()["stopping"], true);
}

#[test]
fn a_method_before_hello_is_refused_and_the_connection_closes() {
	let daemon = Daemon::start();
	let mut client = daemon.connect();

	let response = client.call(&Request::new(1, "status"));

	assert!(!response.ok);
	assert_eq!(response.error.as_ref().unwrap().code, Code::IpcHandshake);
	assert_eq!(
		read_frame(&mut client.reader).unwrap(),
		None,
		"the daemon stopped talking"
	);
}

#[test]
fn a_client_from_another_protocol_major_is_refused() {
	let daemon = Daemon::start();
	let mut client = daemon.connect();

	let request = Request::new(1, "hello")
		.with_params(Hello {
			ipc: "other-ipc-1".to_string(),
			client: "retype-cli/0.1.0".to_string(),
			features: Vec::new(),
		})
		.unwrap();
	let response = client.call(&request);

	assert!(!response.ok);
	assert_eq!(response.error.as_ref().unwrap().code, Code::IpcVersion);
	assert!(response.error.as_ref().unwrap().message.contains(IPC_VERSION));
}

#[test]
fn an_unknown_method_is_refused_by_name() {
	let daemon = Daemon::start();
	let mut client = daemon.connect();
	client.hello();

	let response = client.call(&Request::new(2, "jobs.invent"));

	assert!(!response.ok);
	assert_eq!(response.error.as_ref().unwrap().code, Code::IpcMethod);
	assert_eq!(
		response.error.as_ref().unwrap().detail.as_ref().unwrap()["method"],
		"jobs.invent"
	);
}
