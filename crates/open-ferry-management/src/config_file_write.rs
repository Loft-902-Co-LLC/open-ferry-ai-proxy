// Ported from CLIProxyAPI internal/api/handlers/management/config_basic.go
// (PutConfigYAML), config_v8.go (ConfigV8's PUT, PATCH and DELETE, as the
// route hands them to the writer) and internal/api/server_management.go
// (their routes) (v8.0.27, MIT).
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
//! - Each of those writes may send `If-Match` (see [`crate::config_etag`]):
//!   one that doesn't match the file as it is now answers 412
//!   `{"error":"precondition_failed"}` with the current `ETag`, before the
//!   body or the path is looked at, and writes nothing. A saved change
//!   answers the new file's `ETag`.
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
//! - `If-Match` is checked under the config write lock, which every write
//!   through this API takes, on the file as it is read just before the
//!   writer reads it. Another process that writes the file in between (as
//!   `open-ferry config` can) isn't caught; upstream's check is under its
//!   handler lock alone, so the same holds there.
//! - Those of [`crate::config_write`].

use std::sync::Arc;

use axum::body::Body;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{MethodRouter, put};
use http::{HeaderMap, StatusCode};
use open_ferry_core::config::save::sha256_hex;
use open_ferry_core::config::{Config, ConfigError, ConfigErrorKind};

use crate::Route;
use crate::auth_files::run_blocking;
use crate::config_etag::{if_match_lines, if_match_satisfied, precondition_failed, with_etag};
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
                |State(state): State<ManagementState>, headers: HeaderMap, body: Body| async move {
                    edit_with_body(
                        &state,
                        &headers,
                        V8Method::Put,
                        Some(Vec::new()),
                        body,
                        true,
                    )
                    .await
                },
            ),
        ),
        Route::key(
            "/v8/management/config/{*path}",
            put(
                |State(state): State<ManagementState>,
                 headers: HeaderMap,
                 path: Result<Path<String>, PathRejection>,
                 body: Body| async move {
                    edit_with_body(&state, &headers, V8Method::Put, parts(path), body, false).await
                },
            )
            .patch(
                |State(state): State<ManagementState>,
                 headers: HeaderMap,
                 path: Result<Path<String>, PathRejection>,
                 body: Body| async move {
                    edit_with_body(&state, &headers, V8Method::Patch, parts(path), body, false)
                        .await
                },
            )
            .delete(
                |State(state): State<ManagementState>,
                 headers: HeaderMap,
                 path: Result<Path<String>, PathRejection>| async move {
                    let if_match = if_match_lines(&headers);
                    match parts(path) {
                        Some(path) => {
                            edit(
                                &state,
                                if_match,
                                V8Edit {
                                    method: V8Method::Delete,
                                    path,
                                    body: Vec::new(),
                                    yaml: false,
                                },
                            )
                            .await
                        }
                        None => refuse(&state, if_match, V8EditError::NotFound, true).await,
                    }
                },
            ),
        ),
    ]
}

/// `PUT` and `PATCH` of the whole config, as JSON.
fn whole_config() -> MethodRouter<ManagementState> {
    put(
        |State(state): State<ManagementState>, headers: HeaderMap, body: Body| async move {
            edit_with_body(
                &state,
                &headers,
                V8Method::Put,
                Some(Vec::new()),
                body,
                false,
            )
            .await
        },
    )
    .patch(
        |State(state): State<ManagementState>, headers: HeaderMap, body: Body| async move {
            edit_with_body(
                &state,
                &headers,
                V8Method::Patch,
                Some(Vec::new()),
                body,
                false,
            )
            .await
        },
    )
}

/// `DELETE /v8/management/config/`: the path is empty, which the writer
/// refuses.
async fn delete_whole(State(state): State<ManagementState>, headers: HeaderMap) -> Response {
    edit(
        &state,
        if_match_lines(&headers),
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
    headers: &HeaderMap,
    method: V8Method,
    path: Option<Vec<String>>,
    body: Body,
    yaml: bool,
) -> Response {
    let body = match config_write::request_body(state, body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let if_match = if_match_lines(headers);
    let Some(path) = path else {
        return refuse(state, if_match, V8EditError::InvalidPath, false).await;
    };
    edit(
        state,
        if_match,
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
/// the file (upstream's `ConfigV8` for a change). With `If-Match` lines, the
/// file must still have the tag one of them names, else the answer is 412
/// and nothing is written. The answer carries the `ETag` of the file
/// written.
async fn edit(state: &ManagementState, if_match: Vec<String>, edit: V8Edit) -> Response {
    let writer = match config_write::writer(state) {
        Ok(writer) => writer,
        Err(response) => return response,
    };
    let state = state.clone();
    config_write::run_task(async move {
        let sha256 = {
            let _guard = state.config_write_lock().lock().await;
            if let Err(response) = check_if_match(&state, &if_match).await {
                return response;
            }
            match run_blocking(move || writer.edit_v8(&edit)).await {
                Ok((config, sha256)) => {
                    state.set_loaded_config(Arc::new(config), sha256.clone());
                    sha256
                }
                Err(error) => return config_write::v8_error_response(&error),
            }
        };
        // The version actually on disk: the writer's hash of what it wrote,
        // else the file read back. Without either, upstream sends no tag
        // rather than guess one.
        let sha256 = match sha256 {
            Some(sha256) => Some(sha256),
            None => {
                let state = state.clone();
                run_blocking(move || state.read_config_file().ok().map(|data| sha256_hex(&data)))
                    .await
            }
        };
        config_write::reload(&state).await;
        let response = json::response(
            StatusCode::OK,
            &Json::map([
                ("config-version", Json::Int(8)),
                ("status", Json::Str("ok".into())),
            ]),
        );
        match sha256 {
            Some(sha256) => with_etag(response, &sha256),
            None => response,
        }
    })
    .await
}

/// A write that is refused before any change is made, with `error`: the
/// `If-Match` is still checked first, as upstream checks it before it reads
/// the path or the body. With `writer_first`, a service without a writer
/// answers its own error before either.
async fn refuse(
    state: &ManagementState,
    if_match: Vec<String>,
    error: V8EditError,
    writer_first: bool,
) -> Response {
    if writer_first && let Err(response) = config_write::writer(state) {
        return response;
    }
    let state = state.clone();
    config_write::run_task(async move {
        let _guard = state.config_write_lock().lock().await;
        match check_if_match(&state, &if_match).await {
            Ok(()) => config_write::v8_error_response(&error),
            Err(response) => response,
        }
    })
    .await
}

/// Checks the `If-Match` lines against the config file as it is now: `Ok`
/// when there are none, or one passes; else the 412 with the file's current
/// tag, or 500 `read_failed` when the file can't be read. Called under the
/// config write lock, so no write of this API lands between the check and
/// the change.
async fn check_if_match(state: &ManagementState, if_match: &[String]) -> Result<(), Response> {
    if if_match.is_empty() {
        return Ok(());
    }
    let state = state.clone();
    let hash = run_blocking(move || state.read_config_file().map(|data| sha256_hex(&data))).await;
    let Ok(hash) = hash else {
        return Err(json::error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "read_failed",
        ));
    };
    if if_match_satisfied(if_match, &hash) {
        Ok(())
    } else {
        Err(precondition_failed(&hash))
    }
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
            let sha256 = match run_blocking(move || writer.write_file(&body)).await {
                Ok(sha256) => sha256,
                Err(error) if error.is_switching() => {
                    return json::error(StatusCode::SERVICE_UNAVAILABLE, config_write::SWITCHING);
                }
                Err(_) => {
                    return json::response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &Json::map([
                            ("error", Json::Str("write_failed".into())),
                            ("message", Json::Str("failed to write config".into())),
                        ]),
                    );
                }
            };
            state.set_loaded_config(Arc::new(config), sha256);
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
