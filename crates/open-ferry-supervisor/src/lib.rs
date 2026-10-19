//! Local-only plumbing shared by the supervisor and its workers.
//!
//! This crate does not run a switch, parse server config, or contact providers.
//! Phase two supplies the state machine and worker lifecycle.

pub mod client;
pub mod clock;
pub mod control;
pub mod core;
pub mod disk;
pub mod driver;
pub mod endpoint;
pub mod host;
pub mod install;
pub mod ladder;
pub mod process;
pub mod protocol;
pub mod reentry;
pub mod serve;
pub mod snapshot;
pub mod sockets;
pub mod startup;
pub mod statefile;
pub mod supervise;
pub mod switch;
pub mod timings;
pub mod trust;
pub mod worker;

#[cfg(test)]
mod prune_tests;
#[cfg(test)]
mod startup_tests;
#[cfg(test)]
mod status_tests;
#[cfg(test)]
mod switch_tests;
#[cfg(test)]
mod test_tracing;
#[cfg(test)]
mod testkit;
