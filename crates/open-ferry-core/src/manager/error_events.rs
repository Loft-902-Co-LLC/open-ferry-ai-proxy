// Ported from CLIProxyAPI sdk/cliproxy/auth/error_events.go
// (publishErrorEvent's guard) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The hook the manager tells about failed calls: the usage statistics
//! publish them as error events.
//!
//! The manager calls it once a failed call's outcome is recorded, after the
//! state lock is released, with the credential as the outcome left it.
//!
//! Deviations from upstream: the event is built and queued by the hook's
//! owner, where upstream's manager builds it and queues it itself.

use super::{CallResult, Manager};
use crate::auth::Auth;

/// What is told about failed calls (upstream's `publishErrorEvent`). It
/// runs on the caller's task and must return at once.
pub trait ErrorEvents: Send + Sync {
    /// A failed call's `result` was recorded on `auth`, as it now is.
    fn publish(&self, result: &CallResult, auth: &Auth);
}

impl Manager {
    /// Tells `events` about every failed call from now on. Only the first
    /// hook set is kept.
    pub fn set_error_events(&self, events: std::sync::Arc<dyn ErrorEvents>) {
        let _ = self.shared.error_events.set(events);
    }

    /// Tells the hook, if one is set, that a call's `result` was recorded
    /// on `auth`, unless it succeeded.
    pub(crate) fn publish_error_event(&self, result: &CallResult, auth: &Auth) {
        if result.success {
            return;
        }
        if let Some(events) = self.shared.error_events.get() {
            events.publish(result, auth);
        }
    }
}
