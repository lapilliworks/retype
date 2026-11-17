//! Real pseudo-terminal input, without a graphical desktop or a real login claim.
#![cfg(target_os = "linux")]
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Fixture {
	root: tempfile::TempDir,
	state: std::path::PathBuf,
	config: std::path::PathBuf,
	job: String,
	watch: Option<Child>,
	terminal: Option<File>,
}
impl Fixture {
	fn new() -> Self {
		let root = tempfile::tempdir().unwrap();
		let state = root.path().join("state");
		let config = root.path().join("config.toml");
		fs::write(&config, "version = 'retype-config-1'\n[notify]\nenabled = false\n").unwrap();
		let source = root.path().join("original.tar");
		let mut tar = tar::Builder::new(File::create(&source).unwrap());
		let mut header = tar::Header::new_gnu();
		header.set_size(7);
		header.set_mode(0o600);
		header.set_mtime(1);
		header.set_cksum();
		tar.append_data(&mut header, "entry", &b"payload"[..]).unwrap();
		tar.finish().unwrap();
		drop(tar);
		let output = Command::new(env!("CARGO_BIN_EXE_retype"))
			.arg("convert")
			.arg(source)
			.args(["zip+ask", "--json", "--state-dir"])
			.arg(&state)
			.env("RETYPE_CONFIG", &config)
			.env("RETYPE_TEST_SESSION", "terminal")
			.stdin(Stdio::null())
			.output()
			.unwrap();
		assert_eq!(output.status.code(), Some(7), "{:?}", output);
		let answer: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
		let job = answer["error"]["detail"]["id"].as_str().unwrap().to_string();
		Self {
			root,
			state,
			config,
			job,
			watch: None,
			terminal: None,
		}
	}
	fn start(&mut self) {
		let (mut master, mut slave) = (-1, -1);
		assert_eq!(
			unsafe {
				libc::openpty(
					&mut master,
					&mut slave,
					std::ptr::null_mut(),
					std::ptr::null(),
					std::ptr::null(),
				)
			},
			0
		);
		let terminal = unsafe { File::from_raw_fd(master) };
		let slave = unsafe { File::from_raw_fd(slave) };
		let mut command = Command::new(env!("CARGO_BIN_EXE_retype"));
		command
			.arg("watch")
			.arg(self.root.path())
			.args(["--no-spawn", "--state-dir"])
			.arg(&self.state)
			.env("RETYPE_CONFIG", &self.config)
			.env("RETYPE_TEST_SESSION", "terminal")
			.stdin(slave.try_clone().unwrap())
			.stdout(slave.try_clone().unwrap())
			.stderr(slave);
		unsafe {
			command.pre_exec(move || {
				if libc::setsid() < 0 {
					return Err(std::io::Error::last_os_error());
				}
				if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY, 0) < 0 {
					return Err(std::io::Error::last_os_error());
				}
				Ok(())
			});
		}
		self.watch = Some(command.spawn().unwrap());
		unsafe {
			libc::fcntl(terminal.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
		}
		self.terminal = Some(terminal);
		let deadline = Instant::now() + Duration::from_secs(10);
		let mut output = String::new();
		while !output.contains("[n]ow") {
			let mut buffer = [0; 4096];
			match self.terminal.as_mut().unwrap().read(&mut buffer) {
				Ok(count) => output.push_str(&String::from_utf8_lossy(&buffer[..count])),
				Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
				other => panic!("terminal failed: {other:?}; {output}"),
			}
			assert!(
				self.watch.as_mut().unwrap().try_wait().unwrap().is_none(),
				"watch exited: {output}"
			);
			assert!(Instant::now() < deadline, "prompt did not appear: {output}");
			std::thread::sleep(Duration::from_millis(20));
		}
	}
	fn detach(&mut self) {
		self.terminal.as_mut().unwrap().write_all(&[3]).unwrap();
		let deadline = Instant::now() + Duration::from_secs(5);
		loop {
			if let Some(status) = self.watch.as_mut().unwrap().try_wait().unwrap() {
				assert!(status.success());
				break;
			}
			assert!(Instant::now() < deadline, "interrupt did not detach during a prompt");
			std::thread::sleep(Duration::from_millis(20));
		}
	}
}
impl Drop for Fixture {
	fn drop(&mut self) {
		if let Some(child) = &mut self.watch {
			let _ = child.kill();
			let _ = child.wait();
		}
		if let Ok(mut client) = retype_cli::client::connect(&self.state) {
			let _ = client.shutdown(&retype_ipc::ShutdownRequest { now: true, force: true });
		}
		retype_cli::client::wait_for_stop(&self.state, Duration::from_secs(3));
	}
}

#[test]
fn interrupt_during_a_real_prompt_detaches_and_preserves_the_pending_ask() {
	let mut fixture = Fixture::new();
	fixture.start();
	fixture.detach();
	let job = retype_cli::client::connect(&fixture.state)
		.unwrap()
		.jobs_get(&fixture.job)
		.unwrap();
	assert_eq!(job.state, retype_core::job::JobState::NeedsAttention);
	assert!(!fixture.root.path().join("original.zip").exists());
}

#[test]
fn terminal_eof_is_not_consent_and_stale_prompt_does_not_resolve_twice() {
	let mut fixture = Fixture::new();
	fixture.start();
	fixture.terminal.as_mut().unwrap().write_all(&[4]).unwrap();
	std::thread::sleep(Duration::from_millis(100));
	assert_eq!(
		retype_cli::client::connect(&fixture.state)
			.unwrap()
			.jobs_get(&fixture.job)
			.unwrap()
			.state,
		retype_core::job::JobState::NeedsAttention
	);
	fixture.detach();

	let mut fixture = Fixture::new();
	fixture.start();
	retype_cli::client::connect(&fixture.state)
		.unwrap()
		.jobs_cancel(&fixture.job)
		.unwrap();
	fixture.terminal.as_mut().unwrap().write_all(b"n\n").unwrap();
	std::thread::sleep(Duration::from_millis(100));
	assert!(!fixture.root.path().join("original.zip").exists());
	assert!(fixture.watch.as_mut().unwrap().try_wait().unwrap().is_none());
	fixture.detach();
}
