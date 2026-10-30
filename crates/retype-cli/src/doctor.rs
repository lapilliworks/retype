//! Tool preflight, `docs/tools.md`.

use std::process::ExitCode;

use retype_core::registry::Registry;
use retype_core::tools;

use crate::error::Result;

/// What to do with the pinning snippet, `docs/config.md` §13.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emit {
	/// Report only.
	None,
	/// Print the snippet and write nothing.
	Stdout,
	/// Write it to the user configuration, which has to be absent or empty.
	Write,
}

pub fn run(registry: &Registry, check: bool, require: &[String], emit: Emit) -> Result<ExitCode> {
	let declared: Vec<tools::Tool> = registry.tools().cloned().collect();
	let statuses = tools::probe_all(&declared);

	if emit != Emit::None {
		return choose(&statuses, emit);
	}

	println!("{:<10}{:<24}PURPOSE", "TOOL", "STATUS");
	for status in &statuses {
		println!(
			"{:<10}{:<24}{}",
			status.tool.name,
			status.summary(),
			status.tool.purpose
		);
	}

	// Every declared engine, since naming `pandoc` and `pdf` here would put the
	// support matrix back into the code.
	for tool in registry.tools() {
		for engine in tool.engines.keys() {
			let resolved = tools::resolve_engine(tool, engine).unwrap_or("no engine");
			println!(
				"{:<14}{:<24}{}: {engine} engine",
				format!("{} {engine}", tool.name),
				resolved,
				tool.purpose
			);
		}
	}

	if !check {
		return Ok(ExitCode::SUCCESS);
	}

	let required: Vec<&str> = if require.is_empty() {
		statuses.iter().map(|status| status.tool.name.as_str()).collect()
	} else {
		require.iter().map(String::as_str).collect()
	};

	let mut failed = false;
	for name in &required {
		let Some(status) = statuses.iter().find(|status| status.tool.name == *name) else {
			eprintln!("retype: unknown tool '{name}'");
			failed = true;
			continue;
		};

		if !status.is_usable() {
			let floor = status
				.tool
				.version_min
				.as_ref()
				.map(|version| format!(", at least {version} required"))
				.unwrap_or_default();

			eprintln!(
				"retype: {} is {}{floor} ({})",
				name,
				status.summary(),
				status.tool.purpose
			);
			failed = true;
		}
	}

	if failed {
		eprintln!("retype: install the missing tools or drop the capabilities they provide");
		return Ok(ExitCode::from(4));
	}

	Ok(ExitCode::SUCCESS)
}

/// The `[tools]` snippet that pins what this machine resolved, `docs/config.md`
/// §13.
///
/// Only tools that are present are pinned: a tool that is absent keeps its shipped
/// declaration, and pinning a path that does not exist would make the next run
/// worse rather than more reproducible.
fn choose(statuses: &[tools::ToolStatus<'_>], emit: Emit) -> Result<ExitCode> {
	let mut snippet = String::from(
		"# retype: the tools this machine resolved. Merge into your configuration to pin\n\
# them; anything absent here keeps its shipped declaration.\n\
version = 'retype-config-1'\n",
	);

	for status in statuses.iter().filter(|status| status.is_usable()) {
		let Some(path) = status.path.as_ref() else {
			continue;
		};

		let command = path.display().to_string();

		// A command with a space cannot be named in configuration, which is V5's
		// point: no shell, no quoting, no ambiguity.
		if command.contains(' ') {
			eprintln!(
				"retype: {} cannot be pinned: its path has a space ({command})",
				status.tool.name
			);
			continue;
		}

		snippet.push_str(&format!("\n[tools.{}]\ncommand = '{command}'\n", status.tool.name));
	}

	// The snippet is ordinary configuration, so it is checked as one before it is
	// offered: writing a file this build refuses to load would be a bug with a
	// delayed symptom.
	if let Err(problems) = Registry::with_user(&snippet, "the emitted snippet") {
		eprintln!("retype: {problems}");
		return Ok(ExitCode::from(3));
	}

	match emit {
		Emit::Stdout | Emit::None => print!("{snippet}"),
		Emit::Write => {
			let Some(path) = crate::config::user_path() else {
				eprintln!("retype: no user configuration path on this machine; set RETYPE_CONFIG");
				return Ok(ExitCode::from(4));
			};

			match std::fs::read_to_string(&path) {
				Ok(text) if !text.trim().is_empty() => {
					eprintln!(
						"retype: {} already exists; merge the snippet from `retype doctor --emit` instead",
						path.display()
					);
					return Ok(ExitCode::from(7));
				}
				_ => {}
			}

			if let Some(parent) = path.parent() {
				std::fs::create_dir_all(parent)?;
			}

			std::fs::write(&path, &snippet)?;
			eprintln!("retype: wrote {}", path.display());
		}
	}

	Ok(ExitCode::SUCCESS)
}
