//! The error events of failed calls, for the usage queue's error
//! subscribers (upstream's sdk/cliproxy/auth/error_events.go). Not ported
//! yet (P3 WP-C).
//!
//! Deviations from upstream: nothing is published yet.

use super::Usage;
use crate::auth::Auth;
use crate::manager::{CallResult, ErrorEvents};

/// Publishes the manager's failed calls to the usage queue.
#[derive(Debug)]
pub(super) struct UsageErrorEvents {}

impl UsageErrorEvents {
    pub(super) fn new(usage: &Usage) -> Self {
        let _ = usage;
        Self {}
    }
}

impl ErrorEvents for UsageErrorEvents {
    fn publish(&self, result: &CallResult, auth: &Auth) {
        let _ = (result, auth);
    }
}
