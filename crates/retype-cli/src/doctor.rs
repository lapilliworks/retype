//! Tool preflight, `docs/tools.md`.

use std::process::ExitCode;

use retype_core::registry::Registry;
use retype_core::tools;

use crate::error::Result;

pub fn run(registry: &Registry, check: bool, require: &[String]) -> Result<ExitCode> {
	let declared: Vec<tools::Tool> = registry.tools().cloned().collect();
	let statuses = tools::probe_all(&declared);

	println!("{:<10}{:<24}PURPOSE", "TOOL", "STATUS");
	for status in &statuses {
		println!(
			"{:<10}{:<24}{}",
			status.tool.name,
			status.summary(),
			status.tool.purpose
		);
	}

	if let Some(pandoc) = registry.tool("pandoc") {
		let engine = tools::resolve_engine(pandoc, "pdf");
		println!("{:<10}{:<24}document to PDF", "pdf", engine.unwrap_or("no engine"));
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
