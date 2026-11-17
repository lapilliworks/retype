//! Independent output checks that can authorize source disposal.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use crate::error::{Code, Diagnostic, Error, Result};
use crate::execute::{Cancellation, Validation, executable, run_child};
use crate::safety::Fingerprint;

/// Separate decoders check readability, not fidelity to the source.
pub(crate) fn jpeg(path: &Path, cancellation: &Cancellation) -> Result<Validation> {
	let magick = executable("magick");
	let ffmpeg = executable("ffmpeg");
	jpeg_with_validators(path, cancellation, magick.as_deref(), ffmpeg.as_deref())
}

fn jpeg_with_validators(
	path: &Path,
	cancellation: &Cancellation,
	magick: Option<&Path>,
	ffmpeg: Option<&Path>,
) -> Result<Validation> {
	let checked = checked(path, cancellation, || {
		jpeg_markers(path)?;
		if let Some(executable) = magick {
			validate_silently("magick", executable, &[path.into(), "null:".into()], cancellation)?;
		}
		if let Some(executable) = ffmpeg {
			let args = [
				"-nostdin".into(),
				"-v".into(),
				"error".into(),
				"-xerror".into(),
				"-err_detect".into(),
				"explode".into(),
				"-threads".into(),
				"1".into(),
				"-f".into(),
				"mjpeg".into(),
				"-c:v".into(),
				"mjpeg".into(),
				"-i".into(),
				path.into(),
				"-map".into(),
				"0:v:0".into(),
				"-f".into(),
				"null".into(),
				"-".into(),
			];
			validate_silently("ffmpeg", executable, &args, cancellation)?;
		}
		Ok(magick.is_some() && ffmpeg.is_some())
	})?;
	match checked {
		Some(fingerprint) => Ok(Validation::Verified(fingerprint)),
		None => {
			let missing = match (magick.is_none(), ffmpeg.is_none()) {
				(true, true) => "ImageMagick and FFmpeg are missing",
				(true, false) => "ImageMagick is missing",
				(false, true) => "FFmpeg is missing",
				(false, false) => unreachable!("both validators completed"),
			};
			Ok(Validation::Unavailable(format!(
				"JPEG validation requires both tools; {missing}"
			)))
		}
	}
}

fn checked(
	path: &Path,
	cancellation: &Cancellation,
	validate: impl FnOnce() -> Result<bool>,
) -> Result<Option<Fingerprint>> {
	cancellation.commit(|| Ok(()))?;
	let fingerprint = Fingerprint::of(path)?;
	let valid = validate();
	cancellation.commit(|| Ok(()))?;
	if !valid? {
		return Ok(None);
	}
	if !fingerprint.matches(path)? {
		return Err(Error::Io(io::Error::other(format!(
			"{} changed during output validation; original retained",
			path.display()
		))));
	}
	Ok(Some(fingerprint))
}

fn jpeg_markers(path: &Path) -> Result<()> {
	let mut file = File::open(path)?;
	let mut start = [0; 3];
	let mut end = [0; 2];
	if file.metadata()?.len() < 4 {
		return Err(invalid_jpeg("missing JPEG start or end marker"));
	}
	file.read_exact(&mut start)?;
	file.seek(SeekFrom::End(-2))?;
	file.read_exact(&mut end)?;
	if start != [0xff, 0xd8, 0xff] || end != [0xff, 0xd9] {
		return Err(invalid_jpeg("missing JPEG start or end marker"));
	}
	Ok(())
}

fn validate_silently(command: &str, path: &Path, args: &[OsString], cancellation: &Cancellation) -> Result<()> {
	let warnings = run_child(command, path, args, None, None, cancellation)?;
	if !warnings.is_empty() {
		return Err(invalid_jpeg(&format!(
			"{command} reported a validation warning: {warnings}"
		)));
	}
	Ok(())
}

fn invalid_jpeg(reason: &str) -> Error {
	Error::Diagnostic(Diagnostic::global(
		Code::ToolFailed,
		format!("output failed independent JPEG validation: {reason}"),
	))
}

#[cfg(test)]
mod tests {
	use std::fs;
	use std::path::PathBuf;

	use super::*;

	const JPEG: &[u8] = include_bytes!("../tests/fixtures/chongqing.jpg");

	fn write_jpeg(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
		let directory = tempfile::tempdir().unwrap();
		let path = directory.path().join("output.jpg");
		fs::write(&path, bytes).unwrap();
		(directory, path)
	}

	fn validators() -> Option<(PathBuf, PathBuf)> {
		match (executable("magick"), executable("ffmpeg")) {
			(Some(magick), Some(ffmpeg)) => Some((magick, ffmpeg)),
			_ => {
				assert_ne!(
					std::env::var_os("RETYPE_REQUIRE_TEST_TOOLS").as_deref(),
					Some(std::ffi::OsStr::new("1")),
					"JPEG validation tests require both magick and ffmpeg"
				);
				None
			}
		}
	}

	#[test]
	fn valid_jpeg_is_decoded_and_bound_to_its_identity() {
		let Some((magick, ffmpeg)) = validators() else {
			return;
		};
		let (_directory, path) = write_jpeg(JPEG);
		let validation = jpeg_with_validators(&path, &Cancellation::default(), Some(&magick), Some(&ffmpeg)).unwrap();
		assert!(validation.fingerprint().unwrap().matches(&path).unwrap());
	}

	#[test]
	fn corrupt_and_truncated_jpegs_are_rejected_even_with_a_forged_end_marker() {
		let Some((magick, ffmpeg)) = validators() else {
			return;
		};
		for cut in [1, 8, JPEG.len() / 2] {
			let mut truncated = JPEG[..JPEG.len() - cut - 2].to_vec();
			truncated.extend_from_slice(&[0xff, 0xd9]);
			let (_directory, path) = write_jpeg(&truncated);
			let result = jpeg_with_validators(&path, &Cancellation::default(), Some(&magick), Some(&ffmpeg));
			assert!(result.is_err(), "accepted a JPEG missing {cut} bytes");
		}
	}

	#[test]
	fn wrong_format_and_missing_end_marker_fail_without_decoders() {
		for bytes in [&b"not a JPEG"[..], &JPEG[..JPEG.len() - 2]] {
			let (_directory, path) = write_jpeg(bytes);
			assert!(jpeg_with_validators(&path, &Cancellation::default(), None, None).is_err());
		}
	}

	#[test]
	fn missing_either_validator_never_authorizes_disposal() {
		let (_directory, path) = write_jpeg(JPEG);
		let cancellation = Cancellation::default();
		let unavailable = jpeg_with_validators(&path, &cancellation, None, None).unwrap();
		assert!(unavailable.fingerprint().is_none());
		assert!(unavailable.reason().unwrap().contains("ImageMagick and FFmpeg"));
		if let Some((magick, ffmpeg)) = validators() {
			let missing_ffmpeg = jpeg_with_validators(&path, &cancellation, Some(&magick), None).unwrap();
			assert!(missing_ffmpeg.reason().unwrap().contains("FFmpeg is missing"));
			let missing_magick = jpeg_with_validators(&path, &cancellation, None, Some(&ffmpeg)).unwrap();
			assert!(missing_magick.reason().unwrap().contains("ImageMagick is missing"));
		}
	}

	#[test]
	fn replacement_during_validation_does_not_authorize_disposal() {
		let (_directory, path) = write_jpeg(JPEG);
		let moved = path.with_extension("moved");
		let result = checked(&path, &Cancellation::default(), || {
			fs::rename(&path, &moved)?;
			fs::copy(&moved, &path)?;
			Ok(true)
		});
		assert!(result.is_err());
		assert_eq!(fs::read(path).unwrap(), JPEG);
	}

	#[test]
	fn cancellation_stops_before_validation() {
		let (_directory, path) = write_jpeg(JPEG);
		let cancellation = Cancellation::default();
		cancellation.cancel();
		let error = jpeg_with_validators(&path, &cancellation, None, None).unwrap_err();
		assert_eq!(error.code(), Code::JobCancelled);
	}
}
