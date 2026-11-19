/* Qualified fixed-local NTFS directory synchronization; every participating path is checked. */

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows_sys::Win32::Storage::FileSystem::{
	FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
	FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
	FILE_SHARE_READ, FILE_SHARE_WRITE, GetDriveTypeW, GetFinalPathNameByHandleW, GetVolumeInformationByHandleW,
	VOLUME_NAME_GUID,
};
use windows_sys::Win32::System::WindowsProgramming::DRIVE_FIXED;

use crate::job::FileIdentity;

pub(crate) struct Directory {
	file: File,
	_parents: Vec<File>,
}

impl Directory {
	pub(crate) fn open(path: &Path) -> io::Result<Self> {
		if path
			.components()
			.any(|component| component == std::path::Component::ParentDir)
		{
			return Err(unsupported("directory synchronization refuses parent-path traversal"));
		}
		let path = std::path::absolute(path)?;
		let mut parents = Vec::new();
		/* Hold every ancestor against rename, and never follow a junction or cloud placeholder. */
		for ancestor in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
			let file = open_directory(ancestor, FILE_READ_ATTRIBUTES)?;
			check_directory(&file)?;
			parents.push(file);
		}
		let selected = parents
			.last()
			.ok_or_else(|| unsupported("directory path has no root"))?;
		check_volume(selected)?;
		let file = open_directory(&path, GENERIC_READ | GENERIC_WRITE)?;
		check_directory(&file)?;
		let before =
			FileIdentity::of_open_file(selected).ok_or_else(|| unsupported("directory identity unavailable"))?;
		let after = FileIdentity::of_open_file(&file).ok_or_else(|| unsupported("directory identity unavailable"))?;
		if !before.matches(&after) {
			return Err(unsupported(
				"directory changed while opening its synchronization handle",
			));
		}
		Ok(Self {
			file,
			_parents: parents,
		})
	}

	pub(crate) fn sync(&self) -> io::Result<()> {
		self.file.sync_all()
	}
}

fn open_directory(path: &Path, access: u32) -> io::Result<File> {
	OpenOptions::new()
		.read(true)
		.access_mode(access)
		.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
		.custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
		.open(path)
}

fn check_directory(file: &File) -> io::Result<()> {
	let metadata = file.metadata()?;
	let forbidden = FILE_ATTRIBUTE_REPARSE_POINT
		| FILE_ATTRIBUTE_OFFLINE
		| FILE_ATTRIBUTE_RECALL_ON_OPEN
		| FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;
	if !metadata.is_dir() || metadata.file_attributes() & forbidden != 0 {
		return Err(unsupported(
			"directory synchronization refuses reparse points and placeholders",
		));
	}
	Ok(())
}

fn check_volume(file: &File) -> io::Result<()> {
	let mut filesystem = [0u16; 32];
	let result = unsafe {
		GetVolumeInformationByHandleW(
			file.as_raw_handle() as _,
			std::ptr::null_mut(),
			0,
			std::ptr::null_mut(),
			std::ptr::null_mut(),
			std::ptr::null_mut(),
			filesystem.as_mut_ptr(),
			filesystem.len() as u32,
		)
	};
	if result == 0 {
		return Err(io::Error::last_os_error());
	}
	if !filesystem.starts_with(&[b'N' as u16, b'T' as u16, b'F' as u16, b'S' as u16, 0]) {
		return Err(unsupported(
			"directory synchronization is implemented only for local NTFS",
		));
	}
	let name = final_volume_path(file)?;
	let root = volume_root(&name)?;
	if unsafe { GetDriveTypeW(root.as_ptr()) } != DRIVE_FIXED {
		return Err(unsupported(
			"directory synchronization requires a fixed local NTFS volume",
		));
	}
	Ok(())
}

fn final_volume_path(file: &File) -> io::Result<Vec<u16>> {
	let mut buffer = vec![0u16; 512];
	loop {
		let length = unsafe {
			GetFinalPathNameByHandleW(
				file.as_raw_handle() as _,
				buffer.as_mut_ptr(),
				buffer.len() as u32,
				VOLUME_NAME_GUID,
			)
		} as usize;
		if length == 0 {
			return Err(io::Error::last_os_error());
		}
		if length < buffer.len() {
			buffer.truncate(length);
			return Ok(buffer);
		}
		if length > 32_768 {
			return Err(unsupported("resolved directory path is too long"));
		}
		buffer.resize(length + 1, 0);
	}
}

fn volume_root(path: &[u16]) -> io::Result<Vec<u16>> {
	let prefix: Vec<u16> = r"\\?\Volume{".encode_utf16().collect();
	if !path.starts_with(&prefix) {
		return Err(unsupported("directory is not on a local volume GUID path"));
	}
	let end = path
		.windows(2)
		.position(|pair| pair == [b'}' as u16, b'\\' as u16])
		.ok_or_else(|| unsupported("resolved directory has no volume root"))?;
	let mut root = path[..end + 2].to_vec();
	root.push(0);
	Ok(root)
}

fn unsupported(message: &str) -> io::Error {
	io::Error::new(io::ErrorKind::Unsupported, message)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn volume_roots_require_a_local_guid_path() {
		let path: Vec<u16> = r"\\?\Volume{12345678-1234-1234-1234-123456789abc}\folder"
			.encode_utf16()
			.collect();
		let root = volume_root(&path).unwrap();
		assert_eq!(
			String::from_utf16(&root[..root.len() - 1]).unwrap(),
			r"\\?\Volume{12345678-1234-1234-1234-123456789abc}\"
		);
		for path in [
			r"\\?\UNC\server\share\folder",
			r"\\?\C:\folder",
			r"\\?\Volume{missing-root",
		] {
			assert!(volume_root(&path.encode_utf16().collect::<Vec<_>>()).is_err());
		}
	}

	#[test]
	fn cleanup_qualifies_the_actual_ntfs_directory() {
		let root = tempfile::tempdir().unwrap();
		let directory = Directory::open(root.path()).unwrap();
		directory.sync().unwrap();
		assert!(super::super::supports_durable_cleanup());
		super::super::require_durable_cleanup(&root.path().join("file")).unwrap();
		assert!(Directory::open(&root.path().join("..")).is_err());
		let file = root.path().join("not-a-directory");
		std::fs::write(&file, b"synthetic bytes").unwrap();
		assert!(Directory::open(&file).is_err());
	}

	#[test]
	fn synchronization_holds_ancestors_against_rename() {
		let root = tempfile::tempdir().unwrap();
		let parent = root.path().join("parent");
		let child = parent.join("child");
		std::fs::create_dir_all(&child).unwrap();
		let directory = Directory::open(&child).unwrap();
		assert!(super::super::rename_noreplace(&parent, &root.path().join("moved")).is_err());
		directory.sync().unwrap();
		assert!(child.is_dir());
	}

	#[test]
	fn synchronization_refuses_a_junction_ancestor() {
		let root = tempfile::tempdir().unwrap();
		let target = root.path().join("target");
		let link = root.path().join("junction");
		std::fs::create_dir_all(target.join("child")).unwrap();
		let result = std::process::Command::new("cmd.exe")
			.args(["/d", "/c", "mklink", "/J"])
			.arg(&link)
			.arg(&target)
			.output()
			.unwrap();
		assert!(result.status.success(), "junction fixture failed: {result:?}");
		assert_eq!(
			Directory::open(&link.join("child")).err().unwrap().kind(),
			io::ErrorKind::Unsupported
		);
		assert!(super::super::require_durable_cleanup(&link.join("child/source")).is_err());
		let source = link.join("child/source");
		std::fs::write(&source, b"original bytes").unwrap();
		let fingerprint = crate::safety::Fingerprint::of(&source).unwrap();
		assert!(crate::safety::remove_owned(&source, &fingerprint).is_err());
		assert_eq!(std::fs::read(&source).unwrap(), b"original bytes");
		assert!(target.join("child").is_dir());
	}
}
