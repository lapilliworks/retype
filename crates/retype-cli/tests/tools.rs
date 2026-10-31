//! `retype tools`, `docs/cli.md` §5.
//!
//! The manifest is signed in the test, so the install path exercises the same
//! signature, hash and unpack checks a release would.

use std::fs;
use std::process::Command;

use ed25519_dalek::{Signer, SigningKey};

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

fn hex(bytes: &[u8]) -> String {
	bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn key() -> (SigningKey, String) {
	let signing = SigningKey::from_bytes(&[9u8; 32]);
	let public = hex(signing.verifying_key().as_bytes());
	(signing, public)
}

#[test]
fn an_offline_pack_installs_verifies_and_lists() {
	let dir = tempfile::tempdir().unwrap();
	let pack = dir.path().join("pack");
	let state = dir.path().join("state");
	fs::create_dir_all(&pack).unwrap();

	let artifact = pack.join("ffmpeg");
	fs::write(&artifact, b"#!/bin/sh\necho 'ffmpeg version 9.0.2 managed'\n").unwrap();

	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		fs::set_permissions(&artifact, fs::Permissions::from_mode(0o755)).unwrap();
	}

	let (signing, public) = key();
	let size = fs::metadata(&artifact).unwrap().len();
	let sha = retype_core::managed::sha256_file(&artifact).unwrap();

	let manifest = format!(
		"\
version = 'retype-tools-1'
sequence = 1
expires = '2999-01-01'

[[artifact]]
tool = 'ffmpeg'
variant = 'gpl'
version = '9.0.2'
platform = '{}'
arch = '{}'
url = 'https://example.invalid/ffmpeg'
size = {size}
sha256 = '{sha}'
",
		retype_core::managed::platform(),
		retype_core::managed::arch(),
	);

	fs::write(pack.join("manifest.toml"), &manifest).unwrap();
	fs::write(pack.join("manifest.sig"), signing.sign(manifest.as_bytes()).to_bytes()).unwrap();

	let installed = retype()
		.args(["tools", "install", "ffmpeg@gpl", "--offline"])
		.arg(&pack)
		.args(["--state-dir", state.to_str().unwrap()])
		.env("RETYPE_TOOLS_PUBLIC_KEY", &public)
		.output()
		.unwrap();

	assert_eq!(
		installed.status.code(),
		Some(0),
		"{}",
		String::from_utf8_lossy(&installed.stderr)
	);
	assert!(state.join("tools/bin/ffmpeg").exists());

	let verified = retype()
		.args(["tools", "verify", "--state-dir", state.to_str().unwrap()])
		.env("RETYPE_TOOLS_PUBLIC_KEY", &public)
		.output()
		.unwrap();

	assert_eq!(
		verified.status.code(),
		Some(0),
		"{}",
		String::from_utf8_lossy(&verified.stderr)
	);

	let listed = retype()
		.args(["tools", "list", "--installed", "--state-dir", state.to_str().unwrap()])
		.output()
		.unwrap();
	let stdout = String::from_utf8_lossy(&listed.stdout);

	assert!(stdout.contains("ffmpeg"), "{stdout}");
	assert!(stdout.contains("9.0.2"), "{stdout}");
}

#[test]
fn an_unsigned_pack_is_refused() {
	let dir = tempfile::tempdir().unwrap();
	let pack = dir.path().join("pack");
	fs::create_dir_all(&pack).unwrap();
	fs::write(pack.join("ffmpeg"), b"binary").unwrap();
	fs::write(
		pack.join("manifest.toml"),
		"version = 'retype-tools-1'\nsequence = 1\nexpires = '2999-01-01'\n",
	)
	.unwrap();

	let output = retype()
		.args(["tools", "install", "ffmpeg", "--offline"])
		.arg(&pack)
		.args(["--state-dir", dir.path().join("state").to_str().unwrap()])
		.output()
		.unwrap();

	assert_eq!(output.status.code(), Some(4));
	assert!(String::from_utf8_lossy(&output.stderr).contains("public key"));
}
