//! Helpers shared by the tests that drive the daemon.
//!
//! Test daemons disable notifications, except for the scripted notifier tests.

use std::path::Path;
use std::time::{Duration, Instant};

/// Wait for watch installation and initial source priming to finish.
pub fn ready(state: &Path) {
	let mut client = retype_cli::client::connect(state).expect("the daemon answers");
	let deadline = Instant::now() + Duration::from_secs(20);
	loop {
		let status = client.status().expect("the daemon reports watch readiness");
		if !status.watches.is_empty()
			&& status
				.watches
				.iter()
				.all(|watch| watch.status.as_deref() == Some("ready"))
		{
			return;
		}
		assert!(
			Instant::now() < deadline,
			"the watches did not become ready: {status:?}"
		);
		std::thread::sleep(Duration::from_millis(25));
	}
}
