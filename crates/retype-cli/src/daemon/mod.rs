//! The daemon: one per user session, the only writer of state, `docs/dev/architecture/ipc.md`.
//!
//! Connections are served one at a time, which is enough while every method is a
//! request and answer, and the event ring is read the same way: a client asks for
//! what followed the last sequence it saw rather than holding a stream open.

pub mod batches;
pub mod events;
pub mod immediate;
mod lock;
pub mod machine;
pub mod park;
#[cfg(unix)]
pub mod peer;
#[cfg(windows)]
mod pipe;
pub mod queue;
pub mod reconcile;
pub mod resume;
mod server;
pub mod signals;
pub mod supervision;
pub mod timer;
pub mod watcher;

pub use server::Server;
#[cfg(unix)]
pub use server::serve;

#[cfg(unix)]
use std::fs;

use crate::error::Result;
use crate::state;
use lock::Lock;

/// Run in the foreground, which is what a user unit or a launchd agent
/// executes, `docs/dev/architecture/ipc.md` §1.
#[cfg(unix)]
pub fn run(state_dir: Option<std::path::PathBuf>, banner: bool) -> Result<()> {
	use std::os::unix::net::UnixListener;

	let state_dir = state::resolve(state_dir)?;
	state::ensure_private_directory(&state_dir)?;
	// The GUI installs the managed set into the state directory; the daemon is
	// where plans run, so it has to resolve it too.
	retype_core::tools::use_managed(state_dir.join("tools").join("bin"));

	let _lock = Lock::acquire(&state_dir.join("lock"), &state_dir)?;
	let socket = state::socket_path(&state_dir)?;

	// The lock is held, so anything at the socket path is a corpse.
	if socket.exists() {
		fs::remove_file(&socket)?;
	}

	let mut server = Server::open(&state_dir)?;
	let shared = server.shared();
	let pool = server.pool();
	let watcher = watcher::start(shared, std::sync::Arc::clone(&pool), &state_dir)?;
	let timer = timer::start(server.shared(), std::sync::Arc::clone(&pool));
	let listener = UnixListener::bind(&socket)?;
	set_socket_private(&socket)?;

	let endpoint = socket.display().to_string();
	state::publish_endpoint(&state_dir, &endpoint)?;
	server.set_endpoint(endpoint.clone());

	if banner {
		eprintln!("retype: daemon listening on {endpoint} (pid {})", std::process::id());
	}

	// Before the loop that watches for them, so a stop signal is a drain rather than a
	// death, `docs/dev/architecture/ipc.md` §2.
	signals::install();

	let result = serve(listener, &mut server);

	// A stop drains; a second signal does not wait for a twenty minute transcode.
	signals::drain(&mut server);

	timer.0.store(true, std::sync::atomic::Ordering::Relaxed);
	let _ = timer.1.join();

	if let Some(watcher) = watcher {
		watcher.stop();
	}

	pool.stop();

	// Graceful shutdown should not leave a path that looks like a live daemon.
	let _ = fs::remove_file(&socket);
	state::clear_endpoint(&state_dir);
	result
}

#[cfg(unix)]
fn set_socket_private(path: &std::path::Path) -> Result<()> {
	use std::os::unix::fs::PermissionsExt;
	fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
	Ok(())
}

#[cfg(windows)]
pub fn run(state_dir: Option<std::path::PathBuf>, banner: bool) -> Result<()> {
	let state_dir = state::resolve(state_dir)?;
	state::ensure_private_directory(&state_dir)?;
	// The GUI installs the managed set into the state directory; the daemon is
	// where plans run, so it has to resolve it too.
	retype_core::tools::use_managed(state_dir.join("tools").join("bin"));

	let _lock = Lock::acquire(&state_dir.join("lock"), &state_dir)?;
	let mut server = Server::open(&state_dir)?;
	let shared = server.shared();
	let pool = server.pool();
	let watcher = watcher::start(shared, std::sync::Arc::clone(&pool), &state_dir)?;
	let timer = timer::start(server.shared(), std::sync::Arc::clone(&pool));
	let name = pipe::name(server.machine());

	state::publish_endpoint(&state_dir, &name)?;
	server.set_endpoint(name.clone());

	if banner {
		eprintln!("retype: daemon listening on {name} (pid {})", std::process::id());
	}

	signals::install();
	let result = pipe::serve(&name, &mut server);

	signals::drain(&mut server);

	timer.0.store(true, std::sync::atomic::Ordering::Relaxed);
	let _ = timer.1.join();

	if let Some(watcher) = watcher {
		watcher.stop();
	}

	pool.stop();

	state::clear_endpoint(&state_dir);
	if signals::stop_requested() { Ok(()) } else { result }
}

#[cfg(not(any(unix, windows)))]
pub fn run(_state_dir: Option<std::path::PathBuf>, _banner: bool) -> Result<()> {
	Err(crate::error::Error::UnsupportedTransport)
}
