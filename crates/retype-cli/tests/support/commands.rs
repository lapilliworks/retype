//! Bounded test commands with captured output and isolated Unix process groups.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const MAX_CAPTURE: u64 = 1024 * 1024;

pub struct CommandResult {
	pub status: ExitStatus,
	pub stdout: String,
	pub stderr: String,
}

struct TestCommand {
	child: Child,
	finished: bool,
}

impl TestCommand {
	fn stop(&mut self) -> io::Result<()> {
		if self.finished {
			return Ok(());
		}
		#[cfg(unix)]
		let group_error = {
			let pid = i32::try_from(self.child.id()).map_err(io::Error::other)?;
			if unsafe { libc::kill(-pid, libc::SIGKILL) } == 0 {
				None
			} else {
				let error = io::Error::last_os_error();
				(error.raw_os_error() != Some(libc::ESRCH)).then_some(error)
			}
		};
		#[cfg(not(unix))]
		let group_error: Option<io::Error> = None;
		if let Err(error) = self.child.kill() {
			if self.child.try_wait()?.is_some() {
				self.finished = true;
				return group_error.map_or(Ok(()), Err);
			}
			return Err(error);
		}
		let deadline = Instant::now() + Duration::from_secs(2);
		loop {
			if self.child.try_wait()?.is_some() {
				self.finished = true;
				return group_error.map_or(Ok(()), Err);
			}
			if Instant::now() >= deadline {
				return Err(io::Error::new(io::ErrorKind::TimedOut, "test command did not stop"));
			}
			std::thread::sleep(Duration::from_millis(25));
		}
	}
}

impl Drop for TestCommand {
	fn drop(&mut self) {
		if let Err(error) = self.stop() {
			eprintln!("could not stop test command: {error}");
		}
	}
}

fn text(file: &mut File) -> io::Result<String> {
	if file.metadata()?.len() > MAX_CAPTURE {
		return Err(io::Error::other("test command output exceeded 1 MiB"));
	}
	file.seek(SeekFrom::Start(0))?;
	let mut bytes = Vec::new();
	file.take(MAX_CAPTURE + 1).read_to_end(&mut bytes)?;
	if bytes.len() as u64 > MAX_CAPTURE {
		return Err(io::Error::other("test command output exceeded 1 MiB"));
	}
	Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub fn bounded(command: &mut Command, limit: Duration) -> io::Result<CommandResult> {
	let mut stdout = tempfile::tempfile()?;
	let mut stderr = tempfile::tempfile()?;
	#[cfg(unix)]
	{
		use std::os::unix::process::CommandExt;
		command.process_group(0);
	}
	let description = format!("{command:?}");
	let child = command
		.stdin(Stdio::null())
		.stdout(Stdio::from(stdout.try_clone()?))
		.stderr(Stdio::from(stderr.try_clone()?))
		.spawn()?;
	let mut child = TestCommand { child, finished: false };
	let deadline = Instant::now() + limit;
	let status = loop {
		if let Some(status) = child.child.try_wait()? {
			child.finished = true;
			break status;
		}
		if Instant::now() >= deadline {
			let cleanup = child.stop();
			let stdout = text(&mut stdout).unwrap_or_else(|error| format!("cannot read stdout: {error}"));
			let stderr = text(&mut stderr).unwrap_or_else(|error| format!("cannot read stderr: {error}"));
			return Err(io::Error::new(
				io::ErrorKind::TimedOut,
				format!(
					"{description} timed out after {limit:?}; cleanup: {cleanup:?}; stdout: {stdout}; stderr: {stderr}"
				),
			));
		}
		std::thread::sleep(Duration::from_millis(25));
	};
	Ok(CommandResult {
		status,
		stdout: text(&mut stdout)?,
		stderr: text(&mut stderr)?,
	})
}
