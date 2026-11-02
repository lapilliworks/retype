//! Optional codec tests can be made mandatory for a release-validation run.

pub fn unavailable(reason: impl std::fmt::Display) {
	assert!(
		std::env::var_os("RETYPE_REQUIRE_TEST_TOOLS").as_deref() != Some(std::ffi::OsStr::new("1")),
		"required test prerequisite unavailable: {reason}"
	);
	eprintln!("SKIPPED: {reason}; set RETYPE_REQUIRE_TEST_TOOLS=1 to reject missing prerequisites");
}
