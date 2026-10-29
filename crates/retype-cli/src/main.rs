use std::process::ExitCode;

use clap::{Parser, Subcommand};
use retype_cli::{daemon, doctor};

/// Automatic file format conversion on rename.
#[derive(Parser)]
#[command(name = "retype", version, about, long_about = None)]
struct Cli {
	#[command(subcommand)]
	command: Command,
}

#[derive(Subcommand)]
enum Command {
	/// Run the daemon in the foreground
	Daemon,

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

fn main() -> ExitCode {
	let cli = Cli::parse();

	match cli.command {
		Command::Daemon => match daemon::run() {
			Ok(()) => ExitCode::SUCCESS,
			Err(error) => {
				eprintln!("retype: {error}");
				ExitCode::from(error.exit_code())
			}
		},
		Command::Doctor { check, require } => doctor::run(check || !require.is_empty(), &require),
	}
}
