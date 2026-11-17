//! Test waits must not mistake old job states or nested output states for completion.

#[path = "support/journal.rs"]
mod journal;

#[test]
fn waits_read_the_latest_top_level_state_of_each_job() {
	const RECORDS: &str = concat!(
		"{\"id\":\"first\",\"state\":\"partial\"}\n",
		"{\"id\":\"second\",\"state\":\"running\",\"outputs\":[{\"state\":\"done\"}]}\n",
		"{\"id\":\"first\",\"state\":\"running\",\"outputs\":[{\"state\":\"done\"}]}\n",
	);
	let jobs = journal::latest(RECORDS);
	assert_eq!(jobs.len(), 2, "keep the latest snapshot of both jobs");
	let ids: std::collections::BTreeSet<_> = jobs.iter().map(|job| job["id"].as_str().unwrap()).collect();
	assert_eq!(ids, ["first", "second"].into_iter().collect());
	assert!(jobs.iter().all(|job| job["state"] == "running"), "{jobs:?}");
}
