//! Socket transfer preserves one kernel listener/backlog, not a second bind.
#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;
