//! Measures what the filesystem actually reports for a rename, and what each
//! correlation strategy concludes from it.
//!
//! Not a test: a probe. Its output becomes `docs/events.md`. It answers two
//! questions per platform: which rename events arrive at all, and whether
//! identity correlation recovers the cases where the pair of names does not.
//!
//! Each scenario is split into a setup and an action, because a rename of a
//! file that was created in the same event batch would say nothing about how a
//! real user's rename is seen: their file existed long before.
//!
//! Usage: `cargo run -p retype-watch --example probe_events`

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use retype_core::correlate::{Correlator, RenameFact};
use retype_core::registry::Registry;
use retype_core::resolve::intent_of;
use retype_watch::{Window, collect, feed_identity, feed_names, watch};

type Step = Box<dyn Fn(&Path) -> Result<(), Box<dyn Error>>>;

struct Scenario {
	name: &'static str,
	setup: Step,
	act: Step,
}

fn scenarios() -> Vec<Scenario> {
	fn create(name: &'static str) -> Step {
		Box::new(move |root: &Path| Ok(fs::write(root.join(name), b"source bytes")?))
	}

	fn rename(from: &'static str, to: &'static str) -> Step {
		Box::new(move |root: &Path| Ok(fs::rename(root.join(from), root.join(to))?))
	}

	vec![
		Scenario {
			name: "rename in place",
			setup: create("a.heic"),
			act: rename("a.heic", "a.jpg"),
		},
		Scenario {
			name: "rename case only",
			setup: create("b.heic"),
			act: rename("b.heic", "b.HEIC"),
		},
		Scenario {
			name: "rename stem and extension",
			setup: create("c.heic"),
			act: rename("c.heic", "c-renamed.jpg"),
		},
		Scenario {
			name: "rename across directories",
			setup: create("d.heic"),
			act: rename("d.heic", "sub/d.jpg"),
		},
		Scenario {
			name: "two renames in one window",
			setup: create("e.heic"),
			act: Box::new(|root: &Path| {
				fs::rename(root.join("e.heic"), root.join("e.jpg"))?;
				fs::rename(root.join("e.jpg"), root.join("e.webp"))?;
				Ok(())
			}),
		},
		Scenario {
			name: "rename over an existing file",
			setup: Box::new(|root: &Path| {
				fs::write(root.join("f.heic"), b"source bytes")?;
				fs::write(root.join("f.jpg"), b"existing target")?;
				Ok(())
			}),
			act: rename("f.heic", "f.jpg"),
		},
		Scenario {
			name: "twenty renames in one gesture",
			setup: Box::new(|root: &Path| {
				for index in 0..20 {
					fs::write(root.join(format!("g{index}.heic")), b"source bytes")?;
				}
				Ok(())
			}),
			act: Box::new(|root: &Path| {
				for index in 0..20 {
					fs::rename(root.join(format!("g{index}.heic")), root.join(format!("g{index}.jpg")))?;
				}
				Ok(())
			}),
		},
		Scenario {
			name: "rename then write",
			setup: create("h.heic"),
			act: Box::new(|root: &Path| {
				fs::rename(root.join("h.heic"), root.join("h.jpg"))?;
				fs::write(root.join("h.jpg"), b"rewritten by an importer")?;
				Ok(())
			}),
		},
		Scenario {
			name: "rename then delete",
			setup: create("i.heic"),
			act: Box::new(|root: &Path| {
				fs::rename(root.join("i.heic"), root.join("i.jpg"))?;
				fs::remove_file(root.join("i.jpg"))?;
				Ok(())
			}),
		},
	]
}

fn main() -> Result<(), Box<dyn Error>> {
	let dir = tempfile::tempdir()?;
	// Canonicalised because macOS resolves `/var` to `/private/var`, and the
	// event stream reports resolved paths. Without it every printed path is
	// absolute and the transcript is unreadable.
	let root = dir.path().canonicalize()?;
	fs::create_dir(root.join("sub"))?;

	let (_watcher, receiver) = watch(&root)?;
	let window = Window::default();
	let registry = Registry::builtin().expect("the built-in registry");

	let mut names = Correlator::new(4096);
	let mut identity = Correlator::new(4096);

	println!("# filesystem events");
	println!();
	println!("platform: {}", std::env::consts::OS);
	println!();

	for scenario in scenarios() {
		// Setup is fed silently, so the action starts from a primed correlator,
		// which is what the daemon has after a file has existed for a while.
		(scenario.setup)(&root)?;
		let setup = collect(&receiver, window);
		feed_names(&mut names, &setup);
		feed_identity(&mut identity, &setup);

		println!("## {}", scenario.name);
		(scenario.act)(&root)?;
		let batch = collect(&receiver, window);

		for event in &batch.events {
			println!("raw: {:?} {}", event.kind, join(&root, &event.paths));
		}
		for error in &batch.errors {
			println!("raw: error: {error}");
		}

		report(&root, &registry, "pair", &feed_names(&mut names, &batch));
		report(&root, &registry, "identity", &feed_identity(&mut identity, &batch));
		println!();
	}

	println!("## notes");
	println!();
	if cfg!(windows) {
		println!(
			"- file identity is not exposed by `std` on Windows, so this probe cannot test the identity strategy there"
		);
	}
	Ok(())
}

fn report(root: &Path, registry: &Registry, strategy: &str, facts: &[RenameFact]) {
	if facts.is_empty() {
		println!("{strategy}: no rename facts");
		return;
	}
	for fact in facts {
		let source = relative(root, &fact.source);
		let target = relative(root, &fact.target);
		let intent = match intent_of(registry, &source, &target) {
			Ok(Some(intent)) => intent.text,
			Ok(None) => "none".to_string(),
			Err(diagnoses) => format!("{diagnoses}"),
		};

		println!("{strategy}: {source} -> {target} intent={intent}");
	}
}

fn join(root: &Path, paths: &[PathBuf]) -> String {
	paths
		.iter()
		.map(|path| relative(root, path))
		.collect::<Vec<_>>()
		.join(" -> ")
}

fn relative(root: &Path, path: &Path) -> String {
	path.strip_prefix(root).unwrap_or(path).display().to_string()
}
