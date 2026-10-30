//! Running a plan, `docs/cli.md` §2 and `docs/errors.md` §5.
//!
//! One plan is one argv. The tool writes to a temporary path in the output's own
//! directory and the file is renamed into place only when the tool exits zero, so
//! an output path is either absent or complete. A failure restores nothing
//! because nothing was moved: the source is still where it was, which is why
//! `convert` needs none of the restoration a rename does.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crate::convert::StagedOutput;
use crate::error::{Code, Diagnostic, Error, Result};
use crate::plan::{Backend, Plan};

/// How often a running tool is asked whether it is finished, so a cancel lands
/// within a poll interval rather than when the tool feels like exiting.
const POLL: Duration = Duration::from_millis(50);

/// A job's cancel flag, shared between the runner that owns the child process and
/// whoever asks for it to stop, `docs/ipc.md` §6.
#[derive(Debug, Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);

impl Cancellation {
	pub fn cancel(&self) {
		self.0.store(true, Ordering::Relaxed);
	}

	pub fn is_cancelled(&self) -> bool {
		self.0.load(Ordering::Relaxed)
	}
}

/// How much of a tool's stderr is kept. Enough for the warning a tool prints
/// while succeeding, small enough to sit in a journal record.
const STDERR_TAIL: usize = 4 * 1024;

/// What a successful run produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
	pub output: PathBuf,
	pub bytes: u64,
	/// The tail of stderr, kept because a tool can succeed and still say
	/// something worth reading, `docs/tools.md` §5.2.
	pub stderr: String,
}

/// Run one plan, staging the output and committing it on success.
pub fn run(plan: &Plan<'_>, input: &Path, output: &Path) -> Result<Report> {
	run_cancellable(plan, input, output, &Cancellation::default())
}

/// The same, with a way to stop it. A cancelled run kills the tool's whole process
/// group, so an encoder that forked a helper does not outlive the job, and leaves
/// the output absent: the staged file is discarded rather than committed.
pub fn run_cancellable(plan: &Plan<'_>, input: &Path, output: &Path, cancellation: &Cancellation) -> Result<Report> {
	run_logged(plan, input, output, cancellation, None)
}

/// The same, with the tool's stderr also written to `log` as it arrives.
///
/// The journal keeps a tail and the log keeps everything: a chatty
/// converter can print megabytes while a failure needs the last ten lines. Writing
/// as it arrives is what makes `jobs logs --follow` worth the flag, because a job
/// that runs for twenty minutes has something to watch before it ends.
pub fn run_logged(
	plan: &Plan<'_>,
	input: &Path,
	output: &Path,
	cancellation: &Cancellation,
	log: Option<&Path>,
) -> Result<Report> {
	let Backend::Tool(tool) = plan.backend;

	// The plan resolved this name at plan time, so running it is the same choice
	// the explanation showed.
	let command = plan.command.clone();
	let path = executable(&command).ok_or_else(|| {
		Error::Diagnostic(Diagnostic::global(
			Code::ToolMissing,
			format!("{} is not installed as '{command}'", tool.name),
		))
	})?;

	let staged = StagedOutput::new(output)?;
	let argv = plan.argv(input, staged.temp());

	let mut child = {
		let mut command_line = Command::new(&path);
		command_line
			.args(&argv)
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::piped());

		// A tool that forks a helper is the normal case, so the group is what gets
		// killed rather than the process we happen to know about.
		#[cfg(unix)]
		{
			use std::os::unix::process::CommandExt;
			command_line.process_group(0);
		}

		command_line.spawn().map_err(|error| {
			Error::Diagnostic(Diagnostic::global(
				Code::ToolFailed,
				format!("{command} could not run: {error}"),
			))
		})?
	};

	// Drain stderr while the tool runs: a chatty tool would otherwise block on a
	// full pipe, and we would be waiting for it to exit.
	let stderr_pipe = child.stderr.take();
	let journal = log.map(Path::to_path_buf);
	let reader = thread::spawn(move || {
		let mut kept = Vec::new();

		// A log that cannot be opened is not a reason to fail a conversion; the tail
		// in the journal is what a report depends on.
		let mut file = journal.and_then(|path| std::fs::OpenOptions::new().create(true).append(true).open(path).ok());

		if let Some(mut pipe) = stderr_pipe {
			let mut buffer = [0u8; 8 * 1024];
			while let Ok(read) = pipe.read(&mut buffer) {
				if read == 0 {
					break;
				}

				kept.extend_from_slice(&buffer[..read]);
				if kept.len() > STDERR_TAIL * 4 {
					let excess = kept.len() - STDERR_TAIL * 4;
					kept.drain(..excess);
				}

				if let Some(file) = file.as_mut() {
					use std::io::Write;
					let _ = file.write_all(&buffer[..read]);
					let _ = file.flush();
				}
			}
		}

		kept
	});

	let status = loop {
		if cancellation.is_cancelled() {
			kill(&mut child);
			let _ = reader.join();
			return Err(Error::Diagnostic(Diagnostic::global(
				Code::JobCancelled,
				format!("{command} was cancelled"),
			)));
		}

		match child.try_wait() {
			Ok(Some(status)) => break status,
			Ok(None) => thread::sleep(POLL),
			Err(error) => {
				kill(&mut child);
				let _ = reader.join();

				return Err(Error::Diagnostic(Diagnostic::global(
					Code::ToolFailed,
					format!("{command} could not be waited for: {error}"),
				)));
			}
		}
	};

	let stderr = tail(&reader.join().unwrap_or_default());

	if !status.success() {
		return Err(Error::Diagnostic(Diagnostic::global(
			Code::ToolFailed,
			match stderr.is_empty() {
				true => format!("{command} exited {status}"),
				false => format!("{command} exited {status}: {stderr}"),
			},
		)));
	}

	if !staged.temp().exists() {
		return Err(Error::Diagnostic(Diagnostic::global(
			Code::ToolFailed,
			format!("{command} exited successfully but wrote nothing"),
		)));
	}

	let bytes = std::fs::metadata(staged.temp())?.len();
	staged.commit()?;

	Ok(Report {
		output: output.to_path_buf(),
		bytes,
		stderr,
	})
}

/// Convert a rename that has already happened.
///
/// The target path holds the source bytes under the new name, so the bytes are
/// relabelled first: the input name carries the format its content actually has
/// while the conversion runs, and no observer sees a WebP path holding HEIC
/// bytes. A failure puts the source name back, so the file is left saying what it
/// contains.
#[allow(clippy::too_many_arguments)]
pub fn convert_rename(
	plan: &Plan<'_>,
	renamed: &Path,
	stem: &str,
	source_format: &str,
	output: &Path,
	cancellation: &Cancellation,
	log: Option<&Path>,
) -> Result<Report> {
	let prepared = crate::convert::prepare_input(renamed, stem, source_format)?;
	let result = run_logged(plan, prepared.input(), output, cancellation, log);

	if result.is_ok() && !prepared.is_temporary() {
		// The rename meant this file to become the target, so once the target
		// exists the source-named copy is the leftover of the relabelling.
		let _ = std::fs::remove_file(prepared.input());
	}

	if result.is_err() && prepared.is_temporary() {
		// The source name was occupied, so the bytes sit at the renamed path under a
		// name that lies about them. Nothing is deleted: the record names the path.
		let _ = crate::convert::restore_source_name(renamed, stem, source_format);
	}

	result
}

/// Kill a child and everything it started, then reap it.
fn kill(child: &mut std::process::Child) {
	#[cfg(unix)]
	{
		// The child leads its own group, so the group id is its pid.
		if let Ok(pid) = libc::pid_t::try_from(child.id()) {
			unsafe {
				libc::kill(-pid, libc::SIGKILL);
			}
		}
	}

	#[cfg(not(unix))]
	{
		let _ = child.kill();
	}

	let _ = child.wait();
}

/// Where a resolved command name actually lives. An absolute path was pinned by
/// the configuration, so it is used as written, `docs/config.md` §3.
fn executable(command: &str) -> Option<PathBuf> {
	if Path::new(command).is_absolute() {
		return Path::new(command).is_file().then(|| PathBuf::from(command));
	}

	crate::tools::find_in_path(&std::env::var_os("PATH").unwrap_or_default(), command)
}

/// The last [`STDERR_TAIL`] bytes of a tool's stderr, trimmed.
fn tail(stderr: &[u8]) -> String {
	let start = stderr.len().saturating_sub(STDERR_TAIL);
	String::from_utf8_lossy(&stderr[start..]).trim().to_string()
}

#[cfg(test)]
mod tests {
	use std::fs;

	use super::*;
	use crate::intent;
	use crate::plan::{self, Presence};
	use crate::registry::Registry;
	use crate::resolve::{self, Source};
	use crate::tools::Tool;

	/// Says every declared tool is installed: the fixture declares only the one
	/// the test needs, so the question never comes up.
	struct Anything;

	impl Presence for Anything {
		fn command(&self, tool: &Tool) -> Option<String> {
			tool.commands().next().map(str::to_string)
		}

		fn engine(&self, _tool: &Tool, _name: &str) -> Option<String> {
			None
		}
	}

	/// A fixture whose only tool copies, so the runner is tested without a
	/// converter being installed. `cmd /c copy` on Windows, `cp` elsewhere.
	fn copy_fixture() -> Registry {
		let (command, layout) = match cfg!(windows) {
			true => ("cmd", "layout = ['/c', 'copy', '{input}', '{ops}', '{output}']"),
			false => ("cp", "layout = ['{input}', '{ops}', '{output}']"),
		};

		Registry::load(&format!(
			"\
version = 'retype-config-1'

[backends]
prefer = ['copy']

[tools.copy]
command = '{command}'
{layout}

[formats.png]
family = 'image'
lossy = false
alpha = true
tier = 'exact'

[formats.webp]
family = 'image'
lossy = true
alpha = true
tier = 'converted'
",
		))
		.expect("the fixture loads")
	}

	fn plans<'a>(registry: &'a Registry, text: &str) -> Vec<Plan<'a>> {
		let intent = intent::parse(text).expect("scans");
		let resolved = resolve::resolve(
			&intent,
			Source {
				format: "png",
				is_directory: false,
			},
			registry,
		)
		.expect("resolves");

		plan::compile(registry, &Anything, &resolved).expect("plans")
	}

	#[test]
	fn a_successful_run_writes_the_output() {
		let registry = copy_fixture();
		let plans = plans(&registry, "webp");
		let dir = tempfile::tempdir().unwrap();
		let input = dir.path().join("a.png");
		let output = dir.path().join("a.webp");
		fs::write(&input, b"pixels").unwrap();

		let report = run(&plans[0], &input, &output).expect("runs");

		assert_eq!(report.bytes, 6);
		assert_eq!(fs::read(&output).unwrap(), b"pixels");
	}

	#[test]
	fn a_failed_run_leaves_the_output_absent() {
		let registry = copy_fixture();
		let plans = plans(&registry, "webp");
		let dir = tempfile::tempdir().unwrap();
		let output = dir.path().join("a.webp");

		let error = run(&plans[0], &dir.path().join("absent.png"), &output).unwrap_err();

		assert_eq!(error.code(), Code::ToolFailed);
		assert!(!output.exists(), "an output path is absent or correct, never empty");
	}

	#[test]
	fn a_tool_that_writes_nothing_is_a_failure() {
		let registry = Registry::load(
			"\
version = 'retype-config-1'

[backends]
prefer = ['noop']

[tools.noop]
command = 'true'
layout = ['{input}', '{ops}', '{output}']

[formats.webp]
family = 'image'
lossy = true
alpha = true
tier = 'converted'
",
		)
		.expect("the fixture loads");
		let plans = plans(&registry, "webp");
		let dir = tempfile::tempdir().unwrap();

		let error = run(&plans[0], &dir.path().join("a.png"), &dir.path().join("a.webp")).unwrap_err();

		assert_eq!(error.code(), Code::ToolFailed);
		assert!(error.to_string().contains("wrote nothing"), "{error}");
	}

	#[test]
	fn a_rename_is_relabelled_before_it_is_converted() {
		let registry = copy_fixture();
		let plans = plans(&registry, "webp");
		let dir = tempfile::tempdir().unwrap();
		let target = dir.path().join("photo.webp");
		fs::write(&target, b"pixels").unwrap();

		let report = convert_rename(
			&plans[0],
			&target,
			"photo",
			"png",
			&target,
			&Cancellation::default(),
			None,
		)
		.expect("converts");

		assert_eq!(report.bytes, 6);
		assert_eq!(fs::read(&target).unwrap(), b"pixels");
		assert!(
			!dir.path().join("photo.png").exists(),
			"the source name is not left behind"
		);
	}

	#[test]
	fn a_failed_rename_conversion_puts_the_source_name_back() {
		let registry = Registry::load(
			"\
version = 'retype-config-1'

[backends]
prefer = ['noop']

[tools.noop]
command = 'true'
layout = ['{input}', '{ops}', '{output}']

[formats.webp]
family = 'image'
lossy = true
alpha = true
tier = 'converted'
",
		)
		.expect("the fixture loads");
		let plans = plans(&registry, "webp");
		let dir = tempfile::tempdir().unwrap();
		let target = dir.path().join("photo.webp");
		fs::write(&target, b"pixels").unwrap();

		let error = convert_rename(
			&plans[0],
			&target,
			"photo",
			"png",
			&target,
			&Cancellation::default(),
			None,
		)
		.unwrap_err();

		assert_eq!(error.code(), Code::ToolFailed);
		assert!(!target.exists(), "a webp path does not keep png bytes");
		assert_eq!(
			fs::read(dir.path().join("photo.png")).unwrap(),
			b"pixels",
			"the file says what it contains"
		);
	}

	/// A fixture whose tool sleeps, so a cancel has something to interrupt.
	#[cfg(unix)]
	fn sleeping_fixture() -> Registry {
		Registry::load(
			"\
version = 'retype-config-1'

[backends]
prefer = ['sleeper']

[tools.sleeper]
command = 'sh'
layout = ['-c', 'sleep 30', '{input}', '{ops}', '{output}']

[formats.webp]
family = 'image'
lossy = true
alpha = true
tier = 'converted'
",
		)
		.expect("the fixture loads")
	}

	#[test]
	#[cfg(unix)]
	fn a_cancelled_run_stops_the_tool_and_leaves_no_output() {
		let registry = sleeping_fixture();
		let plans = plans(&registry, "webp");
		let dir = tempfile::tempdir().unwrap();
		let input = dir.path().join("a.png");
		let output = dir.path().join("a.webp");
		fs::write(&input, b"pixels").unwrap();

		let cancellation = Cancellation::default();
		let started = std::time::Instant::now();

		// The cancel arrives from elsewhere, the way a client would send it.
		let canceller = cancellation.clone();
		std::thread::spawn(move || {
			std::thread::sleep(std::time::Duration::from_millis(300));
			canceller.cancel();
		});

		let error = run_cancellable(&plans[0], &input, &output, &cancellation).unwrap_err();

		assert_eq!(error.code(), Code::JobCancelled);
		assert!(
			started.elapsed() < std::time::Duration::from_secs(10),
			"the tool was not killed"
		);
		assert!(!output.exists(), "a cancelled job commits nothing");
	}

	#[test]
	fn a_run_that_is_not_cancelled_finishes_normally() {
		let registry = copy_fixture();
		let plans = plans(&registry, "webp");
		let dir = tempfile::tempdir().unwrap();
		let input = dir.path().join("a.png");
		let output = dir.path().join("a.webp");
		fs::write(&input, b"pixels").unwrap();

		let report = run_cancellable(&plans[0], &input, &output, &Cancellation::default()).expect("runs");

		assert_eq!(report.bytes, 6);
		assert_eq!(fs::read(&output).unwrap(), b"pixels");
	}

	#[test]
	fn the_stderr_tail_is_kept() {
		assert_eq!(tail(b"  warning: ignored property\n"), "warning: ignored property");
		assert_eq!(tail(&vec![b'x'; STDERR_TAIL + 10]), "x".repeat(STDERR_TAIL));
	}
}
