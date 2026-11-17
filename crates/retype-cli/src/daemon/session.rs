//! Login policy is separate from the OS query, so logout decisions are deterministic.
use serde::{Deserialize, Serialize};
#[cfg(target_os = "linux")]
use std::time::Duration;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Login {
	pub graphical: bool,
	pub terminal: bool,
	pub verified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(not(test), derive(Default))]
pub struct State {
	pub watching: bool,
	pub queued: bool,
	pub reason: Option<String>,
	graphical_seen: bool,
	terminal_seen: bool,
}
#[cfg(test)]
impl Default for State {
	fn default() -> Self {
		Self {
			watching: cfg!(test),
			queued: cfg!(test),
			reason: None,
			graphical_seen: false,
			terminal_seen: false,
		}
	}
}
impl State {
	pub fn update(&mut self, login: Login, supervised: bool) {
		self.graphical_seen |= login.verified && login.graphical;
		self.terminal_seen |= login.verified && login.terminal;
		self.watching = login.verified && (login.graphical || login.terminal);
		self.queued = login.verified
			&& (login.graphical || (!self.graphical_seen && (login.terminal || (self.terminal_seen && supervised))));
		self.reason = if !login.verified {
			Some("cannot verify an interactive login; automatic watching and queued renames are inactive".into())
		} else if !self.watching {
			Some("no qualifying interactive login; automatic watching is inactive".into())
		} else if !self.queued {
			Some("graphical logout paused queued automatic work until graphical login returns".into())
		} else {
			None
		};
	}
}

pub fn query() -> Login {
	// Debug-only fixtures cannot authorize a release binary or a real login claim.
	#[cfg(debug_assertions)]
	if let Ok(value) = std::env::var("RETYPE_TEST_SESSION") {
		return match value.as_str() {
			"graphical" => Login {
				graphical: true,
				terminal: false,
				verified: true,
			},
			"terminal" => Login {
				graphical: false,
				terminal: true,
				verified: true,
			},
			"none" => Login {
				verified: true,
				..Default::default()
			},
			_ => Login::default(),
		};
	}
	#[cfg(target_os = "linux")]
	{
		let cancellation = retype_core::execute::Cancellation::default().bounded(Duration::from_secs(2));
		let Some(program) = retype_core::tools::find_in_path(&std::env::var_os("PATH").unwrap_or_default(), "loginctl")
		else {
			return Login::default();
		};
		let uid = unsafe { libc::geteuid() }.to_string();
		let args = [
			"show-user".into(),
			uid.clone().into(),
			"--property=Sessions".into(),
			"--value".into(),
		];
		let Ok(sessions) = retype_core::execute::run_child_output("loginctl", &program, &args, &cancellation, 4096)
		else {
			return Login::default();
		};
		let mut login = Login {
			verified: true,
			..Default::default()
		};
		for id in sessions.split_whitespace().take(32) {
			let args = [
				"show-session".into(),
				id.into(),
				"--property=User,Type,Class,State".into(),
			];
			let Ok(properties) =
				retype_core::execute::run_child_output("loginctl", &program, &args, &cancellation, 8192)
			else {
				return Login::default();
			};
			let get = |name: &str| properties.lines().find_map(|line| line.strip_prefix(name));
			if get("User=") != Some(uid.as_str())
				|| !matches!(get("Class="), Some("user" | "user-early"))
				|| !matches!(get("State="), Some("active" | "online"))
			{
				continue;
			}
			match get("Type=") {
				Some("x11" | "wayland") => login.graphical = true,
				Some("tty") => login.terminal = true,
				_ => {}
			}
		}
		login
	}
	#[cfg(target_os = "macos")]
	{
		let token = retype_core::execute::Cancellation::default().bounded(std::time::Duration::from_secs(2));
		let read = |program: &str, args: &[&str]| {
			let args = args.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
			retype_core::execute::run_child_output(program, std::path::Path::new(program), &args, &token, 16384)
		};
		let (Ok(user), Ok(logins), Ok(console)) = (
			read("/usr/bin/id", &["-un"]),
			read("/usr/bin/who", &[]),
			read("/usr/bin/stat", &["-f", "%u", "/dev/console"]),
		) else {
			return Login::default();
		};
		let uid = unsafe { libc::geteuid() };
		Login {
			graphical: uid != 0 && console.trim().parse::<u32>().ok() == Some(uid),
			terminal: logins.lines().any(|line| {
				let mut parts = line.split_whitespace();
				parts.next() == Some(user.trim()) && parts.next().is_some_and(|tty| tty.starts_with("ttys"))
			}),
			verified: true,
		}
	}
	#[cfg(windows)]
	{
		windows_login()
	}
	#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
	{
		Login::default()
	}
}

#[cfg(windows)]
fn windows_login() -> Login {
	use windows_sys::Win32::Security::Authentication::Identity::{GetUserNameExW, NameSamCompatible};
	use windows_sys::Win32::System::RemoteDesktop::{
		WTSActive, WTSDomainName, WTSEnumerateSessionsW, WTSFreeMemory, WTSQuerySessionInformationW, WTSUserName,
	};
	let mut user = vec![0u16; 512];
	let mut length = user.len() as u32;
	if unsafe { GetUserNameExW(NameSamCompatible, user.as_mut_ptr(), &mut length) } == 0 {
		return Login::default();
	}
	let ours = String::from_utf16_lossy(&user[..length as usize])
		.trim_end_matches('\0')
		.to_lowercase();
	let mut sessions = std::ptr::null_mut();
	let mut count = 0;
	if unsafe { WTSEnumerateSessionsW(std::ptr::null_mut(), 0, 1, &mut sessions, &mut count) } == 0 {
		return Login::default();
	}
	if count == 0 {
		if !sessions.is_null() {
			unsafe {
				WTSFreeMemory(sessions.cast());
			}
		}
		return Login {
			verified: true,
			..Default::default()
		};
	}
	let mut graphical = false;
	let mut verified = true;
	for session in unsafe { std::slice::from_raw_parts(sessions, count as usize) } {
		if session.State != WTSActive {
			continue;
		}
		let mut values = Vec::new();
		for class in [WTSDomainName, WTSUserName] {
			let mut buffer = std::ptr::null_mut();
			let mut bytes = 0;
			if unsafe {
				WTSQuerySessionInformationW(std::ptr::null_mut(), session.SessionId, class, &mut buffer, &mut bytes)
			} == 0
			{
				verified = false;
				break;
			}
			let value = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(buffer, bytes as usize / 2) })
				.trim_end_matches('\0')
				.to_lowercase();
			unsafe {
				WTSFreeMemory(buffer.cast());
			}
			values.push(value);
		}
		if values.len() == 2 && !values[1].is_empty() && format!("{}\\{}", values[0], values[1]) == ours {
			graphical = true;
		}
	}
	unsafe {
		WTSFreeMemory(sessions.cast());
	}
	Login {
		graphical,
		terminal: false,
		verified,
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn graphical_logout_pauses_queue_even_if_ssh_remains() {
		let mut state = State::default();
		state.update(
			Login {
				graphical: true,
				terminal: true,
				verified: true,
			},
			true,
		);
		assert!(state.watching && state.queued);
		state.update(
			Login {
				terminal: true,
				verified: true,
				..Default::default()
			},
			true,
		);
		assert!(state.watching && !state.queued);
		state.update(
			Login {
				graphical: true,
				verified: true,
				..Default::default()
			},
			false,
		);
		assert!(state.queued);
	}
	#[test]
	fn terminal_logout_can_drain_only_under_supervision_and_unknown_login_stops() {
		let mut state = State::default();
		state.update(
			Login {
				terminal: true,
				verified: true,
				..Default::default()
			},
			false,
		);
		state.update(
			Login {
				verified: true,
				..Default::default()
			},
			true,
		);
		assert!(!state.watching && state.queued);
		state.update(Login::default(), true);
		assert!(!state.watching && !state.queued);
	}
}
