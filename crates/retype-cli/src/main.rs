use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use retype_cli::error::{Error, Result};
use retype_cli::{client, daemon, doctor};
use retype_core::registry::Registry;
use serde_json::Value;

/// Automatic file format conversion on rename.
#[derive(Parser)]
#[command(name = "retype", version, about, long_about = None)]
struct Cli {
	/// Where state lives, overriding the environment and the default
	#[arg(long, global = true, value_name = "PATH")]
	state_dir: Option<PathBuf>,

	/// Fail instead of starting a daemon
	#[arg(long = "no-spawn", global = true)]
	no_spawn: bool,

	/// Machine readable output
	#[arg(long, global = true)]
	json: bool,

	#[command(subcommand)]
	command: Command,
}

#[derive(Subcommand)]
enum Command {
	/// Run the daemon, or start, stop and inspect one
	Daemon {
		#[command(subcommand)]
		action: Option<DaemonAction>,

		/// Run without a terminal, as an auto-started daemon does
		#[arg(long)]
		detached: bool,
	},

	/// Report external tool availability
	Doctor {
		/// Exit non zero when a required tool is missing or unusable
		#[arg(long)]
		check: bool,

		/// Require a specific tool, repeatable. Implies --check
		#[arg(long = "require", value_name = "TOOL")]
		require: Vec<String>,
	},
}

#[derive(Subcommand)]
enum DaemonAction {
	/// Start a daemon in the background and wait until it answers
	Start,
	/// Stop the running daemon
	Stop,
	/// Report what the running daemon is doing
	Status,
}

fn main() -> ExitCode {
	let cli = Cli::parse();

	if let Command::Doctor { check, require } = &cli.command {
		let code = Registry::builtin()
			.map_err(Error::Config)
			.and_then(|registry| doctor::run(&registry, *check || !require.is_empty(), require));

		return match code {
			Ok(code) => code,
			Err(error) => {
				eprintln!("retype: {error}");
				ExitCode::from(error.exit_code())
			}
		};
	}

	let outcome = match &cli.command {
		Command::Daemon { action, detached } => match action {
			None => daemon::run(cli.state_dir.clone(), !detached),
			Some(action) => control(&cli, action),
		},
		Command::Doctor { .. } => unreachable!("doctor is dispatched before the outcome is built"),
	};

	match outcome {
		Ok(()) => ExitCode::SUCCESS,
		Err(error) => {
			eprintln!("retype: {error}");
			ExitCode::from(error.exit_code())
		}
	}
}

fn control(cli: &Cli, action: &DaemonAction) -> Result<()> {
	let state_dir = retype_cli::state::resolve(cli.state_dir.clone())?;

	match action {
		DaemonAction::Start => {
			if let Ok(mut running) = client::connect(&state_dir) {
				let status = running.call_without_params("status")?;
				print_status(&status, cli.json);
				return Ok(());
			}

			let log = client::spawn_detached(&state_dir)?;
			let mut client = client::wait_for(&state_dir, &log)?;
			let status = client.call_without_params("status")?;
			print_status(&status, cli.json);
			Ok(())
		}
		DaemonAction::Stop => {
			let mut client = client::connect_or_spawn(&state_dir, !cli.no_spawn)?;
			client.call_without_params("shutdown")?;

			if !client::wait_for_stop(&state_dir, std::time::Duration::from_secs(3)) {
				return Err(retype_cli::error::Error::DaemonUnavailable);
			}

			if !cli.json {
				println!("stopped");
			}
			Ok(())
		}
		DaemonAction::Status => {
			let mut client = client::connect_or_spawn(&state_dir, !cli.no_spawn)?;
			let status = client.call_without_params("status")?;
			print_status(&status, cli.json);
			Ok(())
		}
	}
}

fn print_status(status: &Value, json: bool) {
	if json {
		println!(
			"{}",
			serde_json::to_string_pretty(status).unwrap_or_else(|_| status.to_string())
		);
		return;
	}

	for (name, text) in client::describe_status(status) {
		println!("{name:<9}{text}");
	}
}
