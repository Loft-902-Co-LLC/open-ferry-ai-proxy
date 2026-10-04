// Ported from CLIProxyAPI internal/api/handlers/management/config_basic.go
// (GetUsageStatisticsEnabled, GetLogsMaxTotalSizeMB, GetErrorLogsMaxFiles)
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The observability settings' reads, each answering `{"<name>":<value>}`:
//!
//! - `GET /v0/management/usage-statistics-enabled`, a boolean;
//! - `GET /v0/management/logs-max-total-size-mb`, the size in MB the log
//!   directory is kept under, `0` for no limit;
//! - `GET /v0/management/error-logs-max-files`, how many request error logs
//!   are kept.
//!
//! The v8 API reads them from the config, at
//! `/v8/management/config/observability/...`.
//!
//! Deviations from upstream: their `PUT` and `PATCH`, which write the
//! config, aren't ported, as open-ferry never writes it; they answer with
//! the empty 404. A setting changes when the config file does.

use axum::extract::State;
use axum::routing::{MethodRouter, get};
use http::StatusCode;
use open_ferry_core::config::Config;

use crate::Route;
use crate::json::{self, Json};
use crate::state::ManagementState;

/// How a setting's value is read from the config.
type Read = fn(&Config) -> Json;

/// The settings: the path under `/v0/management/`, which is also the name
/// the answer gives the value, and the value.
const SETTINGS: [(&str, Read); 3] = [
    ("usage-statistics-enabled", |config| {
        Json::Bool(config.usage_statistics_enabled)
    }),
    ("logs-max-total-size-mb", |config| {
        Json::Int(config.logs_max_total_size_mb)
    }),
    ("error-logs-max-files", |config| {
        Json::Int(config.error_logs_max_files)
    }),
];

/// The module's routes.
pub(crate) fn routes() -> Vec<Route> {
    SETTINGS
        .iter()
        .map(|&(name, read)| Route::key(format!("/v0/management/{name}"), setting(name, read)))
        .collect()
}

/// A getter answering `{"<name>":<value>}`.
fn setting(name: &'static str, read: Read) -> MethodRouter<ManagementState> {
    get(move |State(state): State<ManagementState>| async move {
        json::response(StatusCode::OK, &Json::map([(name, read(&state.config()))]))
    })
}
