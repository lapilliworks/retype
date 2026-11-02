//! Starting a child process without a window on Windows.
//!
//! A console program started by a process that has no console of its own is given a
//! new console window by the platform. The daemon has none, and neither has a CLI
//! started from a shortcut or a URI, so every tool it runs would otherwise flash a
//! window of its own.

use std::process::Command;

/// Keep a child from opening a console window, which none of them needs.
///
/// The tool's output is a pipe or the null device, and nothing in the product reads
/// the console, so there is nothing to show.
#[cfg(windows)]
pub fn hide_console(command: &mut Command) {
	use std::os::windows::process::CommandExt;
	use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

	command.creation_flags(CREATE_NO_WINDOW);
}

/// Nothing to hide: a child stays attached to the terminal it was started from and
/// opens no window of its own.
#[cfg(not(windows))]
pub fn hide_console(_command: &mut Command) {}
