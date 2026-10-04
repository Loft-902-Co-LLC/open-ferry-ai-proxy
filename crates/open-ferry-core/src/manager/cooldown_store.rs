//! The cooldown state store: credentials' cooldowns saved beside their
//! files while `save-cooldown-status` is on, and restored at start
//! (upstream's sdk/cliproxy/auth/cooldown_state.go and the store calls in
//! conductor_cooldown.go). Not ported yet (P3 WP-E).
//!
//! What is here are the hooks the rest of the proxy calls, with the
//! signatures the port keeps: the binary calls [`reconfigure`] and then
//! [`restore`] after the credentials are loaded at start and after each
//! reload, and the manager calls `changed` after every change that may
//! move a cooldown, once its state lock is released. For now nothing is
//! saved or restored.
//!
//! Deviations from upstream: nothing is saved yet.

use super::Manager;
use crate::config::Config;

/// The store's state, in the manager.
#[derive(Debug, Default)]
pub(crate) struct CooldownStore {}

/// Points `manager`'s store at `config`'s auth directory, or turns it off,
/// as `save-cooldown-status` says (upstream's `resolveCooldownStateStore`
/// and `SetCooldownStateStore`). `previous` is the config before, `None`
/// at start.
pub fn reconfigure(manager: &Manager, previous: Option<&Config>, config: &Config) {
    let _ = (manager, previous, config);
}

/// Puts the saved cooldowns that haven't run out back on `manager`'s
/// credentials (upstream's `RestoreCooldownStates`).
pub fn restore(manager: &Manager, config: &Config) {
    let _ = (manager, config);
}

/// Notes that the cooldowns of `manager`'s credentials may have changed,
/// for the store to save (upstream's `persistCooldownStates` calls). It
/// runs on the caller's task, outside the state lock, and must return at
/// once.
pub(crate) fn changed(manager: &Manager) {
    let _ = &manager.shared.cooldown_store;
}
