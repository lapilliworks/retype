use std::path::PathBuf;
use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};
use retype_cli::error::Result;
use retype_cli::{
	bench, client, config, daemon, doctor, explain, fidelity, init, list, output, report, state, verbose, why,
};
use retype_core::tools;
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
		action: Option<JobsAction>,

		/// Walk the parked jobs and read one key each
		#[arg(long)]
		interactive: bool,
	},

	/// Register directories, or list what is registered
	Watch {
		/// Directories to watch. With none, the registrations are listed
		directories: Vec<String>,

		/// Watch this directory only, not its subdirectories
		#[arg(long = "non-recursive")]
		non_recursive: bool,

		/// Stop converting renames here until resumed
		#[arg(long, conflicts_with = "resume")]
		pause: bool,

		/// Resume a paused registration, unless its directory policy pauses it
		#[arg(long, conflicts_with = "pause")]
		resume: bool,
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

		/// The property fixtures to check each pair against
		#[arg(long, value_name = "DIR")]
		fixtures: Option<std::path::PathBuf>,
	},

	/// Handle a retype:// link from a notification or a registered scheme
	#[command(hide = true)]
	Uri {
		/// The retype:// URI to handle
		uri: String,
	},

	/// Print the version, and the schemas this build reads
	Version,

	/// Print a shell completion script
	Completions {
		/// The shell to write a script for
		#[arg(value_enum)]
		shell: clap_complete::Shell,
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

		/// Write a support report, here or to this path
		#[arg(long, value_name = "PATH", num_args = 0..=1)]
		report: Option<Option<PathBuf>>,

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
		#[arg(long, value_parser = clap::builder::PossibleValuesParser::new(retype_cli::jobs::STATES))]
		state: Option<String>,

		/// Show at most this many
		#[arg(long)]
		limit: Option<usize>,

		/// Only jobs newer than this id
		#[arg(long, value_name = "ID")]
		since: Option<String>,
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

	/// Run a parked job now, or run what can be carried
	Resolve {
		/// A job id, or a prefix unique among the jobs
		job: String,

		/// Run it as asked, ignoring the wait
		#[arg(long)]
		now: bool,

		/// Run the corrected intent, dropping the tokens that could not be carried
		#[arg(long)]
		convert: bool,

		/// Overwrite an output that appeared while the job was parked
		#[arg(long)]
		force: bool,
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
	Stop {
		/// Cancel what is running instead of letting it drain
		#[arg(long)]
		now: bool,

		/// Stop even when a supervisor would start it again
		#[arg(long)]
		force: bool,
	},
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
	output::start(cli.json);
	retype_ipc::client::trace_with(|line| verbose::detail(line));
	announce(cli.config.as_deref());

	if let Command::Uri { uri } = &cli.command {
		let code = retype_cli::state::resolve(cli.state_dir.clone())
			.and_then(|state_dir| retype_cli::uri::run(&state_dir, uri, cli.json));

		return finish(code);
	}

	if let Command::Undo { job, last } = &cli.command {
		let code = retype_cli::state::resolve(cli.state_dir.clone())
			.and_then(|state_dir| retype_cli::undo::run(&state_dir, job.as_deref(), *last, cli.json));

		return finish(code);
	}

	if let Command::Jobs { action, interactive } = &cli.command {
		if *interactive {
			let code = retype_cli::state::resolve(cli.state_dir.clone())
				.and_then(|state_dir| retype_cli::jobs::interactive(&state_dir, cli.json, !cli.no_spawn));

			return finish(code);
		}

		let Some(action) = action else {
			return ExitCode::from(output::usage(
				retype_core::error::Code::UsageArgs,
				"jobs needs a subcommand, or --interactive",
			));
		};

		let code = retype_cli::state::resolve(cli.state_dir.clone()).and_then(|state_dir| match action {
			JobsAction::List { state, limit, since } => retype_cli::jobs::list(
				&state_dir,
				state.as_deref(),
				*limit,
				since.as_deref(),
				cli.json,
				!cli.no_spawn,
			),
			JobsAction::Cancel { job } => retype_cli::jobs::cancel(&state_dir, job, cli.json, !cli.no_spawn),
			JobsAction::Get { job } => retype_cli::jobs::get(&state_dir, job, !cli.no_spawn),
			JobsAction::Logs { job, follow } => {
				retype_cli::jobs::logs(&state_dir, job, *follow, cli.json, !cli.no_spawn)
			}
			JobsAction::Resume { job, force } => {
				retype_cli::jobs::resume(&state_dir, job, *force, cli.json, !cli.no_spawn)
			}
			JobsAction::Resolve {
				job,
				now,
				convert,
				force,
			} => match (now, convert, force) {
				(true, false, _) | (false, true, false) => {
					retype_cli::jobs::resolve(&state_dir, job, *convert, *force, cli.json, !cli.no_spawn)
				}
				(false, true, true) => {
					let code = output::usage(
						retype_core::error::Code::UsageConflict,
						"--force belongs to --now; a reduced request does not overwrite what somebody else wrote",
					);
					Ok(ExitCode::from(code))
				}
				_ => {
					let code = output::usage(
						retype_core::error::Code::UsageConflict,
						"resolve needs exactly one of --now or --convert",
					);
					Ok(ExitCode::from(code))
				}
			},
		});

		return finish(code);
	}

	if let Command::Watch {
		directories,
		non_recursive,
		pause,
		resume,
	} = &cli.command
	{
		let code = retype_cli::state::resolve(cli.state_dir.clone()).and_then(|state_dir| {
			tools::use_managed(state_dir.join("tools").join("bin"));

			config::load().and_then(|registry| {
				retype_cli::watch::run(
					&state_dir,
					directories,
					!non_recursive,
					cli.json,
					registry.watch().allow_synced,
					*pause,
					*resume,
				)
			})
		});

		return finish(code);
	}

	if let Command::Unwatch { directories } = &cli.command {
		let code = retype_cli::state::resolve(cli.state_dir.clone())
			.and_then(|state_dir| retype_cli::watch::unwatch(&state_dir, directories, cli.json));

		return finish(code);
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
			tools::use_managed(state_dir.join("tools").join("bin"));

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

		return finish(code);
	}

	if let Command::Explain { source, intent } = &cli.command {
		// An explanation does not need writable state; the estimate does, and is
		// simply absent when the machine has no state directory.
		let state_dir = state::resolve(cli.state_dir.clone()).ok();

		if let Some(path) = &state_dir {
			tools::use_managed(path.join("tools").join("bin"));
		}

		let code = config::load().and_then(|registry| {
			config::report_warnings(&registry);

			explain::run(
				&registry,
				&explain::Request {
					source,
					intent,
					state_dir: state_dir.as_deref(),
					json: cli.json,
					quiet: cli.quiet,
					output: None,
					force: false,
					dry_run: false,
				},
			)
		});

		return finish(code);
	}

	if let Command::Bench { pairs, families } = &cli.command {
		let code = retype_cli::state::resolve(cli.state_dir.clone()).and_then(|state_dir| {
			tools::use_managed(state_dir.join("tools").join("bin"));

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

		return finish(code);
	}

	if let Command::Fidelity {
		pairs,
		families,
		write,
		manifest,
		render,
		fixtures,
	} = &cli.command
	{
		let code = retype_cli::state::resolve(cli.state_dir.clone()).and_then(|state_dir| {
			tools::use_managed(state_dir.join("tools").join("bin"));

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
						fixtures: fixtures.as_deref(),
						json: cli.json,
						quiet: cli.quiet,
					},
				)
			})
		});

		return finish(code);
	}

	if matches!(cli.command, Command::Version) {
		print_version(cli.json);
		return ExitCode::SUCCESS;
	}

	if let Command::Completions { shell } = &cli.command {
		// The script is written for the name the binary was installed as, which is
		// `retype` and never the `c` alias: an alias is a command, not a program.
		let mut command = Cli::command();
		clap_complete::generate(*shell, &mut command, "retype", &mut std::io::stdout());
		return ExitCode::SUCCESS;
	}

	if let Command::List { tiers } = &cli.command {
		if let Ok(state_dir) = state::resolve(cli.state_dir.clone()) {
			tools::use_managed(state_dir.join("tools").join("bin"));
		}

		let code = config::load().and_then(|registry| {
			config::report_warnings(&registry);
			list::run(&registry, *tiers, cli.json)
		});

		return finish(code);
	}

	if let Command::Why { path } = &cli.command {
		let code = state::resolve(cli.state_dir.clone()).and_then(|state_dir| {
			tools::use_managed(state_dir.join("tools").join("bin"));

			config::load().and_then(|registry| {
				config::report_warnings(&registry);
				why::run(&registry, path, cli.json, cli.quiet, &state_dir, !cli.no_spawn)
			})
		});

		return finish(code);
	}

	if let Command::Init { output } = &cli.command {
		let code = init::run(output.as_deref(), cli.quiet, cli.json);

		return finish(code);
	}

	if let Command::Doctor {
		check,
		emit,
		write,
		report,
		require,
	} = &cli.command
	{
		// A report and a snippet answer different questions, and one command that
		// did both would write a file nobody asked for.
		if report.is_some() && (*emit || *write) {
			return ExitCode::from(output::usage(
				retype_core::error::Code::UsageConflict,
				"--report cannot be combined with --emit or --write",
			));
		}

		if let Some(path) = report {
			let code = state::resolve(cli.state_dir.clone()).and_then(|state_dir| {
				config::load().and_then(|registry| {
					config::report_warnings(&registry);

					let written = report::write(&registry, &state_dir, path.as_deref())?;

					if cli.json {
						output::success(serde_json::json!({"path": written.display().to_string()}));
					} else if !cli.quiet {
						eprintln!("retype: wrote {}", written.display());
					}

					Ok(ExitCode::SUCCESS)
				})
			});

			return finish(code);
		}

		if *emit && *write {
			return ExitCode::from(output::usage(
				retype_core::error::Code::UsageConflict,
				"--emit and --write answer the same question differently; choose one",
			));
		}

		let mode = match (emit, write) {
			(false, false) => doctor::Emit::None,
			(true, false) => doctor::Emit::Stdout,
			(false, true) | (true, true) => doctor::Emit::Write,
		};

		if let Ok(state_dir) = state::resolve(cli.state_dir.clone()) {
			tools::use_managed(state_dir.join("tools").join("bin"));
		}

		let code = config::load().and_then(|registry| {
			config::report_warnings(&registry);

			doctor::run(
				&registry,
				*check || !require.is_empty(),
				require,
				mode,
				cli.json,
				cli.quiet,
			)
		});

		return finish(code);
	}

	let outcome = match &cli.command {
		Command::Daemon { action, detached } => match action {
			None => daemon::run(cli.state_dir.clone(), !detached).map(|()| ExitCode::SUCCESS),
			Some(action) => control(&cli, action),
		},
		Command::Uri { .. }
		| Command::Undo { .. }
		| Command::Jobs { .. }
		| Command::Watch { .. }
		| Command::Unwatch { .. }
		| Command::Convert { .. }
		| Command::Explain { .. }
		| Command::Why { .. }
		| Command::Bench { .. }
		| Command::Fidelity { .. }
		| Command::Version
		| Command::Completions { .. }
		| Command::List { .. }
		| Command::Init { .. }
		| Command::Doctor { .. } => unreachable!("these are dispatched before the outcome is built"),
	};

	finish(outcome)
}

/// One error rendering for every command: human on stderr, an envelope on stdout
/// when `--json` asked for one, `docs/errors.md` §7.
fn finish(code: Result<ExitCode>) -> ExitCode {
	match code {
		Ok(code) => code,
		Err(error) => ExitCode::from(output::fail_error(&error)),
	}
}

/// `retype version`, `docs/cli.md` §1.
///
/// The schema identifiers are part of the answer: a bug report that names them can
/// be matched to a build without guessing which grammar or journal a file came
/// from.
fn print_version(json: bool) {
	const SCHEMAS: &[(&str, &str)] = &[
		("grammar", retype_core::intent::GRAMMAR_VERSION),
		("config", retype_core::registry::CONFIG_VERSION),
		("ipc", retype_ipc::IPC_VERSION),
		("journal", retype_core::job::JOURNAL_VERSION),
		("undo", retype_core::undo::UNDO_VERSION),
		("watches", retype_core::watches::WATCHES_VERSION),
		("bench", retype_core::estimate::BENCH_VERSION),
		("fidelity", retype_core::fidelity::FIDELITY_VERSION),
		("fixtures", retype_core::axis::FIXTURES_VERSION),
	];

	if json {
		let schemas: serde_json::Map<String, Value> = SCHEMAS
			.iter()
			.map(|(name, version)| (name.to_string(), Value::String(version.to_string())))
			.collect();

		println!(
			"{}",
			serde_json::json!({
				"v": "retype-cli-json-1",
				"ok": true,
				"name": "retype",
				"version": env!("CARGO_PKG_VERSION"),
				"schemas": schemas,
			})
		);
		return;
	}

	println!("retype {}", env!("CARGO_PKG_VERSION"));

	for (name, version) in SCHEMAS {
		println!("  {name:<10}{version}");
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

fn control(cli: &Cli, action: &DaemonAction) -> Result<ExitCode> {
	let state_dir = retype_cli::state::resolve(cli.state_dir.clone())?;

	match action {
		DaemonAction::Start => {
			if let Ok(mut running) = client::connect(&state_dir) {
				let status = running.call_without_params("status")?;
				print_status(&status, cli.json);
				return Ok(ExitCode::SUCCESS);
			}

			let log = client::spawn_detached(&state_dir)?;
			let mut client = client::wait_for(&state_dir, &log)?;
			let status = client.call_without_params("status")?;
			print_status(&status, cli.json);
			Ok(ExitCode::SUCCESS)
		}
		DaemonAction::Stop { now, force } => {
			let mut client = client::connect_or_spawn(&state_dir, !cli.no_spawn)?;

			let answer = match client.call("shutdown", serde_json::json!({"now": now, "force": force})) {
				Ok(answer) => answer,
				// A refusal is an answer, not a transport failure: the daemon is still
				// there and said why, so the message and the code are the daemon's.
				Err(retype_ipc::ClientError::Rpc(error)) => {
					return Ok(ExitCode::from(output::fail(&error.code, &error.message)));
				}
				Err(other) => return Err(other.into()),
			};

			if !client::wait_for_stop(&state_dir, std::time::Duration::from_secs(3)) {
				return Err(retype_cli::error::Error::DaemonUnavailable);
			}

			let cancelled = answer.get("cancelled").and_then(Value::as_u64).unwrap_or(0);

			if cli.json {
				output::success(answer);
			} else {
				match cancelled {
					0 => println!("stopped"),
					count => println!("stopped, {} job(s) cancelled", count),
				}
			}

			Ok(ExitCode::SUCCESS)
		}
		DaemonAction::Status => {
			let mut client = client::connect_or_spawn(&state_dir, !cli.no_spawn)?;
			let status = client.call_without_params("status")?;
			print_status(&status, cli.json);
			Ok(ExitCode::SUCCESS)
		}
	}
}

fn print_status(status: &Value, json: bool) {
	if json {
		output::success(status.clone());
		return;
	}

	for (name, text) in client::describe_status(status) {
		println!("{name:<13}{text}");
	}
}
