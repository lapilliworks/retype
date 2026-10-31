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
use crate::registry::{Fragment, Native, Registry};
use crate::resolve::{self, Resolved};
use crate::tools::Tool;

/// What the planner may ask about the machine.
pub trait Presence {
	/// The executable that will run for this tool, when it is usable. It is the
	/// preferred command or the first alias that answered.
	fn command(&self, tool: &Tool) -> Option<String>;

	/// The first candidate of a named engine that is on the path.
	fn engine(&self, tool: &Tool, name: &str) -> Option<String>;
}

/// A machine with nothing installed: every backend is unusable, which is what a
/// test wants when it asks what the declarations could do.
pub struct Nothing;

impl Presence for Nothing {
	fn command(&self, _tool: &Tool) -> Option<String> {
		None
	}

	fn engine(&self, _tool: &Tool, _name: &str) -> Option<String> {
		None
	}
}

impl Presence for crate::tools::Machine {
	fn command(&self, tool: &Tool) -> Option<String> {
		crate::tools::probe(tool).executable().map(str::to_string)
	}

	fn engine(&self, tool: &Tool, name: &str) -> Option<String> {
		crate::tools::resolve_engine(tool, name).map(str::to_string)
	}
}

/// Where a plan will run.
///
/// A codec is always an external process: parity with libjpeg-turbo and libaom is
/// not a thing to rewrite. A container that only moves bytes is a different
/// problem, and putting it here rather than behind a fake tool keeps the planner's
/// shape instead of teaching it a command name that resolves to nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend<'a> {
	Tool(&'a Tool),
	/// A container this build repacks: the codec it reads and the codec it writes.
	Native {
		from: Native,
		to: Native,
	},
}

impl Backend<'_> {
	/// The name recorded with a job and printed by `explain`.
	pub fn name(&self) -> &str {
		match self {
			Self::Tool(tool) => &tool.name,
			// The target codec names the conversion: the source is in the record's
			// `source` field, and `zip` to `tar.gz` is a `tar.gz` job.
			Self::Native { to, .. } => to.name(),
		}
	}

	/// Whether this backend is this build rather than a tool on the machine.
	pub fn is_native(&self) -> bool {
		matches!(self, Self::Native { .. })
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
	/// The group this output belongs to, which is what a caller needs to find the
	/// name it was given: a kept source has a group and no plan.
	pub group: usize,
	/// The format this output carries.
	pub format: String,
	pub backend: Backend<'a>,
	/// The executable that resolved for the backend at plan time.
	pub command: String,
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

/// Compile every output a resolved intent produces, in written order.
///
/// A group that keeps the source produces no plan. Re-encoding a file to the format
/// it already has would be a conversion nobody asked for, and for a directory there
/// is nothing to encode at all.
pub fn compile<'a>(
	registry: &'a Registry,
	presence: &dyn Presence,
	resolved: &Resolved,
) -> Result<Vec<Plan<'a>>, Diagnoses> {
	let mut plans = Vec::new();
	let mut problems = Vec::new();

	for (index, group) in resolved.groups.iter().enumerate() {
		if group.keep_source {
			continue;
		}

		match plan_group(registry, presence, index, group, &resolved.origin) {
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
	index: usize,
	group: &resolve::Group,
	origin: &str,
) -> Result<Plan<'a>, Vec<Error>> {
	// The other direction of the same rule: a format with no writer cannot be a
	// target, whatever the backend claims.
	if let Some(format) = registry.format(&group.format) {
		if !format.direction.can_encode() {
			return Err(vec![Error::Diagnostic(Diagnostic::global(
				Code::Unsupported,
				format!(
					"{} is read but not written, so it cannot be the target of a conversion",
					group.format
				),
			))]);
		}
	}

	let pipeline = group.pipeline();

	// A format this build handles itself needs no tool and no argv. It needs
	// another archive to read, because a container is repacked rather than made from
	// loose files: a directory is not a conversion.
	if let Some(to) = registry.format(&group.format).and_then(|format| format.native) {
		if !pipeline.is_empty() {
			return Err(vec![Error::Diagnostic(Diagnostic::global(
				Code::Unsupported,
				format!("no operation applies to a {} archive", group.format),
			))]);
		}

		let Some(from) = registry.format(origin).and_then(|format| format.native) else {
			return Err(vec![Error::Diagnostic(Diagnostic::global(
				Code::Unsupported,
				format!(
					"{} is not an archive, so it cannot be repacked into {}: this build converts archives into archives",
					origin, group.format
				),
			))]);
		};

		return Ok(Plan {
			group: index,
			format: group.format.clone(),
			backend: Backend::Native { from, to },
			// Nothing to run, which is what `explain` prints and what the recorder
			// stores as an empty command rather than as a tool that was not found.
			command: String::new(),
			argv: Vec::new(),
			tier: tier_of(registry, &group.format),
			engine: None,
		});
	}

	let mut unavailable: Vec<&Tool> = Vec::new();

	for candidate in registry.backends() {
		let Some(tool) = registry.tool(candidate) else {
			continue;
		};

		let carries = pipeline.iter().all(|op| {
			registry
				.op(&op.key)
				.is_some_and(|declared| declared.fragment(&tool.name).is_some())
		});

		if !carries {
			continue;
		}

		// A tool that states which families it handles is not eligible for a
		// conversion that starts or ends outside them: ImageMagick opening a video
		// container is not a plan, it is a failure with a delay, `docs/config.md` §3.
		if !tool.families.is_empty() {
			let from = family_of(registry, origin);
			let to = family_of(registry, &group.format);

			if !tool.families.contains(&from) || !tool.families.contains(&to) {
				continue;
			}
		}

		let Some(command) = presence.command(tool) else {
			unavailable.push(tool);
			continue;
		};

		return assemble(registry, presence, index, tool, command, group, pipeline);
	}

	Err(vec![unsatisfiable(registry, group, pipeline, &unavailable)])
}

/// The family a format belongs to, or the empty string for a format the registry
/// does not declare, which validation has already refused by then.
fn family_of(registry: &Registry, format: &str) -> String {
	registry
		.format(format)
		.map(|format| format.family.clone())
		.unwrap_or_default()
}

/// Why no backend could carry this group, said in terms the user can act on.
fn unsatisfiable(
	registry: &Registry,
	group: &resolve::Group,
	pipeline: &[resolve::Op],
	unavailable: &[&Tool],
) -> Error {
	let format = &group.format;

	// `docs/tools.md` §1: no tool could ever provide it, as distinct from a tool
	// that could and is not installed.
	if let Some(op) = pipeline
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
#[allow(clippy::too_many_arguments)]
fn assemble<'a>(
	registry: &'a Registry,
	presence: &dyn Presence,
	index: usize,
	tool: &'a Tool,
	command: String,
	group: &resolve::Group,
	pipeline: &[resolve::Op],
) -> Result<Plan<'a>, Vec<Error>> {
	let mut pieces: Vec<(&Fragment, Option<&resolve::Op>)> = Vec::new();

	if let Some(fragment) = registry
		.format(&group.format)
		.and_then(|format| format.fragment(&tool.name))
	{
		pieces.push((fragment, None));
	}

	for op in pipeline {
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
		group: index,
		format: group.format.clone(),
		backend: Backend::Tool(tool),
		command,
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
		fn command(&self, tool: &Tool) -> Option<String> {
			self.installed
				.contains(&tool.name.as_str())
				.then(|| tool.command.clone())
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
	fn a_tool_that_names_families_is_not_chosen_outside_them() {
		let registry = Registry::load(
			"\
version = 'retype-config-1'

[backends]
prefer = ['painter']

[tools.painter]
command = 'true'
families = ['image']
layout = ['{input}', '{ops}', '{output}']

[formats.png]
family = 'image'
lossy = false
alpha = true
tier = 'exact'

[formats.webp]
family = 'image'
lossy = true
alpha = true
tier = 'converted'

[formats.mp4]
family = 'video'
lossy = true
alpha = false
tier = 'converted'
",
		)
		.expect("a configuration");

		let codes = |from: &str, written: &str| {
			let intent = intent::parse(written).expect("the intent scans");
			let resolved = resolve::resolve(
				&intent,
				resolve::Source {
					format: from,
					is_directory: false,
				},
				&registry,
			)
			.expect("the intent resolves");

			compile(&registry, &crate::tools::Machine, &resolved).map_err(|problems| problems.codes())
		};

		// Inside the claim, the only backend carries it.
		assert!(codes("png", "webp").is_ok());

		// Across families it is not eligible, so there is no backend at all, which is
		// the difference between a matrix that offers a failure and one that says no.
		assert_eq!(codes("png", "mp4"), Err(vec![Code::Unsupported]));
		assert_eq!(codes("mp4", "webp"), Err(vec![Code::Unsupported]));
	}

	#[test]
	fn operations_are_spliced_in_the_order_written() {
		let plan = plan_one("webp+quality85+cap640x480", &Machine::with(&["magick"]));

		assert_eq!(
			literals(&plan),
			["-define", "webp:method=6", "-quality", "85", "-resize", "640x480>"],
			"the fragments follow the string, not a slot table"
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
	fn the_plan_records_the_executable_that_resolved() {
		struct Alias;

		impl Presence for Alias {
			fn command(&self, tool: &Tool) -> Option<String> {
				Some(format!("{}-alt", tool.name))
			}

			fn engine(&self, _tool: &Tool, _name: &str) -> Option<String> {
				None
			}
		}

		let registry = config();
		let intent = intent::parse("webp+quality85").expect("the intent scans");
		let resolved = resolve::resolve(
			&intent,
			resolve::Source {
				format: "jpg",
				is_directory: false,
			},
			&registry,
		)
		.expect("the intent resolves");

		let plans = compile(&registry, &Alias, &resolved).expect("plans");

		assert_eq!(plans[0].backend.name(), "magick");
		assert_eq!(plans[0].command, "magick-alt");
	}

	#[test]
	fn a_target_that_can_only_be_read_is_unsupported() {
		let registry = Registry::load(
			"\
version = 'retype-config-1'

[backends]
prefer = ['magick']

[tools.magick]
command = 'magick'
layout = ['{input}', '{ops}', '{output}']

[formats.raw]
family = 'image'
direction = 'decode'
lossy = false
alpha = false
tier = 'exact'
",
		)
		.expect("the fixture loads");

		let intent = intent::parse("raw").expect("scans");
		let resolved = resolve::resolve(
			&intent,
			resolve::Source {
				format: "jpg",
				is_directory: false,
			},
			&registry,
		)
		.expect("resolves: only the target is refused");

		let codes = compile(&registry, &Machine::with(&["magick"]), &resolved)
			.unwrap_err()
			.codes();

		assert_eq!(codes, vec![Code::Unsupported]);
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
		let intent = intent::parse("jpg+rotate90+cap1080+quality80").expect("scans");
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
