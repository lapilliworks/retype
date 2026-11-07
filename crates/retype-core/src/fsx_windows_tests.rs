//! Native diagnostics only: API success does not establish crash durability.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows_sys::Win32::Storage::FileSystem::{
	FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetVolumeInformationByHandleW,
};

fn probe_filesystem(file: &File) {
	let mut filesystem = [0u16; 261];
	let mut flags = 0u32;
	let result = unsafe {
		GetVolumeInformationByHandleW(
			file.as_raw_handle() as _,
			std::ptr::null_mut(),
			0,
			std::ptr::null_mut(),
			std::ptr::null_mut(),
			&mut flags,
			filesystem.as_mut_ptr(),
			filesystem.len() as u32,
		)
	};
	if result == 0 {
		let error = std::io::Error::last_os_error();
		eprintln!(
			"probe filesystem: unavailable, os_error={:?}, {error}",
			error.raw_os_error()
		);
		return;
	}
	let Some(end) = filesystem.iter().position(|&value| value == 0) else {
		eprintln!("probe filesystem: unterminated filesystem name");
		return;
	};
	match String::from_utf16(&filesystem[..end]) {
		Ok(name) if !name.is_empty() => eprintln!("probe filesystem: {name}, flags=0x{flags:08x}"),
		Ok(_) => eprintln!("probe filesystem: empty filesystem name"),
		Err(error) => eprintln!("probe filesystem: invalid filesystem name: {error}"),
	}
}

fn probe_directory(path: &Path, label: &str, access: u32) {
	let opened = OpenOptions::new()
		.read(true)
		.access_mode(access)
		.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
		.custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
		.open(path);
	match opened {
		Ok(directory) => match directory.sync_all() {
			Ok(()) => eprintln!("directory flush {label}: succeeded"),
			Err(error) => eprintln!(
				"directory flush {label}: failed, os_error={:?}, {error}",
				error.raw_os_error()
			),
		},
		Err(error) => eprintln!(
			"directory open {label}: failed, os_error={:?}, {error}",
			error.raw_os_error()
		),
	}
}

#[test]
fn directory_flush_probe_is_not_durability_evidence() {
	let directory = tempfile::tempdir().unwrap();
	let mut file = OpenOptions::new()
		.read(true)
		.write(true)
		.create_new(true)
		.open(directory.path().join("probe.txt"))
		.unwrap();
	file.write_all(b"synthetic durability probe").unwrap();
	file.sync_all().unwrap();
	probe_filesystem(&file);
	drop(file);
	for (label, access) in [
		("read", GENERIC_READ),
		("write", GENERIC_WRITE),
		("read+write", GENERIC_READ | GENERIC_WRITE),
	] {
		probe_directory(directory.path(), label, access);
	}
	eprintln!("Probe only: successful calls do not prove power-loss or directory-entry durability.");
	assert!(
		!super::supports_durable_cleanup(),
		"Windows destructive cleanup must remain gated"
	);
}
