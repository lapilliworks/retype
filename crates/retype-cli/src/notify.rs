//! Saying what a batch of jobs did.
//!
//! One notification per rename batch, because a directory full of photographs should
//! not be twelve interruptions. The message is built from the records, so it says what
//! happened rather than what was attempted, and the only action this build offers is
//! Undo, which is the one that does not need the GUI.
//!
//! Capability is a property of the platform, so there are three tiers and the daemon
//! uses the best one it has:
//!
//! | Tier | How | Where |
//! |---|---|---|
//! | Actions | `notify-send --wait --action` | Linux with a session bus and a libnotify build that has actions |
//! | Plain | `notify-send` | Linux with a session bus |
//! | Plain | `osascript` | macOS |
//! | Plain | a toast through `powershell.exe` | Windows |
//! | Log | a line on the daemon's stderr | anywhere the platform has no notifier |
//!
//! The Windows toast has no button, and does not need a signed bundle: it needs an
//! AppUserModelID the shell can resolve, which the script registers under
//! `HKCU\Software\Classes\AppUserModelId` before the first toast. Protocol activation
//! through a Start-menu shortcut, and the macOS User Notifications centre with its
//! categories, still need the signed notifier bundle, which is not built yet.

use std::path::Path;
use std::process::{Command, Stdio};

use retype_core::job::{Job, JobId, JobState, Output, OutputState};

/// How many conversions a message names before it counts the rest. A notification is a
/// glance, not a listing; `retype jobs` is the listing.
const NAMED: usize = 3;

/// What the daemon tells the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
	pub title: String,
	pub body: String,
	/// The jobs an Undo would reverse, empty when there is nothing to undo.
	pub undo: Vec<JobId>,
}

impl Message {
	/// Describe a finished batch, or nothing when the batch did nothing worth saying.
	pub fn of(jobs: &[Job], undo_enabled: bool) -> Option<Self> {
		if jobs.is_empty() {
			return None;
		}

		let converted: Vec<&Job> = jobs
			.iter()
			.filter(|job| matches!(job.state, JobState::Done | JobState::Partial))
			.collect();

		let failed = with_state(jobs, JobState::Failed);
		let cancelled = with_state(jobs, JobState::Cancelled);
		let superseded = with_state(jobs, JobState::Superseded);
		let parked: Vec<&Job> = jobs
			.iter()
			.filter(|job| matches!(job.state, JobState::Pending | JobState::NeedsAttention))
			.collect();

		let mut parts = Vec::new();
		if !converted.is_empty() {
			// A count alone leaves the user guessing what became of what, so the names
			// and the formats follow it.
			parts.push(format!(
				"{} converted ({})",
				converted.len(),
				named(&converted, conversion)
			));
		}
		if !failed.is_empty() {
			parts.push(format!("{} failed ({})", failed.len(), named(&failed, failure)));
		}
		if !cancelled.is_empty() {
			parts.push(format!(
				"{} cancelled ({})",
				cancelled.len(),
				named(&cancelled, source_name)
			));
		}
		if !superseded.is_empty() {
			parts.push(format!(
				"{} superseded ({})",
				superseded.len(),
				named(&superseded, source_name)
			));
		}
		if !parked.is_empty() {
			parts.push(format!("{} parked ({})", parked.len(), named(&parked, waiting)));
		}

		if parts.is_empty() {
			return None;
		}

		let mut body = parts.join(", ");

		// A parked job that can run as asked only needs the timer; one that cannot needs
		// a person, so the body says which.
		if !parked.is_empty() {
			body.push_str(&parked_hint(&parked));
		}

		let undo = match undo_enabled {
			true => converted.iter().map(|job| job.id.clone()).collect(),
			false => Vec::new(),
		};

		Some(Self {
			title: "retype".to_string(),
			body,
			undo,
		})
	}
}

/// Name each conversion and its product, up to `NAMED`, then count the rest.
fn named(jobs: &[&Job], describe: impl Fn(&Job) -> String) -> String {
	let mut names: Vec<String> = jobs.iter().take(NAMED).map(|job| describe(job)).collect();

	if jobs.len() > NAMED {
		names.push(format!("and {} more", jobs.len() - NAMED));
	}

	names.join(", ")
}

/// `a.heic → a.jpg`, plus what was dropped or missed on the way.
fn conversion(job: &Job) -> String {
	let source = file_name(&job.source.path);
	let written = outputs(job);

	if written.is_empty() {
		return source;
	}

	let targets: Vec<String> = written.iter().map(|output| file_name(&output.path)).collect();
	let mut text = format!("{source} → {}", targets.join(", "));

	// A repair ran an intent it had cut down, and a partial conversion left an output
	// unwritten: both are things the user has to know to judge the result.
	if let Some(dropped) = dropped_tokens(job) {
		text.push_str(&format!("; dropped {}", dropped.join(", ")));
	}

	let missed: Vec<String> = job
		.outputs
		.iter()
		.filter(|output| matches!(output.state, OutputState::Failed | OutputState::Skipped))
		.map(missed_output)
		.collect();

	if !missed.is_empty() {
		text.push_str(&format!("; {}", missed.join(", ")));
	}

	text
}

/// The tokens a repair dropped, which a job that ran as asked never has.
fn dropped_tokens(job: &Job) -> Option<&[String]> {
	let dropped = &job.parked.as_ref()?.dropped;

	(!dropped.is_empty()).then_some(dropped.as_slice())
}

/// `a.webp: magick exited 1`, an output of a batch that was not written.
fn missed_output(output: &Output) -> String {
	let name = file_name(&output.path);

	match output.error.as_ref().map(|error| error.message.as_str()) {
		Some(message) if !message.is_empty() => format!("{name}: {message}"),
		_ => name,
	}
}

/// `a.heic: magick exited 1`: the file and the reason it failed.
fn failure(job: &Job) -> String {
	let source = source_name(job);

	match diagnosis(job) {
		Some(message) => format!("{source}: {message}"),
		None => source,
	}
}

/// `a.heic: cost`: the file and the reason `jobs list` shows for the wait.
fn waiting(job: &Job) -> String {
	let source = source_name(job);
	let reason = job
		.parked
		.as_ref()
		.map(|parked| parked.reason.as_str())
		.unwrap_or_default();

	match reason.is_empty() {
		true => source,
		false => format!("{source}: {}", wait_reason(reason)),
	}
}

/// The reason as a person reads it. `size` alone does not say what is waiting.
fn wait_reason(reason: &str) -> &str {
	match reason {
		"size" => "over the automatic size limit",
		other => other,
	}
}

/// The first thing recorded as wrong. A tool's failure lands on its output, and the
/// job's own error is only set when the job never reached one.
fn diagnosis(job: &Job) -> Option<String> {
	job.error
		.as_ref()
		.or_else(|| job.outputs.iter().find_map(|output| output.error.as_ref()))
		.map(|error| error.message.clone())
}

fn source_name(job: &Job) -> String {
	file_name(&job.source.path)
}

/// What ends the wait, so a parked batch says whether it needs a person.
fn parked_hint(jobs: &[&Job]) -> String {
	if jobs.iter().any(|job| job.state == JobState::NeedsAttention) {
		return "; `retype jobs --interactive` decides what happens".to_string();
	}

	let pronoun = match jobs.len() {
		1 => "it",
		_ => "them",
	};

	format!("; the deferral timer will run {pronoun}")
}

fn with_state(jobs: &[Job], state: JobState) -> Vec<&Job> {
	jobs.iter().filter(|job| job.state == state).collect()
}

/// The outputs that were written, falling back to every output the job declared when no
/// state says otherwise.
fn outputs(job: &Job) -> Vec<&Output> {
	let written: Vec<&Output> = job
		.outputs
		.iter()
		.filter(|output| output.state == OutputState::Done)
		.collect();

	match written.is_empty() {
		true => job.outputs.iter().collect(),
		false => written,
	}
}

fn file_name(path: &Path) -> String {
	path.file_name()
		.map(|name| name.to_string_lossy().into_owned())
		.unwrap_or_else(|| path.display().to_string())
}

/// The best way this machine can say something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
	/// A notification with an Undo button.
	Actions,
	/// A notification without buttons.
	Plain,
	/// A line on the daemon's own output.
	Log,
}

impl Channel {
	/// What this machine can do, decided once at startup.
	pub fn detect(actions: bool) -> Self {
		// macOS has its own notifier. It carries no action, but a person on macOS
		// expects a notification rather than a line they have to go looking for.
		#[cfg(target_os = "macos")]
		{
			let _ = actions;
			if has_command("osascript") {
				Self::Plain
			} else {
				Self::Log
			}
		}

		// Windows has a notifier wherever the shell does: the toast API is part of the
		// OS, and `powershell.exe` is what displays one from a process without a bundle.
		#[cfg(target_os = "windows")]
		{
			let _ = actions;
			match has_command("powershell") {
				true => Self::Plain,
				false => Self::Log,
			}
		}

		#[cfg(not(any(target_os = "macos", target_os = "windows")))]
		{
			if !has_session() || !has_notifier() {
				return Self::Log;
			}

			match actions {
				true => Self::Actions,
				false => Self::Plain,
			}
		}
	}

	/// Show the message, and act on what the user pressed.
	///
	/// `undo` is called only when the platform reported the action, which is why the
	/// actionable tier waits: `notify-send --wait` answers on its own stdout, and there
	/// is nothing else to listen to.
	pub fn send(&self, message: &Message, undo: impl FnOnce(&[JobId])) {
		match self {
			Self::Log => {
				// The title is the product's own name, so a log line would say it twice.
				eprintln!("retype: {}", message.body);
			}
			Self::Plain => {
				let _ = plain(message);
			}
			Self::Actions => match message.undo.is_empty() {
				// Nothing to offer, so nothing to wait for.
				true => {
					let _ = notify_send(message, false);
				}
				false => {
					let action = notify_send(message, true);

					if action.as_deref() == Some("undo") {
						undo(&message.undo);
					}
				}
			},
		}
	}
}

/// A notification without actions, on whatever this platform uses.
fn plain(message: &Message) -> Option<String> {
	#[cfg(target_os = "macos")]
	{
		let quote = |text: &str| format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""));
		let script = format!(
			"display notification {} with title {}",
			quote(&message.body),
			quote(&message.title)
		);

		let output = Command::new("osascript")
			.args(["-e", &script])
			.stdin(Stdio::null())
			.stderr(Stdio::null())
			.output()
			.ok()?;

		output.status.success().then(String::new)
	}

	#[cfg(target_os = "windows")]
	{
		toast(message)
	}

	#[cfg(not(any(target_os = "macos", target_os = "windows")))]
	{
		notify_send(message, false)
	}
}

/// Run `notify-send`, answering with the action the user chose, if any.
fn notify_send(message: &Message, with_action: bool) -> Option<String> {
	let mut command = Command::new("notify-send");
	command.args(["--app-name", "retype", "--expire-time", "15000"]);

	if with_action {
		command.args(["--wait", "--action", "undo=Undo"]);
	}

	let output = command
		.arg(&message.title)
		.arg(&message.body)
		.stdin(Stdio::null())
		.stderr(Stdio::null())
		.output()
		.ok()?;

	let answer = String::from_utf8_lossy(&output.stdout).trim().to_string();
	(!answer.is_empty()).then_some(answer)
}

/// The AppUserModelID the toast is shown under, in the publisher's reverse-DNS form.
#[cfg(any(target_os = "windows", test))]
const APP_ID: &str = "com.lapilliworks.retype.cli";

/// Show a toast through PowerShell, which is the only notifier an unpackaged Windows
/// process has without the signed bundle.
#[cfg(target_os = "windows")]
fn toast(message: &Message) -> Option<String> {
	let mut command = Command::new("powershell.exe");
	command
		.args(["-NoProfile", "-NonInteractive", "-EncodedCommand"])
		.arg(encoded_command(&toast_script(message)))
		.stdin(Stdio::null())
		.stdout(Stdio::null())
		.stderr(Stdio::null());

	// The daemon has no console, so the child must not be given one.
	retype_core::process::hide_console(&mut command);

	let output = command.output().ok()?;

	output.status.success().then(String::new)
}

/// The PowerShell that raises the toast, as `-EncodedCommand` expects it.
///
/// The identifier is registered under the user's own hive first, because the shell
/// refuses a toast whose AppUserModelID it cannot resolve. If that registration or the
/// toast fails, the fallback is PowerShell's own registered identifier: the toast then
/// says `Windows PowerShell` rather than `retype`, which is worse than nothing only if
/// nothing is the alternative.
#[cfg(any(target_os = "windows", test))]
fn toast_script(message: &Message) -> String {
	let xml = format!(
		"<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
		xml_escape(&message.title),
		xml_escape(&message.body)
	);

	format!(
		r#"$ErrorActionPreference = 'Stop'
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null
[Windows.UI.Notifications.ToastNotification, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null
[Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] | Out-Null
$document = New-Object Windows.Data.Xml.Dom.XmlDocument
$document.LoadXml('{xml}')
$toast = New-Object -TypeName Windows.UI.Notifications.ToastNotification -ArgumentList $document
$app = '{APP_ID}'
try {{
	$key = 'HKCU:\Software\Classes\AppUserModelId\' + $app
	if (-not (Test-Path -LiteralPath $key)) {{
		New-Item -Path $key -Force | Out-Null
		New-ItemProperty -LiteralPath $key -Name 'DisplayName' -Value 'retype' -PropertyType String -Force | Out-Null
	}}
	[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier($app).Show($toast)
}} catch {{
	$fallback = '{{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}}\WindowsPowerShell\v1.0\powershell.exe'
	[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier($fallback).Show($toast)
}}"#
	)
}

/// `-EncodedCommand` takes the script in base64 over UTF-16LE, so no quoting and no
/// console code page stands between a file name and the toast.
#[cfg(any(target_os = "windows", test))]
fn encoded_command(script: &str) -> String {
	base64(&script.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>())
}

#[cfg(any(target_os = "windows", test))]
fn base64(bytes: &[u8]) -> String {
	const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

	let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);

	for chunk in bytes.chunks(3) {
		let triple = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
		let bits = u32::from_be_bytes([0, triple[0], triple[1], triple[2]]);

		text.push(ALPHABET[(bits >> 18) as usize & 63] as char);
		text.push(ALPHABET[(bits >> 12) as usize & 63] as char);
		text.push(match chunk.len() > 1 {
			true => ALPHABET[(bits >> 6) as usize & 63] as char,
			false => '=',
		});
		text.push(match chunk.len() > 2 {
			true => ALPHABET[bits as usize & 63] as char,
			false => '=',
		});
	}

	text
}

#[cfg(any(target_os = "windows", test))]
fn xml_escape(text: &str) -> String {
	let mut escaped = String::with_capacity(text.len());

	for character in text.chars() {
		match character {
			'&' => escaped.push_str("&amp;"),
			'<' => escaped.push_str("&lt;"),
			'>' => escaped.push_str("&gt;"),
			'"' => escaped.push_str("&quot;"),
			'\'' => escaped.push_str("&apos;"),
			_ => escaped.push(character),
		}
	}

	escaped
}

/// Whether there is a session to appear in at all.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn has_session() -> bool {
	["DBUS_SESSION_BUS_ADDRESS", "WAYLAND_DISPLAY", "DISPLAY"]
		.iter()
		.any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}

/// Whether a notifier command exists. `notify-send` is libnotify's own client and the
/// one every desktop session ships.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn has_notifier() -> bool {
	has_command("notify-send")
}

fn has_command(command: &str) -> bool {
	retype_core::tools::find_in_path(&std::env::var_os("PATH").unwrap_or_default(), command).is_some()
}

#[cfg(test)]
mod tests {
	use super::*;
	use retype_core::job::{Job, JobError, Parked, Source, Tier};

	fn job(id: usize, state: JobState) -> Job {
		let mut job = Job::new(
			Source {
				path: format!("/photos/{id}.heic").into(),
				format: "heic".to_string(),
				size: 0,
				mtime_ns: 0,
			},
			Some("jpg".to_string()),
			vec![Output::pending(
				format!("/photos/{id}.jpg").into(),
				"jpg".to_string(),
				Tier::Converted,
			)],
			"machine",
		)
		.expect("a job");

		job.id = JobId::parse(&format!("01J{:023}", id)).expect("an id");
		job.state = state;

		if matches!(state, JobState::Done | JobState::Partial) {
			for output in &mut job.outputs {
				output.state = OutputState::Done;
			}
		}

		job
	}

	/// The same job, parked for a reason, as the daemon records one.
	fn park(mut job: Job, reason: &str) -> Job {
		job.parked = Some(Parked {
			reason: reason.to_string(),
			deadline: None,
			batch: None,
			estimate: None,
			requested: None,
			corrected: None,
			dropped: Vec::new(),
			consume: true,
			force: false,
			input_fingerprint: None,
		});

		job
	}

	#[test]
	fn a_batch_of_conversions_names_what_became_what() {
		let jobs = vec![job(1, JobState::Done), job(2, JobState::Done)];

		let message = Message::of(&jobs, true).expect("a message");

		assert_eq!(message.body, "2 converted (1.heic → 1.jpg, 2.heic → 2.jpg)");
		assert_eq!(message.undo.len(), 2);
	}

	#[test]
	fn a_long_batch_names_a_few_and_counts_the_rest() {
		let jobs: Vec<Job> = (1..=5).map(|id| job(id, JobState::Done)).collect();

		let message = Message::of(&jobs, true).expect("a message");

		assert_eq!(
			message.body,
			"5 converted (1.heic → 1.jpg, 2.heic → 2.jpg, 3.heic → 3.jpg, and 2 more)"
		);
	}

	#[test]
	fn several_outputs_of_one_job_are_all_named() {
		let mut job = job(1, JobState::Done);
		job.outputs = ["webp", "jpg"]
			.iter()
			.map(|format| Output::pending(format!("/photos/1.{format}").into(), *format, Tier::Converted))
			.collect();
		for output in &mut job.outputs {
			output.state = OutputState::Done;
		}

		let message = Message::of(&[job], true).expect("a message");

		assert_eq!(message.body, "1 converted (1.heic → 1.webp, 1.jpg)");
	}

	#[test]
	fn a_partial_conversion_names_the_output_it_missed() {
		let mut partial = job(1, JobState::Partial);
		let mut missed = Output::pending("/photos/1.webp".into(), "webp", Tier::Converted);
		missed.state = OutputState::Failed;
		missed.error = Some(JobError {
			code: "E_TOOL_FAILED".to_string(),
			message: "magick exited 1".to_string(),
		});
		partial.outputs.push(missed);

		let message = Message::of(&[partial], true).expect("a message");

		assert_eq!(message.body, "1 converted (1.heic → 1.jpg; 1.webp: magick exited 1)");
	}

	#[test]
	fn a_repaired_conversion_says_what_it_dropped() {
		let mut repaired = park(job(1, JobState::Done), "inapplicable");
		repaired.parked.as_mut().expect("a parked record").dropped = vec!["quality80".to_string()];

		let message = Message::of(&[repaired], true).expect("a message");

		assert_eq!(message.body, "1 converted (1.heic → 1.jpg; dropped quality80)");
	}

	#[test]
	fn a_failure_is_named_rather_than_counted() {
		let mut failed = job(1, JobState::Failed);
		failed.error = Some(JobError {
			code: "E_TOOL_MISSING".to_string(),
			message: "magick is not installed".to_string(),
		});

		let message = Message::of(&[job(2, JobState::Done), failed], true).expect("a message");

		assert_eq!(
			message.body,
			"1 converted (2.heic → 2.jpg), 1 failed (1.heic: magick is not installed)"
		);
	}

	#[test]
	fn a_failure_without_a_job_error_takes_the_outputs_reason() {
		let mut failed = job(1, JobState::Failed);
		failed.outputs[0].state = OutputState::Failed;
		failed.outputs[0].error = Some(JobError {
			code: "E_TOOL_FAILED".to_string(),
			message: "magick exited 1: no decode delegate".to_string(),
		});

		let message = Message::of(&[failed], true).expect("a message");

		assert_eq!(message.body, "1 failed (1.heic: magick exited 1: no decode delegate)");
	}

	#[test]
	fn a_cancelled_job_and_a_superseded_one_are_named_and_kept_apart() {
		let jobs = vec![job(1, JobState::Cancelled), job(2, JobState::Superseded)];

		let message = Message::of(&jobs, true).expect("a message");

		assert_eq!(message.body, "1 cancelled (1.heic), 1 superseded (2.heic)");
	}

	#[test]
	fn a_pending_batch_names_the_file_and_what_ends_the_wait() {
		let message = Message::of(&[park(job(1, JobState::Pending), "cost")], true).expect("a message");

		assert_eq!(message.body, "1 parked (1.heic: cost); the deferral timer will run it");
		assert!(message.undo.is_empty());
	}

	#[test]
	fn a_batch_that_needs_attention_says_where_to_decide() {
		let message = Message::of(&[park(job(1, JobState::NeedsAttention), "intent")], true).expect("a message");

		assert_eq!(
			message.body,
			"1 parked (1.heic: intent); `retype jobs --interactive` decides what happens"
		);
		assert!(message.undo.is_empty());
	}

	#[test]
	fn a_size_park_is_named_in_plain_words() {
		let message = Message::of(&[park(job(1, JobState::NeedsAttention), "size")], true).expect("a message");

		assert_eq!(
			message.body,
			"1 parked (1.heic: over the automatic size limit); `retype jobs --interactive` decides what happens"
		);
	}

	#[test]
	fn with_undo_disabled_there_is_no_button() {
		let message = Message::of(&[job(1, JobState::Done)], false).expect("a message");

		assert!(message.undo.is_empty());
	}

	#[test]
	fn a_batch_with_nothing_to_say_says_nothing() {
		assert!(Message::of(&[], true).is_none());
	}

	#[test]
	fn a_command_is_encoded_as_utf16_little_endian() {
		assert_eq!(encoded_command(""), "");
		assert_eq!(encoded_command("A"), "QQA=");
		assert_eq!(encoded_command("AB"), "QQBCAA==");
	}

	#[test]
	fn the_encoding_pads_the_tail() {
		assert_eq!(base64(b""), "");
		assert_eq!(base64(b"f"), "Zg==");
		assert_eq!(base64(b"fo"), "Zm8=");
		assert_eq!(base64(b"foo"), "Zm9v");
		assert_eq!(base64(b"foobar"), "Zm9vYmFy");
	}

	#[test]
	fn a_toast_carries_both_lines_and_registers_the_identifier() {
		let message = Message {
			title: "retype".to_string(),
			body: "1 converted (a&b<c>.heic → a.jpg)".to_string(),
			undo: Vec::new(),
		};

		let script = toast_script(&message);

		assert!(script.contains("<text>retype</text>"), "{script}");
		assert!(
			script.contains("<text>1 converted (a&amp;b&lt;c&gt;.heic → a.jpg)</text>"),
			"{script}"
		);
		assert!(script.contains(APP_ID), "{script}");
		assert!(script.contains("AppUserModelId"), "{script}");
	}
}
