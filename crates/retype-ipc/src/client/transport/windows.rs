//! Wait for a busy named-pipe instance without repeating any request.

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_SEM_TIMEOUT};
use windows_sys::Win32::System::Pipes::WaitNamedPipeW;

pub(super) fn is_busy(error: &io::Error) -> bool {
	error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
}

pub(super) fn open_with_timeout(endpoint: &Path, timeout: Duration) -> io::Result<std::fs::File> {
	let started = Instant::now();
	let deadline = started + timeout;
	let missing_deadline = started + timeout.min(Duration::from_millis(150));
	let mut busy = None;
	loop {
		match std::fs::OpenOptions::new().read(true).write(true).open(endpoint) {
			Ok(stream) => return Ok(stream),
			Err(error) if is_busy(&error) => {
				if busy.is_none() {
					busy = Some(error);
				}
			}
			Err(error) if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => {
				if busy.is_none() {
					let remaining = missing_deadline.saturating_duration_since(Instant::now());
					if remaining.is_zero() {
						return Err(error);
					}
					std::thread::sleep(remaining.min(Duration::from_millis(10)));
					continue;
				}
			}
			Err(error) => return Err(error),
		}

		let remaining = deadline.saturating_duration_since(Instant::now());
		if remaining.is_zero() {
			return Err(busy.expect("only a busy pipe is retried"));
		}
		wait_for_instance(endpoint, remaining)?;
	}
}

fn wait_for_instance(endpoint: &Path, remaining: Duration) -> io::Result<()> {
	let name: Vec<u16> = endpoint.as_os_str().encode_wide().chain(Some(0)).collect();
	let wait = remaining.min(Duration::from_millis(100));
	let milliseconds = wait.as_millis().max(1) as u32;
	if unsafe { WaitNamedPipeW(name.as_ptr(), milliseconds) } != 0 {
		return Ok(());
	}
	let error = io::Error::last_os_error();
	match error.raw_os_error() {
		Some(code) if code == ERROR_SEM_TIMEOUT as i32 => Ok(()),
		Some(code) if code == ERROR_FILE_NOT_FOUND as i32 || code == ERROR_PIPE_BUSY as i32 => {
			/* The daemon closes one instance before creating the next. */
			std::thread::sleep(remaining.min(Duration::from_millis(10)));
			Ok(())
		}
		_ => Err(error),
	}
}
