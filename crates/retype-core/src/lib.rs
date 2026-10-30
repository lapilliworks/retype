//! Conversion engine for `retype`.
//!
//! Domain logic only: the intent grammar, the plan compiler, conversion
//! backends, the journal and undo store. No process, no socket, no watcher.
//! The long-running daemon and the command line client live beside this crate
//! and use it.

pub mod convert;
pub mod correlate;
pub mod error;
pub mod execute;
pub mod intent;
pub mod job;
pub mod journal;
pub mod output;
pub mod plan;
pub mod print;
pub mod registry;
pub mod resolve;
pub mod tools;
pub mod value;
pub mod watches;
