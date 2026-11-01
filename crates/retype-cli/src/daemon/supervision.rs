//! Whether a supervisor started this daemon, `docs/dev/architecture/ipc.md` §6.
//!
//! A daemon under systemd or launchd is restarted when it exits, so a `stop` that
//! obeyed would be undone a second later and the user would see the daemon come
//! back. Saying so is better than pretending the stop worked.
//!
//! The answer is read from the environment the supervisor sets, not from the
//! process tree: a parent lookup is a guess that gets `systemd --user` wrong when
//! the daemon is started from a shell inside the session, and the environment
//! variables are what the supervisor itself promises.
//!
//! The variables are inherited by every process the supervisor's children start,
//! so only the process a unit actually runs may trust them. The CLI clears them
//! when it starts a daemon of its own, `client::spawn_detached`.

/// Which supervisor started this process, if any.
pub fn supervisor() -> Option<&'static str> {
	detect(|name| std::env::var_os(name).is_some())
}

/// The detection itself, with the environment injected so it can be tested without
/// mutating the process environment, which edition 2024 makes unsafe.
fn detect(set: impl Fn(&str) -> bool) -> Option<&'static str> {
	// `INVOCATION_ID` is set for every unit systemd starts, user or system, and is
	// unique per start, which is what it exists for.
	if set("INVOCATION_ID") {
		return Some("systemd");
	}

	// launchd sets this to the job's label.
	if set("XPC_SERVICE_NAME") {
		return Some("launchd");
	}

	None
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_supervisor_is_recognised_by_its_own_environment() {
		assert_eq!(detect(|name| name == "INVOCATION_ID"), Some("systemd"));
		assert_eq!(detect(|name| name == "XPC_SERVICE_NAME"), Some("launchd"));
		assert_eq!(detect(|_| false), None);
	}

	#[test]
	fn systemd_wins_when_both_are_present() {
		// A systemd unit that starts a launchd-shaped wrapper is still a systemd
		// unit, and its restart is the one that would undo a stop.
		assert_eq!(detect(|_| true), Some("systemd"));
	}
}
