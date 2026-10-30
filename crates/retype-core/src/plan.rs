//! Compiling a resolved intent into work, `docs/config.md` §8.
//!
//! A plan names a backend and, for an external one, the argv template that will
//! be spliced into the tool's layout. Planning is deterministic: the preference
//! list in the configuration decides, never the order in which tools were found,
//! and what the machine has is a question asked through [`Presence`], so a plan
//! can be compiled without probing by a caller that only wants the answer.

use std::ffi::OsString;
use std::path::Path;

use crate::error::{Code, Diagnoses, Diagnostic, Error};
use crate::job::Tier;
use crate::print;
use crate::registry::{Fragment, Registry};
use crate::resolve::{self, Resolved};
use crate::tools::Tool;

/// What the planner may ask about the machine.
pub trait Presence {
	/// Whether a tool is on the path, new enough, and carrying its components.
	fn is_available(&self, tool: &Tool) -> bool;

	/// The first candidate of a named engine that is on the path.
	fn engine(&self, tool: &Tool, name: &str) -> Option<String>;
}

/// A machine with nothing installed: every backend is unusable, which is what a
/// test wants when it asks what the declarations could do.
pub struct Nothing;

impl Presence for Nothing {
	fn is_available(&self, _tool: &Tool) -> bool {
		false
	}

	fn engine(&self, _tool: &Tool, _name: &str) -> Option<String> {
		None
	}
}

impl Presence for crate::tools::Machine {
	fn is_available(&self, tool: &Tool) -> bool {
		crate::tools::probe(tool).is_usable()
	}

	fn engine(&self, tool: &Tool, name: &str) -> Option<String> {
		crate::tools::resolve_engine(tool, name).map(str::to_string)
	}
}

/// Where a plan will run. One variant today: every conversion is a process. The
/// enum is the seam a second kind of backend would arrive through, so that the
/// rest of the compiler does not change shape when one does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend<'a> {
	Tool(&'a Tool),
}

impl Backend<'_> {
	pub fn name(&self) -> &str {
		let Self::Tool(tool) = self;
		&tool.name
	}
}

/// One element of an argv template. Paths are not known until the job runs, so
/// they stay holes rather than being filled in here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arg {
	Literal(String),
	Input,
	Output,
}

/// What one output of one rename will do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan<'a> {
	/// The format this output carries.
	pub format: String,
	pub backend: Backend<'a>,
	/// The layout with `{ops}` expanded.
	pub argv: Vec<Arg>,
	pub tier: Tier,
	/// The engine that made it in: its name and the candidate that resolved it,
	/// which is recorded with the job because the same intent with a different
	/// engine is a different result.
	pub engine: Option<(String, String)>,
}

impl Plan<'_> {
	/// The argv for a concrete input and output path.
	pub fn argv(&self, input: &Path, output: &Path) -> Vec<OsString> {
		self.argv
			.iter()
			.map(|arg| match arg {
				Arg::Literal(text) => OsString::from(text),
				Arg::Input => input.as_os_str().to_os_string(),
				Arg::Output => output.as_os_str().to_os_string(),
			})
			.collect()
	}
}

/// Compile every output of a resolved intent, in written order.
pub fn compile<'a>(
	registry: &'a Registry,
	presence: &dyn Presence,
	resolved: &Resolved,
) -> Result<Vec<Plan<'a>>, Diagnoses> {
	let mut plans = Vec::new();
	let mut problems = Vec::new();

	for group in &resolved.groups {
		match plan_group(registry, presence, group) {
			Ok(plan) => plans.push(plan),
			Err(diagnosed) => problems.extend(diagnosed),
		}
	}

	if problems.is_empty() {
		Ok(plans)
	} else {
		Err(Diagnoses::new(problems))
	}
}

fn plan_group<'a>(
	registry: &'a Registry,
	presence: &dyn Presence,
	group: &resolve::Group,
) -> Result<Plan<'a>, Vec<Error>> {
	let ordered = group.ordered();
	let mut unavailable: Vec<&Tool> = Vec::new();

	for candidate in registry.backends() {
		let Some(tool) = registry.tool(candidate) else {
			continue;
		};

		let carries = ordered.iter().all(|op| {
			registry
				.op(&op.key)
				.is_some_and(|declared| declared.fragment(&tool.name).is_some())
		});

		if !carries {
			continue;
		}

		if !presence.is_available(tool) {
			unavailable.push(tool);
			continue;
		}

		return assemble(registry, presence, tool, group, &ordered);
	}

	Err(vec![unsatisfiable(registry, group, &ordered, &unavailable)])
}

/// Why no backend could carry this group, said in terms the user can act on.
fn unsatisfiable(
	registry: &Registry,
	group: &resolve::Group,
	ordered: &[&resolve::Op],
	unavailable: &[&Tool],
) -> Error {
	let format = &group.format;

	// `docs/tools.md` §1: no tool could ever provide it, as distinct from a tool
	// that could and is not installed.
	if let Some(op) = ordered
		.iter()
		.find(|op| registry.op(&op.key).is_none_or(|declared| declared.argv.is_empty()))
	{
		return Error::Diagnostic(Diagnostic::new(
			Code::Unsupported,
			format!("no backend carries '{}' for {format}", op.name),
			op.span,
		));
	}

	if unavailable.is_empty() {
		return Error::Diagnostic(Diagnostic::global(
			Code::Unsupported,
			format!("no single backend carries every operation of this {format} output"),
		));
	}

	let names: Vec<&str> = unavailable.iter().map(|tool| tool.name.as_str()).collect();
	Error::Diagnostic(Diagnostic::global(
		Code::ToolMissing,
		format!(
			"{} is needed for this {format} output and is not usable",
			names.join(" and ")
		),
	))
}

fn tier_of(registry: &Registry, format: &str) -> Tier {
	registry
		.format(format)
		.map(|format| format.tier)
		.unwrap_or(Tier::Converted)
}

/// Splice the format fragment and the operation fragments into the tool's
/// layout, `docs/config.md` §8.
fn assemble<'a>(
	registry: &'a Registry,
	presence: &dyn Presence,
	tool: &'a Tool,
	group: &resolve::Group,
	ordered: &[&resolve::Op],
) -> Result<Plan<'a>, Vec<Error>> {
	let mut pieces: Vec<(&Fragment, Option<&resolve::Op>)> = Vec::new();

	if let Some(fragment) = registry
		.format(&group.format)
		.and_then(|format| format.fragment(&tool.name))
	{
		pieces.push((fragment, None));
	}

	for op in ordered {
		if let Some(fragment) = registry.op(&op.key).and_then(|declared| declared.fragment(&tool.name)) {
			pieces.push((fragment, Some(op)));
		}
	}

	let mut argv = Vec::new();
	let mut problems = Vec::new();
	let mut engine = None;

	for argument in &tool.layout {
		match argument.as_str() {
			"{input}" => argv.push(Arg::Input),
			"{output}" => argv.push(Arg::Output),
			"{ops}" => {
				for (fragment, owner) in &pieces {
					let value = owner
						.and_then(|op| op.value.as_ref())
						.map(|value| print::value(&value.parsed));

					for argument in &fragment.args {
						match splice(argument, value.as_deref(), tool, presence) {
							Ok((text, resolved)) => {
								if let Some(resolved) = resolved {
									engine = Some(resolved);
								}
								argv.push(Arg::Literal(text));
							}
							Err(problem) => problems.push(problem),
						}
					}
				}
			}
			literal => argv.push(Arg::Literal(literal.to_string())),
		}
	}

	if !problems.is_empty() {
		return Err(problems);
	}

	Ok(Plan {
		format: group.format.clone(),
		backend: Backend::Tool(tool),
		argv,
		tier: tier_of(registry, &group.format),
		engine,
	})
}

/// Fill one argument: `{value}` becomes the operation's parameter, and
/// `{engine.<name>}` becomes the candidate that resolved it.
fn splice(
	argument: &str,
	value: Option<&str>,
	tool: &Tool,
	presence: &dyn Presence,
) -> Result<(String, Option<(String, String)>), Error> {
	let mut text = argument.to_string();
	let mut resolved = None;

	if let Some(value) = value {
		text = text.replace("{value}", value);
	}

	while let Some(name) = engine_name(&text) {
		let Some(candidate) = presence.engine(tool, &name) else {
			return Err(Error::Diagnostic(Diagnostic::global(
				Code::ToolMissing,
				format!(
					"tool '{}' needs an engine for '{name}', and none of {} is installed",
					tool.name,
					tool.candidates(&name).join(", ")
				),
			)));
		};

		text = text.replace(&format!("{{engine.{name}}}"), &candidate);
		resolved = Some((name, candidate));
	}

	Ok((text, resolved))
}

/// The engine name in `{engine.<name>}`, if the text names one.
fn engine_name(text: &str) -> Option<String> {
	let start = text.find("{engine.")? + "{engine.".len();
	let end = text[start..].find('}')? + start;
	Some(text[start..end].to_string())
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeMap;

	use super::*;
	use crate::intent;
	use crate::resolve;

	fn config() -> Registry {
		Registry::load(
			"\
version = 'retype-config-1'

[backends]
prefer = ['magick', 'pandoc', 'ffmpeg']

[tools.magick]
command = 'magick'
layout = ['{input}', '{ops}', '{output}']

[tools.ffmpeg]
command = 'ffmpeg'
layout = ['-i', '{input}', '{ops}', '{output}']

[tools.pandoc]
command = 'pandoc'
layout = ['{input}', '{ops}', '-o', '{output}']
engines = { pdf = ['typst', 'weasyprint'] }

[formats.webp]
family = 'image'
lossy = true
alpha = true
tier = 'converted'

[formats.webp.argv.magick]
args = ['-define', 'webp:method=6']

[ops.quality]
slot = 5
param = { class = 'int', min = 1, max = 100 }
formats = ['webp']

[ops.quality.argv.magick]
args = ['-quality', '{value}']

[ops.quality.argv.ffmpeg]
args = ['-q:v', '{value}']

[ops.cap]
slot = 1
param = { class = 'dim', min = 1 }
formats = ['webp']

[ops.cap.argv.magick]
args = ['-resize', '{value}>']

[ops.pdf]
slot = 9
formats = ['webp']

[ops.pdf.argv.pandoc]
args = ['--pdf-engine', '{engine.pdf}']
",
		)
		.expect("the fixture loads")
	}

	/// A machine with a fixed set of tools and engines.
	struct Machine {
		installed: Vec<&'static str>,
		engines: BTreeMap<&'static str, &'static str>,
	}

	impl Machine {
		fn with(tools: &[&'static str]) -> Self {
			Self {
				installed: tools.to_vec(),
				engines: BTreeMap::new(),
			}
		}
	}

	impl Presence for Machine {
		fn is_available(&self, tool: &Tool) -> bool {
			self.installed.contains(&tool.name.as_str())
		}

		fn engine(&self, tool: &Tool, name: &str) -> Option<String> {
			self.engines
				.get(format!("{}.{name}", tool.name).as_str())
				.map(|found| found.to_string())
		}
	}

	fn plan_for(text: &str, machine: &Machine) -> Result<Vec<Plan<'static>>, Vec<Code>> {
		let registry = Box::leak(Box::new(config()));
		let intent = intent::parse(text).expect("the intent scans");
		let resolved = resolve::resolve(
			&intent,
			resolve::Source {
				format: "jpg",
				is_directory: false,
			},
			registry,
		)
		.expect("the intent resolves");

		compile(registry, machine, &resolved).map_err(|problems| problems.codes())
	}

	fn plan_one(text: &str, machine: &Machine) -> Plan<'static> {
		let mut plans = plan_for(text, machine).expect("a plan");
		assert_eq!(plans.len(), 1, "one output");
		plans.remove(0)
	}

	fn literals(plan: &Plan<'_>) -> Vec<String> {
		plan.argv
			.iter()
			.filter_map(|arg| match arg {
				Arg::Literal(text) => Some(text.clone()),
				_ => None,
			})
			.collect()
	}

	#[test]
	fn a_format_fragment_comes_before_the_operation_fragments() {
		let plan = plan_one("webp+quality85", &Machine::with(&["magick"]));

		assert_eq!(plan.backend.name(), "magick");
		assert_eq!(
			plan.argv,
			[
				Arg::Input,
				Arg::Literal("-define".to_string()),
				Arg::Literal("webp:method=6".to_string()),
				Arg::Literal("-quality".to_string()),
				Arg::Literal("85".to_string()),
				Arg::Output,
			]
		);
	}

	#[test]
	fn operations_are_spliced_in_slot_order() {
		let plan = plan_one("webp+quality85+cap640x480", &Machine::with(&["magick"]));

		assert_eq!(
			literals(&plan),
			["-define", "webp:method=6", "-resize", "640x480>", "-quality", "85"],
			"cap precedes quality whatever the written order"
		);
	}

	#[test]
	fn a_value_is_printed_canonically() {
		let plan = plan_one("webp+quality085", &Machine::with(&["magick"]));

		assert!(literals(&plan).contains(&"85".to_string()));
	}

	#[test]
	fn the_preference_list_decides_and_skips_what_is_absent() {
		let with_magick = plan_one("webp+quality85", &Machine::with(&["magick", "ffmpeg"]));
		assert_eq!(with_magick.backend.name(), "magick");

		let without = plan_one("webp+quality85", &Machine::with(&["ffmpeg"]));
		assert_eq!(
			without.backend.name(),
			"ffmpeg",
			"the next backend that carries the group wins"
		);
	}

	#[test]
	fn an_engine_is_resolved_and_recorded() {
		let machine = Machine {
			installed: vec!["pandoc"],
			engines: BTreeMap::from([("pandoc.pdf", "weasyprint")]),
		};

		let plan = plan_one("webp+pdf", &machine);

		assert!(literals(&plan).contains(&"weasyprint".to_string()));
		assert_eq!(plan.engine, Some(("pdf".to_string(), "weasyprint".to_string())));
	}

	#[test]
	fn a_missing_engine_is_a_missing_tool() {
		let problems = plan_for("webp+pdf", &Machine::with(&["pandoc"])).unwrap_err();

		assert_eq!(problems, [Code::ToolMissing]);
	}

	#[test]
	fn a_group_no_backend_carries_is_unsupported() {
		// `pdf` has a fragment for pandoc only, and pandoc does not carry `cap`.
		let machine = Machine::with(&[]);
		let problems = plan_for("webp+cap640x480+pdf", &machine).unwrap_err();

		assert_eq!(problems, [Code::Unsupported]);
	}

	#[test]
	fn an_operation_no_backend_declares_is_unsupported() {
		let registry = Registry::load(
			"\
version = 'retype-config-1'

[tools.magick]
command = 'magick'
layout = ['{input}', '{ops}', '{output}']

[formats.webp]
family = 'image'
lossy = true
alpha = true
tier = 'converted'

[ops.quality]
slot = 5
formats = ['webp']
",
		)
		.expect("the fixture loads");

		let intent = intent::parse("webp+quality").expect("scans");
		let resolved = resolve::resolve(
			&intent,
			resolve::Source {
				format: "jpg",
				is_directory: false,
			},
			&registry,
		)
		.expect("resolves");

		assert_eq!(
			compile(&registry, &Machine::with(&["magick"]), &resolved)
				.unwrap_err()
				.codes(),
			[Code::Unsupported]
		);
	}

	#[test]
	fn a_backend_that_could_carry_the_group_but_is_absent_is_a_missing_tool() {
		let problems = plan_for("webp+quality85", &Machine::with(&[])).unwrap_err();

		assert_eq!(problems, [Code::ToolMissing]);
	}

	#[test]
	fn every_output_gets_its_own_plan() {
		let plans = plan_for("webp+quality85,webp+cap640x480", &Machine::with(&["magick"])).expect("two plans");

		assert_eq!(plans.len(), 2);
		assert!(literals(&plans[0]).contains(&"85".to_string()));
		assert!(literals(&plans[1]).contains(&"640x480>".to_string()));
	}

	#[test]
	fn the_tier_comes_from_the_target_format() {
		let plan = plan_one("webp+quality85", &Machine::with(&["magick"]));

		assert_eq!(plan.tier, Tier::Converted);
	}

	#[test]
	fn the_shipped_table_plans_an_image_conversion_through_magick() {
		let registry = Registry::builtin().expect("the built-in table loads");
		let intent = intent::parse("jpg+quality80+cap1080+rotate90").expect("scans");
		let resolved = resolve::resolve(
			&intent,
			resolve::Source {
				format: "heic",
				is_directory: false,
			},
			&registry,
		)
		.expect("resolves");

		let plans = compile(&registry, &Machine::with(&["magick"]), &resolved).expect("plans");

		assert_eq!(plans[0].backend.name(), "magick");
		assert_eq!(
			literals(&plans[0]),
			[
				"-rotate",
				"90",
				"-filter",
				"Lanczos",
				"-resize",
				"1080x1080>",
				"-quality",
				"80"
			],
			"geometry runs in the order docs/operations.md 2 gives"
		);
	}

	#[test]
	fn an_operation_with_no_fragment_is_unsupported_even_though_it_is_declared() {
		let registry = Registry::builtin().expect("the built-in table loads");
		let intent = intent::parse("png+compress9").expect("scans");
		let resolved = resolve::resolve(
			&intent,
			resolve::Source {
				format: "heic",
				is_directory: false,
			},
			&registry,
		)
		.expect("the operation applies");

		assert_eq!(
			compile(&registry, &Machine::with(&["magick"]), &resolved)
				.unwrap_err()
				.codes(),
			[Code::Unsupported]
		);
	}

	#[test]
	fn argv_fills_the_paths_it_was_given() {
		let plan = plan_one("webp+quality85", &Machine::with(&["magick"]));
		let argv = plan.argv(Path::new("/in.jpg"), Path::new("/out.webp"));

		assert_eq!(argv.first().unwrap(), "/in.jpg");
		assert_eq!(argv.last().unwrap(), "/out.webp");
	}
}
