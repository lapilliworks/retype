//! Archive conversions must carry the source inventory before disposal can be authorized.

use std::fs;
use std::io::{Read, Write};

use retype_core::archive::repack;
use retype_core::execute::Cancellation;
use retype_core::registry::Native;

#[test]
fn a_pax_size_override_is_carried_into_the_new_container() {
	let dir = tempfile::tempdir().unwrap();
	let input = dir.path().join("in.tar");
	let output = dir.path().join("out.zip");
	let data: Vec<u8> = (0..20).collect();

	let mut builder = tar::Builder::new(fs::File::create(&input).unwrap());
	builder.append_pax_extensions([("size", &b"20"[..])]).unwrap();
	let mut header = tar::Header::new_gnu();
	header.set_entry_type(tar::EntryType::Regular);
	header.set_size(10); // the raw header disagrees with the PAX override
	header.set_mode(0o644);
	header.set_path("entry.bin").unwrap();
	header.set_cksum();
	builder.append(&header, &data[..]).unwrap();
	builder.finish().unwrap();

	let report = repack(
		None,
		Native::Tar,
		Native::Zip,
		&input,
		&output,
		&Cancellation::default(),
	)
	.unwrap();
	assert!(report.validation.fingerprint().is_some());
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		assert_eq!(fs::metadata(&output).unwrap().permissions().mode() & 0o777, 0o600);
	}

	let mut zip = zip::ZipArchive::new(fs::File::open(&output).unwrap()).unwrap();
	let mut entry = zip.by_index(0).unwrap();
	let mut bytes = Vec::new();
	entry.read_to_end(&mut bytes).unwrap();
	assert_eq!(bytes, data);
}

#[test]
fn a_backslash_parent_component_is_refused() {
	let dir = tempfile::tempdir().unwrap();
	let input = dir.path().join("in.tar");
	let output = dir.path().join("out.zip");

	let mut builder = tar::Builder::new(fs::File::create(&input).unwrap());
	let mut header = tar::Header::new_gnu();
	header.set_size(4);
	header.set_mode(0o644);
	/* A safe archive writer rejects this name on Windows before Retype can test it. */
	let name = b"..\\escape.txt";
	header.as_mut_bytes()[..name.len()].copy_from_slice(name);
	header.set_cksum();
	builder.append(&header, &b"evil"[..]).unwrap();
	builder.finish().unwrap();
	drop(builder);

	let bytes = fs::read(&input).unwrap();
	assert_eq!(&bytes[..name.len()], name);
	let mut archive = tar::Archive::new(&bytes[..]);
	assert_eq!(archive.entries().unwrap().next().unwrap().unwrap().size(), 4);
	let error = repack(
		None,
		Native::Tar,
		Native::Zip,
		&input,
		&output,
		&Cancellation::default(),
	)
	.unwrap_err();
	assert_eq!(error.code(), retype_core::error::Code::Unsupported);
	assert!(!output.exists());
}

#[test]
fn native_repacking_preserves_permissions_and_whole_second_modification_times() {
	let dir = tempfile::tempdir().unwrap();
	let input = dir.path().join("input.tar");
	let mut builder = tar::Builder::new(fs::File::create(&input).unwrap());
	for (name, mtime) in [("epoch.txt", 0), ("dated.txt", 1_700_000_001)] {
		let mut header = tar::Header::new_gnu();
		header.set_size(4);
		header.set_mode(0o640);
		header.set_mtime(mtime);
		header.set_cksum();
		builder.append_data(&mut header, name, &b"data"[..]).unwrap();
	}
	builder.finish().unwrap();
	for (codec, extension) in [(Native::Zip, "zip"), (Native::TarGz, "tar.gz")] {
		let output = dir.path().join(format!("output.{extension}"));
		repack(None, Native::Tar, codec, &input, &output, &Cancellation::default()).unwrap();
		let restored = dir.path().join(format!("restored-{extension}.tar"));
		repack(None, codec, Native::Tar, &output, &restored, &Cancellation::default()).unwrap();
		let mut archive = tar::Archive::new(fs::File::open(restored).unwrap());
		let entries: Vec<_> = archive
			.entries()
			.unwrap()
			.map(|entry| {
				let entry = entry.unwrap();
				(entry.header().mode().unwrap() & 0o7777, entry.header().mtime().unwrap())
			})
			.collect();
		assert_eq!(entries, [(0o640, 0), (0o640, 1_700_000_001)]);
	}
}

#[test]
fn zip_repacking_refuses_special_permissions_instead_of_dropping_them() {
	let dir = tempfile::tempdir().unwrap();
	let input = dir.path().join("input.tar");
	let output = dir.path().join("output.zip");
	let mut builder = tar::Builder::new(fs::File::create(&input).unwrap());
	let mut header = tar::Header::new_gnu();
	header.set_entry_type(tar::EntryType::Directory);
	header.set_size(0);
	header.set_mode(0o1777);
	header.set_cksum();
	builder.append_data(&mut header, "shared/", std::io::empty()).unwrap();
	builder.finish().unwrap();
	let original = fs::read(&input).unwrap();
	let error = repack(
		None,
		Native::Tar,
		Native::Zip,
		&input,
		&output,
		&Cancellation::default(),
	)
	.unwrap_err();
	assert!(error.to_string().contains("special permission"));
	assert!(!output.exists());
	assert_eq!(fs::read(input).unwrap(), original);
}

#[test]
fn a_zip_fifo_is_refused_instead_of_becoming_a_regular_file() {
	let dir = tempfile::tempdir().unwrap();
	let input = dir.path().join("in.zip");
	let output = dir.path().join("out.tar");

	let writer_file = fs::File::create(&input).unwrap();
	let mut writer = zip::ZipWriter::new(writer_file);
	writer
		.start_file("pipe", zip::write::SimpleFileOptions::default())
		.unwrap();
	writer.write_all(b"not a regular file").unwrap();
	writer.finish().unwrap();

	/* The writer refuses to store special type bits, so set the Unix mode in the
	 * central directory the way an archive made on a Unix host can. */
	let mut bytes = fs::read(&input).unwrap();
	let central = bytes
		.windows(4)
		.position(|window| window == b"PK\x01\x02")
		.expect("central directory");
	let mode = 0o010644u32 << 16;
	bytes[central + 38..central + 42].copy_from_slice(&mode.to_le_bytes());
	fs::write(&input, bytes).unwrap();

	let mut check = zip::ZipArchive::new(fs::File::open(&input).unwrap()).unwrap();
	assert_eq!(check.by_index(0).unwrap().unix_mode(), Some(0o010644));

	assert!(
		repack(
			None,
			Native::Zip,
			Native::Tar,
			&input,
			&output,
			&Cancellation::default()
		)
		.is_err()
	);
	assert!(!output.exists());
}
