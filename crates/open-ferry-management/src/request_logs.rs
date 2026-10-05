// Ported from CLIProxyAPI internal/api/handlers/management/logs.go
// (GetRequestErrorLogs, DownloadRequestErrorLog, GetRequestLogByID),
// internal/api/server_management.go and server_management_v8.go (their
// routes) (v8.0.10, MIT), with gin's Context.FileAttachment.
// https://github.com/router-for-me/CLIProxyAPI

//! The request log's routes, for clients with the management key:
//!
//! | v0 | v8 | |
//! |---|---|---|
//! | `GET /v0/management/request-error-logs` | `GET /v8/management/observability/logs/errors` | the error logs |
//! | `GET /v0/management/request-error-logs/:name` | `GET /v8/management/observability/logs/errors/:name` | one error log |
//! | `GET /v0/management/request-log-by-id/:id` | `GET /v8/management/observability/logs/requests/:id` | a request's log |
//!
//! The list has the `error-*.log` files of the log directory, newest
//! first, as `{"files":[{"name","size","modified"}]}`, `modified` in Unix
//! seconds; it is empty while `request-log` is on, as upstream's is. A
//! request's log is the newest file whose name ends with `-<id>.log`, `id`
//! the request ID's last eight characters, so the full ID or the short one
//! finds it; the ID may also be given as `?id=`. A log is sent as an
//! attachment named after its file.
//!
//! The directory is the one the binary resolved at start (see
//! [`log_directory`]), as upstream's handler reads it. The request logger
//! takes a relative directory from the config file's directory instead, as
//! upstream's does, so with a relative log directory and a config file
//! elsewhere than the working directory these routes look where the logs
//! aren't, as upstream's do.
//!
//! Deviations from upstream:
//! - A log that is a symbolic link or other reparse point, isn't a plain
//!   file, or has more than one hard link is refused with upstream's
//!   answer to a directory, a 400 `invalid log file`, not followed (see
//!   [`open_log_file`]). It is checked on the handle the log is read from.
//! - A log is read whole and sent as `text/plain; charset=utf-8`, with its
//!   `Last-Modified`; Go's `http.ServeFile` sniffs the type and answers
//!   range and conditional requests.
//! - Files with the same change time are listed in name order; upstream's
//!   sort leaves their order open.
//! - The errors' texts are Rust's.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read as _};
use std::path::{Path as FsPath, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, RawQuery, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::{DateTime, Utc};
use http::{HeaderValue, StatusCode, Uri, header};
use open_ferry_core::observe::request_log::{is_error_log_name, log_file_is_newer};
use open_ferry_core::observe::short_request_id;

use crate::Route;
use crate::auth_files::run_blocking;
use crate::go::lossy;
use crate::json::{self, Json};
use crate::log_dir::{Access, is_refused, log_directory, open_log_file};
use crate::query::Query;
use crate::state::ManagementState;

/// The module's routes.
pub(crate) fn routes() -> Vec<Route> {
    vec![
        Route::key("/v0/management/request-error-logs", get(list)),
        Route::key("/v0/management/request-error-logs/{name}", get(download)),
        Route::key("/v0/management/request-log-by-id/{id}", get(by_id)),
        Route::key("/v8/management/observability/logs/errors", get(list)),
        Route::key(
            "/v8/management/observability/logs/errors/{name}",
            get(download),
        ),
        Route::key(
            "/v8/management/observability/logs/requests/{id}",
            get(by_id),
        ),
    ]
}

/// `GET /v0/management/request-error-logs` (upstream's
/// `GetRequestErrorLogs`): the error logs, newest first, or none while
/// `request-log` is on.
async fn list(State(state): State<ManagementState>) -> Response {
    if state.config().request_log {
        return files_response(Vec::new());
    }
    let dir = log_directory(&state);
    if is_blank(&dir) {
        return not_configured();
    }
    run_blocking(move || list_error_logs(&dir)).await
}

/// The error logs of `dir`, as [`list`] answers them.
fn list_error_logs(dir: &FsPath) -> Response {
    let entries = match read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return files_response(Vec::new()),
        Err(error) => {
            return internal(&format!("failed to list request error logs: {error}"));
        }
    };
    let mut files = Vec::new();
    for entry in entries {
        if entry.is_dir {
            continue;
        }
        if !is_error_log_name(&entry.name) {
            continue;
        }
        let meta = match fs::symlink_metadata(&entry.path) {
            Ok(meta) => meta,
            Err(error) => {
                return internal(&format!(
                    "failed to read log info for {}: {error}",
                    entry.name
                ));
            }
        };
        let modified = meta.modified().map(unix_seconds).unwrap_or_default();
        files.push((entry.name, meta.len(), modified));
    }
    files.sort_by_key(|file| std::cmp::Reverse(file.2));
    files_response(
        files
            .into_iter()
            .map(|(name, size, modified)| {
                Json::Struct(vec![
                    ("name", Json::Str(name)),
                    ("size", Json::Uint(size)),
                    ("modified", Json::Int(modified)),
                ])
            })
            .collect(),
    )
}

/// `{"files":files}`.
fn files_response(files: Vec<Json>) -> Response {
    json::response(StatusCode::OK, &Json::map([("files", Json::Array(files))]))
}

/// `GET /v0/management/request-error-logs/:name` (upstream's
/// `DownloadRequestErrorLog`): error log `name`.
async fn download(
    State(state): State<ManagementState>,
    name: Result<Path<String>, PathRejection>,
    uri: Uri,
) -> Response {
    let dir = log_directory(&state);
    if is_blank(&dir) {
        return not_configured();
    }
    let name = param(name, &uri);
    let name = name.trim().to_owned();
    if name.is_empty() || name.contains(['/', '\\']) {
        return json::error(StatusCode::BAD_REQUEST, "invalid log file name");
    }
    if !is_error_log_name(&name) {
        return json::error(StatusCode::NOT_FOUND, "log file not found");
    }
    run_blocking(move || serve(&dir, OsString::from(&name), &name)).await
}

/// `GET /v0/management/request-log-by-id/:id` (upstream's
/// `GetRequestLogByID`): the newest log of the request with ID `id`.
async fn by_id(
    State(state): State<ManagementState>,
    id: Result<Path<String>, PathRejection>,
    uri: Uri,
    RawQuery(raw): RawQuery,
) -> Response {
    let dir = log_directory(&state);
    if is_blank(&dir) {
        return not_configured();
    }
    let mut id = param(id, &uri).trim().to_owned();
    if id.is_empty() {
        id = lossy(Query::parse(raw.as_deref()).value("id"))
            .trim()
            .to_owned();
    }
    if id.is_empty() {
        return json::error(StatusCode::BAD_REQUEST, "missing request ID");
    }
    if id.contains(['/', '\\']) {
        return json::error(StatusCode::BAD_REQUEST, "invalid request ID");
    }
    run_blocking(move || serve_by_id(&dir, &id)).await
}

/// The newest log in `dir` of the request with ID `id`, as [`by_id`]
/// answers it.
fn serve_by_id(dir: &FsPath, id: &str) -> Response {
    let entries = match read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return json::error(StatusCode::NOT_FOUND, "log directory not found");
        }
        Err(error) => return internal(&format!("failed to list log directory: {error}")),
    };
    let suffix = format!("-{}.log", short_request_id(id));
    let mut matched: Option<(Entry, Option<SystemTime>)> = None;
    for entry in entries {
        if entry.is_dir || !entry.name.ends_with(&suffix) {
            continue;
        }
        match fs::symlink_metadata(&entry.path).and_then(|meta| meta.modified()) {
            Ok(modified) => {
                let newer = matched.as_ref().is_none_or(|(current, current_mod)| {
                    log_file_is_newer(&entry.name, modified, &current.name, *current_mod)
                });
                if newer {
                    matched = Some((entry, Some(modified)));
                }
            }
            Err(_) => {
                if matched.is_none() {
                    matched = Some((entry, None));
                }
            }
        }
    }
    match matched {
        Some((entry, _)) => serve(dir, entry.file_name, &entry.name),
        None => json::error(
            StatusCode::NOT_FOUND,
            "log file not found for the given request ID",
        ),
    }
}

/// A file of the log directory.
struct Entry {
    /// Its name as the system has it.
    file_name: OsString,
    /// Its name as text.
    name: String,
    /// Its path.
    path: PathBuf,
    /// Whether it is a directory (not following a link).
    is_dir: bool,
}

/// The files of `dir`, by name, as Go's `os.ReadDir` lists them.
fn read_dir(dir: &FsPath) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_name = entry.file_name();
        entries.push(Entry {
            name: file_name.to_string_lossy().into_owned(),
            path: entry.path(),
            is_dir: entry.file_type().is_ok_and(|kind| kind.is_dir()),
            file_name,
        });
    }
    entries.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    Ok(entries)
}

/// File `file_name` of `dir` as an attachment named `name` (upstream's
/// checks, then gin's `FileAttachment`).
fn serve(dir: &FsPath, file_name: OsString, name: &str) -> Response {
    // Go's `filepath.Abs`, which cleans the path too.
    let dir = match std::path::absolute(dir) {
        Ok(dir) => open_ferry_core::auth::path::clean(&dir),
        Err(error) => return internal(&format!("failed to resolve log directory: {error}")),
    };
    let path = dir.join(file_name);
    // Upstream's `os.Stat` and `ServeFile`'s open, in one.
    let (mut file, meta) = match open_log_file(&path, Access::Read) {
        Ok(opened) => opened,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return json::error(StatusCode::NOT_FOUND, "log file not found");
        }
        Err(error) if is_refused(&error) => {
            return json::error(StatusCode::BAD_REQUEST, "invalid log file");
        }
        Err(error) => return serve_error(&error),
    };
    let mut data = Vec::new();
    if let Err(error) = file.read_to_end(&mut data) {
        return serve_error(&error);
    }
    let mut response = Response::new(Body::from(data));
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&content_disposition(name)) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    if let Some(modified) = meta.modified().ok().filter(|time| *time != UNIX_EPOCH) {
        let modified = DateTime::<Utc>::from(modified)
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string();
        if let Ok(value) = HeaderValue::from_str(&modified) {
            headers.insert(header::LAST_MODIFIED, value);
        }
    }
    response
}

/// What Go's `http.ServeFile` answers when the file can't be read.
fn serve_error(error: &io::Error) -> Response {
    let (status, text) = match error.kind() {
        io::ErrorKind::NotFound => (StatusCode::NOT_FOUND, "404 page not found\n"),
        io::ErrorKind::PermissionDenied => (StatusCode::FORBIDDEN, "403 Forbidden\n"),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "500 Internal Server Error\n",
        ),
    };
    let mut response = (status, text).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// gin's `Content-Disposition` for an attachment named `name`.
fn content_disposition(name: &str) -> String {
    if name.is_ascii() {
        let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
        format!("attachment; filename=\"{escaped}\"")
    } else {
        format!("attachment; filename*=UTF-8''{}", query_escape(name))
    }
}

/// `text` as Go's `url.QueryEscape` gives it.
fn query_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for &byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte));
            }
            b' ' => out.push('+'),
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// The route's path parameter; a parameter that isn't UTF-8 once decoded
/// is taken as it is in the path.
fn param(param: Result<Path<String>, PathRejection>, uri: &Uri) -> String {
    match param {
        Ok(Path(param)) => param,
        Err(_) => uri.path().rsplit('/').next().unwrap_or_default().to_owned(),
    }
}

/// `time` in Unix seconds, rounded down, as Go's `Time.Unix` gives it.
fn unix_seconds(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
        Err(before) => {
            let before = before.duration();
            let seconds = i64::try_from(before.as_secs()).unwrap_or(i64::MAX);
            let rounded = if before.subsec_nanos() > 0 { 1 } else { 0 };
            seconds.saturating_add(rounded).saturating_neg()
        }
    }
}

/// Whether `dir` is empty once trimmed.
fn is_blank(dir: &FsPath) -> bool {
    dir.as_os_str().to_string_lossy().trim().is_empty()
}

fn not_configured() -> Response {
    internal("log directory not configured")
}

fn internal(message: &str) -> Response {
    json::error(StatusCode::INTERNAL_SERVER_ERROR, message)
}
