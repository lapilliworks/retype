//! Whether the daemon says what it did, `docs/config.md` §17.
//!
//! The policy is here and the delivery is in the daemon, because a notification is a
//! fact about this machine's session rather than about a directory.

/// Host-level, like retention: a machine either has a place to show a notification or
/// it does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
	/// Whether the daemon notifies at all. Off is a supported answer, because a
	/// notification nobody wanted is worse than none.
	pub enabled: bool,
	/// Whether the notification carries an action button. Off leaves the plain form,
	/// which is what a platform without actions gets anyway.
	pub actions: bool,
}

impl Default for Policy {
	fn default() -> Self {
		Self {
			enabled: true,
			actions: true,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_defaults_are_on_for_both() {
		let policy = Policy::default();

		assert!(policy.enabled);
		assert!(policy.actions);
	}
}
