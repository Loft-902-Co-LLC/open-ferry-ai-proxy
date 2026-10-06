// Ported from CLIProxyAPI internal/api/handlers/management/config_basic.go
// (PutConfigYAML), config_v8.go (ConfigV8's PUT, PATCH and DELETE, as the
// route hands them to the writer) and internal/api/server_management.go
// (their routes) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Writing the config file whole, or a part of it in the v8 layout.
//!
//! - `PUT /v0/management/config.yaml` replaces the file with the body,
//!   once the body loads as a config. YAML that doesn't parse or decode
//!   answers 400 `{"error":"invalid_yaml","message":...}`; a config the
//!   loader's checks refuse (a weight over the limit, a trusted proxy that
//!   isn't an IP or CIDR) answers 422 `invalid_config` with the message. A
//!   file that can't be written answers 500 `{"error":"write_failed",
//!   "message":"failed to write config"}`. Else it answers
//!   `{"changed":["config"],"ok":true}`.
//! - `PUT` and `PATCH /v8/management/config` (or `config/`) replace or
//!   merge into the whole config, `PUT`, `PATCH` and `DELETE
//!   /v8/management/config/*path` the value at the path, one mapping key
//!   per segment, and `PUT /v8/management/config.yaml` replaces the whole
//!   config with a YAML body. The writer reads the file in the v8 layout,
//!   makes the change, checks it and saves the file in that layout (see
//!   [`ConfigWriter::edit_v8`](crate::ConfigWriter::edit_v8) and
//!   [`V8EditError`](crate::V8EditError) for its answers). A saved change
//!   answers `{"config-version":8,"status":"ok"}`.
//!
//! Each write is made under the config write lock, becomes the config the
//! handlers read, and has the service load the file again, as
//! [`crate::config_write`] describes.
//!
//! Deviations from upstream:
//! - `PUT /v0/management/config.yaml` has the service load the file at
//!   once; upstream leaves the change to the file watcher. The config the
//!   handlers read is the one the body loaded as, so there is no
//!   `reload_failed` answer.
//! - The body is checked by loading it in memory; upstream writes it to a
//!   temporary file beside the config to load it, and answers 500
//!   `write_failed` with the system's message when it can't.
//! - YAML errors are worded as open-ferry's config loader words them (see
//!   [`crate::config_read`]). A `weight` in the legacy layout that is a
//!   string or a boolean answers 422 `invalid_config` with `<list>[i].weight:
//!   weight must be an integer`; upstream fails to decode it first, and
//!   answers 400 `invalid_yaml` with the decoder's message.
//! - As everywhere, loading writes nothing back: a plain management key in
//!   the body stays plain in the file. Upstream hashes it into the file when
//!   it loads the file after writing it.
//! - A v8 path that isn't UTF-8 answers 400 `invalid_path` to `PUT` and
//!   `PATCH`, and 404 `not_found` to `DELETE`, before the file is read.
//!   Upstream uses its bytes as a key.
//! - Those of [`crate::config_write`].

use std::sync::Arc;

use axum::body::Body;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{MethodRouter, put};
use http::StatusCode;
use open_ferry_core::config::{Config, ConfigError, ConfigErrorKind};

use crate::Route;
use crate::auth_files::run_blocking;
use crate::config_write::{self, V8Edit, V8EditError, V8Method};
use crate::json::{self, Json};
use crate::state::ManagementState;

/// What starts the loader's message for YAML that doesn't parse or decode;
/// upstream's decoder error, which the answer gives, doesn't have it.
const LOAD_PREFIX: &str = "failed to parse config file: ";

/// The module's routes.
pub(crate) fn routes() -> Vec<Route> {
    vec![
        Route::key("/v0/management/config.yaml", put(put_config_yaml)),
        Route::key("/v8/management/config", whole_config()),
        Route::key(
            "/v8/management/config/",
            whole_config().delete(delete_whole),
        ),
        Route::key(
            "/v8/management/config.yaml",
            put(
                |State(state): State<ManagementState>, body: Body| async move {
                    edit_with_body(&state, V8Method::Put, Some(Vec::new()), body, true).await
                },
            ),
        ),
        Route::key(
            "/v8/management/config/{*path}",
            put(
                |State(state): State<ManagementState>,
                 path: Result<Path<String>, PathRejection>,
                 body: Body| async move {
                    edit_with_body(&state, V8Method::Put, parts(path), body, false).await
                },
            )
            .patch(
                |State(state): State<ManagementState>,
                 path: Result<Path<String>, PathRejection>,
                 body: Body| async move {
                    edit_with_body(&state, V8Method::Patch, parts(path), body, false).await
                },
            )
            .delete(
                |State(state): State<ManagementState>,
                 path: Result<Path<String>, PathRejection>| async move {
                    match parts(path) {
                        Some(path) => {
                            edit(
                                &state,
                                V8Edit {
                                    method: V8Method::Delete,
                                    path,
                                    body: Vec::new(),
                                    yaml: false,
                                },
                            )
                            .await
                        }
                        None => match config_write::writer(&state) {
                            Ok(_) => config_write::v8_error_response(&V8EditError::NotFound),
                            Err(response) => response,
                        },
                    }
                },
            ),
        ),
    ]
}

/// `PUT` and `PATCH` of the whole config, as JSON.
fn whole_config() -> MethodRouter<ManagementState> {
    put(
        |State(state): State<ManagementState>, body: Body| async move {
            edit_with_body(&state, V8Method::Put, Some(Vec::new()), body, false).await
        },
    )
    .patch(
        |State(state): State<ManagementState>, body: Body| async move {
            edit_with_body(&state, V8Method::Patch, Some(Vec::new()), body, false).await
        },
    )
}

/// `DELETE /v8/management/config/`: the path is empty, which the writer
/// refuses.
async fn delete_whole(State(state): State<ManagementState>) -> Response {
    edit(
        &state,
        V8Edit {
            method: V8Method::Delete,
            path: Vec::new(),
            body: Vec::new(),
            yaml: false,
        },
    )
    .await
}

/// The keys a v8 path names: its segments, once the slashes around it are
/// trimmed; `None` for a path that isn't UTF-8.
fn parts(path: Result<Path<String>, PathRejection>) -> Option<Vec<String>> {
    let Path(path) = path.ok()?;
    let path = path.trim_matches('/');
    Some(if path.is_empty() {
        Vec::new()
    } else {
        path.split('/').map(str::to_owned).collect()
    })
}

/// A v8 `PUT` or `PATCH`: reads the body, then has the writer make the
/// change. A path that names no key answers 400 `invalid_path`.
async fn edit_with_body(
    state: &ManagementState,
    method: V8Method,
    path: Option<Vec<String>>,
    body: Body,
    yaml: bool,
) -> Response {
    let body = match config_write::request_body(state, body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(path) = path else {
        return config_write::v8_error_response(&V8EditError::InvalidPath);
    };
    edit(
        state,
        V8Edit {
            method,
            path,
            body: body.to_vec(),
            yaml,
        },
    )
    .await
}

/// Has the writer make `edit` under the write lock, then the service load
/// the file (upstream's `ConfigV8` for a change).
async fn edit(state: &ManagementState, edit: V8Edit) -> Response {
    let writer = match config_write::writer(state) {
        Ok(writer) => writer,
        Err(response) => return response,
    };
    let state = state.clone();
    config_write::run_task(async move {
        {
            let _guard = state.config_write_lock().lock().await;
            match run_blocking(move || writer.edit_v8(&edit)).await {
                Ok(config) => state.set_config(Arc::new(config)),
                Err(error) => return config_write::v8_error_response(&error),
            }
        }
        config_write::reload(&state).await;
        json::response(
            StatusCode::OK,
            &Json::map([
                ("config-version", Json::Int(8)),
                ("status", Json::Str("ok".into())),
            ]),
        )
    })
    .await
}

/// `PUT /v0/management/config.yaml` (upstream's `PutConfigYAML`).
async fn put_config_yaml(State(state): State<ManagementState>, body: Body) -> Response {
    let body = match config_write::request_body(&state, body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let config = match Config::load_bytes(&body) {
        Ok(config) => config,
        Err(error) => return load_error(&error),
    };
    let writer = match config_write::writer(&state) {
        Ok(writer) => writer,
        Err(response) => return response,
    };
    config_write::run_task(async move {
        {
            let _guard = state.config_write_lock().lock().await;
            if run_blocking(move || writer.write_file(&body))
                .await
                .is_err()
            {
                return json::response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &Json::map([
                        ("error", Json::Str("write_failed".into())),
                        ("message", Json::Str("failed to write config".into())),
                    ]),
                );
            }
            state.set_config(Arc::new(config));
        }
        config_write::reload(&state).await;
        json::response(
            StatusCode::OK,
            &Json::map([
                ("changed", Json::Array(vec![Json::Str("config".into())])),
                ("ok", Json::Bool(true)),
            ]),
        )
    })
    .await
}

/// What a body that doesn't load answers: 400 `invalid_yaml` where
/// upstream's decode into its config type fails (a parse or type error, or
/// a v8 layout it can't flatten), else 422 `invalid_config` (the checks its
/// loader makes after decoding).
fn load_error(error: &ConfigError) -> Response {
    let message = error.to_string();
    let decode_error = match error.kind() {
        ConfigErrorKind::Syntax | ConfigErrorKind::Decode => true,
        ConfigErrorKind::Invalid => !is_load_check(&message),
        _ => false,
    };
    let (status, code, message) = if decode_error {
        let message = message.strip_prefix(LOAD_PREFIX).unwrap_or(&message);
        (StatusCode::BAD_REQUEST, "invalid_yaml", message)
    } else {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_config",
            message.as_str(),
        )
    };
    json::response(
        status,
        &Json::map([
            ("error", Json::Str(code.to_owned())),
            ("message", Json::Str(message.to_owned())),
        ]),
    )
}

/// Whether an invalid config's `message` comes from the checks upstream's
/// loader makes after decoding: a legacy list's weight, or a trusted proxy.
/// A v8 group's weight (`api-keys.<provider>.keys[i].weight`) is checked
/// while decoding.
fn is_load_check(message: &str) -> bool {
    message.starts_with("invalid trusted-proxies entry")
        || (message.contains(".weight: ") && !message.starts_with("api-keys."))
}
