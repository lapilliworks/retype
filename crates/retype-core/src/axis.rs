//! The properties a claim is about, `docs/fidelity.md` §4.
//!
//! A tier says what a conversion promises in general; an axis says what was actually
//! checked on this pair. The two halves of `docs/fidelity.md` §2 are the same idea: a
//! conversion that threw away the alpha channel is not exact for a logo, whatever the
//! target format is capable of in principle.
//!
//! A fixture carries an axis, and the check is the same fixture before and after: the
//! property is either in the output or it is not. Only the fixtures' own declared axes
//! are examined, so a target format that cannot carry one is never accused of losing
//! something the source did not have.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::error::{Code, Diagnostic, Error, Result};

/// Schema identifier for `fixtures/fidelity/fixtures.toml`.
pub const FIXTURES_VERSION: &str = "retype-fixtures-1";

/// What a lossless claim can be about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Axis {
	/// Transparency survives.
	Alpha,
	/// Every frame survives.
	Animation,
	/// Bits per channel do not shrink.
	Depth,
	/// The embedded colour profile survives.
	Icc,
	/// The EXIF orientation is applied exactly once: baked into the pixels or kept as a
	/// tag, never both.
	Orientation,
	/// A limited-colour image does not gain colours it did not have.
	Palette,
}

impl Axis {
	pub const ALL: [Axis; 6] = [
		Axis::Alpha,
		Axis::Animation,
		Axis::Depth,
		Axis::Icc,
		Axis::Orientation,
		Axis::Palette,
	];

	pub fn as_str(self) -> &'static str {
		match self {
			Self::Alpha => "alpha",
			Self::Animation => "animation",
			Self::Depth => "depth",
			Self::Icc => "icc",
			Self::Orientation => "orientation",
			Self::Palette => "palette",
		}
	}
}

/// Whether one property survived one conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
	Preserved,
	Lost,
}

impl Verdict {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Preserved => "kept",
			Self::Lost => "lost",
		}
	}
}

/// What one file says about itself, as far as the axes are concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
	pub width: u32,
	pub height: u32,
	/// Bits per channel.
	pub depth: u32,
	pub opaque: bool,
	/// Whether the file carries an embedded ICC profile, read from the bytes for the
	/// same reason the orientation is: an axis that a decoder will not report cannot be
	/// checked, and asking for a property that is absent makes some builds complain.
	pub icc: bool,
	pub scenes: u32,
	pub colors: u32,
	/// The EXIF orientation tag, as it appears in the file, `1` for absent.
	///
	/// Read from the bytes rather than from the decoder: the ImageMagick this build
	/// ships reads the EXIF block into a profile and does not surface the tag as a
	/// property, and an axis that cannot be read cannot be checked.
	pub orientation: u16,
}

impl Probe {
	/// Read a file's properties.
	///
	/// The decoder is ImageMagick, because it is what the release's fixtures are written
	/// by and what every image backend here delegates to. A machine without it cannot
	/// check an axis, and `retype fidelity` says so rather than guessing.
	pub fn read(path: &Path) -> Result<Self> {
		let format = "%w|%h|%z|%[opaque]|%n|%[colors]\\n";
		let output = decoder()
			.args(["-format", format])
			.arg(path)
			.output()
			.map_err(|error| {
				Error::Diagnostic(Diagnostic::global(
					Code::ToolMissing,
					format!("no ImageMagick to check a property with: {error}"),
				))
			})?;

		if !output.status.success() {
			return Err(Error::Diagnostic(Diagnostic::global(
				Code::ToolFailed,
				format!(
					"{} cannot be read: {}",
					path.display(),
					String::from_utf8_lossy(&output.stderr).trim()
				),
			)));
		}

		let text = String::from_utf8_lossy(&output.stdout);
		let line = text.lines().next().unwrap_or_default();
		let fields: Vec<&str> = line.split('|').collect();

		if fields.len() < 6 {
			return Err(Error::Diagnostic(Diagnostic::global(
				Code::InternalInvariant,
				format!("{}: unexpected identify output {line:?}", path.display()),
			)));
		}

		let number = |value: &str| value.trim().parse::<u32>().unwrap_or(0);

		Ok(Self {
			width: number(fields[0]),
			height: number(fields[1]),
			depth: number(fields[2]),
			opaque: fields[3].trim().eq_ignore_ascii_case("true"),
			icc: icc_present(path),
			scenes: number(fields[4]),
			colors: number(fields[5]),
			orientation: orientation_of(path).unwrap_or(1),
		})
	}
}

/// The command that reports a file's properties, `identify` spelled as the version
/// seven front end or as the version six program, whichever this machine has.
fn decoder() -> Command {
	let path = std::env::var_os("PATH").unwrap_or_default();

	let mut command = match crate::tools::find_in_path(&path, "magick") {
		Some(tool) => {
			let mut command = Command::new(tool);
			command.arg("identify");
			command
		}
		None => {
			Command::new(crate::tools::find_in_path(&path, "identify").unwrap_or_else(|| PathBuf::from("identify")))
		}
	};

	crate::process::hide_console(&mut command);
	command
}

/// How one fixture turned out through one pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
	pub fixture: String,
	pub axis: Axis,
	pub verdict: Verdict,
}

/// A checked-in file with a property to check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fixture {
	pub file: String,
	/// The format the file is, which is the source of the pairs it applies to.
	pub format: String,
	pub axes: Vec<Axis>,
}

/// `fixtures/fidelity/fixtures.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fixtures {
	#[serde(default = "version")]
	pub version: String,
	#[serde(default)]
	pub fixture: Vec<Fixture>,
}

fn version() -> String {
	FIXTURES_VERSION.to_string()
}

impl Default for Fixtures {
	fn default() -> Self {
		Self {
			version: version(),
			fixture: Vec::new(),
		}
	}
}

impl Fixtures {
	/// Read the declaration beside the fixtures. A directory without one has no
	/// fixtures, which is a claim that carries no axes rather than a broken release.
	pub fn open(directory: &Path) -> Result<Self> {
		let path = directory.join("fixtures.toml");
		let text = match std::fs::read_to_string(&path) {
			Ok(text) => text,
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
			Err(error) => return Err(Error::Io(error)),
		};

		let fixtures: Self = toml::from_str(&text).map_err(|error| {
			Error::Diagnostic(Diagnostic::global(
				Code::CfgParse,
				format!("{}: {error}", path.display()),
			))
		})?;

		if fixtures.version != FIXTURES_VERSION {
			return Err(Error::Diagnostic(Diagnostic::global(
				Code::CfgVersion,
				format!(
					"{} declares '{}', this build reads '{FIXTURES_VERSION}'",
					path.display(),
					fixtures.version
				),
			)));
		}

		for fixture in &fixtures.fixture {
			if !directory.join(&fixture.file).exists() {
				return Err(Error::Diagnostic(Diagnostic::global(
					Code::CfgMissing,
					format!("{}: declares {}, which is not there", path.display(), fixture.file),
				)));
			}
		}

		Ok(fixtures)
	}

	pub fn for_format<'a>(&'a self, format: &'a str) -> impl Iterator<Item = &'a Fixture> + 'a {
		self.fixture.iter().filter(move |fixture| fixture.format == format)
	}

	/// Every axis any fixture of this format carries, in order.
	pub fn axes(&self, format: &str) -> BTreeSet<Axis> {
		self.for_format(format)
			.flat_map(|fixture| fixture.axes.iter().copied())
			.collect()
	}
}

/// Check each axis a fixture declares, comparing the fixture with what came out.
pub fn compare(fixture: &Fixture, source: &Probe, output: &Probe) -> Vec<Check> {
	fixture
		.axes
		.iter()
		.map(|axis| Check {
			fixture: fixture.file.clone(),
			axis: *axis,
			verdict: verdict(*axis, source, output),
		})
		.collect()
}

/// Whether one property survived, given both sides.
pub fn verdict(axis: Axis, source: &Probe, output: &Probe) -> Verdict {
	match axis {
		// A fixture with a transparent region loses its alpha when the output is fully
		// opaque; nothing else about it matters.
		Axis::Alpha => match source.opaque || !output.opaque {
			true => Verdict::Preserved,
			false => Verdict::Lost,
		},
		Axis::Animation => match output.scenes >= source.scenes {
			true => Verdict::Preserved,
			false => Verdict::Lost,
		},
		// Sixteen bits into eight loses something no target can recover.
		Axis::Depth => match output.depth >= source.depth {
			true => Verdict::Preserved,
			false => Verdict::Lost,
		},
		Axis::Icc => match !source.icc || output.icc {
			true => Verdict::Preserved,
			false => Verdict::Lost,
		},
		Axis::Palette => match output.colors == 0 || output.colors <= source.colors {
			true => Verdict::Preserved,
			false => Verdict::Lost,
		},
		Axis::Orientation => orientation(source, output),
	}
}

/// The EXIF orientation is applied exactly once.
///
/// Two answers are correct. Either the tag survives and the pixels are untouched, in
/// which case the viewer rotates once, or the rotation is baked into the pixels and the
/// tag is cleared. The failure is both at once, which rotates twice on screen, and the
/// silence is neither, which loses the rotation altogether.
fn orientation(source: &Probe, output: &Probe) -> Verdict {
	if source.orientation <= 1 {
		// Nothing to apply, so nothing to get wrong.
		return match output.orientation <= 1 {
			true => Verdict::Preserved,
			false => Verdict::Lost,
		};
	}

	let keeps_tag = output.orientation == source.orientation;
	let swapped = output.width == source.height && output.height == source.width;
	let turned = swapped || (output.width > output.height && source.width < source.height);

	match (keeps_tag, turned) {
		// The tag is gone and the pixels did not move: the rotation is lost.
		(false, false) => Verdict::Lost,
		// The tag survives beside untouched pixels: correct.
		(true, false) => Verdict::Preserved,
		// The rotation was baked and the tag was cleared: correct.
		(false, true) => Verdict::Preserved,
		// Both: a viewer would rotate pixels that are already rotated.
		(true, true) => Verdict::Lost,
	}
}

/// Whether an embedded ICC profile is in the file.
///
/// JPEG keeps it in an APP2 segment that may be split across several, PNG in an `iCCP`
/// chunk, WebP in an `ICCP` chunk.
pub fn icc_present(path: &Path) -> bool {
	let Ok(data) = std::fs::read(path) else {
		return false;
	};

	match data.get(..3) {
		Some([0xff, 0xd8, ..]) => data.windows(12).any(|window| window == b"ICC_PROFILE\0"),
		Some([0x89, 0x50, 0x4e]) => chunk_present(&data, 8, b"iCCP"),
		Some([0x52, 0x49, 0x46]) => data.windows(4).any(|window| window == b"ICCP"),
		Some(_) => tiff_has_tag(&data, 34675),
		None => false,
	}
}

/// Whether a TIFF keeps a tag in its first directory. The ICC profile is tag 34675
/// there, where the other formats keep it in a named chunk.
fn tiff_has_tag(data: &[u8], wanted: u16) -> bool {
	let order = match data.get(..2) {
		Some([b'I', b'I']) => Order::Little,
		Some([b'M', b'M']) => Order::Big,
		_ => return false,
	};

	let Some(ifd) = order.u32(data.get(4..8).unwrap_or_default()).map(|at| at as usize) else {
		return false;
	};

	let Some(count) = order.u16(data.get(ifd..ifd + 2).unwrap_or_default()) else {
		return false;
	};

	let count = count as usize;

	(0..count.min(64)).any(|index| {
		let entry = ifd + 2 + index * 12;

		order.u16(data.get(entry..entry + 2).unwrap_or_default()) == Some(wanted)
	})
}

/// Whether a PNG chunk of this kind is there, walking the length-prefixed chunks.
fn chunk_present(data: &[u8], mut at: usize, kind: &[u8; 4]) -> bool {
	while at + 8 <= data.len() {
		let length = u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) as usize;
		let Some(found) = data.get(at + 4..at + 8) else {
			return false;
		};

		if found == kind {
			return true;
		}

		at += 12 + length;
	}

	false
}

/// The EXIF orientation tag, read from the bytes.
///
/// JPEG keeps it in an APP1 segment, PNG in an `eXIf` chunk. Anything else is a format
/// this build does not read an orientation from, which is not a failure: it is a file
/// with no orientation to check.
pub fn orientation_of(path: &Path) -> Option<u16> {
	let data = std::fs::read(path).ok()?;

	match data.get(..2)? {
		[0xff, 0xd8] => jpeg_orientation(&data),
		[0x89, 0x50] => png_orientation(&data),
		_ => None,
	}
}

fn jpeg_orientation(data: &[u8]) -> Option<u16> {
	let mut at = 2usize;

	while at + 4 <= data.len() {
		if data[at] != 0xff {
			return None;
		}

		let marker = data[at + 1];
		let length = u16::from_be_bytes([data[at + 2], data[at + 3]]) as usize;

		// The scan's payload is not a segment, so nothing after it is a header.
		if marker == 0xda {
			return None;
		}

		let body = data.get(at + 4..at + 2 + length)?;

		if marker == 0xe1 && body.starts_with(b"Exif\0\0") {
			return tiff_orientation(&body[6..]);
		}

		at += 2 + length;
	}

	None
}

fn png_orientation(data: &[u8]) -> Option<u16> {
	let mut at = 8usize;

	while at + 8 <= data.len() {
		let length = u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) as usize;
		let kind = data.get(at + 4..at + 8)?;
		let body = data.get(at + 8..at + 8 + length)?;

		if kind == b"eXIf" {
			return tiff_orientation(body);
		}

		at += 12 + length;
	}

	None
}

/// The first IFD's orientation entry, from a TIFF block in either byte order.
fn tiff_orientation(tiff: &[u8]) -> Option<u16> {
	let order = match tiff.get(..2)? {
		[b'I', b'I'] => Order::Little,
		[b'M', b'M'] => Order::Big,
		_ => return None,
	};

	let ifd = order.u32(tiff.get(4..8)?)? as usize;
	let count = order.u16(tiff.get(ifd..ifd + 2)?)? as usize;

	for index in 0..count.min(64) {
		let entry = ifd + 2 + index * 12;
		let tag = order.u16(tiff.get(entry..entry + 2)?)?;

		if tag == 0x0112 {
			let value = order.u16(tiff.get(entry + 8..entry + 10)?)?;

			return (1..=8).contains(&value).then_some(value);
		}
	}

	None
}

#[derive(Clone, Copy)]
enum Order {
	Little,
	Big,
}

impl Order {
	fn u16(self, bytes: &[u8]) -> Option<u16> {
		let pair = [*bytes.first()?, *bytes.get(1)?];

		Some(match self {
			Self::Little => u16::from_le_bytes(pair),
			Self::Big => u16::from_be_bytes(pair),
		})
	}

	fn u32(self, bytes: &[u8]) -> Option<u32> {
		let quad = [*bytes.first()?, *bytes.get(1)?, *bytes.get(2)?, *bytes.get(3)?];

		Some(match self {
			Self::Little => u32::from_le_bytes(quad),
			Self::Big => u32::from_be_bytes(quad),
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn probe(width: u32, height: u32) -> Probe {
		Probe {
			width,
			height,
			depth: 8,
			opaque: true,
			icc: false,
			scenes: 1,
			colors: 8,
			orientation: 1,
		}
	}

	#[test]
	fn alpha_is_lost_when_the_output_says_it_is_opaque() {
		let mut source = probe(32, 32);
		source.opaque = false;

		assert_eq!(verdict(Axis::Alpha, &source, &probe(32, 32)), Verdict::Lost);
		assert_eq!(verdict(Axis::Alpha, &source, &source), Verdict::Preserved);
	}

	#[test]
	fn depth_may_grow_but_not_shrink() {
		let mut deep = probe(32, 32);
		deep.depth = 16;

		assert_eq!(verdict(Axis::Depth, &deep, &probe(32, 32)), Verdict::Lost);
		assert_eq!(verdict(Axis::Depth, &probe(32, 32), &deep), Verdict::Preserved);
	}

	#[test]
	fn an_orientation_tag_beside_baked_pixels_was_applied_twice() {
		// A wide picture turned on its side, with the tag still saying "rotate".
		let mut source = probe(32, 16);
		source.orientation = 6;

		let mut doubled = probe(16, 32);
		doubled.orientation = 6;
		assert_eq!(verdict(Axis::Orientation, &source, &doubled), Verdict::Lost);

		// Baked and cleared: right.
		assert_eq!(verdict(Axis::Orientation, &source, &probe(16, 32)), Verdict::Preserved);

		// Tagged and untouched: also right.
		let mut tagged = probe(32, 16);
		tagged.orientation = 6;
		assert_eq!(verdict(Axis::Orientation, &source, &tagged), Verdict::Preserved);

		// Neither: the rotation is gone.
		assert_eq!(verdict(Axis::Orientation, &source, &probe(32, 16)), Verdict::Lost);
	}

	#[test]
	fn a_palette_may_shrink_but_not_grow() {
		let mut limited = probe(32, 32);
		limited.colors = 3;

		assert_eq!(verdict(Axis::Palette, &limited, &probe(32, 32)), Verdict::Lost);
		assert_eq!(verdict(Axis::Palette, &probe(32, 32), &limited), Verdict::Preserved);
	}

	#[test]
	fn an_icc_profile_that_was_there_and_is_not_is_lost() {
		let mut tagged = probe(32, 32);
		tagged.icc = true;

		assert_eq!(verdict(Axis::Icc, &tagged, &probe(32, 32)), Verdict::Lost);
		assert_eq!(verdict(Axis::Icc, &probe(32, 32), &tagged), Verdict::Preserved);
	}

	#[test]
	fn an_animation_compared_with_a_single_frame_is_lost() {
		let mut animated = probe(16, 16);
		animated.scenes = 3;

		assert_eq!(verdict(Axis::Animation, &animated, &probe(16, 16)), Verdict::Lost);
	}

	#[test]
	fn a_tiff_in_either_byte_order_yields_its_orientation() {
		let mut little = Vec::new();
		little.extend_from_slice(b"II");
		little.extend_from_slice(&42u16.to_le_bytes());
		little.extend_from_slice(&8u32.to_le_bytes());
		little.extend_from_slice(&1u16.to_le_bytes());
		little.extend_from_slice(&0x0112u16.to_le_bytes());
		little.extend_from_slice(&3u16.to_le_bytes());
		little.extend_from_slice(&1u32.to_le_bytes());
		little.extend_from_slice(&6u16.to_le_bytes());
		little.extend_from_slice(&[0, 0]);
		little.extend_from_slice(&0u32.to_le_bytes());

		assert_eq!(tiff_orientation(&little), Some(6));

		let mut big = Vec::new();
		big.extend_from_slice(b"MM");
		big.extend_from_slice(&42u16.to_be_bytes());
		big.extend_from_slice(&8u32.to_be_bytes());
		big.extend_from_slice(&1u16.to_be_bytes());
		big.extend_from_slice(&0x0112u16.to_be_bytes());
		big.extend_from_slice(&3u16.to_be_bytes());
		big.extend_from_slice(&1u32.to_be_bytes());
		big.extend_from_slice(&8u16.to_be_bytes());
		big.extend_from_slice(&[0, 0]);
		big.extend_from_slice(&0u32.to_be_bytes());

		assert_eq!(tiff_orientation(&big), Some(8));
	}

	#[test]
	fn a_tag_that_is_not_an_orientation_is_not_read_as_one() {
		let mut tiff = Vec::new();
		tiff.extend_from_slice(b"II");
		tiff.extend_from_slice(&42u16.to_le_bytes());
		tiff.extend_from_slice(&8u32.to_le_bytes());
		tiff.extend_from_slice(&1u16.to_le_bytes());
		tiff.extend_from_slice(&0x0112u16.to_le_bytes());
		tiff.extend_from_slice(&3u16.to_le_bytes());
		tiff.extend_from_slice(&1u32.to_le_bytes());
		tiff.extend_from_slice(&99u16.to_le_bytes());
		tiff.extend_from_slice(&[0, 0]);
		tiff.extend_from_slice(&0u32.to_le_bytes());

		assert_eq!(tiff_orientation(&tiff), None);
	}
}
