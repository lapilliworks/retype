//! Derived output names, `docs/grammar.md` §11.
//!
//! A rename carries one intent string, which may name several outputs. This
//! module turns each group into the path it produces, and settles collisions
//! within the job so that no output overwrites another or the kept source.

use std::path::PathBuf;

use crate::resolve::Group;

/// One file the job will produce, or the source it keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derived {
	/// Index of the group that produced it, which is what a job records.
	pub group: usize,
	pub path: PathBuf,
	pub format: String,
	/// No file is produced: the source already has this format and the group
	/// asked for nothing.
	pub keep_source: bool,
}

/// Derive one path per group from the source stem, `docs/grammar.md` §11.
///
/// The stem is passed in rather than taken from a path, because the source stem
/// is whatever remains when the source format is removed, and only the registry
/// knows that `archive.tar.gz` carries one format rather than two.
pub fn derive(stem: &str, source_format: &str, groups: &[Group]) -> Vec<Derived> {
	let kept = format!("{stem}.{source_format}");
	let mut taken: Vec<String> = Vec::new();
	let mut derived = Vec::with_capacity(groups.len());

	for (index, group) in groups.iter().enumerate() {
		if group.keep_source {
			derived.push(Derived {
				group: index,
				path: PathBuf::from(&kept),
				format: group.format.clone(),
				keep_source: true,
			});
			continue;
		}

		let base = match group.ordered().is_empty() {
			true => stem.to_string(),
			false => {
				let mut base = stem.to_string();
				for op in group.ordered() {
					base.push('-');
					base.push_str(&token_text(op));
				}
				base
			}
		};

		let mut attempt = 1;
		let path = loop {
			let candidate = match attempt {
				1 => format!("{base}.{}", group.format),
				number => format!("{base}-{number}.{}", group.format),
			};

 //: unique within the job, and never the kept source.
			if !taken.contains(&candidate) && candidate != kept {
				break candidate;
			}
			attempt += 1;
		};

		taken.push(path.clone());
		derived.push(Derived {
			group: index,
			path: PathBuf::from(path),
			format: group.format.clone(),
			keep_source: false,
		});
	}

	derived
}

/// The canonical spelling of one token inside a derived name: the primary name
/// and its value, with a `.` in the value written as `_` so that the name has
/// one extension.
pub fn token_text(op: &crate::resolve::Op) -> String {
	let mut text = op.name.clone();

	if let Some(value) = &op.value {
		for character in value.written.text.chars() {
			text.push(if character == '.' { '_' } else { character });
		}
	}

	text
}

/// Everything a derived name must never contain, `docs/grammar.md`.
pub fn is_derivable(name: &str) -> bool {
	!name.contains('+') && !name.contains(',') && !name.ends_with('!')
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::intent::{self, Raw};
	use crate::registry::Registry;
	use crate::resolve::{self, Op, Source};
	use crate::resolve::{Argument, Resolved};

	fn resolved(text: &str, source_format: &'static str) -> Resolved {
		let registry = Registry::builtin().expect("built-in registry");
		let intent = intent::parse(text).expect("the intent should scan");

		resolve::resolve(
			&intent,
			Source {
				format: source_format,
				is_directory: false,
			},
			&registry,
		)
		.expect("the intent should resolve")
	}

	fn names(stem: &str, source_format: &str, text: &str) -> Vec<String> {
		let resolved = resolved(text, Box::leak(source_format.to_string().into_boxed_str()));
		derive(stem, source_format, &resolved.groups)
			.into_iter()
			.map(|derived| derived.path.display().to_string())
			.collect()
	}

	#[test]
	fn one_group_derives_the_plain_name() {
		assert_eq!(names("photo", "heic", "jpg"), vec!["photo.jpg"]);
	}

	#[test]
	fn a_compound_target_format_keeps_both_parts() {
		let group = Group {
			format: "tar.gz".to_string(),
			keep_source: false,
			ops: Vec::new(),
		};

		let derived = derive("photo", "heic", &[group]);

		assert_eq!(derived[0].path, PathBuf::from("photo.tar.gz"));
	}

	#[test]
	fn tokens_appear_in_canonical_order_with_their_values() {
		assert_eq!(
			names("photo", "heic", "webp+nometa+q80+cap1000"),
			vec!["photo-cap1000-quality80-nometa.webp"]
		);
	}

	#[test]
	fn a_kept_source_produces_no_second_name() {
		let derived = derive("photo", "jpg", &resolved("jpg,webp+q60", "jpg").groups);

		assert_eq!(derived.len(), 2);
		assert!(derived[0].keep_source);
		assert_eq!(derived[0].path, PathBuf::from("photo.jpg"));
		assert_eq!(derived[1].path, PathBuf::from("photo-quality60.webp"));
	}

	#[test]
	fn two_groups_with_the_same_tokens_get_numbered() {
		assert_eq!(
			names("photo", "heic", "webp+q60,webp+q60"),
			vec!["photo-quality60.webp", "photo-quality60-2.webp"]
		);
	}

	#[test]
	fn the_two_quality_variants_of_one_source_coexist() {
		assert_eq!(
			names("photo", "heic", "jpg+q60,jpg+q90"),
			vec!["photo-quality60.jpg", "photo-quality90.jpg"]
		);
	}

	#[test]
	fn a_dot_in_a_value_becomes_an_underscore() {
		let group = Group {
			format: "jpg".to_string(),
			keep_source: false,
			ops: vec![Op {
				key: "thumb".to_string(),
				name: "thumb".to_string(),
				slot: 60,
				value: Some(Argument {
					written: Raw {
						text: "1.5x".to_string(),
						span: crate::error::Span::new(0, 4),
					},
					parsed: crate::value::Value::Text("1.5x".to_string()),
				}),
				span: crate::error::Span::new(0, 4),
			}],
		};

		let derived = derive("photo", "heic", std::slice::from_ref(&group));

		assert_eq!(derived[0].path, PathBuf::from("photo-thumb1_5x.jpg"));
	}

	#[test]
	fn a_derived_name_never_carries_intent_syntax() {
		let derived = derive("photo", "heic", &resolved("jpg+q60,png,webp+cap100", "heic").groups);

		for entry in derived {
			let name = entry.path.display().to_string();
			assert!(is_derivable(&name), "{name} could be read as an intent");
		}
	}

	#[test]
	fn every_output_maps_back_to_its_group() {
		let derived = derive("photo", "heic", &resolved("jpg,png,webp", "heic").groups);
		let groups: Vec<usize> = derived.iter().map(|entry| entry.group).collect();

		assert_eq!(groups, vec![0, 1, 2]);
	}
}
