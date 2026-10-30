//! Running a plan, `docs/cli.md` §2 and `docs/errors.md` §5.
//!
//! One plan is one argv. The tool writes to a temporary path in the output's own
//! directory and the file is renamed into place only when the tool exits zero, so
//! an output path is either absent or complete. A failure restores nothing
//! because nothing was moved: the source is still where it was, which is why
//! `convert` needs none of the restoration a rename does.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::convert::StagedOutput;
use crate::error::{Code, Diagnostic, Error, Result};
use crate::plan::{Backend, Plan};

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

	let finished = Command::new(&path).args(&argv).output().map_err(|error| {
		Error::Diagnostic(Diagnostic::global(
			Code::ToolFailed,
			format!("{command} could not run: {error}"),
		))
	})?;

	let stderr = tail(&finished.stderr);

	if !finished.status.success() {
		return Err(Error::Diagnostic(Diagnostic::global(
			Code::ToolFailed,
			match stderr.is_empty() {
				true => format!("{command} exited {}", finished.status),
				false => format!("{command} exited {}: {stderr}", finished.status),
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
	fn the_stderr_tail_is_kept() {
		assert_eq!(tail(b"  warning: ignored property\n"), "warning: ignored property");
		assert_eq!(tail(&vec![b'x'; STDERR_TAIL + 10]), "x".repeat(STDERR_TAIL));
	}
}
