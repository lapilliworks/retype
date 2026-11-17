//! Archive names must not be silently changed by text decoding.

use std::fs;

use retype_core::archive::repack;
use retype_core::execute::Cancellation;
use retype_core::registry::Native;

#[test]
fn a_non_utf8_tar_name_is_refused_without_publication() {
	let directory = tempfile::tempdir().unwrap();
	let input = directory.path().join("input.tar");
	let output = directory.path().join("output.zip");
	let mut builder = tar::Builder::new(fs::File::create(&input).unwrap());
	let mut header = tar::Header::new_gnu();
	header.set_size(4);
	header.set_mode(0o644);
	let name = b"name-\xff.txt";
	header.as_mut_bytes()[..name.len()].copy_from_slice(name);
	header.set_cksum();
	builder.append(&header, &b"data"[..]).unwrap();
	builder.finish().unwrap();
	drop(builder);
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
	assert!(error.to_string().contains("entry name is not UTF-8"), "{error}");
	assert!(!output.exists());
	assert_eq!(fs::read(input).unwrap(), original);
}

fn zip_name_fixture(directory: &std::path::Path, flags: u16) -> std::path::PathBuf {
	use std::io::Write;
	let input = directory.join("input.zip");
	let mut writer = zip::ZipWriter::new(fs::File::create(&input).unwrap());
	writer
		.start_file("name-x.txt", zip::write::SimpleFileOptions::default())
		.unwrap();
	writer.write_all(b"data").unwrap();
	writer.finish().unwrap();
	let mut bytes = fs::read(&input).unwrap();
	let central = bytes.windows(4).position(|window| window == b"PK\x01\x02").unwrap();
	bytes[30 + 5] = 0xff;
	bytes[central + 46 + 5] = 0xff;
	for offset in [6, central + 8] {
		let existing = u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap());
		bytes[offset..offset + 2].copy_from_slice(&(existing | flags).to_le_bytes());
	}
	fs::write(&input, bytes).unwrap();
	input
}

#[test]
fn a_malformed_utf8_zip_name_is_refused_without_publication() {
	let directory = tempfile::tempdir().unwrap();
	let input = zip_name_fixture(directory.path(), 1 << 11);
	let output = directory.path().join("output.tar");
	let error = repack(
		None,
		Native::Zip,
		Native::Tar,
		&input,
		&output,
		&Cancellation::default(),
	)
	.unwrap_err();
	assert!(error.to_string().contains("entry name is not valid UTF-8"));
	assert!(!output.exists());
}

#[test]
fn a_legacy_cp437_zip_name_is_carried_without_loss() {
	let directory = tempfile::tempdir().unwrap();
	let input = zip_name_fixture(directory.path(), 0);
	let output = directory.path().join("output.tar");
	repack(
		None,
		Native::Zip,
		Native::Tar,
		&input,
		&output,
		&Cancellation::default(),
	)
	.unwrap();
	let mut archive = tar::Archive::new(fs::File::open(output).unwrap());
	let entry = archive.entries().unwrap().next().unwrap().unwrap();
	assert_eq!(entry.path().unwrap().to_str().unwrap(), "name-\u{00a0}.txt");
}
