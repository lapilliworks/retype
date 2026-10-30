use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use retype_cli::error::Result;
use retype_cli::{bench, client, config, daemon, doctor, explain, fidelity, init, list, state, verbose, why};
use serde_json::Value;

/// Automatic file format conversion on rename.
#[derive(Parser)]
#[command(name = "retype", version, about, long_about = None)]
struct Cli {
	/// Where state lives, overriding the environment and the default
	#[arg(long, global = true, value_name = "PATH")]
	state_dir: Option<PathBuf>,

	/// The configuration file, instead of the environment and the default
	#[arg(long, global = true, value_name = "PATH")]
	config: Option<PathBuf>,

	/// Fail instead of starting a daemon
	#[arg(long = "no-spawn", global = true)]
	no_spawn: bool,

	/// Machine readable output
	#[arg(long, global = true)]
	json: bool,

	/// Suppress everything below errors
	#[arg(long, global = true)]
	quiet: bool,

	/// Say what the command is doing, repeatable
	#[arg(short = 'v', long = "verbose", global = true, action = clap::ArgAction::Count)]
	verbose: u8,

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

	/// Reverse a job: put the source back and remove what it produced
	Undo {
		/// A job id, or a prefix unique among the undo records
		job: Option<String>,

		/// Undo the most recent job
		#[arg(long)]
		last: bool,
	},

	/// Inspect and control jobs
	Jobs {
		#[command(subcommand)]
		action: JobsAction,
	},

	/// Register directories, or list what is registered
	Watch {
		/// Directories to watch. With none, the registrations are listed
		directories: Vec<String>,

		/// Watch this directory only, not its subdirectories
		#[arg(long = "non-recursive")]
		non_recursive: bool,
	},

	/// Deregister directories
	Unwatch {
		/// Directories to stop watching
		directories: Vec<String>,
	},

	/// Convert one file, as a rename would
	#[command(alias = "c")]
	Convert {
		/// The file to convert
		input: String,

		/// The intent string, as it would be written in a rename
		intent: String,

		/// The output path, instead of the one the intent derives
		#[arg(short, long)]
		output: Option<String>,

		/// Overwrite an existing output
		#[arg(long)]
		force: bool,

		/// Print what would happen and write nothing
		#[arg(long = "dry-run")]
		dry_run: bool,
	},

	/// Parse, plan, and explain an intent without touching a file
	Explain {
		/// The name being replaced; it need not exist
		source: String,

		/// The intent string, as it would be written in a rename
		intent: String,
	},

	/// Measure this machine, so estimates are about it rather than about a guess
	Bench {
		/// Measure this pair, written source:target; repeatable
		#[arg(long = "pair", value_name = "SOURCE:TARGET")]
		pairs: Vec<String>,

		/// Measure every pair of this family; repeatable
		#[arg(long = "family", value_name = "FAMILY")]
		families: Vec<String>,
	},

	/// Earn the fidelity claims for this machine, and record the build that did
	Fidelity {
		/// Check this pair, written source:target; repeatable
		#[arg(long = "pair", value_name = "SOURCE:TARGET")]
		pairs: Vec<String>,

		/// Check every pair of this family; repeatable
		#[arg(long = "family", value_name = "FAMILY")]
		families: Vec<String>,

		/// Read the manifest and write it back with what this machine earned
		#[arg(long, value_name = "PATH")]
		write: Option<std::path::PathBuf>,

		/// Compare against this manifest without writing it
		#[arg(long, value_name = "PATH")]
		manifest: Option<std::path::PathBuf>,

		/// Rewrite the claim table between the markers in this document
		#[arg(long, value_name = "PATH")]
		render: Option<std::path::PathBuf>,
	},

	/// Show the configured formats, operations and backends
	List {
		/// Add one line per pair, with the tier it claims and the backend that carries it
		#[arg(long)]
		tiers: bool,
	},

	/// Explain a file's conversion
	Why {
		/// The file to ask about
		path: String,
	},

	/// Write the example configuration
	Init {
		/// Write here instead of to standard output
		#[arg(long, value_name = "PATH")]
		output: Option<PathBuf>,
	},

	/// Report external tool availability
	Doctor {
		/// Exit non zero when a required tool is missing or unusable
		#[arg(long)]
		check: bool,

		/// Print a [tools] snippet pinning what this machine resolved
		#[arg(long)]
		emit: bool,

		/// Write that snippet to the user configuration, which must be absent or empty
		#[arg(long)]
		write: bool,

		/// Require a specific tool, repeatable. Implies --check
		#[arg(long = "require", value_name = "TOOL")]
		require: Vec<String>,
	},
}

#[derive(Subcommand)]
enum JobsAction {
	/// List jobs, newest first
	List {
		/// Only jobs in this state
		#[arg(long)]
		state: Option<String>,

		/// Show at most this many
		#[arg(long)]
		limit: Option<usize>,
	},

	/// Show one job as the daemon recorded it
	Get {
		/// A job id, or a prefix unique among the jobs
		job: String,
	},

	/// Show what a job's tool printed
	Logs {
		/// A job id, or a prefix unique among the jobs
		job: String,

		/// Print what arrives while the job runs
		#[arg(long)]
		follow: bool,
	},

	/// Stop a job that is waiting or running
	Cancel {
		/// A job id, or a prefix unique among the jobs
		job: String,
	},

	/// Continue a partial job, running the outputs that did not succeed
	Resume {
		/// A job id, or a prefix unique among the jobs
		job: String,

		/// Run every output again instead of skipping the ones that are still there
		#[arg(long)]
		force: bool,
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
	// A pipe that closes early is how `list | head` is meant to behave. Rust
	// ignores SIGPIPE, which turns that into a panic and a backtrace.
	#[cfg(unix)]
	unsafe {
		libc::signal(libc::SIGPIPE, libc::SIG_DFL);
	}

	let cli = Cli::parse();
	config::use_path(cli.config.clone());
	verbose::start(cli.verbose, cli.quiet);
	announce(cli.config.as_deref());

	if let Command::Undo { job, last } = &cli.command {
		let code = retype_cli::state::resolve(cli.state_dir.clone())
			.and_then(|state_dir| retype_cli::undo::run(&state_dir, job.as_deref(), *last, cli.json));

		return match code {
			Ok(code) => code,
			Err(error) => {
				eprintln!("retype: {error}");
				ExitCode::from(error.exit_code())
			}
		};
	}

	if let Command::Jobs { action } = &cli.command {
		let code = retype_cli::state::resolve(cli.state_dir.clone()).and_then(|state_dir| match action {
			JobsAction::List { state, limit } => {
				retype_cli::jobs::list(&state_dir, state.as_deref(), *limit, cli.json, !cli.no_spawn)
			}
			JobsAction::Cancel { job } => retype_cli::jobs::cancel(&state_dir, job, cli.json, !cli.no_spawn),
			JobsAction::Get { job } => retype_cli::jobs::get(&state_dir, job, !cli.no_spawn),
			JobsAction::Logs { job, follow } => retype_cli::jobs::logs(&state_dir, job, *follow, !cli.no_spawn),
			JobsAction::Resume { job, force } => {
				retype_cli::jobs::resume(&state_dir, job, *force, cli.json, !cli.no_spawn)
			}
		});

		return match code {
			Ok(code) => code,
			Err(error) => {
				eprintln!("retype: {error}");
				ExitCode::from(error.exit_code())
			}
		};
	}

	if let Command::Watch {
		directories,
		non_recursive,
	} = &cli.command
	{
		let code = retype_cli::state::resolve(cli.state_dir.clone())
			.and_then(|state_dir| retype_cli::watch::run(&state_dir, directories, !non_recursive, cli.json));

		return match code {
			Ok(code) => code,
			Err(error) => {
				eprintln!("retype: {error}");
				ExitCode::from(error.exit_code())
			}
		};
	}

	if let Command::Unwatch { directories } = &cli.command {
		let code = retype_cli::state::resolve(cli.state_dir.clone())
			.and_then(|state_dir| retype_cli::watch::unwatch(&state_dir, directories, cli.json));

		return match code {
			Ok(code) => code,
			Err(error) => {
				eprintln!("retype: {error}");
				ExitCode::from(error.exit_code())
			}
		};
	}

	if let Command::Convert {
		input,
		intent,
		output,
		force,
		dry_run,
	} = &cli.command
	{
		let code = retype_cli::state::resolve(cli.state_dir.clone()).and_then(|state_dir| {
			config::load().and_then(|registry| {
				config::report_warnings(&registry);

				retype_cli::convert::run(
					&registry,
					&retype_cli::convert::Request {
						input,
						intent,
						output: output.as_deref(),
						force: *force,
						dry_run: *dry_run,
						state_dir: &state_dir,
						quiet: cli.quiet,
						json: cli.json,
						allow_spawn: !cli.no_spawn,
					},
				)
			})
		});

		return match code {
			Ok(code) => code,
			Err(error) => {
				eprintln!("retype: {error}");
				ExitCode::from(error.exit_code())
			}
		};
	}

	if let Command::Explain { source, intent } = &cli.command {
		let code = state::resolve(cli.state_dir.clone()).and_then(|state_dir| {
			config::load().and_then(|registry| {
				config::report_warnings(&registry);

				explain::run(
					&registry,
					&explain::Request {
						source,
						intent,
						state_dir: &state_dir,
						json: cli.json,
						quiet: cli.quiet,
					},
				)
			})
		});

		return match code {
			Ok(code) => code,
			Err(error) => {
				eprintln!("retype: {error}");
				ExitCode::from(error.exit_code())
			}
		};
	}

	if let Command::Bench { pairs, families } = &cli.command {
		let code = retype_cli::state::resolve(cli.state_dir.clone()).and_then(|state_dir| {
			config::load().and_then(|registry| {
				config::report_warnings(&registry);

				bench::run(
					&registry,
					&state_dir,
					&bench::Request {
						pairs,
						families,
						json: cli.json,
						quiet: cli.quiet,
					},
				)
			})
		});

		return match code {
			Ok(code) => code,
			Err(error) => {
				eprintln!("retype: {error}");
				ExitCode::from(error.exit_code())
			}
		};
	}

	if let Command::Fidelity {
		pairs,
		families,
		write,
		manifest,
		render,
	} = &cli.command
	{
		let code = retype_cli::state::resolve(cli.state_dir.clone()).and_then(|state_dir| {
			config::load().and_then(|registry| {
				config::report_warnings(&registry);

				fidelity::run(
					&registry,
					&state_dir,
					&fidelity::Request {
						pairs,
						families,
						write: write.as_deref(),
						manifest: manifest.as_deref(),
						render: render.as_deref(),
						json: cli.json,
						quiet: cli.quiet,
					},
				)
			})
		});

		return match code {
			Ok(code) => code,
			Err(error) => {
				eprintln!("retype: {error}");
				ExitCode::from(error.exit_code())
			}
		};
	}

	if let Command::List { tiers } = &cli.command {
		let code = config::load().and_then(|registry| {
			config::report_warnings(&registry);
			list::run(&registry, *tiers, cli.json)
		});

		return match code {
			Ok(code) => code,
			Err(error) => {
				eprintln!("retype: {error}");
				ExitCode::from(error.exit_code())
			}
		};
	}

	if let Command::Why { path } = &cli.command {
		let code = state::resolve(cli.state_dir.clone())
			.and_then(|state_dir| why::run(path, cli.json, cli.quiet, &state_dir, !cli.no_spawn));

		return match code {
			Ok(code) => code,
			Err(error) => {
				eprintln!("retype: {error}");
				ExitCode::from(error.exit_code())
			}
		};
	}

	if let Command::Init { output } = &cli.command {
		let code = init::run(output.as_deref(), cli.quiet);

		return match code {
			Ok(code) => code,
			Err(error) => {
				eprintln!("retype: {error}");
				ExitCode::from(error.exit_code())
			}
		};
	}

	if let Command::Doctor {
		check,
		emit,
		write,
		require,
	} = &cli.command
	{
		let mode = match (emit, write) {
			(false, false) => doctor::Emit::None,
			(true, false) => doctor::Emit::Stdout,
			(false, true) | (true, true) => doctor::Emit::Write,
		};

		let code = config::load().and_then(|registry| {
			config::report_warnings(&registry);

			doctor::run(&registry, *check || !require.is_empty(), require, mode)
		});

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
		Command::Undo { .. }
		| Command::Jobs { .. }
		| Command::Watch { .. }
		| Command::Unwatch { .. }
		| Command::Convert { .. }
		| Command::Explain { .. }
		| Command::Why { .. }
		| Command::Bench { .. }
		| Command::Fidelity { .. }
		| Command::List { .. }
		| Command::Init { .. }
		| Command::Doctor { .. } => unreachable!("these are dispatched before the outcome is built"),
	};

	match outcome {
		Ok(()) => ExitCode::SUCCESS,
		Err(error) => {
			eprintln!("retype: {error}");
			ExitCode::from(error.exit_code())
		}
	}
}

/// Say which files a command is about to read, which is the first question when
/// output looks wrong.
fn announce(config: Option<&std::path::Path>) {
	match config.or(config::user_path().as_deref()) {
		Some(path) => verbose::note(format!("configuration {}", path.display())),
		None => verbose::note("configuration: the built-in table"),
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
