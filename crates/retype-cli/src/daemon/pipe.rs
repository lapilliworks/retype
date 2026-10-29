//! The named-pipe transport, `docs/ipc.md` §4.
//!
//! One instance at a time, created before each client and closed after it, which
//! is what allows while every method is a request and an answer.

use std::ffi::OsStr;
use std::fs::File;
use std::io::BufReader;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, RevertToSelf, SECURITY_ATTRIBUTES};
use windows_sys::Win32::Storage::FileSystem::{FlushFileBuffers, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::Pipes::{
	ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, ImpersonateNamedPipeClient, PIPE_READMODE_BYTE,
	PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

use crate::daemon::Server;
use crate::error::{Error, Result};

/// Owner may read and write, nobody else may do anything. `P` makes the DACL
/// protected, so inherited permissions cannot widen it.
const OWNER_ONLY: &str = "D:P(A;;GA;;;OW)";

const SDDL_REVISION_1: u32 = 1;
const BUFFER_BYTES: u32 = 64 * 1024;
const NUL: u16 = 0;

/// `\\.\pipe\retype-<machine>-<user>`, `docs/ipc.md` §4.
pub fn name(machine: &str) -> String {
	format!(r"\\.\pipe\retype-{machine}-{}", user_token())
}

pub fn serve(name: &str, server: &mut Server) -> Result<()> {
	let name = wide(name);
	let descriptor = Descriptor::create()?;
	let attributes = SECURITY_ATTRIBUTES {
		nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
		lpSecurityDescriptor: descriptor.0,
		bInheritHandle: 0,
	};

	while !server.stopped() {
		let handle = unsafe {
			CreateNamedPipeW(
				name.as_ptr(),
				PIPE_ACCESS_DUPLEX,
				PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
				PIPE_UNLIMITED_INSTANCES,
				BUFFER_BYTES,
				BUFFER_BYTES,
				0,
				&attributes,
			)
		};

		if handle == INVALID_HANDLE_VALUE {
			return Err(Error::Io(std::io::Error::last_os_error()));
		}

		let connected = match connect(handle) {
			Ok(connected) => connected,
			Err(error) => {
				unsafe { close(handle) };
				return Err(error);
			}
		};

		if !connected {
			// A failed connect leaves nothing to serve, and the instance cannot
			// be reused, so it is closed rather than retried.
			unsafe { close(handle) };
			continue;
		}

		serve_client(handle, server)?;
	}

	Ok(())
}

fn serve_client(handle: HANDLE, server: &mut Server) -> Result<()> {
	// Impersonation is the authentication: a client that cannot be impersonated
	// is not the user this daemon belongs to, `docs/ipc.md` §4.
	if unsafe { ImpersonateNamedPipeClient(handle) } == 0 {
		eprintln!(
			"retype: refused a pipe client that could not be impersonated: {}",
			std::io::Error::last_os_error()
		);
		unsafe {
			flush_and_disconnect(handle);
			close(handle);
		}
		return Ok(());
	}
	unsafe { RevertToSelf() };

	let file = unsafe { File::from_raw_handle(handle) };
	let mut reader = BufReader::new(file.try_clone()?);
	let mut writer = &file;

	let result = server.serve_connection(&mut reader, &mut writer);

	unsafe { flush_and_disconnect(handle) };
	drop(file);

	result
}

fn connect(handle: HANDLE) -> Result<bool> {
	if unsafe { ConnectNamedPipe(handle, ptr::null_mut()) } != 0 {
		return Ok(true);
	}

	let error = std::io::Error::last_os_error();
	Ok(error.raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32))
}

fn wide(text: &str) -> Vec<u16> {
	OsStr::new(text).encode_wide().chain(Some(NUL)).collect()
}

fn user_token() -> String {
	let token: String = std::env::var("USERNAME")
		.unwrap_or_else(|_| "user".to_string())
		.chars()
		.filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
		.take(24)
		.collect();

	if token.is_empty() { "user".to_string() } else { token }
}

unsafe fn flush_and_disconnect(handle: HANDLE) {
	unsafe {
		FlushFileBuffers(handle);
		DisconnectNamedPipe(handle);
	}
}

unsafe fn close(handle: HANDLE) {
	unsafe {
		windows_sys::Win32::Foundation::CloseHandle(handle);
	}
}

/// Freed when it goes out of scope, which is after the last instance is created.
struct Descriptor(PSECURITY_DESCRIPTOR);

impl Descriptor {
	fn create() -> Result<Self> {
		let sddl = wide(OWNER_ONLY);
		let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
		let mut size = 0u32;

		let converted = unsafe {
			ConvertStringSecurityDescriptorToSecurityDescriptorW(
				sddl.as_ptr(),
				SDDL_REVISION_1,
				&mut descriptor,
				&mut size,
			)
		};

		if converted == 0 {
			return Err(Error::Io(std::io::Error::last_os_error()));
		}

		Ok(Self(descriptor))
	}
}

impl Drop for Descriptor {
	fn drop(&mut self) {
		unsafe {
			windows_sys::Win32::Foundation::LocalFree(self.0);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_pipe_name_carries_the_machine_and_the_user() {
		let name = name("7f3c1a2b");

		assert!(name.starts_with(r"\\.\pipe\retype-7f3c1a2b-"));
	}

	#[test]
	fn a_user_token_is_never_empty_and_never_contains_a_separator() {
		let token = user_token();

		assert!(!token.is_empty());
		assert!(!token.contains('\\'));
	}
}
