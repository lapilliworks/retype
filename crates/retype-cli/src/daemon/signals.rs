//! Draining on a signal, `docs/ipc.md` §2.
//!
//! The first `SIGTERM` or `SIGINT` asks the daemon to stop the way `daemon stop` does:
//! refuse new work and let the running jobs finish. A second one is a person in a
//! hurry, so it behaves as `daemon stop --now`.
//!
//! The handler does one thing, because a signal handler may barely do anything: it
//! counts. The accept loop reads the count, which is why the listener is polled rather
//! than blocked on. A self-pipe or a wake-up connection would answer the same question
//! with more machinery for fifty milliseconds of latency.

use std::sync::atomic::{AtomicUsize, Ordering};

/// How many stop signals this process has seen. A static, because a handler is given
/// nothing to hold state in.
static SEEN: AtomicUsize = AtomicUsize::new(0);

/// How often the accept loop looks at the count while it waits for a client.
pub const POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// What the daemon should do about signals it has seen since its last look.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asked {
	/// Nothing new.
	Nothing,
	/// Stop once the running jobs finish.
	Drain,
	/// Stop now, whatever is running.
	Now,
}

/// A snapshot of the count, so the accept loop knows what it has already acted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seen(usize);

impl Default for Seen {
	fn default() -> Self {
		Self::now()
	}
}

impl Seen {
	pub fn now() -> Self {
		Self(SEEN.load(Ordering::Relaxed))
	}

	/// What arrived since this snapshot, and the snapshot to carry forward.
	pub fn asked(mut self) -> (Asked, Self) {
		let count = SEEN.load(Ordering::Relaxed);

		if count == self.0 {
			return (Asked::Nothing, self);
		}

		let asked = match count - self.0 {
			1 => Asked::Drain,
			_ => Asked::Now,
		};

		self.0 = count;
		(asked, self)
	}
}

/// Take the two stop signals away from the platform's default, which is to die.
///
/// A daemon that dies on `SIGTERM` is a daemon a supervisor stops in the middle of a
/// conversion, and the journal then says `running` for a job nothing is running.
#[cfg(unix)]
pub fn install() {
	unsafe {
		let handler = handle as *const () as libc::sighandler_t;
		libc::signal(libc::SIGTERM, handler);
		libc::signal(libc::SIGINT, handler);
	}
}

/// Windows has console control events instead. A handler may only return quickly,
/// so it counts and closes the listening pipe, which is what makes a blocked
/// `ConnectNamedPipe` return and lets the daemon reach its drain.
#[cfg(windows)]
static PIPE: AtomicUsize = AtomicUsize::new(0);

#[cfg(windows)]
pub fn install() {
	use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

	unsafe {
		SetConsoleCtrlHandler(Some(handle_console), 1);
	}
}

#[cfg(windows)]
pub fn watch_pipe(handle: usize) {
	PIPE.store(handle, Ordering::SeqCst);
}

/// Return whether this caller still owns the handle, so it closes it exactly once.
#[cfg(windows)]
pub fn unwatch_pipe(handle: usize) -> bool {
	PIPE.compare_exchange(handle, 0, Ordering::SeqCst, Ordering::SeqCst)
		.is_ok()
}

#[cfg(windows)]
pub fn stop_requested() -> bool {
	SEEN.load(Ordering::Relaxed) > 0
}

#[cfg(windows)]
unsafe extern "system" fn handle_console(_: u32) -> i32 {
	SEEN.fetch_add(1, Ordering::Relaxed);

	let handle = PIPE.swap(0, Ordering::SeqCst);
	if handle != 0 {
		unsafe {
			windows_sys::Win32::Foundation::CloseHandle(handle as *mut std::ffi::c_void);
		}
	}

	1
}

#[cfg(not(any(unix, windows)))]
pub fn install() {}

/// Wait for the running jobs, unless another signal says not to.
///
/// This is where the escalation lives rather than in the accept loop: the loop has
/// already stopped accepting when a drain begins, and a person who sends a second
/// `SIGTERM` is talking to the wait, not to the loop.
pub fn drain(server: &mut crate::daemon::server::Server) {
	let mut seen = Seen::now();

	while !server.idle() {
		let (asked, next) = seen.asked();
		seen = next;

		// Any signal here is a second one: the first is what started this wait.
		if asked != Asked::Nothing {
			let cancelled = server.stop(true);
			eprintln!("retype: stopping now, {cancelled} job(s) cancelled");

			return;
		}

		std::thread::sleep(POLL);
	}
}

#[cfg(unix)]
extern "C" fn handle(_: libc::c_int) {
	// Atomic and nothing else: this is the one thing a handler may do safely.
	SEEN.fetch_add(1, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_first_signal_drains_and_a_second_one_does_not_wait() {
		SEEN.store(0, Ordering::Relaxed);
		let start = Seen::now();

		assert_eq!(start.asked(), (Asked::Nothing, Seen(0)));

		SEEN.store(1, Ordering::Relaxed);
		let (asked, seen) = start.asked();
		assert_eq!(asked, Asked::Drain);
		assert_eq!(seen, Seen(1));

		// Two arrivals between one snapshot is a person who meant it.
		SEEN.store(3, Ordering::Relaxed);
		assert_eq!(start.asked().0, Asked::Now);

		// And a snapshot taken after them sees nothing new.
		assert_eq!(Seen::now().asked().0, Asked::Nothing);
		SEEN.store(0, Ordering::Relaxed);
	}
}
