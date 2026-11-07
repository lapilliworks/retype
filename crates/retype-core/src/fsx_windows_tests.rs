//! Native diagnostics only: API success does not establish crash durability.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::mem::{align_of, offset_of, size_of};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows_sys::Win32::Storage::FileSystem::{
	DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_WRITE_THROUGH, FILE_RENAME_INFO,
	FILE_RENAME_INFO_0, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileRenameInfo,
	GetVolumeInformationByHandleW, SetFileInformationByHandle,
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

fn rename_open_file(file: &File, destination: &Path) -> io::Result<()> {
	let name: Vec<u16> = destination.as_os_str().encode_wide().chain(Some(0)).collect();
	let name_bytes = name
		.len()
		.checked_mul(size_of::<u16>())
		.ok_or_else(|| io::Error::other("rename path is too long"))?;
	let size = offset_of!(FILE_RENAME_INFO, FileName)
		.checked_add(name_bytes)
		.ok_or_else(|| io::Error::other("rename buffer is too large"))?;
	let name_length = u32::try_from(name_bytes - size_of::<u16>()).map_err(io::Error::other)?;
	let size_in_bytes = u32::try_from(size).map_err(io::Error::other)?;
	assert!(align_of::<usize>() >= align_of::<FILE_RENAME_INFO>());
	let mut buffer = vec![0usize; size.div_ceil(size_of::<usize>())];
	let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
	/* The aligned buffer includes the variable-length UTF-16 name and terminator. */
	unsafe {
		(&raw mut (*info).Anonymous).write(FILE_RENAME_INFO_0 { ReplaceIfExists: 0 });
		(&raw mut (*info).RootDirectory).write(std::ptr::null_mut());
		(&raw mut (*info).FileNameLength).write(name_length);
		std::ptr::copy_nonoverlapping(name.as_ptr(), (&raw mut (*info).FileName).cast::<u16>(), name.len());
		if SetFileInformationByHandle(file.as_raw_handle() as _, FileRenameInfo, info.cast(), size_in_bytes) == 0 {
			return Err(io::Error::last_os_error());
		}
	}
	Ok(())
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
	let collision_error = rename_open_file(&file, &occupied).unwrap_err();
	assert_eq!(fs::read(&source).unwrap(), original);
	assert_eq!(fs::read(&occupied).unwrap(), other);
	assert!(occupied_before.same_file_and_content(&crate::safety::Fingerprint::of(&occupied).unwrap()));
	assert!(!unrelated.try_exists().unwrap());

	rename_open_file(&file, &renamed).expect("cannot rename the synthetic file by its open handle");
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
