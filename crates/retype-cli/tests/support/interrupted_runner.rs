//! Diagnose the interrupted-runner marker without treating a partial write as a PID.

use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use retype_cli::daemon::{Server, resume};
use retype_core::execute::Cancellation;
use retype_core::job::JobState;
use retype_core::undo::Store;

use super::{Fixture, ORIGINAL};

#[path = "commands.rs"]
mod commands;

const START_TIMEOUT: Duration = Duration::from_secs(20);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MarkerSchedule {
	Natural,
	HeldOpen,
}

impl MarkerSchedule {
	fn environment(self) -> &'static str {
		match self {
			Self::Natural => "natural",
			Self::HeldOpen => "held",
		}
	}
}

struct Worker {
	child: Child,
	control: Option<UnixStream>,
	tool_pid: Option<i32>,
	stopped: bool,
}

impl Worker {
	fn spawn(root: &Path, schedule: MarkerSchedule) -> Self {
		let (control, child_control) = UnixStream::pair().unwrap();
		control.set_read_timeout(Some(START_TIMEOUT)).unwrap();
		let child_control: OwnedFd = child_control.into();
		let child = Command::new(std::env::current_exe().unwrap())
			.args(["--exact", "interrupted_job_worker", "--nocapture"])
			.env("RETYPE_SAFETY_INTERRUPTED_ROOT", root)
			.env("RETYPE_SAFETY_MARKER_SCHEDULE", schedule.environment())
			.stdin(Stdio::from(child_control))
			.spawn()
			.unwrap();
		Self {
			child,
			control: Some(control),
			tool_pid: None,
			stopped: false,
		}
	}

	fn announce_tool(&mut self, deadline: Instant) -> i32 {
		let remaining = deadline.saturating_duration_since(Instant::now());
		assert!(
			!remaining.is_zero(),
			"worker did not reach the tool before the deadline"
		);
		self.control
			.as_ref()
			.unwrap()
			.set_read_timeout(Some(remaining))
			.unwrap();
		let mut line = String::new();
		let result = BufReader::new(self.control.as_ref().unwrap()).read_line(&mut line);
		assert!(
			matches!(result, Ok(size) if size > 0) && line.ends_with('\n'),
			"tool did not announce its PID: {result:?}; bytes: {line:?}; worker: {:?}",
			self.child.try_wait()
		);
		let pid = line.trim_end_matches('\n').parse::<i32>().unwrap();
		assert!(pid > 1, "invalid private tool PID: {pid}");
		let group = unsafe { libc::getpgid(pid) };
		assert_eq!(group, pid, "test tool is not its own process-group leader");
		self.tool_pid = Some(pid);
		pid
	}

	fn release_marker(&mut self) {
		self.control.as_mut().unwrap().write_all(b"write\n").unwrap();
	}

	fn kill_tool(&mut self) -> io::Result<()> {
		let pid = self
			.tool_pid
			.ok_or_else(|| io::Error::other("tool PID was not registered"))?;
		let group = unsafe { libc::getpgid(pid) };
		if group < 0 {
			return Err(io::Error::last_os_error());
		}
		if group != pid {
			return Err(io::Error::other("test tool process group changed"));
		}
		if unsafe { libc::kill(-pid, libc::SIGKILL) } != 0 {
			return Err(io::Error::last_os_error());
		}
		self.tool_pid = None;
		Ok(())
	}

	fn wait_for_child(&mut self) -> io::Result<ExitStatus> {
		let deadline = Instant::now() + CLEANUP_TIMEOUT;
		loop {
			if let Some(status) = self.child.try_wait()? {
				return Ok(status);
			}
			if Instant::now() >= deadline {
				return Err(io::Error::new(
					io::ErrorKind::TimedOut,
					"interrupted worker did not stop",
				));
			}
			std::thread::sleep(Duration::from_millis(10));
		}
	}

	fn interrupt(&mut self) {
		assert!(
			self.child.try_wait().unwrap().is_none(),
			"worker exited before interruption"
		);
		self.child.kill().unwrap();
		let status = self.wait_for_child().unwrap();
		assert_eq!(status.signal(), Some(libc::SIGKILL), "worker was not interrupted");
		self.kill_tool().expect("cannot stop the private test tool group");
		self.control.take();
		self.stopped = true;
	}
}

impl Drop for Worker {
	fn drop(&mut self) {
		if self.stopped {
			return;
		}
		if self.tool_pid.is_some() {
			if let Err(error) = self.kill_tool() {
				if error.raw_os_error() != Some(libc::ESRCH) {
					eprintln!("could not stop interrupted test tool: {error}");
				}
			}
		}
		/* Closing the peer also releases a tool whose PID was never received. */
		self.control.take();
		match self.child.try_wait() {
			Ok(Some(_)) => return,
			Ok(None) => {
				if let Err(error) = self.child.kill() {
					eprintln!("could not stop interrupted test worker: {error}");
				}
			}
			Err(error) => eprintln!("could not inspect interrupted test worker: {error}"),
		}
		if let Err(error) = self.wait_for_child() {
			eprintln!("could not reap interrupted test worker: {error}");
		}
	}
}

fn control_descriptor() -> OwnedFd {
	/* The nested worker has a private socket on stdin; the tool replaces stdin. */
	let descriptor = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_DUPFD, 3) };
	assert!(
		descriptor >= 0,
		"cannot duplicate test control socket: {}",
		io::Error::last_os_error()
	);
	let control = unsafe { OwnedFd::from_raw_fd(descriptor) };
	assert!(
		descriptor <= 9,
		"no single-digit descriptor is available for the test shell"
	);
	control
}

fn converter_script(control: i32) -> String {
	format!(
		"case \"$2\" in *.jpg)\n\
		printf '%s\\n' \"$$\" >&{control} || exit 1\n\
		if [ \"${{RETYPE_SAFETY_MARKER_SCHEDULE-}}\" = 'held' ]; then\n\
		  {{\n\
		    IFS= read -r signal <&{control} || :\n\
		    [ \"$signal\" = 'write' ] && printf '%s' \"$$\"\n\
		  }} > \"$(dirname \"$0\")/second-started\" || printf 'cannot write marker\\n' >&2\n\
		else\n\
		  printf '%s' \"$$\" > \"$(dirname \"$0\")/second-started\" || printf 'cannot write marker\\n' >&2\n\
		fi\n\
		IFS= read -r ignored <&{control} || :\n\
		exit 1;; esac\n\
		cp \"$1\" \"$2\"\n"
	)
}

pub(super) fn run_worker() {
	let Some(root) = std::env::var_os("RETYPE_SAFETY_INTERRUPTED_ROOT") else {
		return;
	};
	let control = control_descriptor();
	let fixture = Fixture::new(&converter_script(control.as_raw_fd()), "enabled = true");
	let root = Path::new(&root);
	fs::write(root.join("fixture-dir.tmp"), fixture.dir.path().to_str().unwrap()).unwrap();
	fs::rename(root.join("fixture-dir.tmp"), root.join("fixture-dir")).unwrap();
	fixture.convert("webp,jpg", &Cancellation::default());
	panic!("the parent should terminate this worker while the second output is running");
}

#[derive(Debug)]
pub(super) struct MarkerObservation {
	directory: PathBuf,
	pub saw_empty: bool,
	pub incomplete_reads: usize,
}

fn optional_bytes(path: &Path) -> Option<Vec<u8>> {
	match fs::read(path) {
		Ok(contents) => Some(contents),
		Err(error) if error.kind() == io::ErrorKind::NotFound => None,
		Err(error) => panic!("cannot read {}: {error}", path.display()),
	}
}

fn marker_is_complete(contents: &[u8], expected: &[u8]) -> Result<bool, String> {
	if contents == expected {
		Ok(true)
	} else if expected.starts_with(contents) {
		Ok(false)
	} else {
		Err(format!("invalid PID marker: {contents:?}; expected {expected:?}"))
	}
}

fn wait_for_marker(
	worker: &mut Worker,
	root: &Path,
	schedule: MarkerSchedule,
	pid: i32,
	deadline: Instant,
) -> MarkerObservation {
	let expected = pid.to_string().into_bytes();
	let mut first = None;
	let mut incomplete_reads = 0;
	let mut saw_empty = false;
	let mut released = false;
	loop {
		if let Some(directory) = optional_bytes(&root.join("fixture-dir")) {
			let directory = PathBuf::from(String::from_utf8(directory).unwrap());
			if let Some(contents) = optional_bytes(&directory.join("second-started")) {
				first.get_or_insert_with(|| contents.clone());
				let complete = marker_is_complete(&contents, &expected)
					.unwrap_or_else(|error| panic!("{error}; first marker: {first:?}"));
				if complete {
					let first_text = String::from_utf8_lossy(first.as_deref().unwrap());
					eprintln!(
						"interrupted-runner marker: schedule={schedule:?}, tool_pid={pid}, first={first_text:?}, incomplete_reads={incomplete_reads}, complete={}",
						String::from_utf8_lossy(&contents)
					);
					return MarkerObservation {
						directory,
						saw_empty,
						incomplete_reads,
					};
				}
				if incomplete_reads == 0 {
					eprintln!(
						"interrupted-runner marker observed before its write completed: schedule={schedule:?}, tool_pid={pid}, bytes={contents:?}"
					);
				}
				incomplete_reads += 1;
				saw_empty |= contents.is_empty();
				if schedule == MarkerSchedule::HeldOpen && !released {
					assert!(contents.is_empty(), "held marker was already written: {contents:?}");
					worker.release_marker();
					released = true;
				}
			}
		}
		assert!(
			worker.child.try_wait().unwrap().is_none(),
			"worker exited before the second output; first marker: {first:?}; incomplete reads: {incomplete_reads}"
		);
		assert!(
			Instant::now() < deadline,
			"worker did not finish its PID marker; schedule={schedule:?}, tool_pid={pid}, first={first:?}, incomplete_reads={incomplete_reads}"
		);
		std::thread::sleep(Duration::from_millis(10));
	}
}

fn verify_recovery(directory: &Path) {
	let first = directory.join("photo.webp");
	let identity = retype_core::job::FileIdentity::of(&first).unwrap();
	let server = Server::open(&directory.join("state")).unwrap();
	server.pool().stop();
	let copy = Fixture::new("cp \"$1\" \"$2\"\n", "enabled = true");
	server.shared().reload_registry(copy.shared.registry().as_ref().clone());
	let job = server.shared().jobs.lock().unwrap()[0].clone();
	assert_eq!(
		job.state,
		JobState::Partial,
		"the committed first output must survive reconciliation"
	);
	assert_eq!(
		resume::run(&server.shared(), "audit", &job, false).unwrap(),
		JobState::Done
	);
	assert!(identity.matches(&retype_core::job::FileIdentity::of(&first).unwrap()));
	assert_eq!(fs::read(directory.join("photo.jpg")).unwrap(), ORIGINAL);
	assert!(Store::open(&directory.join("state")).source(&job.id).unwrap().is_some());
}

pub(super) fn exercise(schedule: MarkerSchedule) -> MarkerObservation {
	let root = tempfile::tempdir().unwrap();
	let deadline = Instant::now() + START_TIMEOUT;
	let mut worker = Worker::spawn(root.path(), schedule);
	let pid = worker.announce_tool(deadline);
	let observation = wait_for_marker(&mut worker, root.path(), schedule, pid, deadline);
	worker.interrupt();
	verify_recovery(&observation.directory);
	eprintln!("interrupted-runner recovery verified: schedule={schedule:?}, tool_pid={pid}");
	observation
}

fn tool_is_running(pid: i32) -> bool {
	let mut command = Command::new("ps");
	command.args(["-p", &pid.to_string(), "-o", "stat="]);
	let output = commands::bounded(&mut command, CLEANUP_TIMEOUT).expect("cannot inspect test tool");
	if !output.status.success() {
		assert_eq!(
			output.status.code(),
			Some(1),
			"cannot inspect test tool: {}",
			output.stderr
		);
		assert!(output.stdout.trim().is_empty() && output.stderr.trim().is_empty());
		return false;
	}
	output
		.stdout
		.split_whitespace()
		.next()
		.is_some_and(|state| !state.starts_with('Z'))
}

fn assert_tool_stopped(pid: i32) {
	let deadline = Instant::now() + CLEANUP_TIMEOUT;
	while tool_is_running(pid) {
		assert!(
			Instant::now() < deadline,
			"interrupted test tool {pid} is still running"
		);
		std::thread::sleep(Duration::from_millis(25));
	}
}

#[test]
fn a_marker_that_is_not_the_control_pid_or_its_prefix_is_rejected() {
	assert_eq!(marker_is_complete(b"", b"1234"), Ok(false));
	assert_eq!(marker_is_complete(b"12", b"1234"), Ok(false));
	assert_eq!(marker_is_complete(b"1234", b"1234"), Ok(true));
	assert!(marker_is_complete(b"1235", b"1234").is_err());
	assert!(marker_is_complete(b"12345", b"1234").is_err());
}

#[test]
fn a_disconnected_probe_releases_an_unregistered_tool() {
	let root = tempfile::tempdir().unwrap();
	let mut worker = Worker::spawn(root.path(), MarkerSchedule::HeldOpen);
	let pid = worker.announce_tool(Instant::now() + START_TIMEOUT);
	worker.tool_pid = None;
	drop(worker);
	assert_tool_stopped(pid);
}

#[test]
fn a_known_tool_is_stopped_when_the_probe_aborts() {
	let root = tempfile::tempdir().unwrap();
	let mut worker = Worker::spawn(root.path(), MarkerSchedule::HeldOpen);
	let pid = worker.announce_tool(Instant::now() + START_TIMEOUT);
	drop(worker);
	assert_tool_stopped(pid);
}
