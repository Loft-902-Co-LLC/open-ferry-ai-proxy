// Ported from CLIProxyAPI internal/watcher/diff/config_diff.go
// (BuildConfigChangeDetails) and the change logging of
// internal/watcher/config_reload.go (reloadConfig) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! What a reload changed, as the lines upstream logs after
//! `config changes detected:`.
//!
//! Not ported yet: both functions are stubs that report no change. The
//! reload path already calls [`log_changes`] with the previous and the new
//! config.
//!
//! Deviations from upstream:
//! - No change is reported yet.

use super::Config;

/// The changes from `old` to `new`, one readable line each, with secrets
/// left out (upstream's `BuildConfigChangeDetails`).
pub fn build_change_details(old: &Config, new: &Config) -> Vec<String> {
    let _ = (old, new);
    Vec::new()
}

/// Logs what a reload changed, as upstream's `reloadConfig` does once it
/// has the new config.
pub fn log_changes(previous: &Config, config: &Config) {
    let _ = (previous, config);
}
