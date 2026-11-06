use std::fs::File;
use std::io::BufReader;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
	ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
use windows_sys::Win32::System::IO::CancelSynchronousIo;
use windows_sys::Win32::System::Pipes::{
	ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};

use super::super::{Client, ClientError};
use crate::{Response, read_frame, write_frame};

type TestResult = Result<(), String>;

static NEXT_PIPE: AtomicU64 = AtomicU64::new(0);

fn pipe() -> (PathBuf, File) {
	let name = PathBuf::from(format!(
		r"\\.\pipe\retype-ipc-test-{}-{}",
		std::process::id(),
		NEXT_PIPE.fetch_add(1, Ordering::Relaxed)
	));
	let file = pipe_at(&name);
	(name, file)
}

fn pipe_at(name: &Path) -> File {
	let wide: Vec<u16> = name.as_os_str().encode_wide().chain(Some(0)).collect();
	let handle = unsafe {
		CreateNamedPipeW(
			wide.as_ptr(),
			PIPE_ACCESS_DUPLEX,
			PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
			1,
			4096,
			4096,
			0,
			ptr::null(),
		)
	};
	assert_ne!(handle, INVALID_HANDLE_VALUE, "{}", std::io::Error::last_os_error());
	unsafe { File::from_raw_handle(handle as _) }
}

fn connect_server(file: &File) -> TestResult {
	if unsafe { ConnectNamedPipe(file.as_raw_handle() as _, ptr::null_mut()) } != 0 {
		return Ok(());
	}
	let error = std::io::Error::last_os_error();
	if error.raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32) {
		Ok(())
	} else {
		Err(error.to_string())
	}
}

struct Running {
	client: JoinHandle<()>,
	server: JoinHandle<()>,
	client_result: Receiver<TestResult>,
	server_result: Receiver<TestResult>,
}

impl Running {
	fn start(
		server_action: impl FnOnce(File) -> TestResult + Send + 'static,
		client_action: impl FnOnce(Client) -> TestResult + Send + 'static,
	) -> Self {
		let (path, file) = pipe();
		let (server_sender, server_result) = mpsc::channel();
		let server = thread::spawn(move || {
			let result = connect_server(&file).and_then(|()| server_action(file));
			let _ = server_sender.send(result);
		});
		let (client_sender, client_result) = mpsc::channel();
		let client = thread::spawn(move || {
			let result = Client::connect(&path)
				.map_err(|error| error.to_string())
				.and_then(client_action);
			let _ = client_sender.send(result);
		});
		Self {
			client,
			server,
			client_result,
			server_result,
		}
	}

	fn finish(self, release: Option<Sender<()>>) {
		let client_result = self.client_result.recv_timeout(Duration::from_secs(6));
		if let Some(release) = release {
			let _ = release.send(());
		}
		if !matches!(&client_result, Ok(Ok(()))) {
			cancel_threads(&self.client, &self.server);
		}
		let server_result = self.server_result.recv_timeout(Duration::from_secs(3));
		if !matches!(&server_result, Ok(Ok(()))) {
			cancel_threads(&self.client, &self.server);
		}
		let deadline = Instant::now() + Duration::from_secs(1);
		while (!self.client.is_finished() || !self.server.is_finished()) && Instant::now() < deadline {
			thread::sleep(Duration::from_millis(5));
		}
		let client_stopped = self.client.is_finished();
		let server_stopped = self.server.is_finished();
		if !client_stopped || !server_stopped {
			cancel_threads(&self.client, &self.server);
		}
		if self.client.is_finished() {
			self.client.join().unwrap();
		}
		if self.server.is_finished() {
			self.server.join().unwrap();
		}
		client_result
			.expect("the pipe client did not finish")
			.expect("client failed");
		server_result
			.expect("the pipe server did not finish")
			.expect("server failed");
		assert!(client_stopped && server_stopped, "pipe test threads did not stop");
	}
}

fn cancel_threads(client: &JoinHandle<()>, server: &JoinHandle<()>) {
	let deadline = Instant::now() + Duration::from_secs(1);
	while (!client.is_finished() || !server.is_finished()) && Instant::now() < deadline {
		for thread in [client, server] {
			if !thread.is_finished() {
				unsafe { CancelSynchronousIo(thread.as_raw_handle() as _) };
			}
		}
		thread::sleep(Duration::from_millis(5));
	}
}

fn read_one(reader: &mut BufReader<&File>) -> Result<serde_json::Value, String> {
	read_frame(reader)
		.map_err(|error| error.to_string())?
		.ok_or_else(|| "the pipe closed before the request".to_string())
}

fn expect_closed(reader: &mut BufReader<&File>) -> TestResult {
	match read_frame(reader) {
		Ok(None) => Ok(()),
		other => Err(format!("the pipe stayed open or returned another frame: {other:?}")),
	}
}

#[test]
fn a_synchronous_pipe_serves_multiple_requests_and_closes() {
	let running = Running::start(
		|file| {
			let mut reader = BufReader::new(&file);
			for id in 1..=2 {
				let request = read_one(&mut reader)?;
				if request["id"] != id {
					return Err(format!("unexpected request: {request}"));
				}
				let response = Response::ok(id, id).map_err(|error| error.to_string())?;
				write_frame(&mut &file, &response).map_err(|error| error.to_string())?;
			}
			expect_closed(&mut reader)
		},
		|mut client| {
			for id in 1..=2 {
				let response = client
					.request_with_timeout("status", serde_json::Value::Null, Duration::from_secs(2))
					.map_err(|error| error.to_string())?;
				if response != id {
					return Err(format!("unexpected response: {response}"));
				}
			}
			Ok(())
		},
	);
	running.finish(None);
}

#[test]
fn a_timed_out_pipe_read_is_cancelled_and_closed() {
	let running = Running::start(
		|file| {
			let mut reader = BufReader::new(&file);
			read_one(&mut reader)?;
			expect_closed(&mut reader)
		},
		|mut client| expect_timeout(&mut client, serde_json::Value::Null),
	);
	running.finish(None);
}

#[test]
fn a_timed_out_pipe_write_is_cancelled() {
	let (release, wait) = mpsc::channel();
	let running = Running::start(
		move |_file| {
			wait.recv_timeout(Duration::from_secs(3))
				.map_err(|error| error.to_string())
		},
		|mut client| expect_timeout(&mut client, serde_json::json!({"data": "x".repeat(4 * 1024 * 1024)})),
	);
	running.finish(Some(release));
}

#[test]
fn a_busy_pipe_reports_busy_after_the_bounded_wait() {
	let (path, server) = pipe();
	let first = Client::connect(&path).unwrap();
	let started = Instant::now();
	let error = super::super::transport::open_with_timeout(&path, Duration::from_millis(250)).unwrap_err();
	assert_eq!(error.raw_os_error(), Some(ERROR_PIPE_BUSY as i32));
	assert!(started.elapsed() >= Duration::from_millis(200));
	assert!(started.elapsed() < Duration::from_secs(2));
	let error = ClientError::Connect(error);
	assert!(error.is_connection_busy());
	assert!(!error.is_unavailable());
	drop(first);
	drop(server);
}

#[test]
fn an_absent_pipe_is_not_treated_as_busy() {
	let (path, server) = pipe();
	drop(server);
	let started = Instant::now();
	let error = super::super::transport::open_with_timeout(&path, Duration::from_secs(2)).unwrap_err();
	assert_eq!(error.raw_os_error(), Some(ERROR_FILE_NOT_FOUND as i32));
	assert!(started.elapsed() < Duration::from_secs(1));
	let error = ClientError::Connect(error);
	assert!(!error.is_connection_busy());
	assert!(error.is_unavailable());
}

#[test]
fn an_initial_pipe_instance_gap_is_retried() {
	let (path, file) = pipe();
	drop(file);
	let server_path = path.clone();
	let (server_sender, server_result) = mpsc::channel();
	let server = thread::spawn(move || {
		thread::sleep(Duration::from_millis(50));
		let file = pipe_at(&server_path);
		let result = connect_server(&file).and_then(|()| expect_closed(&mut BufReader::new(&file)));
		let _ = server_sender.send(result);
	});
	let (client_sender, client_result) = mpsc::channel();
	let client = thread::spawn(move || {
		let result = Client::connect(&path).map(drop).map_err(|error| error.to_string());
		let _ = client_sender.send(result);
	});
	Running {
		client,
		server,
		client_result,
		server_result,
	}
	.finish(None);
}

#[test]
fn a_busy_pipe_waits_through_the_gap_before_the_next_instance() {
	let (path, file) = pipe();
	let first = Client::connect(&path).unwrap();
	let server_path = path.clone();
	let (ready, wait_ready) = mpsc::channel();
	let (release, wait_release) = mpsc::channel();
	let (server_sender, server_result) = mpsc::channel();
	let server = thread::spawn(move || {
		let result = (|| {
			connect_server(&file)?;
			ready.send(()).map_err(|error| error.to_string())?;
			wait_release
				.recv_timeout(Duration::from_secs(3))
				.map_err(|error| error.to_string())?;
			drop(file);
			thread::sleep(Duration::from_millis(80));
			let file = pipe_at(&server_path);
			connect_server(&file)?;
			expect_closed(&mut BufReader::new(&file))
		})();
		let _ = server_sender.send(result);
	});
	wait_ready.recv_timeout(Duration::from_secs(2)).unwrap();

	let (client_sender, client_result) = mpsc::channel();
	let client = thread::spawn(move || {
		let result = Client::connect(&path).map(drop).map_err(|error| error.to_string());
		let _ = client_sender.send(result);
	});
	thread::sleep(Duration::from_millis(100));
	let ended_while_busy = client.is_finished();
	drop(first);
	release.send(()).unwrap();
	Running {
		client,
		server,
		client_result,
		server_result,
	}
	.finish(None);
	assert!(!ended_while_busy, "the second connection failed instead of waiting");
}

fn expect_timeout(client: &mut Client, params: serde_json::Value) -> TestResult {
	let timeout = Duration::from_millis(500);
	let started = Instant::now();
	match client.request_with_timeout("status", params, timeout) {
		Err(ClientError::Timeout(actual)) if actual == timeout && started.elapsed() < Duration::from_secs(3) => Ok(()),
		other => Err(format!("expected a bounded timeout, got {other:?}")),
	}
}
