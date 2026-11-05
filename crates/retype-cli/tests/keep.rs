//! The keep-original control and exact source-name preservation.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Daemon {
	child: Child,
	state: PathBuf,
}

impl Drop for Daemon {
	fn drop(&mut self) {
		if let Ok(mut client) = retype_cli::client::connect(&self.state) {
			let _ = client.shutdown(&retype_ipc::ShutdownRequest { now: true, force: true });
		}
		let _ = self.child.kill();
		let _ = self.child.wait();
	}
}

fn archive() -> Vec<u8> {
	let mut builder = tar::Builder::new(Vec::new());
	let mut header = tar::Header::new_gnu();
	header.set_size(7);
	header.set_mode(0o644);
	header.set_cksum();
	builder.append_data(&mut header, "file.txt", &b"payload"[..]).unwrap();
	builder.into_inner().unwrap()
}

#[derive(Clone, Copy)]
enum UndoPolicy {
	Enabled,
	Disabled,
}

fn configuration(dir: &Path, undo: UndoPolicy) -> PathBuf {
	let undo_enabled = matches!(undo, UndoPolicy::Enabled);
	let config = dir.join("retype.toml");
	fs::write(
		&config,
		format!(
			"version = 'retype-config-1'\n[notify]\nenabled = false\n[deferral]\nmode = 'now'\n[undo]\nenabled = {undo_enabled}\n[formats.tar]\naliases = ['mytar']\n"
		),
	)
	.unwrap();
	config
}

fn start_daemon(state: &Path, config: &Path) -> Daemon {
	let daemon = Daemon {
		child: Command::new(env!("CARGO_BIN_EXE_retype"))
			.args(["daemon", "--detached"])
			.arg("--state-dir")
			.arg(state)
			.env("RETYPE_CONFIG", config)
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.spawn()
			.unwrap(),
		state: state.to_path_buf(),
	};
	let deadline = Instant::now() + Duration::from_secs(10);
	while retype_cli::client::connect(state).is_err() {
		assert!(Instant::now() < deadline, "the daemon did not start");
		std::thread::sleep(Duration::from_millis(25));
	}
	daemon
}

fn wait_for_watch(state: &Path) {
	let deadline = Instant::now() + Duration::from_secs(10);
	loop {
		let status = {
			let mut client = retype_cli::client::connect(state).unwrap();
			client.status().unwrap()
		};
		if status.watches.first().and_then(|watch| watch.status.as_deref()) == Some("ready") {
			return;
		}
		assert!(Instant::now() < deadline, "watch did not become ready: {status:?}");
		std::thread::sleep(Duration::from_millis(25));
	}
}

fn wait_for_job(state: &Path, expected: retype_core::job::JobState) -> retype_core::job::Job {
	let deadline = Instant::now() + Duration::from_secs(15);
	loop {
		let job = {
			let mut client = retype_cli::client::connect(state).unwrap();
			let list = client.jobs_list().unwrap();
			list.jobs.last().map(|summary| summary.id.clone())
		};
		if let Some(id) = job {
			let job = {
				let mut client = retype_cli::client::connect(state).unwrap();
				client.jobs_get(id.as_str()).unwrap()
			};
			if job.state == expected {
				return job;
			}
			assert_ne!(job.state, retype_core::job::JobState::Failed, "{job:?}");
		}
		assert!(Instant::now() < deadline, "job did not reach {expected:?}");
		std::thread::sleep(Duration::from_millis(25));
	}
}

fn check_zip(path: &Path) {
	let mut zip = zip::ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
	let mut contents = String::new();
	zip.by_name("file.txt").unwrap().read_to_string(&mut contents).unwrap();
	assert_eq!(contents, "payload");
}

struct WatchCase<'a> {
	source_name: &'a str,
	intent: &'a str,
	needs_confirmation: bool,
	undo: UndoPolicy,
}

fn watched(case: WatchCase<'_>) {
	let WatchCase {
		source_name,
		intent,
		needs_confirmation,
		undo,
	} = case;
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let photos = dir.path().join("photos");
	fs::create_dir(&photos).unwrap();
	let source = photos.join(source_name);
	let original = archive();
	fs::write(&source, &original).unwrap();
	let config = configuration(dir.path(), undo);
	let registered = Command::new(env!("CARGO_BIN_EXE_retype"))
		.arg("watch")
		.arg(&photos)
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.output()
		.unwrap();
	assert!(registered.status.success());
	let _daemon = start_daemon(&state, &config);
	wait_for_watch(&state);
	fs::rename(&source, photos.join(format!("Bundle.{intent}"))).unwrap();
	if needs_confirmation {
		let pending = wait_for_job(&state, retype_core::job::JobState::NeedsAttention);
		assert_eq!(fs::read(&source).unwrap(), original);
		let mut client = retype_cli::client::connect(&state).unwrap();
		client
			.jobs_resolve(pending.id.as_str(), retype_ipc::ResolveMode::Now, false)
			.unwrap();
	}
	let job = wait_for_job(&state, retype_core::job::JobState::Done);
	let output = photos.join("Bundle.zip");
	check_zip(&output);
	let second = photos.join("Bundle.tar.gz");
	if intent.contains("tar.gz") {
		assert!(second.is_file(), "the second requested output exists");
	}
	assert_eq!(
		fs::read(&source).unwrap(),
		original,
		"the exact source name remains visible"
	);
	if intent.contains("+keep") {
		assert!(job.intent.as_deref().unwrap().contains("+keep"));
	}
	let store = retype_core::undo::Store::open(&state);
	if matches!(undo, UndoPolicy::Disabled) {
		assert!(store.list().unwrap().is_empty());
		return;
	}
	let undo = Command::new(env!("CARGO_BIN_EXE_retype"))
		.args(["undo", job.id.as_str(), "--no-spawn"])
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.output()
		.unwrap();
	if retype_core::fsx::supports_durable_cleanup() {
		assert!(undo.status.success(), "{}", String::from_utf8_lossy(&undo.stderr));
		assert!(!output.exists());
		assert!(!second.exists());
	} else {
		assert!(!undo.status.success());
		assert!(output.exists());
	}
	assert_eq!(fs::read(source).unwrap(), original);
}

#[test]
fn watched_keep_preserves_the_original_and_undo() {
	watched(WatchCase {
		source_name: "Bundle.TAR",
		intent: "zip+keep",
		needs_confirmation: false,
		undo: UndoPolicy::Enabled,
	});
}

#[test]
fn watched_keep_applies_from_any_output_group() {
	watched(WatchCase {
		source_name: "Bundle.TAR",
		intent: "zip,tar.gz+keep",
		needs_confirmation: false,
		undo: UndoPolicy::Enabled,
	});
}

#[test]
fn explicit_source_group_preserves_original_case() {
	watched(WatchCase {
		source_name: "Bundle.TAR",
		intent: "tar,zip",
		needs_confirmation: false,
		undo: UndoPolicy::Enabled,
	});
}

#[test]
fn an_explicit_source_alias_keeps_the_exact_original_name() {
	watched(WatchCase {
		source_name: "Bundle.MYTAR",
		intent: "mytar,zip",
		needs_confirmation: false,
		undo: UndoPolicy::Enabled,
	});
}

#[test]
fn keep_and_ask_restore_the_exact_name_before_confirmation() {
	watched(WatchCase {
		source_name: "Bundle.TAR",
		intent: "zip+keep+ask",
		needs_confirmation: true,
		undo: UndoPolicy::Enabled,
	});
}

#[test]
fn keep_and_ask_preserve_a_source_format_alias() {
	watched(WatchCase {
		source_name: "Bundle.MYTAR",
		intent: "zip+keep+ask",
		needs_confirmation: true,
		undo: UndoPolicy::Enabled,
	});
}

#[test]
fn keep_is_independent_of_undo() {
	watched(WatchCase {
		source_name: "Bundle.TAR",
		intent: "zip+keep",
		needs_confirmation: false,
		undo: UndoPolicy::Disabled,
	});
}

#[test]
fn direct_keep_preserves_the_original_and_undo_leaves_later_edits_alone() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let source = dir.path().join("Bundle.TAR");
	let output = dir.path().join("Bundle.zip");
	fs::write(&source, archive()).unwrap();
	let config = configuration(dir.path(), UndoPolicy::Enabled);
	let _daemon = start_daemon(&state, &config);
	let converted = Command::new(env!("CARGO_BIN_EXE_retype"))
		.arg("convert")
		.arg(&source)
		.arg("zip+keep")
		.args(["--json", "--no-spawn"])
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.output()
		.unwrap();
	assert!(
		converted.status.success(),
		"{}",
		String::from_utf8_lossy(&converted.stderr)
	);
	let answer: serde_json::Value = serde_json::from_slice(&converted.stdout).unwrap();
	assert_eq!(answer["job"]["state"], "done");
	assert!(answer["job"]["intent"].as_str().unwrap().contains("+keep"));
	check_zip(&output);
	fs::write(&source, b"later edit").unwrap();
	let undo = Command::new(env!("CARGO_BIN_EXE_retype"))
		.args(["undo", "--last", "--no-spawn"])
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.output()
		.unwrap();
	if retype_core::fsx::supports_durable_cleanup() {
		assert!(undo.status.success(), "{}", String::from_utf8_lossy(&undo.stderr));
		assert!(!output.exists());
	} else {
		assert!(!undo.status.success());
		assert!(output.exists());
	}
	assert_eq!(fs::read(source).unwrap(), b"later edit");
}

#[test]
fn keep_does_not_hide_an_undo_storage_failure_from_the_cli() {
	let dir = tempfile::tempdir().unwrap();
	let state = dir.path().join("state");
	let source = dir.path().join("Bundle.TAR");
	let original = archive();
	fs::write(&source, &original).unwrap();
	let config = configuration(dir.path(), UndoPolicy::Enabled);
	let _daemon = start_daemon(&state, &config);
	fs::write(state.join("undo"), b"not a directory").unwrap();
	let converted = Command::new(env!("CARGO_BIN_EXE_retype"))
		.arg("convert")
		.arg(&source)
		.arg("zip+keep")
		.args(["--json", "--no-spawn"])
		.arg("--state-dir")
		.arg(&state)
		.env("RETYPE_CONFIG", &config)
		.output()
		.unwrap();
	assert!(!converted.status.success());
	let answer: serde_json::Value = serde_json::from_slice(&converted.stdout).unwrap();
	assert_eq!(answer["job"]["state"], "failed");
	assert!(answer["job"]["error"]["message"].as_str().unwrap().contains("undo"));
	assert_eq!(fs::read(source).unwrap(), original);
	assert!(!dir.path().join("Bundle.zip").exists());
}

#[test]
fn explanation_preserves_an_alias_and_shows_the_kept_original() {
	let dir = tempfile::tempdir().unwrap();
	let output = Command::new(env!("CARGO_BIN_EXE_retype"))
		.args(["explain", "Photo.JPEG", "jpg,png+keep", "--json"])
		.arg("--state-dir")
		.arg(dir.path())
		.output()
		.unwrap();
	let answer: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
	let outputs = answer["outputs"].as_array().unwrap();
	assert!(
		outputs
			.iter()
			.any(|entry| entry["kept"] == true && entry["path"] == "Photo.JPEG")
	);
	assert!(outputs.iter().any(|entry| entry["path"] == "Photo.png"));
}
