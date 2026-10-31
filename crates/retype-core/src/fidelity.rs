//! Fidelity claims and the manifest they are checked in as, `docs/fidelity.md` §4.
//!
//! A claim is evidence about one pair through one backend: that a conversion was
//! run, what it produced, and which build of the tool produced it. The tier alone
//! is a statement about loss; the manifest adds the part that decides whether the
//! statement still applies to the tool that is installed now.
//!
//! The manifest is a release artifact: it is checked in, shipped, and compared
//! against each machine's tools. It is not state, so it does not live in the state
//! directory.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Code, Diagnostic, Error, Result};
use crate::job::Tier;

/// Schema identifier, `docs/fidelity.md` §4.
pub const FIDELITY_VERSION: &str = "retype-fidelity-1";

/// One earned claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
	pub source: String,
	pub target: String,
	pub backend: String,
	pub tier: Tier,
	/// The named engine that resolved, for a tool that delegates, `docs/tools.md` §5.1.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub engine: Option<String>,
	/// The version the tool reported when the claim was earned.
	#[serde(default)]
	pub version: String,
	/// The build identity: the probe output hashed, `docs/tools.md` §2.3.
	#[serde(default)]
	pub fingerprint: String,
	/// What the conversion was run on, so the claim can be reproduced.
	pub fixture: String,
	/// The properties the fixtures carried and the output kept, `docs/fidelity.md` §4.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub axes: Vec<crate::axis::Axis>,
	/// The properties the fixtures carried and the output did not keep. A `converted`
	/// pair may lose one; an `exact` pair that loses one is not exact.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub lost: Vec<crate::axis::Axis>,
	/// Unix milliseconds, `docs/state.md` §3.
	pub earned: i64,
}

impl Claim {
	pub fn matches(&self, source: &str, target: &str, backend: &str) -> bool {
		self.source == source && self.target == target && self.backend == backend
	}

	/// Whether the installed build is the one this claim was earned with,
	/// `docs/fidelity.md` §6.
	pub fn is_current(&self, version: &str, fingerprint: &str) -> bool {
		self.version == version && self.fingerprint == fingerprint
	}
}

/// What a claim looks like against this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
	/// No claim existed for the pair.
	New,
	/// A claim existed and the installed build matches it.
	Earned,
	/// A claim existed and the installed build is not the one it was earned with.
	Unverified,
}

impl State {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::New => "new",
			Self::Earned => "earned",
			Self::Unverified => "unverified",
		}
	}
}

/// `fidelity.toml`: every claim the release ships.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
	#[serde(default = "version")]
	pub version: String,
	#[serde(default)]
	pub claim: Vec<Claim>,
}

fn version() -> String {
	FIDELITY_VERSION.to_string()
}

impl Default for Manifest {
	fn default() -> Self {
		Self {
			version: version(),
			claim: Vec::new(),
		}
	}
}

impl Manifest {
	/// Read a manifest. A missing file is an empty manifest, because a release that
	/// has not run the fixtures yet has no claims rather than a broken record.
	pub fn open(path: &Path) -> Result<Self> {
		let text = match fs::read_to_string(path) {
			Ok(text) => text,
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
			Err(error) => return Err(Error::Io(error)),
		};

		let manifest: Self = toml::from_str(&text).map_err(|error| {
			Error::Diagnostic(Diagnostic::global(
				Code::CfgParse,
				format!("{}: {error}", path.display()),
			))
		})?;

		if manifest.version != FIDELITY_VERSION {
			return Err(Error::Diagnostic(Diagnostic::global(
				Code::CfgVersion,
				format!(
					"{} declares '{}', this build reads '{FIDELITY_VERSION}'",
					path.display(),
					manifest.version
				),
			)));
		}

		Ok(manifest)
	}

	pub fn save(&self, path: &Path) -> Result<()> {
		let text = toml::to_string(self).map_err(|error| {
			Error::Diagnostic(Diagnostic::global(
				Code::InternalInvariant,
				format!("the manifest does not serialize: {error}"),
			))
		})?;

		if let Some(parent) = path.parent() {
			fs::create_dir_all(parent)?;
		}

		fs::write(path, text)?;
		Ok(())
	}

	pub fn get(&self, source: &str, target: &str, backend: &str) -> Option<&Claim> {
		self.claim.iter().find(|claim| claim.matches(source, target, backend))
	}

	/// Record a claim, replacing the one for the same pair and backend.
	pub fn record(&mut self, claim: Claim) {
		match self
			.claim
			.iter_mut()
			.find(|existing| existing.matches(&claim.source, &claim.target, &claim.backend))
		{
			Some(existing) => *existing = claim,
			None => self.claim.push(claim),
		}

		self.claim.sort_by(|left, right| {
			(&left.source, &left.target, &left.backend).cmp(&(&right.source, &right.target, &right.backend))
		});
	}

	/// The markdown table `docs/fidelity.md` §8 carries, between its markers.
	pub fn render(&self) -> String {
		if self.claim.is_empty() {
			return String::new();
		}

		let mut text = String::from(
			"| From | To | Tier | Backend | Engine | Version | Fingerprint | Properties | Fixture |\n\
			 |---|---|---|---|---|---|---|---|\n",
		);

		for claim in &self.claim {
			text.push_str(&format!(
				"| {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
				claim.source,
				claim.target,
				tier(claim.tier),
				claim.backend,
				claim.engine.as_deref().unwrap_or_default(),
				claim.version,
				match claim.fingerprint.len() {
					0 => String::new(),
					_ => claim.fingerprint.chars().take(16).collect::<String>(),
				},
				properties(claim),
				claim.fixture,
			));
		}

		text
	}
}

/// What the claim's fixtures kept, and what they lost.
fn properties(claim: &Claim) -> String {
	let kept: Vec<&str> = claim.axes.iter().map(|axis| axis.as_str()).collect();
	let lost: Vec<&str> = claim.lost.iter().map(|axis| axis.as_str()).collect();

	let mut text = kept.join(", ");

	if !lost.is_empty() {
		if !text.is_empty() {
			text.push_str("; ");
		}

		text.push_str("lost ");
		text.push_str(&lost.join(", "));
	}

	match text.is_empty() {
		true => "-".to_string(),
		false => text,
	}
}

/// Replace what sits between the claim markers in a document, `docs/fidelity.md` §8.
///
/// A missing marker is an error rather than an insertion point: the markers exist so
/// that a generated table cannot overwrite prose by being written in the wrong place.
pub fn render_into(document: &str, table: &str) -> Result<String> {
	const START: &str = "<!-- claims:start -->";
	const END: &str = "<!-- claims:end -->";

	let start = document.find(START).ok_or_else(|| {
		Error::Diagnostic(Diagnostic::global(
			Code::InternalInvariant,
			format!("the document has no {START} marker"),
		))
	})?;
	let end = document.find(END).ok_or_else(|| {
		Error::Diagnostic(Diagnostic::global(
			Code::InternalInvariant,
			format!("the document has no {END} marker"),
		))
	})?;

	let mut rendered = String::new();
	rendered.push_str(&document[..start + START.len()]);
	rendered.push('\n');
	rendered.push_str(table);
	rendered.push_str(&document[end..]);
	Ok(rendered)
}

fn tier(tier: Tier) -> &'static str {
	match tier {
		Tier::Exact => "exact",
		Tier::Converted => "converted",
		Tier::Delegated => "delegated",
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn claim(source: &str, target: &str) -> Claim {
		Claim {
			source: source.to_string(),
			target: target.to_string(),
			backend: "magick".to_string(),
			tier: Tier::Converted,
			engine: None,
			version: "7.1.2-34".to_string(),
			fingerprint: "0123456789abcdef0123456789abcdef".to_string(),
			fixture: "image sample".to_string(),
			axes: vec![crate::axis::Axis::Alpha],
			lost: Vec::new(),
			earned: 1_793_380_889_377,
		}
	}

	#[test]
	fn a_manifest_round_trips_through_its_file() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("fidelity.toml");

		let mut manifest = Manifest::default();
		manifest.record(claim("heic", "jpg"));
		manifest.save(&path).unwrap();

		assert_eq!(Manifest::open(&path).unwrap(), manifest);
	}

	#[test]
	fn a_pair_has_one_claim_and_recording_twice_replaces_it() {
		let mut manifest = Manifest::default();
		manifest.record(claim("heic", "jpg"));

		let mut second = claim("heic", "jpg");
		second.version = "7.1.3".to_string();
		manifest.record(second.clone());

		assert_eq!(manifest.claim, vec![second]);
	}

	#[test]
	fn a_claim_is_unverified_when_the_build_changed() {
		let claim = claim("heic", "jpg");

		assert!(claim.is_current("7.1.2-34", "0123456789abcdef0123456789abcdef"));
		assert!(!claim.is_current("7.1.3", "0123456789abcdef0123456789abcdef"));
		assert!(!claim.is_current("7.1.2-34", "ffffffffffffffffffffffffffffffff"));
	}

	#[test]
	fn claims_are_rendered_in_a_stable_order() {
		let mut manifest = Manifest::default();
		manifest.record(claim("png", "jpg"));
		manifest.record(claim("heic", "webp"));
		manifest.record(claim("heic", "jpg"));

		let rendered = manifest.render();
		let rows: Vec<&str> = rendered.lines().skip(2).collect();

		assert!(rows[0].starts_with("| heic | jpg |"), "{rendered}");
		assert!(rows[1].starts_with("| heic | webp |"), "{rendered}");
		assert!(rows[2].starts_with("| png | jpg |"), "{rendered}");
	}

	#[test]
	fn rendering_replaces_only_what_the_markers_enclose() {
		let document =
			"# Fidelity\n\nProse above.\n\n<!-- claims:start -->\nold\n<!-- claims:end -->\n\nProse below.\n";

		let rendered = render_into(document, "| a |\n").unwrap();

		assert!(rendered.contains("Prose above."));
		assert!(rendered.contains("Prose below."));
		assert!(!rendered.contains("old"));
		assert!(rendered.contains("<!-- claims:start -->\n| a |\n<!-- claims:end -->"));
	}

	#[test]
	fn a_document_without_markers_is_refused() {
		assert!(render_into("# Fidelity\n", "").is_err());
	}
}
