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
	assert!(report.validated);

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
