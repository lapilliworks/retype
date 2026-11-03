//! Real Job Object descendants, using this Rust test binary instead of a shell or encoder.

use super::*;

const WORKER: &str = "execute::windows_tests::process_worker";
const MODE: &str = "RETYPE_PROCESS_FIXTURE";
const ROOT: &str = "RETYPE_PROCESS_ROOT";

fn worker_command(root: &Path, mode: &str) -> Command {
	let mut command = Command::new(std::env::current_exe().unwrap());
	crate::process::hide_console(&mut command);
	command
		.args(["--exact", WORKER, "--nocapture"])
		.env(MODE, mode)
		.env(ROOT, root)
		.stdin(Stdio::null())
		.stdout(Stdio::null())
		.stderr(Stdio::null());
	command
}

fn exited_parent(root: &Path) -> crate::tool_process::ToolProcess {
	let mut child = crate::tool_process::ToolProcess::spawn(&mut worker_command(root, "parent")).unwrap();
	let deadline = std::time::Instant::now() + Duration::from_secs(20);
	loop {
		if let Some(status) = child.try_wait().unwrap() {
			assert!(status.success());
			return child;
		}
		assert!(std::time::Instant::now() < deadline, "fixture parent did not exit");
		thread::sleep(POLL);
	}
}

#[test]
#[allow(
	clippy::zombie_processes,
	reason = "the fixture must exit before its job-owned descendant"
)]
fn process_worker() {
	let Some(mode) = std::env::var_os(MODE) else {
		return;
	};
	let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
	if mode == "parent" {
		let descendant = worker_command(&root, "writer").spawn().unwrap();
		std::fs::write(root.join("helper.pid"), descendant.id().to_string()).unwrap();
		std::process::exit(0);
	}
	assert_eq!(mode, "writer");
	let deadline = std::time::Instant::now() + Duration::from_secs(20);
	while !root.join("release").exists() {
		assert!(std::time::Instant::now() < deadline, "fixture helper was not released");
		thread::sleep(POLL);
	}
	std::fs::write(root.join("finished"), b"complete").unwrap();
}

#[test]
fn a_parent_exit_waits_for_its_contained_writer() {
	let dir = tempfile::tempdir().unwrap();
	let child = exited_parent(dir.path());
	assert!(child.has_helpers().unwrap(), "the controlled descendant is still alive");
	let mut released = false;
	wait_for_helpers("fixture", &Cancellation::default(), Duration::from_secs(5), || {
		let active = child.has_helpers()?;
		if active && !released {
			std::fs::write(dir.path().join("release"), b"go")?;
			released = true;
		}
		Ok(active)
	})
	.unwrap();
	assert!(released);
	assert_eq!(std::fs::read(dir.path().join("finished")).unwrap(), b"complete");
}

#[test]
fn a_timed_out_descendant_is_killed_when_containment_closes() {
	use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
	use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
	struct ProcessHandle(HANDLE);
	impl Drop for ProcessHandle {
		fn drop(&mut self) {
			unsafe {
				CloseHandle(self.0);
			}
		}
	}
	let dir = tempfile::tempdir().unwrap();
	let child = exited_parent(dir.path());
	let pid = std::fs::read_to_string(dir.path().join("helper.pid"))
		.unwrap()
		.parse()
		.unwrap();
	let process = ProcessHandle(unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) });
	assert!(!process.0.is_null(), "OpenProcess: {}", std::io::Error::last_os_error());
	let error = wait_for_helpers("fixture", &Cancellation::default(), Duration::from_millis(100), || {
		child.has_helpers()
	})
	.unwrap_err();
	assert_eq!(error.code(), Code::ToolFailed);
	drop(child);
	assert_eq!(unsafe { WaitForSingleObject(process.0, 5_000) }, WAIT_OBJECT_0);
	assert!(!dir.path().join("finished").exists());
}
