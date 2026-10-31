//! Grammar conformance, `docs/grammar.md` §14.
//!
//! The six properties are checked over a generated corpus rather than by hand:
//! a deterministic generator produces intents from the registry's own declarations,
//! so a format or operation added to the shipped table is covered without a new
//! test.
//!
//! The seed is fixed. A failing case has to be reproducible from the code alone, so
//! the corpus is a function of the registry and nothing else.

use std::collections::BTreeSet;

use retype_core::intent;
use retype_core::output;
use retype_core::print;
use retype_core::registry::{Class, Param, Registry, Scalar};
use retype_core::resolve::{self, Argument, Resolved, Source};

/// How many intents the valid corpus aims for, and how many random strings are
/// thrown at the parser. Both are enough to cover the matrix and fast enough to run
/// on every commit.
const WANTED: usize = 400;
const RANDOM: usize = 4_000;

/// xorshift64*, so a corpus is reproducible without a dependency and without a
/// platform-specific random source.
struct Rng(u64);

impl Rng {
	fn new(seed: u64) -> Self {
		Self(seed | 1)
	}

	fn next(&mut self) -> u64 {
		let mut state = self.0;
		state ^= state >> 12;
		state ^= state << 25;
		state ^= state >> 27;
		self.0 = state;
		state.wrapping_mul(0x2545_f491_4f6c_dd1d)
	}

	fn below(&mut self, bound: usize) -> usize {
		(self.next() % bound as u64) as usize
	}

	fn chance(&mut self, one_in: usize) -> bool {
		self.below(one_in) == 0
	}

	fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
		&items[self.below(items.len())]
	}
}

/// One conversion: what the file is now, and what the rename asked for.
#[derive(Debug, Clone)]
struct Job {
	source: String,
	intent: String,
}

fn sources(registry: &Registry) -> Vec<String> {
	let mut names: Vec<String> = registry
		.formats()
		.filter(|(_, format)| format.direction.can_decode())
		.map(|(name, _)| name.clone())
		.collect();
	names.sort();
	names
}

fn targets(registry: &Registry) -> Vec<String> {
	let mut names: Vec<String> = registry
		.formats()
		.filter(|(_, format)| format.direction.can_encode())
		.map(|(name, _)| name.clone())
		.collect();
	names.sort();
	names
}

/// The operations a target format can carry, which is what makes a generated token
/// plausible rather than a guess.
fn ops_for<'a>(registry: &'a Registry, format: &str) -> Vec<&'a retype_core::registry::Op> {
	let mut ops: Vec<&retype_core::registry::Op> = registry
		.ops()
		.map(|(_, op)| op)
		.filter(|op| !op.argv.is_empty() && op.applies_to(format))
		.collect();
	ops.sort_by(|left, right| left.name.cmp(&right.name));
	ops
}

/// A value the operation's own bounds accept.
fn value_for(param: &Param, rng: &mut Rng) -> String {
	let within = |min: Option<u64>, max: Option<u64>, rng: &mut Rng| {
		let low = min.unwrap_or(1);
		let high = max.unwrap_or(low + 100).max(low);
		low + rng.below((high - low + 1) as usize) as u64
	};

	match param.class {
		Class::Int => within(param.min, param.max, rng).to_string(),
		Class::Dim => format!("{}", within(param.min, param.max, rng)),
		Class::Ratio => match rng.chance(2) {
			true => format!("{}%", within(param.min, param.max, rng).min(100)),
			false => "16:9".to_string(),
		},
		Class::Size => format!("{}kb", within(param.min, param.max, rng)),
		Class::Time => format!("{}s", within(param.min, param.max, rng).max(1)),
		Class::Range => match param.scalar {
			Some(Scalar::Time) => "1s-2s".to_string(),
			_ => {
				let low = within(param.min, param.max, rng).max(1);
				format!("{low}-{}", low + 1)
			}
		},
		Class::Enum => param.values.first().cloned().unwrap_or_else(|| "x".to_string()),
		Class::Code => "libx264".to_string(),
		Class::Text => "tag".to_string(),
	}
}

/// One intent built from the registry, which may or may not resolve: the corpus is
/// what does, and the generator is deliberately allowed to be wrong.
fn generate(registry: &Registry, rng: &mut Rng) -> Job {
	let sources = sources(registry);
	let targets = targets(registry);

	let source = rng.pick(&sources).clone();
	let mut groups: Vec<String> = Vec::new();
	let mut used: BTreeSet<String> = BTreeSet::new();

	for _ in 0..1 + rng.below(3) {
		let format = rng.pick(&targets).clone();
		let mut group = format.clone();
		let mut exclusive: BTreeSet<String> = BTreeSet::new();

		for _ in 0..rng.below(3) {
			let ops = ops_for(registry, &format);
			if ops.is_empty() {
				break;
			}

			let op = rng.pick(&ops);

			// Duplicates and exclusive groups are refusals, and a generator that
			// produced them on purpose would be testing the guard rather than the round
			// trip.
			if used.contains(&op.key) || op.exclusive.iter().any(|group| exclusive.contains(group)) {
				continue;
			}
			used.insert(op.key.clone());
			exclusive.extend(op.exclusive.iter().cloned());

			group.push('+');
			group.push_str(match rng.chance(3) {
				true => &op.name,
				false => op.aliases.first().unwrap_or(&op.name),
			});

			if let Some(param) = &op.param {
				group.push_str(&value_for(param, rng));
			}
		}

		groups.push(group);
	}

	match (rng.chance(4), rng.chance(4)) {
		(true, _) => groups[0].push_str("+ask"),
		(_, true) => groups[0].push_str("+now"),
		_ => {}
	}

	Job {
		source,
		intent: groups.join(","),
	}
}

fn parse(source: &str, intent: &str) -> Result<Resolved, retype_core::error::Diagnoses> {
	let registry = registry();
	let parsed = intent::parse(intent)?;
	resolve::resolve(
		&parsed,
		Source {
			format: source,
			is_directory: false,
		},
		&registry,
	)
}

fn registry() -> Registry {
	Registry::builtin().expect("the shipped registry loads")
}

/// What a resolved intent means, with spans and written spellings dropped: those
/// are positions in one string, and a round trip is about meaning.
type Shape = (String, (bool, bool), Vec<(String, bool, Vec<(String, String)>)>);

fn shape(resolved: &Resolved) -> Shape {
	let value = |argument: &Argument| print::value(&argument.parsed);

	(
		resolved.origin.clone(),
		(resolved.meta.ask, resolved.meta.now),
		resolved
			.groups
			.iter()
			.map(|group| {
				(
					group.format.clone(),
					group.keep_source,
					group
						.pipeline()
						.iter()
						.map(|op| (op.key.clone(), op.value.as_ref().map(value).unwrap_or_default()))
						.collect(),
				)
			})
			.collect(),
	)
}

fn corpus(registry: &Registry) -> Vec<(Job, Resolved)> {
	let mut rng = Rng::new(0x9e37_79b9_7f4a_7c15);
	let mut valid = Vec::new();
	let mut attempts = 0;

	while valid.len() < WANTED && attempts < WANTED * 20 {
		attempts += 1;
		let job = generate(registry, &mut rng);

		if let Ok(resolved) = parse(&job.source, &job.intent) {
			valid.push((job, resolved));
		}
	}

	// A generator that resolves almost nothing would make every property below
	// vacuous, so the acceptance rate is part of the test.
	assert_eq!(
		valid.len(),
		WANTED,
		"the generator produced only {} valid intents",
		valid.len()
	);

	valid
}

#[test]
fn t1_a_canonical_intent_parses_back_to_the_same_job() {
	let registry = registry();

	for (job, resolved) in corpus(&registry) {
		let canonical = print::intent(&resolved, &registry);
		let again = parse(&job.source, &canonical).unwrap_or_else(|problems| {
			panic!(
				"{:?} printed as '{canonical}', which does not parse: {problems}",
				job.intent
			)
		});

		assert_eq!(
			shape(&again),
			shape(&resolved),
			"'{canonical}' means something else than {:?} for {}",
			job.intent,
			job.source
		);
	}
}

#[test]
fn t2_printing_a_parsed_intent_is_idempotent() {
	let registry = registry();

	for (job, resolved) in corpus(&registry) {
		let once = print::intent(&resolved, &registry);
		let twice = print::intent(
			&parse(&job.source, &once).expect("the canonical form parses"),
			&registry,
		);

		assert_eq!(once, twice, "{:?} did not settle after one printing", job.intent);
	}
}

#[test]
fn t3_every_string_either_parses_or_carries_a_diagnosis() {
	let registry = registry();
	let alphabet: Vec<char> = "abcxyz019.,+!_=:%- /\\?#*;@AZé\t".chars().collect();
	let mut rng = Rng::new(0xdead_beef_f00d_1234);

	for _ in 0..RANDOM {
		let length = rng.below(24);
		let text: String = (0..length).map(|_| *rng.pick(&alphabet)).collect();

		match intent::parse(&text) {
			Ok(parsed) => {
				// A parsed intent is a whole intent, and asking what it means either
				// answers or diagnoses: neither silently succeeds with nothing to do.
				let outcome = resolve::resolve(
					&parsed,
					Source {
						format: "jpg",
						is_directory: false,
					},
					&registry,
				);

				match outcome {
					Ok(resolved) => assert!(!resolved.groups.is_empty(), "{text:?} resolved to no groups"),
					Err(problems) => assert!(!problems.is_empty(), "{text:?} failed to resolve and said nothing"),
				}
			}
			Err(problems) => assert!(!problems.is_empty(), "{text:?} failed to parse and said nothing"),
		}
	}
}

#[test]
fn t4_a_derived_name_is_never_an_intent() {
	let registry = registry();

	for (job, resolved) in corpus(&registry) {
		let stem = "photo";
		let derived = output::derive(stem, &job.source, &resolved.groups);

		for entry in derived.iter().filter(|entry| !entry.keep_source) {
			let name = entry.path.display().to_string();

			assert!(
				output::is_derivable(&name),
				"{name} carries intent syntax, so producing it would ask for another conversion"
			);

			// A produced name reads as a plain rename, or as nothing: it never looks
			// like an intent that is not anchored on the stem it was produced from,
			// which is the case the watcher cannot tell apart from a mistake.
			assert!(
				intent::anchor(&name, stem).is_ok(),
				"{name} looks like an intent that is not anchored on its stem"
			);
		}
	}
}

#[test]
fn t5_every_declared_name_resolves_and_a_duplicate_refuses_to_load() {
	let registry = registry();

	for (name, _) in registry.formats() {
		assert!(
			registry.canonical_format(name).is_some(),
			"format {name} does not resolve to itself"
		);
	}

	for (name, op) in registry.ops() {
		let found = registry.resolve(name).expect("a declared operation resolves");
		assert_eq!(found.op().map(|op| op.key.as_str()), Some(op.key.as_str()));

		for alias in &op.aliases {
			assert_eq!(
				registry
					.resolve(alias)
					.and_then(|found| found.op())
					.map(|op| op.key.clone()),
				Some(op.key.clone()),
				"alias {alias} of {name} does not resolve to it"
			);
		}
	}

	for preset in registry.presets() {
		assert!(
			registry.resolve(&preset.name).is_some(),
			"preset {} does not resolve",
			preset.name
		);
	}

	// R1: two declarations cannot share a name.
	let duplicate = Registry::load(
		"\
version = 'retype-config-1'

[formats.jpg]
family = 'image'
lossy = true
alpha = false
tier = 'converted'

[formats.png]
family = 'image'
lossy = false
alpha = true
tier = 'converted'

[ops.quality]
slot = 50
formats = ['jpg']

[ops.size]
slot = 51
aliases = ['quality']
formats = ['jpg']
",
	);

	assert!(
		duplicate.is_err(),
		"two operations sharing an alias loaded without a word"
	);
}

#[test]
fn t6_a_diagnosis_points_at_the_offending_part_and_not_at_the_whole_string() {
	let registry = registry();
	let cases = [
		"jpg++quality80",
		"jpg,",
		"jpg+zzz",
		"webp+quality",
		"webp+quality1e2",
		"webp+quality101",
		"png+quality80",
		"jpg+cap",
		"jpg+quality80,webp+quality80+quality90",
		"folder",
	];

	for text in cases {
		let problems = match intent::parse(text) {
			Ok(parsed) => resolve::resolve(
				&parsed,
				Source {
					format: "png",
					is_directory: false,
				},
				&registry,
			)
			.err()
			.unwrap_or_else(|| panic!("{text:?} was accepted")),
			Err(problems) => problems,
		};

		assert!(!problems.is_empty(), "{text:?} failed and said nothing");

		let mut inside = false;

		for problem in problems.iter() {
			let Some(span) = problem.span() else {
				continue;
			};

			assert!(span.start <= span.end, "{text:?}: {span:?} is backwards");
			assert!(span.end <= text.len(), "{text:?}: {span:?} runs past the string");
			assert!(
				text.get(span.start..span.end).is_some(),
				"{text:?}: {span:?} is not on a character boundary"
			);

			if span.start > 0 || span.end < text.len() {
				inside = true;
			}
		}

		// A single-token string can have only itself to point at; anything with a
		// separator in it has a part to name, and pointing at all of it tells the user
		// nothing about which part is wrong.
		if text.contains(',') || text.contains('+') {
			assert!(
				inside,
				"{text:?} was diagnosed only by spans covering the whole string: {problems}"
			);
		}
	}

	// The random corpus is checked for bounds only: whether a span names a
	// sub-part is a property of the diagnostic, and the cases above pin it.
	let alphabet: Vec<char> = "abcxyz019.,+!_=:%- /".chars().collect();
	let mut rng = Rng::new(0x0123_4567_89ab_cdef);

	for _ in 0..RANDOM {
		let length = rng.below(24);
		let text: String = (0..length).map(|_| *rng.pick(&alphabet)).collect();

		let Ok(parsed) = intent::parse(&text) else {
			continue;
		};

		let problems = match resolve::resolve(
			&parsed,
			Source {
				format: "jpg",
				is_directory: false,
			},
			&registry,
		) {
			Ok(_) => continue,
			Err(problems) => problems,
		};

		for problem in problems.iter() {
			if let Some(span) = problem.span() {
				assert!(span.start <= span.end && span.end <= text.len(), "{text:?}: {span:?}");
				assert!(text.get(span.start..span.end).is_some(), "{text:?}: {span:?}");
			}
		}
	}
}
