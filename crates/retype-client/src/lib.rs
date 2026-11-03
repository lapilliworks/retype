//! Shared daemon client for the command line and the GUI.
//!
//! Transport, endpoint discovery and process start live here so the two
//! interfaces cannot disagree about how a daemon is found or started. Policy
//! about what a command prints stays in the caller.

pub mod error;
pub mod jobs;
pub mod session;
pub mod state;

pub use error::{Error, Result};
pub use retype_ipc::Client;
pub use session::{Context, Daemon, connect, connect_or_spawn, spawn_detached, wait_for, wait_for_stop};
