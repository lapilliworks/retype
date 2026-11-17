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
	#[serde(default, skip_serializing_if = "String::is_empty")]
	pub engine_version: String,
	#[serde(default, skip_serializing_if = "String::is_empty")]
	pub engine_fingerprint: String,
	/// What the conversion was run on, so the claim can be reproduced.
	pub fixture: String,
	/// The properties the fixtures carried and the output kept, `docs/fidelity.md` §4.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub axes: Vec<crate::axis::Axis>,
	/// The properties the fixtures carried and the output did not keep. A `converted`
	/// pair may lose one; an `exact` pair that loses one is not exact.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub lost: Vec<crate::axis::Axis>,
	/// Unix milliseconds, `docs/dev/architecture/state.md` §3.
	pub earned: i64,
}

impl Claim {
	pub fn matches(&self, source: &str, target: &str, backend: &str, engine: Option<&str>) -> bool {
		self.source == source && self.target == target && self.backend == backend && self.engine.as_deref() == engine
	}

	/// Whether the installed build is the one this claim was earned with,
	/// `docs/fidelity.md` §6.
	pub fn is_current(&self, version: &str, fingerprint: &str) -> bool {
		self.version == version && self.fingerprint == fingerprint
	}
}

/// What a claim looks like against this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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

/// One pair's claim, or the absence of one, against the build installed now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairClaim<'a> {
	pub claim: Option<&'a Claim>,
	pub state: State,
	pub version: String,
	pub fingerprint: String,
	pub engine_version: String,
	pub engine_fingerprint: String,
}

/// Evidence persisted with a planned output, separate from its declared tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
	pub state: State,
	pub version: String,
	pub fingerprint: String,
	pub engine_version: String,
	pub engine_fingerprint: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub claim: Option<Claim>,
}

impl Evidence {
	pub fn reported_tier(&self, declared: Tier) -> &'static str {
		match self.state {
			State::New => "unclaimed",
			State::Unverified => "unverified",
			State::Earned => tier(declared),
		}
	}
}

/// What the release says about a pair and whether this machine still matches it.
///
/// A native container is made by this build, so its version is this build's and
/// its fingerprint is empty. An external backend is probed, because a package
/// update is exactly how a claim goes stale.
pub fn claim_for<'a>(
	manifest: &'a Manifest,
	registry: &crate::registry::Registry,
	source: &str,
	target: &str,
	backend: &str,
	engine: Option<&str>,
) -> PairClaim<'a> {
	claim_for_cancellable(
		manifest,
		registry,
		source,
		target,
		backend,
		engine,
		&crate::execute::Cancellation::default(),
	)
}

pub fn claim_for_cancellable<'a>(
	manifest: &'a Manifest,
	registry: &crate::registry::Registry,
	source: &str,
	target: &str,
	backend: &str,
	engine: Option<&str>,
	cancellation: &crate::execute::Cancellation,
) -> PairClaim<'a> {
	let archive_tool = registry
		.format(source)
		.and_then(|format| format.native)
		.zip(registry.format(target).and_then(|format| format.native))
		.filter(|(from, to)| from.needs_tool() || to.needs_tool())
		.and_then(|_| registry.tool(crate::tools::SEVEN_ZIP));
	let tool = registry.tool(backend).or(archive_tool);
	let (version, fingerprint, is_usable) = match tool {
		Some(tool) => {
			let status = crate::tools::probe_cancellable(tool, cancellation);
			(
				status.version.as_ref().map(ToString::to_string).unwrap_or_default(),
				status.fingerprint().unwrap_or_default(),
				status.is_usable() && status.version.is_some() && status.fingerprint().is_some(),
			)
		}
		None => (env!("CARGO_PKG_VERSION").to_string(), String::new(), true),
	};

	let (engine_version, engine_fingerprint) = match (tool, engine) {
		(Some(tool), Some(candidate)) => {
			let mut engine_tool = tool.clone();
			engine_tool.command = candidate.to_string();
			engine_tool.aliases.clear();
			engine_tool.probe = vec!["--version".to_string()];
			engine_tool.version_min = None;
			engine_tool.components.clear();
			let status = crate::tools::probe_cancellable(&engine_tool, cancellation);
			if status.is_usable() {
				(
					status.version.as_ref().map(ToString::to_string).unwrap_or_default(),
					status.fingerprint().unwrap_or_default(),
				)
			} else {
				(String::new(), String::new())
			}
		}
		_ => (String::new(), String::new()),
	};
	let claim = manifest.get(source, target, backend, engine);
	let state = match claim {
		None => State::New,
		Some(claim)
			if is_usable
				&& claim.is_current(&version, &fingerprint)
				&& registry.format(target).is_some_and(|format| format.tier == claim.tier)
				&& (claim.tier != Tier::Exact || (!claim.axes.is_empty() && claim.lost.is_empty()))
				&& claim.engine_version == engine_version
				&& claim.engine_fingerprint == engine_fingerprint
				&& (engine.is_none() || (!engine_version.is_empty() && !engine_fingerprint.is_empty())) =>
		{
			State::Earned
		}
		Some(_) => State::Unverified,
	};

	PairClaim {
		claim,
		state,
		version,
		fingerprint,
		engine_version,
		engine_fingerprint,
	}
}

/// Attach the same evidence used by inspection to every output plan.
pub fn qualify(
	manifest: &Manifest,
	registry: &crate::registry::Registry,
	source: &str,
	plans: &mut [crate::plan::Plan],
) {
	qualify_cancellable(
		manifest,
		registry,
		source,
		plans,
		&crate::execute::Cancellation::default(),
	);
}

pub fn qualify_cancellable(
	manifest: &Manifest,
	registry: &crate::registry::Registry,
	source: &str,
	plans: &mut [crate::plan::Plan],
	cancellation: &crate::execute::Cancellation,
) {
	for plan in plans {
		let current = claim_for_cancellable(
			manifest,
			registry,
			source,
			&plan.format,
			plan.backend.name(),
			plan.engine.as_ref().map(|(_, candidate)| candidate.as_str()),
			cancellation,
		);
		plan.fidelity = Some(Evidence {
			state: current.state,
			version: current.version,
			fingerprint: current.fingerprint,
			engine_version: current.engine_version,
			engine_fingerprint: current.engine_fingerprint,
			claim: current.claim.cloned(),
		});
	}
}

/// Refuse a stale exact promise before execution or output publication.
pub fn check(plan: &crate::plan::Plan) -> Result<()> {
	if let Some(evidence) = &plan.fidelity {
		if plan.tier == Tier::Exact && evidence.state == State::Unverified {
			let Some(claim) = &evidence.claim else {
				return Err(Diagnostic::global(Code::Unsupported, "exact output has no verified claim").into());
			};
			return Err(Diagnostic::global(
				Code::Unsupported,
				format!(
					"{} to {}: exact claim for {} is stale; recorded version {} fingerprint {}, \
					 installed version {} fingerprint {}; engine {:?}: recorded {} {}, installed {} {}; \
					 rerun fidelity or relax [formats.{}] tier",
					claim.source,
					claim.target,
					claim.backend,
					claim.version,
					claim.fingerprint,
					evidence.version,
					evidence.fingerprint,
					claim.engine,
					claim.engine_version,
					claim.engine_fingerprint,
					evidence.engine_version,
					evidence.engine_fingerprint,
					plan.format,
				),
			)
			.into());
		}
	}
	Ok(())
}

/// Locate release evidence consistently for conversion and inspection.
pub fn open_for(state_dir: Option<&Path>) -> Result<Manifest> {
	if let Some(path) = std::env::var_os("RETYPE_FIDELITY_MANIFEST") {
		return Manifest::open(Path::new(&path));
	}
	if let Some(state_dir) = state_dir {
		let path = state_dir.join("fidelity.toml");
		if path.try_exists()? {
			return Manifest::open(&path);
		}
	}
	let local = Path::new("fidelity.toml");
	if local.try_exists()? {
		return Manifest::open(local);
	}
	if let Ok(executable) = std::env::current_exe() {
		for ancestor in executable.ancestors().skip(1) {
			let path = ancestor.join("share/retype/fidelity.toml");
			if path.try_exists()? {
				return Manifest::open(&path);
			}
		}
	}
	Ok(Manifest::default())
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

	pub fn get(&self, source: &str, target: &str, backend: &str, engine: Option<&str>) -> Option<&Claim> {
		self.claim
			.iter()
			.find(|claim| claim.matches(source, target, backend, engine))
	}

	/// Record a claim, replacing the one for the same pair, backend and resolved engine.
	pub fn record(&mut self, claim: Claim) {
		match self
			.claim
			.iter_mut()
			.find(|existing| existing.matches(&claim.source, &claim.target, &claim.backend, claim.engine.as_deref()))
		{
			Some(existing) => *existing = claim,
			None => self.claim.push(claim),
		}

		self.claim.sort_by(|left, right| {
			(&left.source, &left.target, &left.backend, &left.engine).cmp(&(
				&right.source,
				&right.target,
				&right.backend,
				&right.engine,
			))
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
			engine_version: String::new(),
			engine_fingerprint: String::new(),
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
	fn resolved_engines_have_independent_claims() {
		let mut manifest = Manifest::default();
		let mut typst = claim("md", "pdf");
		typst.engine = Some("typst".to_string());
		let mut weasyprint = typst.clone();
		weasyprint.engine = Some("weasyprint".to_string());
		manifest.record(weasyprint.clone());
		manifest.record(typst.clone());

		assert_eq!(manifest.get("md", "pdf", "magick", Some("typst")), Some(&typst));
		assert_eq!(
			manifest.get("md", "pdf", "magick", Some("weasyprint")),
			Some(&weasyprint)
		);
		assert!(manifest.get("md", "pdf", "magick", None).is_none());
		assert!(manifest.get("md", "pdf", "magick", Some("xelatex")).is_none());
		typst.version = "new build".to_string();
		manifest.record(typst.clone());
		assert_eq!(manifest.claim, vec![typst, weasyprint]);
	}

	#[test]
	fn a_native_claim_is_measured_against_this_build() {
		let registry = crate::registry::Registry::builtin().expect("the shipped table loads");
		let mut native = claim("tar", "zip");
		native.backend = "zip".to_string();
		native.tier = registry.format("zip").unwrap().tier;
		native.version = env!("CARGO_PKG_VERSION").to_string();
		native.fingerprint = String::new();

		let mut manifest = Manifest::default();
		manifest.record(native);

		let current = claim_for(&manifest, &registry, "tar", "zip", "zip", None);
		assert_eq!(current.state, State::Earned);

		let mut stale = manifest;
		stale.claim[0].fingerprint = "not-this-build".to_string();
		let stale = claim_for(&stale, &registry, "tar", "zip", "zip", None);
		assert_eq!(stale.state, State::Unverified);
	}

	#[cfg(unix)]
	#[test]
	fn an_engine_build_change_invalidates_only_its_claim() {
		use std::os::unix::fs::PermissionsExt;
		let dir = tempfile::tempdir().unwrap();
		let backend = dir.path().join("backend");
		let engine = dir.path().join("engine");
		for path in [&backend, &engine] {
			fs::write(path, "#!/bin/sh\necho 'version 1.0 build A'\n").unwrap();
			fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
		}
		let registry = crate::registry::Registry::load(&format!(
			"version = 'retype-config-1'\n[tools.copy]\ncommand = {:?}\nprobe = ['--version']\nlayout = ['{{input}}', '{{ops}}', '{{output}}']\n\n[formats.pdf]\nfamily = 'document'\nlossy = false\nalpha = false\ntier = 'converted'\n",
			backend.to_str().unwrap(),
		)).unwrap();
		let candidate = engine.to_str().unwrap();
		let mut manifest = Manifest::default();
		let current = claim_for(&manifest, &registry, "md", "pdf", "copy", Some(candidate));
		let mut earned = claim("md", "pdf");
		earned.backend = "copy".to_string();
		earned.engine = Some(candidate.to_string());
		earned.version = current.version;
		earned.fingerprint = current.fingerprint;
		earned.engine_version = current.engine_version;
		earned.engine_fingerprint = current.engine_fingerprint;
		manifest.record(earned);
		assert_eq!(
			claim_for(&manifest, &registry, "md", "pdf", "copy", Some(candidate)).state,
			State::Earned
		);
		fs::write(&engine, "#!/bin/sh\necho 'version 1.0 build B'\n").unwrap();
		assert_eq!(
			claim_for(&manifest, &registry, "md", "pdf", "copy", Some(candidate)).state,
			State::Unverified
		);
		assert_eq!(
			claim_for(&manifest, &registry, "md", "pdf", "copy", None).state,
			State::New
		);
	}

	#[test]
	fn a_stale_exact_plan_fails_before_creating_an_output() {
		let registry = crate::registry::Registry::builtin().unwrap();
		let parsed = crate::intent::parse("zip").unwrap();
		let resolved = crate::resolve::resolve(
			&parsed,
			crate::resolve::Source {
				format: "tar",
				is_directory: false,
			},
			&registry,
		)
		.unwrap();
		let mut plans = crate::plan::compile(&registry, &crate::plan::Nothing, &resolved).unwrap();
		let mut manifest = Manifest::default();
		let mut stale = claim("tar", "zip");
		stale.backend = "zip".to_string();
		stale.tier = Tier::Exact;
		manifest.record(stale);
		qualify(&manifest, &registry, "tar", &mut plans);
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source.tar");
		let output = dir.path().join("output.zip");
		fs::write(&source, "not read by a refused plan").unwrap();
		let error = crate::execute::run(&plans[0], &source, &output).unwrap_err();
		assert_eq!(error.code(), Code::Unsupported);
		assert_eq!(fs::read_to_string(source).unwrap(), "not read by a refused plan");
		assert!(!output.exists());
	}

	#[test]
	fn a_pair_with_no_claim_is_new() {
		let registry = crate::registry::Registry::builtin().expect("the shipped table loads");
		let manifest = Manifest::default();

		let answer = claim_for(&manifest, &registry, "tar", "zip", "zip", None);
		assert_eq!(answer.state, State::New);
		assert!(answer.claim.is_none());
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
