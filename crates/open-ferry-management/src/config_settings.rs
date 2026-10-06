// Ported from CLIProxyAPI internal/api/handlers/management/config_basic.go
// (PutDebug, PutUsageStatisticsEnabled, PutLoggingToFile,
// PutLogsMaxTotalSizeMB, PutErrorLogsMaxFiles, PutRequestLog,
// PutWebsocketAuth, PutRequestRetry, PutMaxRetryCredentials,
// PutMaxRetryInterval, PutForceModelPrefix, PutRoutingStrategy,
// PutProxyURL, DeleteProxyURL), quota.go (PutSwitchProject,
// PutSwitchPreviewModel), handler.go (updateBoolField, updateIntField,
// updateStringField) and internal/api/server_management.go (their routes)
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Changing one setting: `PUT` or `PATCH /v0/management/<setting>` with
//! `{"value":<value>}`, which saves the config and answers
//! `{"status":"ok"}`.
//!
//! - Booleans: `debug`, `usage-statistics-enabled`, `logging-to-file`,
//!   `request-log`, `ws-auth`, `force-model-prefix`,
//!   `quota-exceeded/switch-project` and
//!   `quota-exceeded/switch-preview-model`.
//! - Integers: `request-retry`, `max-retry-credentials`,
//!   `max-retry-interval`, `logs-max-total-size-mb` (a negative size is 0,
//!   no limit) and `error-logs-max-files` (a negative count is 10).
//! - Strings: `proxy-url`, stored as given, and `routing/strategy`, stored
//!   by its canonical name (`round-robin`, `weighted-round-robin` or
//!   `fill-first`, or one of their short names); another name answers 400
//!   `invalid strategy`.
//!
//! `DELETE /v0/management/proxy-url` clears the proxy URL.
//!
//! A body that isn't an object with a `value` of the setting's type
//! answers 400 `invalid body`. See [`crate::config_write`] for how the
//! config is saved.
//!
//! Deviations from upstream: those of [`crate::config_write`] and
//! [`crate::go_json`].

use axum::body::Body;
use axum::extract::State;
use axum::response::Response;
use axum::routing::{MethodRouter, put};
use open_ferry_core::config::Config;
use serde_json::Value;

use crate::Route;
use crate::config_sanitize::normalize_routing_strategy;
use crate::config_write;
use crate::go_json;
use crate::state::ManagementState;

/// A setting: what its value is, and where it goes in the config.
#[derive(Clone, Copy)]
enum Setting {
    /// A boolean (upstream's `updateBoolField`).
    Bool(fn(&mut Config) -> &mut bool),
    /// An integer, mapped by the function before it is stored (upstream's
    /// `updateIntField`, or a handler of its own where the value is
    /// mapped).
    Int(fn(i64) -> i64, fn(&mut Config) -> &mut i64),
    /// A string, checked and mapped by the function before it is stored
    /// (upstream's `updateStringField`, or `PutRoutingStrategy`).
    Str(fn(&str) -> Option<String>, fn(&mut Config) -> &mut String),
}

/// A setting's new value, read from a request.
enum NewValue {
    Bool(bool),
    Int(i64),
    Str(String),
}

/// The settings, by path under `/v0/management/`.
const SETTINGS: [(&str, Setting); 15] = [
    ("debug", Setting::Bool(|config| &mut config.debug)),
    (
        "usage-statistics-enabled",
        Setting::Bool(|config| &mut config.usage_statistics_enabled),
    ),
    (
        "logging-to-file",
        Setting::Bool(|config| &mut config.logging_to_file),
    ),
    (
        "logs-max-total-size-mb",
        Setting::Int(
            |size| size.max(0),
            |config| &mut config.logs_max_total_size_mb,
        ),
    ),
    (
        "error-logs-max-files",
        Setting::Int(
            |count| if count < 0 { 10 } else { count },
            |config| &mut config.error_logs_max_files,
        ),
    ),
    (
        "proxy-url",
        Setting::Str(|url| Some(url.to_owned()), |config| &mut config.proxy_url),
    ),
    (
        "quota-exceeded/switch-project",
        Setting::Bool(|config| &mut config.quota_exceeded.switch_project),
    ),
    (
        "quota-exceeded/switch-preview-model",
        Setting::Bool(|config| &mut config.quota_exceeded.switch_preview_model),
    ),
    (
        "request-log",
        Setting::Bool(|config| &mut config.request_log),
    ),
    ("ws-auth", Setting::Bool(|config| &mut config.ws_auth)),
    (
        "request-retry",
        Setting::Int(|retries| retries, |config| &mut config.request_retry),
    ),
    (
        "max-retry-credentials",
        Setting::Int(|count| count, |config| &mut config.max_retry_credentials),
    ),
    (
        "max-retry-interval",
        Setting::Int(|seconds| seconds, |config| &mut config.max_retry_interval),
    ),
    (
        "force-model-prefix",
        Setting::Bool(|config| &mut config.force_model_prefix),
    ),
    (
        "routing/strategy",
        Setting::Str(
            |strategy| normalize_routing_strategy(strategy).map(str::to_owned),
            |config| &mut config.routing.strategy,
        ),
    ),
];

/// The module's routes.
pub(crate) fn routes() -> Vec<Route> {
    SETTINGS
        .iter()
        .map(|&(path, setting)| {
            let handler = handler(setting);
            let handler = if path == "proxy-url" {
                handler.delete(delete_proxy_url)
            } else {
                handler
            };
            Route::key(format!("/v0/management/{path}"), handler)
        })
        .collect()
}

/// `PUT` and `PATCH` of `setting`.
fn handler(setting: Setting) -> MethodRouter<ManagementState> {
    let change = move |State(state): State<ManagementState>, body: Body| async move {
        change(&state, setting, body).await
    };
    put(change).patch(change)
}

/// Upstream's `Put<Setting>`: reads `{"value":...}`, stores it and saves.
async fn change(state: &ManagementState, setting: Setting, body: Body) -> Response {
    let body = match config_write::request_body(state, body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let value = match read_value(setting, &body) {
        Ok(value) => value,
        Err(message) => return config_write::bad_request(message),
    };
    config_write::update(state, false, move |config| {
        match (setting, value) {
            (Setting::Bool(field), NewValue::Bool(value)) => *field(config) = value,
            (Setting::Int(_, field), NewValue::Int(value)) => *field(config) = value,
            (Setting::Str(_, field), NewValue::Str(value)) => *field(config) = value,
            _ => {}
        }
        Ok(())
    })
    .await
}

/// The new value in `body`, mapped as `setting` maps it, or the message of
/// the 400 answer.
fn read_value(setting: Setting, body: &[u8]) -> Result<NewValue, &'static str> {
    const INVALID: &str = "invalid body";
    let request = go_json::first(body).ok_or(INVALID)?;
    let [value] = go_json::fields(&request, ["value"]).ok_or(INVALID)?;
    match setting {
        Setting::Bool(_) => pointer::<bool>(value).map(NewValue::Bool),
        Setting::Int(map, _) => pointer::<i64>(value).map(|value| NewValue::Int(map(value))),
        Setting::Str(map, _) => {
            let value = pointer::<String>(value)?;
            map(&value).map(NewValue::Str).ok_or("invalid strategy")
        }
    }
}

/// A `*T` field that must be set: its value, or `invalid body`.
fn pointer<T: serde::de::DeserializeOwned>(value: Option<&Value>) -> Result<T, &'static str> {
    go_json::pointer(value).ok().flatten().ok_or("invalid body")
}

/// `DELETE /v0/management/proxy-url` (upstream's `DeleteProxyURL`).
async fn delete_proxy_url(State(state): State<ManagementState>) -> Response {
    config_write::update(&state, false, |config| {
        config.proxy_url.clear();
        Ok(())
    })
    .await
}
