//! The daemon: one per user session, the only writer of state, `docs/ipc.md`.
//!
//! Connections are served one at a time, which is enough while every method is
//! a request and answer. The event stream in `docs/ipc.md` §7 is what forces a
//! connection per thread, and it arrives with the job queue.

mod lock;
mod machine;
#[cfg(windows)]
mod pipe;
mod server;

pub use server::Server;
#[cfg(unix)]
pub use server::serve;

#[cfg(unix)]
use std::fs;

use crate::error::Result;
use crate::state;
use lock::Lock;

/// Run in the foreground, which is what a user unit or a launchd agent
/// executes, `docs/ipc.md` §1.
#[cfg(unix)]
pub fn run(state_dir: Option<std::path::PathBuf>, banner: bool) -> Result<()> {
	use std::os::unix::net::UnixListener;

	let state_dir = state::resolve(state_dir)?;
	state::ensure_private_directory(&state_dir)?;

	let _lock = Lock::acquire(&state_dir.join("lock"), &state_dir)?;
	let socket = state::socket_path(&state_dir)?;

	// The lock is held, so anything at the socket path is a corpse.
	if socket.exists() {
		fs::remove_file(&socket)?;
	}

	let mut server = Server::open(&state_dir)?;
	let listener = UnixListener::bind(&socket)?;
	set_socket_private(&socket)?;

	let endpoint = socket.display().to_string();
	state::publish_endpoint(&state_dir, &endpoint)?;
	server.set_endpoint(endpoint.clone());

	if banner {
		eprintln!("retype: daemon listening on {endpoint} (pid {})", std::process::id());
	}

	let result = serve(listener, server);

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

	let _lock = Lock::acquire(&state_dir.join("lock"), &state_dir)?;
	let mut server = Server::open(&state_dir)?;
	let name = pipe::name(server.machine());

	state::publish_endpoint(&state_dir, &name)?;
	server.set_endpoint(name.clone());

	if banner {
		eprintln!("retype: daemon listening on {name} (pid {})", std::process::id());
	}

	let result = pipe::serve(&name, &mut server);

	state::clear_endpoint(&state_dir);
	result
}

#[cfg(not(any(unix, windows)))]
pub fn run(_state_dir: Option<std::path::PathBuf>, _banner: bool) -> Result<()> {
	Err(crate::error::Error::UnsupportedTransport)
}
