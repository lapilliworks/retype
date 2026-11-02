//! Journal predicates must use the latest top-level snapshot, not nested output states.

pub fn latest(contents: &str) -> Vec<serde_json::Value> {
	let mut jobs = std::collections::BTreeMap::new();
	for line in contents.lines() {
		let Ok(job) = serde_json::from_str::<serde_json::Value>(line) else {
			continue;
		};
		let Some(id) = job["id"].as_str() else {
			continue;
		};
		jobs.insert(id.to_string(), job);
	}
	jobs.into_values().collect()
}
