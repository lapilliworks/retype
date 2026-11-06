//! Native diagnostics only: API success does not establish crash durability.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;

use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows_sys::Win32::Storage::FileSystem::{
	FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

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
	let mut file = File::create(directory.path().join("probe.txt")).unwrap();
	file.write_all(b"synthetic durability probe").unwrap();
	file.sync_all().unwrap();
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
