//! The request log: a file per request, with the client's request, each
//! upstream attempt and the answer, or a file per failed request only
//! (upstream's internal/logging/request_logger*.go). Not ported yet (P3
//! WP-A).
//!
//! What is here are the hooks the rest of the proxy calls, with the
//! signatures the port keeps: the binary makes a [`RequestLogger`] at start
//! and [`reconfigure`]s it on every config load, the server asks it for a
//! [`Tap`] for each call, and each request's [`RequestState`] lives in its
//! [`RequestContext`]. For now it logs nothing and gives no tap.
//!
//! Deviations from upstream: nothing is logged yet.

use std::path::Path;
use std::sync::Arc;

use super::{RequestContext, Tap};
use crate::config::Config;

#[cfg(test)]
mod tests;

/// The request logger (upstream's `FileRequestLogger`). Cloning gives
/// another handle to the same logger.
#[derive(Clone, Debug, Default)]
pub struct RequestLogger {}

impl RequestLogger {
    /// A logger for `config`, writing to `log_dir`, which is taken from the
    /// directory of `config_path` when relative (upstream's
    /// `NewFileRequestLogger`, as `defaultRequestLoggerFactory` calls it).
    pub fn new(config: &Config, log_dir: &Path, config_path: &Path) -> Self {
        let _ = (config, log_dir, config_path);
        Self::default()
    }

    /// The tap that records the upstream attempts of a call made for the
    /// request of `context`, or `None` when they aren't recorded. Every
    /// call a request makes asks for one, the Alpha Search pass-through
    /// included.
    pub fn tap(&self, context: &Arc<RequestContext>) -> Option<Arc<dyn Tap>> {
        let _ = context;
        None
    }
}

/// What the request log keeps of one request while it runs, in its
/// [`RequestContext`]: the server's capture layer and the request's taps
/// share it.
#[derive(Debug, Default)]
pub struct RequestState {}

/// Applies `config` to `logger` (upstream's `SetEnabled` and
/// `SetErrorLogsMaxFiles` on reload). `previous` is the config before,
/// `None` at start.
pub fn reconfigure(logger: &RequestLogger, previous: Option<&Config>, config: &Config) {
    let _ = (logger, previous, config);
}
