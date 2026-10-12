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

/// The observability handles for `config`, loaded from `config_path`. The
/// request logger cleans by the request-cleaning handle of `file_log`'s
/// controls, which is the process's own for a plain run and a worker's
/// private one under the supervisor: its `peers_gone` turns it on.
pub fn build(config: &Config, config_path: &Path, file_log: &FileLog) -> Observability {
    build_in(config, config_path, resolve_log_directory(config), file_log)
}

/// [`build`] with the log directory given.
fn build_in(
    config: &Config,
    config_path: &Path,
    log_dir: std::path::PathBuf,
    file_log: &FileLog,
) -> Observability {
    Observability {
        request_log: RequestLogger::with_cleaning(
            config,
            &log_dir,
            config_path,
            file_log.controls().request_cleaning(),
        ),
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Bytes;
    use axum::http::{HeaderMap, Method};
    use open_ferry_core::observe::RequestContext;
    use open_ferry_core::observe::request_log::{
        Answer, Downstream, RequestBody, ResponseCapture, finish,
    };

    use super::*;
    use crate::logging::LogLevel;

    /// One request that ends in a 500, which the log keeps as an error log
    /// whatever the request-log setting says.
    fn fail(observability: &Observability, n: usize) {
        let path = format!("/v1/fail-{n}");
        let request = Arc::new(RequestContext::new(Method::POST, path.clone()));
        observability.request_log.start(&request);
        let mut capture = ResponseCapture::default();
        capture.push(&Bytes::from_static(b"{}"));
        finish(
            &request,
            Downstream {
                url: Downstream::url(&path, None),
                secrets: Downstream::url_secrets(&path, None),
                method: "POST".to_owned(),
                headers: HeaderMap::new(),
                body: RequestBody::Captured {
                    raw: Bytes::from_static(b"{}"),
                    truncated: false,
                },
            },
            Answer {
                status: 500,
                headers: HeaderMap::new(),
                body: capture,
                head_at: chrono::Local::now(),
                canceled: false,
            },
        );
        observability.request_log.flush();
    }

    fn error_logs(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("error-")
            })
            .count()
    }

    // The request log cleans by the worker's own handle: with the worker's
    // cleaning off (a supervised worker's until `peers_gone`) it deletes no
    // older error log, and with it on it does.
    #[test]
    fn the_request_log_follows_the_workers_cleaning_switch() {
        let log = LogLevel::detached_supervised();
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.error_logs_max_files = 1;
        let observability = build_in(
            &config,
            Path::new(""),
            dir.path().to_path_buf(),
            log.file_log(),
        );

        for n in 0..3 {
            fail(&observability, n);
        }
        assert_eq!(error_logs(dir.path()), 3, "cleaning was off");

        log.file_log().controls().set_cleaning(true);
        fail(&observability, 3);
        assert_eq!(error_logs(dir.path()), 1, "cleaning was on");
    }
}
