//! A request writes and reads on one worker, including when Windows serializes pipe I/O.

use std::io::{BufReader, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::Value;

use super::ClientError;
use crate::{FrameError, Request, read_frame, write_frame};

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;

#[cfg(unix)]
pub(super) type Stream = std::os::unix::net::UnixStream;
#[cfg(windows)]
pub(super) type Stream = std::fs::File;

type Frame = Result<Option<Value>, FrameError>;

#[cfg(unix)]
pub(super) fn open(endpoint: &Path) -> std::io::Result<Stream> {
	Stream::connect(endpoint)
}

#[cfg(windows)]
pub(super) fn open(endpoint: &Path) -> std::io::Result<Stream> {
	std::fs::OpenOptions::new().read(true).write(true).open(endpoint)
}

/// The deadline covers the request write as well as the response read.
pub(super) fn exchange(
	mut stream: BufReader<Stream>,
	request: Request,
	timeout: Duration,
) -> Result<(BufReader<Stream>, Frame), ClientError> {
	let cancellation = cancellation_handle(stream.get_ref())
		.map_err(|error| ClientError::Protocol(format!("cannot prepare IPC cancellation: {error}")))?;
	let cancelled = Arc::new(AtomicBool::new(false));
	let worker_cancelled = Arc::clone(&cancelled);
	let (sender, receiver) = mpsc::sync_channel(1);
	let worker = thread::Builder::new()
		.name("retype-ipc".to_string())
		.spawn(move || {
			let result = transact(&mut stream, &request, &worker_cancelled);
			let _ = sender.send((stream, result));
		})
		.map_err(|error| ClientError::Protocol(format!("cannot start IPC worker: {error}")))?;

	match receiver.recv_timeout(timeout) {
		Ok(result) => Ok(result),
		Err(mpsc::RecvTimeoutError::Timeout) => {
			cancelled.store(true, Ordering::Release);
			abort(cancellation, worker).map_err(|error| {
				ClientError::Protocol(format!(
					"IPC request timed out after {} ms and its I/O could not be stopped: {error}",
					timeout.as_millis()
				))
			})?;
			Err(ClientError::Timeout(timeout))
		}
		Err(mpsc::RecvTimeoutError::Disconnected) => {
			Err(ClientError::Protocol("the IPC worker stopped unexpectedly".to_string()))
		}
	}
}

fn transact<T: Read + Write>(stream: &mut BufReader<T>, request: &Request, cancelled: &AtomicBool) -> Frame {
	if cancelled.load(Ordering::Acquire) {
		return Err(FrameError::Io("request cancelled".to_string()));
	}
	write_frame(stream.get_mut(), request)?;
	if cancelled.load(Ordering::Acquire) {
		return Err(FrameError::Io("request cancelled".to_string()));
	}
	read_frame(stream)
}

#[cfg(unix)]
fn cancellation_handle(stream: &Stream) -> std::io::Result<Stream> {
	stream.try_clone()
}

#[cfg(windows)]
struct Cancellation;

#[cfg(windows)]
fn cancellation_handle(_: &Stream) -> std::io::Result<Cancellation> {
	Ok(Cancellation)
}

#[cfg(unix)]
fn abort(stream: Stream, worker: JoinHandle<()>) -> std::io::Result<()> {
	use std::net::Shutdown;

	match stream.shutdown(Shutdown::Both) {
		Ok(()) => Ok(()),
		Err(error) if error.kind() == std::io::ErrorKind::NotConnected || worker.is_finished() => Ok(()),
		Err(error) => Err(error),
	}
}

#[cfg(windows)]
fn abort(_: Cancellation, worker: JoinHandle<()>) -> std::io::Result<()> {
	use std::time::Instant;

	let deadline = Instant::now() + Duration::from_secs(1);
	let error = loop {
		if worker.is_finished() {
			return Ok(());
		}
		if let Err(error) = cancel_synchronous_io(&worker) {
			break error;
		}
		if Instant::now() >= deadline {
			break std::io::Error::new(std::io::ErrorKind::TimedOut, "the IPC worker did not stop");
		}
		thread::sleep(Duration::from_millis(5));
	};

	// A delayed worker can start I/O after the first cancellation. Keep trying
	// briefly after reporting the failure, without blocking the caller further.
	thread::Builder::new()
		.name("retype-ipc-cancel".to_string())
		.spawn(move || {
			let deadline = Instant::now() + Duration::from_secs(5);
			while !worker.is_finished() && Instant::now() < deadline {
				let _ = cancel_synchronous_io(&worker);
				thread::sleep(Duration::from_millis(5));
			}
		})
		.map_err(|reaper| std::io::Error::other(format!("{error}; cannot start IPC cleanup: {reaper}")))?;
	Err(error)
}

#[cfg(windows)]
fn cancel_synchronous_io(worker: &JoinHandle<()>) -> std::io::Result<()> {
	use std::os::windows::io::AsRawHandle;
	use windows_sys::Win32::Foundation::ERROR_NOT_FOUND;
	use windows_sys::Win32::System::IO::CancelSynchronousIo;

	if unsafe { CancelSynchronousIo(worker.as_raw_handle() as _) } != 0 {
		return Ok(());
	}
	let error = std::io::Error::last_os_error();
	if error.raw_os_error() == Some(ERROR_NOT_FOUND as i32) {
		Ok(())
	} else {
		Err(error)
	}
}
