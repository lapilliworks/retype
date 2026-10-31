//! Cloud providers that can leave a file as a placeholder, `docs/watch.md` §5.
//!
//! A provider like that is a network filesystem with a proprietary protocol: the file
//! looks local, its contents are elsewhere, and reading it triggers a download. That is
//! the deferred network stage, so it is refused rather than half-supported, and a
//! dataless file found inside an ordinary registration is skipped, logged and counted.
//!
//! Ordinary sync clients are not affected: Dropbox with its files available offline,
//! Syncthing, rsync targets and git worktrees are ordinary local directories, and
//! refusing them would be both wrong and infuriating. The rule is about placeholders,
//! not about the word "sync".

use std::path::Path;

/// The provider a directory is inside, when it is one that can hold placeholders.
///
/// Answers with the name to print rather than a boolean, because "refused" without
/// "because iCloud" is a message the user cannot act on.
pub fn provider(path: &Path) -> Option<String> {
	if let Some(home) = home() {
		if let Some(name) = cloud_storage(path, &home) {
			return Some(name);
		}
	}

	#[cfg(target_os = "linux")]
	{
		let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
		fuse_provider(path, &mounts)
	}

	#[cfg(not(target_os = "linux"))]
	{
		None
	}
}

/// The two cloud directories whose layout names the provider, `docs/watch.md` §5.
///
/// Both are plain prefixes rather than probes: a directory under
/// `~/Library/CloudStorage` is a provider's mirror on every macOS that has one, and the
/// same is true of `OneDrive` under the user's profile on Windows.
pub fn cloud_storage(path: &Path, home: &Path) -> Option<String> {
	for (root, joined) in [
		// macOS keeps every provider's mirror under one directory, named after the
		// provider: `~/Library/CloudStorage/OneDrive-Personal`.
		("Library/CloudStorage", true),
		// Windows puts OneDrive in the profile, with the account in its name.
		("OneDrive", false),
	] {
		let base = home.join(root);

		let inside = match joined {
			// A file directly in the provider's mirror is inside it too.
			true => path.strip_prefix(&base).ok().and_then(|rest| {
				rest.components()
					.next()
					.map(|component| component.as_os_str().to_string_lossy().into_owned())
			}),
			false => path.parent().and_then(|parent| {
				parent
					.components()
					.map(|component| component.as_os_str().to_string_lossy().into_owned())
					.find(|name| name.starts_with(root))
			}),
		};

		if let Some(name) = inside {
			return Some(name);
		}
	}

	None
}

fn home() -> Option<std::path::PathBuf> {
	["HOME", "USERPROFILE"]
		.iter()
		.filter_map(std::env::var_os)
		.map(std::path::PathBuf::from)
		.next()
}

/// Whether the filesystem a path lives on is a FUSE mount that can hold placeholders.
///
/// The mount table is the check on Linux because it is the only list of what is mounted
/// and by whom. `gvfs` is the one a desktop session creates; `rclone` is the common
/// user-level mirror. An `sshfs` or `sftp` mount is an ordinary remote filesystem with
/// real contents, so it is not refused.
pub fn fuse_provider(path: &Path, mounts: &str) -> Option<String> {
	let mut found: Option<(String, &str)> = None;

	for line in mounts.lines() {
		let mut fields = line.split_whitespace();
		let (Some(_device), Some(point), Some(fs)) = (fields.next(), fields.next(), fields.next()) else {
			continue;
		};

		// `mounts` escapes a space in a mount point as `\040`, which a path never
		// contains literally.
		let point = point.replace("\\040", " ");

		if path.starts_with(&point) && found.as_ref().is_none_or(|(at, _)| point.len() >= at.len()) {
			found = Some((point, fs));
		}
	}

	let (_, fs) = found?;

	// Only the mounts whose whole purpose is a mirror that may not hold the bytes.
	// Anything else mounted through FUSE is judged per file by `dataless`, because a
	// real remote filesystem has real contents and refusing it would be wrong.
	match fs {
		"fuse.gvfsd-fuse" | "gvfs" | "gvfsd-fuse" => Some("gvfs".to_string()),
		"fuse.rclone" => Some("rclone".to_string()),
		_ => None,
	}
}

/// Whether this file is a placeholder rather than a copy.
///
/// On Linux and macOS a placeholder has no contents to read, so the answer is what
/// happens when it is read. On Windows the attribute says so directly.
pub fn dataless(path: &Path) -> bool {
	#[cfg(unix)]
	{
		use std::io::Read;

		let Ok(mut file) = std::fs::File::open(path) else {
			return false;
		};

		let mut byte = [0u8; 1];

		match file.read(&mut byte) {
			// The provider answered "not now", which is what a placeholder does.
			Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => true,
			Err(error) if error.raw_os_error() == Some(libc::EAGAIN) => true,
			_ => false,
		}
	}
	#[cfg(windows)]
	{
		placeholders::dataless_windows(path)
	}
}

#[cfg(windows)]
mod placeholders {
	/// `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`, which is what OneDrive sets on a file
	/// whose contents are elsewhere.
	pub const RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

	pub fn dataless_windows(path: &std::path::Path) -> bool {
		use std::os::windows::fs::MetadataExt;

		std::fs::metadata(path)
			.map(|metadata| metadata.file_attributes() & RECALL_ON_DATA_ACCESS != 0)
			.unwrap_or(false)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_cloud_storage_mirror_names_its_provider() {
		let path = Path::new("/Users/someone/Library/CloudStorage/OneDrive-Personal/Photos/a.jpg");

		assert_eq!(
			cloud_storage(path, Path::new("/Users/someone")),
			Some("OneDrive-Personal".to_string())
		);
	}

	#[test]
	fn a_onedrive_profile_names_its_provider() {
		assert_eq!(
			cloud_storage(
				Path::new("C:/Users/someone/OneDrive - Contoso/Photos/a.jpg"),
				Path::new("C:/Users/someone")
			),
			Some("OneDrive - Contoso".to_string())
		);
	}

	#[test]
	fn an_ordinary_directory_is_not_a_provider() {
		assert_eq!(
			cloud_storage(Path::new("/Users/someone/Photos/a.jpg"), Path::new("/Users/someone")),
			None
		);
	}

	const MOUNTS: &str = "\
/dev/sda1 / ext4 rw,relatime 0 0
gvfsd-fuse /run/user/1000/gvfs fuse.gvfsd-fuse rw,nosuid,nodev,relatime,user_id=1000 0 0
rclone /home/someone/cloud fuse.rclone rw,nosuid,nodev,relatime 0 0
/dev/sdb1 /home/someone/remote fuse.sshfs rw,nosuid,nodev 0 0
";

	#[test]
	fn a_gvfs_mount_is_named() {
		assert_eq!(
			fuse_provider(Path::new("/run/user/1000/gvfs/smb-share/x.jpg"), MOUNTS),
			Some("gvfs".to_string())
		);
	}

	#[test]
	fn a_fuse_mirror_is_named_by_its_filesystem() {
		assert_eq!(
			fuse_provider(Path::new("/home/someone/cloud/a.jpg"), MOUNTS),
			Some("rclone".to_string())
		);
	}

	#[test]
	fn an_ordinary_remote_filesystem_is_not_refused() {
		// A real remote filesystem has real contents; refusing it would be infuriating.
		assert_eq!(fuse_provider(Path::new("/home/someone/remote/a.jpg"), MOUNTS), None);
		assert_eq!(fuse_provider(Path::new("/home/someone/Photos/a.jpg"), MOUNTS), None);
	}

	#[test]
	fn the_longest_mount_point_wins() {
		let mounts = "/dev/sda1 / ext4 rw 0 0\ntmpfs /home/someone ext4 rw 0 0\n";

		// The outer mount claims the path too, and the inner one is where the file is.
		assert_eq!(fuse_provider(Path::new("/home/someone/a.jpg"), mounts), None);
	}

	#[test]
	fn a_placeholder_is_a_file_that_is_not_read() {
		// A file that holds nothing is readable, so it is not a placeholder: the
		// difference is the answer, not the size.
		let directory = std::env::temp_dir().join(format!("retype-dataless-{}", std::process::id()));
		let _ = std::fs::create_dir_all(&directory);
		let path = directory.join("a.jpg");
		std::fs::write(&path, b"pixels").unwrap();

		assert!(!dataless(&path));
		assert!(!dataless(&directory.join("missing.jpg")));

		let _ = std::fs::remove_dir_all(&directory);
	}
}
