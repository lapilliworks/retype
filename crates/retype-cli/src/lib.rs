//! Command line client and daemon for `retype`.
//!
//! The binary is `retype`; this library holds what its tests need to reach.
//! Domain logic lives in `retype-core`, the filesystem pipeline in
//! `retype-watch`, and the wire in `retype-ipc`.

pub mod client;
pub mod config;
pub mod convert;
pub mod daemon;
pub mod doctor;
pub mod error;
pub mod explain;
pub mod state;
