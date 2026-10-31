//! Who is on the other end of the socket, `docs/ipc.md` §4.
//!
//! The boundary is the state directory: `0700` with the socket at `0600` inside it.
//! This is the second lock, for the case where the first one is wrong because a
//! backup, an unzip or a `chmod -R` moved the permission bits. A peer running as
//! another user is refused before it is allowed to say `hello`.
//!
//! A platform that cannot say who the peer is does not refuse: the directory is
//! still the boundary, and closing the door on a check we could not make would
//! break the daemon on a system where the socket is unreachable by anyone else
//! anyway.

use std::os::unix::net::UnixStream;

/// Whether a connection from `peer` may be served, given that this daemon runs as
/// `ours`. An unknown peer is allowed, because the platform could not answer rather
/// than because it answered badly.
pub fn accepts(peer: Option<u32>, ours: u32) -> bool {
	peer.is_none_or(|peer| peer == ours)
}

/// This process's effective user.
pub fn our_uid() -> u32 {
	// `geteuid` cannot fail and has no side effects.
	unsafe { libc::geteuid() }
}

/// The peer's user id, when the platform can say.
///
/// `std`'s `peer_cred` is still unstable, so this is the system call itself: one
/// `getsockopt` on Linux, one `getpeereid` elsewhere.
#[cfg(target_os = "linux")]
pub fn peer_uid(stream: &UnixStream) -> Option<u32> {
	use std::os::unix::io::AsRawFd;

	let mut credential = libc::ucred { pid: 0, uid: 0, gid: 0 };
	let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;

	let asked = unsafe {
		libc::getsockopt(
			stream.as_raw_fd(),
			libc::SOL_SOCKET,
			libc::SO_PEERCRED,
			std::ptr::from_mut(&mut credential).cast(),
			&mut length,
		)
	};

	(asked == 0).then_some(credential.uid)
}

/// The same on macOS and the BSDs, through the call they share.
#[cfg(not(target_os = "linux"))]
pub fn peer_uid(stream: &UnixStream) -> Option<u32> {
	use std::os::unix::io::AsRawFd;

	let mut uid: libc::uid_t = 0;
	let mut gid: libc::gid_t = 0;

	// `getpeereid` is what macOS's `LOCAL_PEERCRED` and the BSDs' own call have in
	// common, and it is the one function both expose.
	let asked = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
	(asked == 0).then_some(uid)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn another_user_is_refused_and_an_unknown_one_is_not() {
		assert!(accepts(Some(1000), 1000));
		assert!(!accepts(Some(1001), 1000));

		// Not a policy choice: a platform that cannot answer leaves the state
		// directory as the boundary rather than locking the user out of their daemon.
		assert!(accepts(None, 1000));
	}

	#[test]
	fn the_system_call_answers_for_a_real_socket() {
		let (ours, _theirs) = UnixStream::pair().unwrap();

		assert_eq!(peer_uid(&ours), Some(our_uid()), "a socket pair has one user");
	}

	#[test]
	fn our_own_effective_user_is_known() {
		assert!(our_uid() > 0, "tests do not run as root");
	}
}
