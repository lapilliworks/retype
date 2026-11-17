//! Native diagnostics only: API success does not establish crash durability.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::{ERROR_SHARING_VIOLATION, GENERIC_READ, GENERIC_WRITE};
use windows_sys::Win32::Storage::FileSystem::{
	DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_WRITE_THROUGH, FILE_SHARE_DELETE,
	FILE_SHARE_READ, FILE_SHARE_WRITE, GetVolumeInformationByHandleW,
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

#[test]
fn write_through_handle_rename_probe_is_not_durability_evidence() {
	let directory = tempfile::tempdir().unwrap();
	let root = fs::canonicalize(directory.path()).unwrap();
	let source = root.join("source-\u{00df}.txt");
	let occupied = root.join("occupied.txt");
	let renamed = root.join("renamed-\u{6f22}.txt");
	let unrelated = root.join("unrelated.txt");
	let original = b"synthetic original";
	let other = b"unrelated occupied target";
	let mut initial = File::create(&source).unwrap();
	initial.write_all(original).unwrap();
	initial.sync_all().unwrap();
	drop(initial);
	fs::write(&occupied, other).unwrap();
	let file = OpenOptions::new()
		.read(true)
		.write(true)
		.access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
		.share_mode(FILE_SHARE_READ)
		.custom_flags(FILE_FLAG_WRITE_THROUGH | FILE_FLAG_OPEN_REPARSE_POINT)
		.open(&source)
		.expect("cannot open the synthetic source with write-through and exclusive mutation access");
	let before = crate::safety::Fingerprint::of(&source).unwrap();
	let occupied_before = crate::safety::Fingerprint::of(&occupied).unwrap();
	let opened_id = crate::job::FileIdentity::of_open_file(&file).unwrap();
	assert!(opened_id.matches(before.identity.as_ref().unwrap()));

	let write_error = OpenOptions::new().write(true).open(&source).unwrap_err();
	let rename_error = super::rename_noreplace(&source, &unrelated).unwrap_err();
	let collision_error = super::rename_open(&file, &occupied, crate::convert::Publication::Create).unwrap_err();
	assert_eq!(
		write_error.raw_os_error(),
		Some(ERROR_SHARING_VIOLATION as i32),
		"competing write did not fail with a sharing violation: {write_error}"
	);
	assert_eq!(
		rename_error.raw_os_error(),
		Some(ERROR_SHARING_VIOLATION as i32),
		"competing rename did not fail with a sharing violation: {rename_error}"
	);
	assert_eq!(
		collision_error.kind(),
		io::ErrorKind::AlreadyExists,
		"occupied-target rename did not report a collision: {collision_error}"
	);
	assert_eq!(fs::read(&source).unwrap(), original);
	assert_eq!(fs::read(&occupied).unwrap(), other);
	assert!(occupied_before.same_file_and_content(&crate::safety::Fingerprint::of(&occupied).unwrap()));
	assert!(!unrelated.try_exists().unwrap());

	super::rename_open(&file, &renamed, crate::convert::Publication::Create)
		.expect("cannot rename the synthetic file by its open handle");
	file.sync_all().expect("cannot flush the renamed synthetic file");
	assert!(!source.try_exists().unwrap());
	assert_eq!(fs::read(&renamed).unwrap(), original);
	assert!(before.same_file_and_content(&crate::safety::Fingerprint::of(&renamed).unwrap()));
	assert!(occupied_before.same_file_and_content(&crate::safety::Fingerprint::of(&occupied).unwrap()));
	eprintln!(
		"handle rename probe: competing_write={:?}, competing_rename={:?}, occupied_target={:?}, identity_preserved=true",
		write_error.raw_os_error(),
		rename_error.raw_os_error(),
		collision_error.raw_os_error()
	);
	eprintln!("Probe only: successful handle operations do not prove crash or power-loss durability.");
	assert!(
		!super::supports_durable_cleanup(),
		"Windows destructive cleanup must remain gated"
	);
}
