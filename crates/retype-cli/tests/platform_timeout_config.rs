//! Safeguards for the Windows platform runner's per-test isolation.

const CONFIG: &str = include_str!("../../../scripts/nextest-platform.toml");

#[test]
fn platform_tests_have_bounded_timeouts_without_retries() {
	let config: toml::Value = toml::from_str(CONFIG).unwrap();
	let profile = &config["profile"]["platform"];
	assert_eq!(profile["test-threads"].as_integer(), Some(4));
	assert_eq!(profile["retries"].as_integer(), Some(0));
	assert_eq!(profile["fail-fast"].as_bool(), Some(false));
	assert_eq!(profile["slow-timeout"]["period"].as_str(), Some("30s"));
	assert_eq!(profile["slow-timeout"]["terminate-after"].as_integer(), Some(2));
	assert_eq!(profile["leak-timeout"]["period"].as_str(), Some("3s"));
	assert_eq!(profile["leak-timeout"]["result"].as_str(), Some("fail"));
}
