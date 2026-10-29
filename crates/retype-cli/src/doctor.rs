//! Tool preflight, `docs/tools.md`.

use std::process::ExitCode;

use retype_core::tools;

pub fn run(check: bool, require: &[String]) -> ExitCode {
	let statuses = tools::probe_all();

	println!("{:<10}{:<24}PURPOSE", "TOOL", "STATUS");
	for status in &statuses {
		println!(
			"{:<10}{:<24}{}",
			status.spec.name,
			status.summary(),
			status.spec.purpose
		);
	}

	let engine = tools::resolve_pdf_engine();
	println!("{:<10}{:<24}document to PDF", "pdf", engine.unwrap_or("no engine"));

	if !check {
		return ExitCode::SUCCESS;
	}

	let required: Vec<&str> = if require.is_empty() {
		statuses.iter().map(|status| status.spec.name).collect()
	} else {
		require.iter().map(String::as_str).collect()
	};

	let mut failed = false;
	for name in &required {
		let Some(status) = statuses.iter().find(|status| status.spec.name == *name) else {
			eprintln!("retype: unknown tool '{name}'");
			failed = true;
			continue;
		};

		if !status.is_usable() {
			eprintln!("retype: {} is {} ({})", name, status.summary(), status.spec.purpose);
			failed = true;
		} else if !tools::meets_minimum(status) {
			eprintln!(
				"retype: {} is {} but at least {} is required",
				name,
				status.version.as_ref().map(ToString::to_string).unwrap_or_default(),
				status.spec.min_version
			);
			failed = true;
		}
	}

	if failed {
		eprintln!("retype: install the missing tools or drop the capabilities they provide");
		return ExitCode::from(4);
	}

	ExitCode::SUCCESS
}
