//! Local-only plumbing shared by the supervisor and its workers.
//!
//! This crate does not run a switch, parse server config, or contact providers.
//! Phase two supplies the state machine and worker lifecycle.

pub mod endpoint;
pub mod process;
pub mod protocol;
pub mod sockets;
pub mod trust;
