//! The observability handles, made once at start, and the config applied
//! to them as each one is loaded.
//!
//! [`build`] makes the [`Observability`] the server and the management API
//! share: the log directory, resolved once as upstream resolves it at
//! start, the request logger and the usage statistics. [`reconfigure`]
//! applies a config to each subsystem in turn, as upstream's start
//! (cmd/server/main.go) and reload (internal/api/server_reload.go and the
//! service's config runtime) apply theirs: the main log's output, the
//! request log, the usage statistics, the log of what a reload changed,
//! the cooldown state store and the payload rules. Each subsystem's hook
//! lives in its owner's module; this one only calls them.
//!
//! Deviations from upstream: the config is applied once the credentials are
//! loaded and the rest of the config applied, at start as on a reload, so
//! lines logged before go to standard output only, and the lines saying
//! what a reload changed come after the reload's own.

use std::path::Path;

use open_ferry_core::config::{Config, diff as config_diff};
use open_ferry_core::manager::{Manager, cooldown_store};
use open_ferry_core::observe::Observability;
use open_ferry_core::observe::dirs::resolve_log_directory;
use open_ferry_core::observe::request_log::{self, RequestLogger};
use open_ferry_core::observe::usage::{self, Usage};

use crate::file_log::{self, FileLog};

/// The observability handles for `config`, loaded from `config_path`.
pub fn build(config: &Config, config_path: &Path) -> Observability {
    let log_dir = resolve_log_directory(config);
    Observability {
        request_log: RequestLogger::new(config, &log_dir, config_path),
        usage: Usage::new(config),
        log_dir: Some(log_dir),
    }
}

/// Applies `config` to the main log's output `file_log`, to
/// `observability`'s request logger and usage statistics, and to
/// `manager`'s cooldown store, installs its payload rules for the
/// executors, and logs what changed since `previous`, the config before
/// (`None` at start). `management_available` says whether the management
/// API serves requests, which the usage queue follows.
pub fn reconfigure(
    observability: &Observability,
    file_log: &FileLog,
    manager: &Manager,
    management_available: bool,
    previous: Option<&Config>,
    config: &Config,
) {
    file_log::reconfigure(file_log, previous, config);
    request_log::reconfigure(&observability.request_log, previous, config);
    usage::reconfigure(&observability.usage, previous, config, management_available);
    if let Some(previous) = previous {
        config_diff::log_changes(previous, config);
    }
    cooldown_store::reconfigure(manager, previous, config);
    cooldown_store::restore(manager, config);
    open_ferry_providers::payload::reconfigure(config);
}
