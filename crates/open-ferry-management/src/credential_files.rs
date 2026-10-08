// Ported from CLIProxyAPI internal/api/handlers/management/
// auth_files_crud.go (DownloadAuthFile, UploadAuthFile,
// multipartAuthFileHeaders, storeUploadedAuthFile, writeAuthFile,
// buildAuthFromFileData, upsertAuthRecord, DeleteAuthFile,
// requestedAuthFileNamesForDelete, uniqueAuthFileNames,
// deleteAuthFileByName, findAuthForDelete, authIDForPath), auth_files.go
// (isUnsafeAuthFileName) and auth_files_fields.go (removeAuth,
// removeAuthsForPath, deleteTokenRecord) (v8.0.20, MIT), with gin-gonic/gin
// v1.10.1 context.go (ContentType, QueryArray, MultipartForm) (MIT) and
// Go's mime/multipart formdata.go (ReadForm) and multipart.go (FormName,
// FileName, parseContentDisposition), mime/mediatype.go (ParseMediaType,
// checkMediaTypeDisposition, consumeToken, consumeValue, consumeMediaParam,
// decode2231Enc, percentHexUnescape, isTSpecial, isTokenChar) and
// path/filepath (Base) (go1.26, BSD-3-Clause).
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/gin-gonic/gin
// https://github.com/golang/go

//! Credential files, each route needing the management key:
//! `GET /v0/management/auth-files/download` (also
//! `/v8/management/credentials/download`) sends one; `POST
//! /v0/management/auth-files` (also `/v8/management/credentials`) uploads
//! one or more; `DELETE` on the same paths deletes some or all.
//!
//! An upload is a `multipart/form-data` form, whose files are taken in the
//! order of their field names, or a body sent with `?name=`. A part's field
//! and file names are read from its `Content-Disposition` as Go reads them,
//! with parameter names in any case, RFC 2231 continuations, and extended
//! values (`filename*=UTF-8''...`) taken over plain ones; a part whose
//! header Go can't parse, as one giving a parameter twice, is skipped. One
//! file answers `{"status":"ok"}` or its error; several answer the names
//! uploaded, with a 207 and the failures when some failed. A file must be
//! named `*.json` and hold a credential the service serves, else it isn't
//! written, so a file that doesn't parse never replaces the one there. The
//! service is sent each file written and serves its credential once the
//! upload answers.
//!
//! A delete names its files with `?name=` (repeated for several), else in
//! a JSON body (a list of names, or `{"name":..., "names":[...]}`), and
//! answers as an upload does; `?all=true` (or `1` or `*`) deletes every
//! `*.json` file at the top of the auth directory. A name may be a
//! credential's ID or its file's name: the file removed is the
//! credential's, else the one of that name in the auth directory. The
//! service is told of each file removed and stops serving its credential.
//!
//! Names are checked as Windows needs them, on every system (see
//! [`is_unsafe_name`]), and files are only ever read, written or removed
//! at the top of the auth directory. A failure answers
//! `{"error":"<reason>"}`.
//!
//! Deviations from upstream:
//! - A name is refused with 400 `invalid name` if it holds `/`, `\` or
//!   `:` (so no path, drive, UNC share or NTFS stream), a control
//!   character or one of `<>"|?*`, ends in `.` or a space, or is a Windows
//!   device name such as `CON` or `nul.json`. Upstream refuses only a blank
//!   name, a separator and, on Windows, a volume name, and checks an
//!   uploaded file's name for `.json` only. An uploaded file's name is what
//!   follows the last `/` or `\` of its `filename` on every system, a drive
//!   such as `C:` kept (so refused); Go splits only at `/` outside Windows,
//!   and drops the drive on Windows.
//! - A field or file name's bytes that aren't UTF-8 are read as U+FFFD each,
//!   so such a file is saved with U+FFFD in its name. Go keeps the bytes
//!   outside Windows, and on Windows reads them as U+FFFD too, but for an
//!   encoded surrogate.
//! - An upload is written only if the core's file synthesizer reads a
//!   credential from it: a JSON object with a type the service serves.
//!   Upstream also writes `null`, a file without a type (registering it as
//!   provider `unknown`) and a Gemini CLI file. The reason after `invalid
//!   auth file: ` may read differently from Go's.
//! - An upload is written atomically, as every auth file is; upstream's
//!   `os.WriteFile` writes it in place. An upload over a symlink is refused
//!   with `failed to write file: ... is a symlink` (checked just before the
//!   write); upstream writes through it.
//! - A download never reads through a symlink at the file's name, nor on
//!   Windows through any reparse point, a junction included: it fails with
//!   500 `failed to read file: <path> is a symlink`. The check is made on
//!   the file opened, so no link can be put there between the check and
//!   the read: Windows opens a link itself rather than its target, and on
//!   Unix the file opened must be the one found at the name. Upstream reads
//!   through it.
//! - Uploads are bounded: a form or body over 32 MiB answers 413 `request
//!   body too large`, a file over 8 MiB (the most the service reads) fails
//!   with 413 `auth file too large`, and a form of more than 1000 parts
//!   answers 400 `invalid multipart form: multipart: message too large`, as
//!   Go's does. Upstream takes any size. The reason after `invalid
//!   multipart form: ` is the form parser's.
//! - A delete removes only a `*.json` file at the top of the auth
//!   directory. When the credential named has its file elsewhere, the
//!   delete is refused with 409 `auth file is outside the auth directory`
//!   and nothing changes; upstream removes the file wherever it is. The
//!   file is there when the directory its path names is the auth directory
//!   itself, as the file system resolves both, so case, `.`, `..` and links
//!   count as it counts them; a directory it can't resolve, as one that is
//!   gone, is elsewhere. A credential whose file isn't `*.json` is refused
//!   with 400 `name must end with .json`, where upstream removes its file.
//! - On Windows a delete matches a credential's ID or file name regardless
//!   of case when nothing matches exactly, as the file system does.
//! - When the service can't be told of a file written or removed (it has
//!   stopped), the route answers 503 with the reason; the file stays
//!   written or removed. Upstream registers the credential itself.
//! - `?all=true` deletes in name order, skips names that aren't valid
//!   UTF-8, and stops at the first file the service can't be told of.
//! - A download of a file over 8 MiB fails with a 500.
//! - The plugin host isn't ported, so there are no plugin credentials to
//!   refuse deleting.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::multipart::MultipartError;
use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, RawQuery, Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use chrono::Utc;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use open_ferry_core::auth::Auth;
use open_ferry_core::auth::file_store::MAX_AUTH_FILE_SIZE;
use open_ferry_core::auth::synthesizer::SynthesisContext;
use open_ferry_core::auth::synthesizer::file::synthesize_auth_file;
use open_ferry_core::config::AuthFile;
use open_ferry_core::manager::Manager;
use open_ferry_translate::go::{to_lower, trim_space};
use serde::de::{IgnoredAny, MapAccess};
use serde_json::Value;

use crate::Route;
use crate::auth_files::run_blocking;
use crate::bind::{self, GoStruct, Nullable, set_string};
use crate::go::{decode_rune, equal_fold, lossy};
use crate::json::{self, Json};
use crate::query::Query;
use crate::state::{CredentialStore, ManagementState, StoreUnavailable};

/// The most a form or body on the upload routes may hold.
pub(crate) const MAX_FORM: usize = 32 << 20;

/// The most parts a form may have (Go's `multipartmaxparts`).
const MAX_PARTS: usize = 1000;

/// The routes this module serves.
pub(crate) fn routes() -> Vec<Route> {
    vec![
        Route::key("/v0/management/auth-files/download", get(download)),
        Route::key("/v8/management/credentials/download", get(download)),
        Route::key(
            "/v0/management/auth-files",
            post(upload)
                .delete(delete)
                .layer(DefaultBodyLimit::max(MAX_FORM)),
        ),
        Route::key(
            "/v8/management/credentials",
            post(upload)
                .delete(delete)
                .layer(DefaultBodyLimit::max(MAX_FORM)),
        ),
    ]
}

/// Why a file couldn't be uploaded or deleted.
#[derive(Debug)]
struct Failure {
    status: StatusCode,
    message: String,
}

impl Failure {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }

    fn response(&self) -> Response {
        json::error(self.status, &self.message)
    }
}

/// `GET /v0/management/auth-files/download` (upstream's
/// `DownloadAuthFile`): file `name` of the auth directory, as an
/// attachment.
async fn download(State(state): State<ManagementState>, RawQuery(raw): RawQuery) -> Response {
    let name = match query_name(raw.as_deref()) {
        Ok(name) => name,
        Err(failure) => return failure.response(),
    };
    let Some(files) = state.store().map(Arc::clone) else {
        return StoreUnavailable.into_response();
    };
    let file = name.clone();
    let read = run_blocking(move || read_unlinked(&files.file_path(&file)?)).await;
    let data = match read {
        Ok(data) => data,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return json::error(StatusCode::NOT_FOUND, "file not found");
        }
        Err(error) => {
            return json::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("failed to read file: {error}"),
            );
        }
    };
    let mut response = Response::new(Body::from(data));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    // A safe name holds no control character or quote.
    if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    response
}

/// The file `?name=` names, trimmed: a safe name ending in `.json`.
fn query_name(raw: Option<&str>) -> Result<String, Failure> {
    let query = Query::parse(raw);
    let name = match std::str::from_utf8(trim_space(query.value("name"))) {
        Ok(name) if !is_unsafe_name(name) => name,
        _ => return Err(Failure::bad_request("invalid name")),
    };
    if !has_json_suffix(name) {
        return Err(Failure::bad_request("name must end with .json"));
    }
    Ok(name.to_owned())
}

/// `POST /v0/management/auth-files` (upstream's `UploadAuthFile`).
async fn upload(
    State(state): State<ManagementState>,
    RawQuery(raw): RawQuery,
    request: Request,
) -> Response {
    let store = match state.credential_store() {
        Ok(store) => store,
        Err(unavailable) => return unavailable.into_response(),
    };
    if !is_multipart(request.headers()) {
        return upload_body(&state, &store, raw.as_deref(), request).await;
    }
    let files = match read_form(request).await {
        Ok(form) => form.files,
        Err(FormError::TooLarge) => {
            return json::error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
        }
        Err(FormError::Invalid(reason)) => {
            return json::error(
                StatusCode::BAD_REQUEST,
                &format!("invalid multipart form: {reason}"),
            );
        }
    };
    match files.as_slice() {
        [] => json::error(StatusCode::BAD_REQUEST, "no files uploaded"),
        [file] => match store_uploaded(&state, &store, file).await {
            Ok(_) => ok(),
            Err(failure) => failure.response(),
        },
        files => {
            let mut uploaded = Vec::new();
            let mut failed = Vec::new();
            for file in files {
                match store_uploaded(&state, &store, file).await {
                    Ok(name) => uploaded.push(Json::Str(name)),
                    Err(failure) => failed.push(Json::map([
                        ("error", Json::Str(failure.message)),
                        ("name", Json::Str(base_name(&file.file_name).to_owned())),
                    ])),
                }
            }
            batch_answer("uploaded", uploaded, failed)
        }
    }
}

/// An upload sent as the body, named by `?name=`.
async fn upload_body(
    state: &ManagementState,
    store: &CredentialStore,
    raw: Option<&str>,
    request: Request,
) -> Response {
    let name = match query_name(raw) {
        Ok(name) => name,
        Err(failure) => return failure.response(),
    };
    // Read with the route's body limit.
    let data = match Bytes::from_request(request, &()).await {
        Ok(data) => data,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return json::error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
        }
        Err(_) => return json::error(StatusCode::BAD_REQUEST, "failed to read body"),
    };
    match write_auth_file(state, store, &name, data).await {
        Ok(()) => ok(),
        Err(failure) => failure.response(),
    }
}

/// `storeUploadedAuthFile`: writes an uploaded file, and returns its name.
async fn store_uploaded(
    state: &ManagementState,
    store: &CredentialStore,
    file: &FormFile,
) -> Result<String, Failure> {
    // The form's file name is already a base name, as Go's; upstream trims
    // it and takes the base name again.
    let name = base_name(trim_str(&file.file_name));
    if !has_json_suffix(name) {
        return Err(Failure::bad_request("file must be .json"));
    }
    write_auth_file(state, store, name, file.data.clone()).await?;
    Ok(name.to_owned())
}

/// `writeAuthFile`: writes `data` as file `name` of the auth directory if
/// it holds a credential the service serves, then sends it to the service.
async fn write_auth_file(
    state: &ManagementState,
    store: &CredentialStore,
    name: &str,
    data: Bytes,
) -> Result<(), Failure> {
    if is_unsafe_name(name) {
        return Err(Failure::bad_request("invalid name"));
    }
    if data.len() as u64 > MAX_AUTH_FILE_SIZE {
        return Err(Failure::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "auth file too large",
        ));
    }
    let files = Arc::clone(&store.files);
    let file = name.to_owned();
    let contents = data.clone();
    let guard = state.credential_lock().lock().await;
    let written = run_blocking(move || {
        let path = files
            .file_path(&file)
            .map_err(|error| Failure::internal(format!("failed to write file: {error}")))?;
        check_credential(&files.base_dir(), &path, &contents)?;
        if fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Err(Failure::internal(format!(
                "failed to write file: {} is a symlink",
                path.display()
            )));
        }
        files
            .write_file(&file, &contents)
            .map_err(|error| Failure::internal(format!("failed to write file: {error}")))
    })
    .await?;
    // Made under the lock, so the change takes its revision there.
    let synced = store.sync.file_written(AuthFile {
        path: written,
        data: Arc::from(data.as_ref()),
    });
    drop(guard);
    synced
        .await
        .map_err(|error| Failure::new(error.status(), error.to_string()))
}

/// `buildAuthFromFileData`: checks that the service would read a credential
/// from `data` saved at `path` in the auth directory `dir`.
fn check_credential(dir: &Path, path: &Path, data: &[u8]) -> Result<(), Failure> {
    let ctx = SynthesisContext::new(dir, Utc::now());
    let reason = match synthesize_auth_file(&ctx, path, data) {
        Ok(Some(_)) => return Ok(()),
        Err(error) => error.to_string(),
        Ok(None) if path.to_str().is_none() => "the auth directory isn't valid UTF-8".to_owned(),
        Ok(None) => skipped_reason(data),
    };
    Err(Failure::internal(format!("invalid auth file: {reason}")))
}

/// Why the synthesizer reads no credential from `data`.
fn skipped_reason(data: &[u8]) -> String {
    let kind = match serde_json::from_str::<Value>(&lossy(data)) {
        Err(error) => return error.to_string(),
        Ok(Value::Object(fields)) => {
            return match fields.get("type").and_then(Value::as_str).map(str::trim) {
                None | Some("") => "missing type".to_owned(),
                Some(kind) => format!("type {kind} isn't served"),
            };
        }
        Ok(Value::Null) => return "not a JSON object".to_owned(),
        Ok(Value::Bool(_)) => "bool",
        Ok(Value::Number(_)) => "number",
        Ok(Value::String(_)) => "string",
        Ok(Value::Array(_)) => "array",
    };
    format!("json: cannot unmarshal {kind} into Go value of type map[string]interface {{}}")
}

/// `DELETE /v0/management/auth-files` (upstream's `DeleteAuthFile`).
async fn delete(
    State(state): State<ManagementState>,
    RawQuery(raw): RawQuery,
    body: Body,
) -> Response {
    let store = match state.credential_store() {
        Ok(store) => store,
        Err(unavailable) => return unavailable.into_response(),
    };
    let query = Query::parse(raw.as_deref());
    if matches!(query.value("all"), b"true" | b"1" | b"*") {
        return delete_all(&state, &store).await;
    }
    let mut names = unique_names(query_values(raw.as_deref(), "name"));
    if names.is_empty() {
        let body = match bind::read_body(body).await {
            Ok(body) => body,
            Err(response) => return response,
        };
        match body_names(&body) {
            Some(listed) => names = unique_names(listed),
            None => return json::error(StatusCode::BAD_REQUEST, "invalid request body"),
        }
    }
    match names.as_slice() {
        [] => json::error(StatusCode::BAD_REQUEST, "invalid name"),
        [name] => match delete_one(&state, &store, name).await {
            Ok(()) => ok(),
            Err(failure) => failure.response(),
        },
        names => {
            let mut deleted = Vec::new();
            let mut failed = Vec::new();
            for name in names {
                match delete_one(&state, &store, name).await {
                    Ok(()) => deleted.push(name_json(name)),
                    Err(failure) => failed.push(Json::map([
                        ("error", Json::Str(failure.message)),
                        ("name", name_json(name)),
                    ])),
                }
            }
            batch_answer("deleted", deleted, failed)
        }
    }
}

/// `?all=true`: deletes every `*.json` file at the top of the auth
/// directory.
async fn delete_all(state: &ManagementState, store: &CredentialStore) -> Response {
    let files = Arc::clone(&store.files);
    let listed = run_blocking(move || -> io::Result<Vec<String>> {
        let mut names: Vec<String> = fs::read_dir(files.base_dir())?
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| !kind.is_dir()))
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| has_json_suffix(name))
            .collect();
        names.sort();
        Ok(names)
    })
    .await;
    let names = match listed {
        Ok(names) => names,
        Err(error) => {
            return json::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("failed to read auth dir: {error}"),
            );
        }
    };
    let mut deleted: i64 = 0;
    for name in names {
        let files = Arc::clone(&store.files);
        let guard = state.credential_lock().lock().await;
        // As upstream, a file that can't be removed is passed over.
        let Ok(path) = run_blocking(move || files.remove_file(&name)).await else {
            continue;
        };
        // Made under the lock, so the change takes its revision there.
        let synced = store.sync.file_removed(path);
        drop(guard);
        if let Err(error) = synced.await {
            return error.into_response();
        }
        deleted += 1;
    }
    json::response(
        StatusCode::OK,
        &Json::map([
            ("deleted", Json::Int(deleted)),
            ("status", Json::Str("ok".to_owned())),
        ]),
    )
}

/// `deleteAuthFileByName`: removes the file of the credential `name`
/// names, else file `name` of the auth directory, and tells the service.
async fn delete_one(
    state: &ManagementState,
    store: &CredentialStore,
    name: &[u8],
) -> Result<(), Failure> {
    let name = match std::str::from_utf8(trim_space(name)) {
        Ok(name) if !is_unsafe_name(name) => name,
        _ => return Err(Failure::bad_request("invalid name")),
    };
    let credential_path = find_auth_for_delete(state.manager(), name)
        .and_then(|auth| auth.attribute("path").map(|path| path.trim().to_owned()))
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    let file_name = match &credential_path {
        Some(path) => {
            let dir = store.files.base_dir();
            let path = path.clone();
            run_blocking(move || top_level_name(&dir, &path))
                .await
                .ok_or_else(|| {
                    Failure::new(
                        StatusCode::CONFLICT,
                        "auth file is outside the auth directory",
                    )
                })?
        }
        None => name.to_owned(),
    };
    if !has_json_suffix(&file_name) {
        return Err(Failure::bad_request("name must end with .json"));
    }
    let files = Arc::clone(&store.files);
    let guard = state.credential_lock().lock().await;
    let removed = match run_blocking(move || files.remove_file(&file_name)).await {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(Failure::new(StatusCode::NOT_FOUND, "auth file not found"));
        }
        Err(error) => return Err(Failure::internal(format!("failed to remove file: {error}"))),
    };
    // The service knows a credential by the path it was read from. The call
    // is made under the lock, so the change takes its revision there.
    let synced = store.sync.file_removed(credential_path.unwrap_or(removed));
    drop(guard);
    synced
        .await
        .map_err(|error| Failure::new(error.status(), error.to_string()))
}

/// `findAuthForDelete`: the credential with ID `name`, else the first
/// whose file name, or the last element of whose `path`, is `name`. On
/// Windows, failing that, the same regardless of case.
fn find_auth_for_delete(manager: &Manager, name: &str) -> Option<Arc<Auth>> {
    if let Some(auth) = manager.get(name) {
        return Some(auth);
    }
    let auths = manager.list();
    let named = |auth: &Auth, same: fn(&str, &str) -> bool| {
        same(auth.file_name.trim(), name)
            || auth
                .attribute("path")
                .is_some_and(|path| same(base_name(path.trim()), name))
    };
    if let Some(auth) = auths.iter().find(|auth| named(auth, |a, b| a == b)) {
        return Some(Arc::clone(auth));
    }
    if cfg!(windows) {
        // The IDs of auth files are lower-cased on Windows.
        if let Some(auth) = manager.get(&to_lower(name)) {
            return Some(auth);
        }
        return auths.into_iter().find(|auth| named(auth, equal_fold));
    }
    None
}

/// The name of the file at `path` if it is directly in `dir`: when the
/// directory `path` names is `dir` itself, as the file system resolves
/// both, so case and links are matched as it matches them. A directory
/// that can't be resolved, as one that is gone, isn't `dir`.
fn top_level_name(dir: &Path, path: &Path) -> Option<String> {
    let mut components = path.components();
    let Some(Component::Normal(name)) = components.next_back() else {
        return None;
    };
    let parent = components.as_path();
    if dir.as_os_str().is_empty() || parent.as_os_str().is_empty() {
        return None;
    }
    if fs::canonicalize(dir).ok()? != fs::canonicalize(parent).ok()? {
        return None;
    }
    name.to_str().map(str::to_owned)
}

/// The values of query parameter `name`, in order (gin's `QueryArray`).
fn query_values(raw: Option<&str>, name: &str) -> Vec<Vec<u8>> {
    // Parsed whole first, for Go's limit on the number of parameters.
    if Query::parse(raw).get(name).is_none() {
        return Vec::new();
    }
    raw.unwrap_or_default()
        .split('&')
        .filter_map(|pair| Query::parse(Some(pair)).get(name).map(<[u8]>::to_vec))
        .collect()
}

/// `uniqueAuthFileNames`: the names trimmed, without blanks or repeats.
fn unique_names(names: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(names.len());
    for name in names {
        let name = trim_space(&name);
        if !name.is_empty() && !out.iter().any(|seen| seen == name) {
            out.push(name.to_vec());
        }
    }
    out
}

/// The names a delete's body lists, as Go's `json.Unmarshal` reads them;
/// `None` when it fails.
fn body_names(body: &[u8]) -> Option<Vec<Vec<u8>>> {
    let body = trim_space(body);
    if body.is_empty() {
        return Some(Vec::new());
    }
    let text = lossy(body);
    if body.first() == Some(&b'[') {
        let names: Vec<Nullable> = serde_json::from_str(&text).ok()?;
        return Some(
            names
                .into_iter()
                .map(|Nullable(name)| name.unwrap_or_default().into_bytes())
                .collect(),
        );
    }
    let parsed = bind::decode::<DeleteBody>(body)?;
    // Unlike a decoder, `json.Unmarshal` takes one value and nothing more.
    serde_json::from_str::<IgnoredAny>(&text).ok()?;
    let mut names = Vec::new();
    if !parsed.name.trim().is_empty() {
        names.push(parsed.name.into_bytes());
    }
    names.extend(
        parsed
            .names
            .unwrap_or_default()
            .into_iter()
            .map(String::into_bytes),
    );
    Some(names)
}

/// A delete's body as an object.
#[derive(Default)]
struct DeleteBody {
    name: String,
    names: Option<Vec<String>>,
}

impl GoStruct for DeleteBody {
    const FIELDS: &'static [&'static str] = &["name", "names"];

    fn set<'de, A: MapAccess<'de>>(&mut self, index: usize, map: &mut A) -> Result<(), A::Error> {
        if index == 0 {
            return set_string(&mut self.name, map);
        }
        self.names = map.next_value::<Option<Vec<Nullable>>>()?.map(|names| {
            names
                .into_iter()
                .map(|Nullable(name)| name.unwrap_or_default())
                .collect()
        });
        Ok(())
    }
}

/// A name as JSON, as Go writes a string that may not be UTF-8.
fn name_json(name: &[u8]) -> Json {
    match std::str::from_utf8(name) {
        Ok(name) => Json::Str(name.to_owned()),
        Err(_) => Json::Bytes(name.to_vec()),
    }
}

/// `{"status":"ok"}`.
fn ok() -> Response {
    json::response(
        StatusCode::OK,
        &Json::map([("status", Json::Str("ok".to_owned()))]),
    )
}

/// The answer to an upload or delete of several files: those done, counted
/// under `count` and named under `files`, and with a 207 those that failed.
fn batch_answer(count: &str, done: Vec<Json>, failed: Vec<Json>) -> Response {
    let done_count = Json::Int(i64::try_from(done.len()).unwrap_or(i64::MAX));
    if failed.is_empty() {
        return json::response(
            StatusCode::OK,
            &Json::map([
                (count, done_count),
                ("files", Json::Array(done)),
                ("status", Json::Str("ok".to_owned())),
            ]),
        );
    }
    json::response(
        StatusCode::MULTI_STATUS,
        &Json::map([
            (count, done_count),
            ("failed", Json::Array(failed)),
            ("files", Json::Array(done)),
            ("status", Json::Str("partial".to_owned())),
        ]),
    )
}

/// Whether `name` can't name a file at the top of the auth directory on
/// every system (upstream's `isUnsafeAuthFileName`, made stricter): it is
/// blank; holds `/`, `\` or `:` (a path, drive, UNC share or NTFS stream),
/// a control character or one of `<>"|?*`; ends in `.` or a space, which
/// Windows drops (so `.` and `..` too); or is a device name Windows
/// reserves.
pub(crate) fn is_unsafe_name(name: &str) -> bool {
    name.trim().is_empty()
        || name.chars().any(|c| {
            c.is_control() || matches!(c, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')
        })
        || name.ends_with(['.', ' '])
        || is_device_name(name)
}

/// Whether Windows takes `name` for a device: `CON`, `PRN`, `AUX`, `NUL`,
/// `COM0` to `COM9` and `LPT0` to `LPT9` (with a superscript digit too),
/// `CONIN$` or `CONOUT$`, in any case, before any extension and trailing
/// spaces.
fn is_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    let stem = stem.to_ascii_uppercase();
    match stem.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        stem => ["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix).is_some_and(|rest| {
                let mut chars = rest.chars();
                matches!(
                    (chars.next(), chars.next()),
                    (Some('0'..='9' | '\u{b9}' | '\u{b2}' | '\u{b3}'), None)
                )
            })
        }),
    }
}

/// Whether `name` ends in `.json`, case aside.
pub(crate) fn has_json_suffix(name: &str) -> bool {
    to_lower(name).ends_with(".json")
}

/// Go's `filepath.Base` on Windows, less volume names: what follows the
/// last `/` or `\`, past any at the end; `.` for an empty path, `\` for
/// a path of separators only.
fn base_name(path: &str) -> &str {
    // Splitting at ASCII bytes leaves the parts UTF-8.
    std::str::from_utf8(base_bytes(path.as_bytes())).unwrap_or(path)
}

/// [`base_name`] of bytes that needn't be UTF-8.
fn base_bytes(path: &[u8]) -> &[u8] {
    let is_separator = |b: &u8| matches!(b, b'/' | b'\x5c');
    if path.is_empty() {
        return b".";
    }
    let end = path
        .iter()
        .rposition(|b| !is_separator(b))
        .map_or(0, |last| last + 1);
    let trimmed = path.get(..end).unwrap_or_default();
    if trimmed.is_empty() {
        return b"\x5c";
    }
    trimmed.rsplit(is_separator).next().unwrap_or(trimmed)
}

/// Go's `strings.TrimSpace`.
fn trim_str(text: &str) -> &str {
    std::str::from_utf8(trim_space(text.as_bytes())).unwrap_or(text)
}

/// Why a file wasn't read: its name is a link, at this path.
#[derive(Debug)]
pub(crate) struct Linked(PathBuf);

impl fmt::Display for Linked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} is a symlink", self.0.display())
    }
}

impl std::error::Error for Linked {}

/// The error of a read refused because `path` is a link.
pub(crate) fn linked(path: &Path) -> io::Error {
    io::Error::other(Linked(path.to_path_buf()))
}

/// Whether `error` is a read refused because the file's name is a link.
pub(crate) fn is_linked(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|inner| inner.is::<Linked>())
}

/// Whether `meta` is a link's: a symlink, or any reparse point, a junction
/// included.
#[cfg(windows)]
fn is_link(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || meta.file_type().is_symlink()
}

/// Reads the file at `path` as
/// [`read_capped`](open_ferry_core::auth::file_store::read_capped) does, at most
/// [`MAX_AUTH_FILE_SIZE`] bytes, but never through a link: a symlink or
/// reparse point at `path` is refused with [`Linked`], checked on the file
/// opened so that one can't be put there between the check and the read.
pub(crate) fn read_unlinked(path: &Path) -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    open_unlinked(path)?
        .take(MAX_AUTH_FILE_SIZE + 1)
        .read_to_end(&mut data)?;
    if data.len() as u64 > MAX_AUTH_FILE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            format!("file exceeds {MAX_AUTH_FILE_SIZE} bytes"),
        ));
    }
    Ok(data)
}

/// Opens `path` to read, refusing a link: Windows opens a link itself
/// rather than its target (`FILE_FLAG_OPEN_REPARSE_POINT`), then refuses
/// what it opened if that is a link.
#[cfg(windows)]
fn open_unlinked(path: &Path) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let opened = fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path);
    let file = match opened {
        Ok(file) => file,
        // A link to a directory, as a junction, doesn't open as a file.
        Err(_) if fs::symlink_metadata(path).is_ok_and(|meta| is_link(&meta)) => {
            return Err(linked(path));
        }
        Err(error) => return Err(error),
    };
    if is_link(&file.metadata()?) {
        return Err(linked(path));
    }
    Ok(file)
}

/// Opens `path` to read, refusing a link: Unix refuses a symlink at
/// `path`, then a file opened that isn't the one found there, as one a
/// symlink put there since leads to.
#[cfg(unix)]
fn open_unlinked(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::MetadataExt;
    let found = fs::symlink_metadata(path)?;
    if found.file_type().is_symlink() {
        return Err(linked(path));
    }
    let file = fs::File::open(path)?;
    let opened = file.metadata()?;
    if (opened.dev(), opened.ino()) != (found.dev(), found.ino()) {
        return Err(linked(path));
    }
    Ok(file)
}

/// Opens `path` to read, refusing a symlink found there first.
#[cfg(not(any(unix, windows)))]
fn open_unlinked(path: &Path) -> io::Result<fs::File> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(linked(path));
    }
    fs::File::open(path)
}

/// Gin's `c.ContentType() == "multipart/form-data"`: the first
/// `Content-Type`, up to a space or `;`, compared exactly.
pub(crate) fn is_multipart(headers: &HeaderMap) -> bool {
    let value = headers
        .get(header::CONTENT_TYPE)
        .map(HeaderValue::as_bytes)
        .unwrap_or_default();
    let end = value
        .iter()
        .position(|&b| b == b' ' || b == b';')
        .unwrap_or(value.len());
    value.get(..end) == Some(b"multipart/form-data".as_slice())
}

/// A `multipart/form-data` form, as Go's `ReadForm` reads it. Its `Debug`
/// shows names and sizes only, for its files and values may hold tokens
/// and private keys.
#[derive(Default)]
pub(crate) struct Form {
    /// The parts sent with a file name, in the order of their field names.
    pub(crate) files: Vec<FormFile>,
    /// The other parts, as field name and value, in order.
    pub(crate) values: Vec<(String, Bytes)>,
}

impl Form {
    /// The first value of field `name`.
    pub(crate) fn value(&self, name: &str) -> Option<&[u8]> {
        self.values
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.as_ref())
    }

    /// The first file of field `name` (Go's `FormFile`).
    pub(crate) fn file(&self, name: &str) -> Option<&FormFile> {
        self.files.iter().find(|file| file.field == name)
    }
}

impl fmt::Debug for Form {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let values: Vec<(&String, ByteCount)> = self
            .values
            .iter()
            .map(|(name, value)| (name, ByteCount(value.len())))
            .collect();
        f.debug_struct("Form")
            .field("files", &self.files)
            .field("values", &values)
            .finish()
    }
}

/// A file of a form. Its `Debug` shows names and size only.
pub(crate) struct FormFile {
    /// Its field's name.
    pub(crate) field: String,
    /// The base name of the `filename` it was sent with (Go's `FileName`).
    pub(crate) file_name: String,
    /// Its contents.
    pub(crate) data: Bytes,
}

impl fmt::Debug for FormFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FormFile")
            .field("field", &self.field)
            .field("file_name", &self.file_name)
            .field("data", &ByteCount(self.data.len()))
            .finish()
    }
}

/// A size in bytes, shown in place of the bytes.
struct ByteCount(usize);

impl fmt::Debug for ByteCount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} bytes", self.0)
    }
}

/// Why a form couldn't be read.
#[derive(Debug)]
pub(crate) enum FormError {
    /// It is over the route's body limit.
    TooLarge,
    /// It isn't a valid form, for this reason.
    Invalid(String),
}

/// Reads the `multipart/form-data` form of `request`, of at most the
/// route's body limit and [`MAX_PARTS`] parts. Parts without a field name
/// are skipped, as Go skips them.
pub(crate) async fn read_form(request: Request) -> Result<Form, FormError> {
    let Ok(mut multipart) = Multipart::from_request(request, &()).await else {
        return Err(FormError::Invalid(
            "no multipart boundary param in Content-Type".to_owned(),
        ));
    };
    let mut form = Form::default();
    // Each file with its field name's bytes, which Go sorts by.
    let mut files = Vec::new();
    let mut parts = 0;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => return Err(form_error(&error)),
        };
        parts += 1;
        if parts > MAX_PARTS {
            return Err(FormError::Invalid(
                "multipart: message too large".to_owned(),
            ));
        }
        let disposition = field
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .map(HeaderValue::as_bytes)
            .unwrap_or_default();
        let (field_name, file_name) = part_names(disposition);
        let data = field.bytes().await.map_err(|error| form_error(&error))?;
        if field_name.is_empty() {
            continue;
        }
        let field = lossy(&field_name);
        if file_name.is_empty() {
            form.values.push((field, data));
        } else {
            let file_name = lossy(&file_name);
            files.push((
                field_name,
                FormFile {
                    field,
                    file_name,
                    data,
                },
            ));
        }
    }
    // Go keeps files by field name, and upstream takes the names sorted.
    files.sort_by(|a, b| a.0.cmp(&b.0));
    form.files = files.into_iter().map(|(_, file)| file).collect();
    Ok(form)
}

/// A part's field and file names, as Go's `multipart.Part` reads them from
/// its `Content-Disposition` (`FormName` and `FileName`): the `name`
/// parameter, only when the disposition is `form-data`, and the
/// [`base_bytes`] of the `filename` parameter. Each is empty when absent or
/// empty, and both when the header doesn't parse.
pub(crate) fn part_names(disposition: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let Some((kind, params)) = parse_media_type(disposition) else {
        return (Vec::new(), Vec::new());
    };
    let field = match params.get("name") {
        Some(name) if kind == "form-data" => name.clone(),
        _ => Vec::new(),
    };
    let file = match params.get("filename") {
        Some(name) if !name.is_empty() => base_bytes(name).to_vec(),
        _ => Vec::new(),
    };
    (field, file)
}

/// Go's `mime.ParseMediaType`: the media type or disposition `value`
/// names, lower case, and its parameters by lower-case name, RFC 2231
/// continuations joined and extended values (`name*=charset'lang'value`)
/// decoded and put first. `None` when it doesn't parse: the type isn't a
/// token (or two joined by `/`), a parameter is malformed, or one is given
/// twice with different values.
fn parse_media_type(value: &[u8]) -> Option<(String, HashMap<String, Vec<u8>>)> {
    let base_len = value.iter().position(|&b| b == b';').unwrap_or(value.len());
    let (base, mut rest) = value.split_at_checked(base_len)?;
    let kind = to_lower(&lossy(base)).trim().to_owned();
    if !is_media_type(kind.as_bytes()) {
        return None;
    }
    let mut params = HashMap::new();
    // The parameters whose names hold a `*`, by the name before it.
    let mut continued: HashMap<String, HashMap<String, Vec<u8>>> = HashMap::new();
    while !rest.is_empty() {
        rest = trim_left_space(rest);
        if rest.is_empty() {
            break;
        }
        let Some((key, value, after)) = consume_param(rest) else {
            // One `;` at the end is let be.
            if trim_space(rest) == b";" {
                break;
            }
            return None;
        };
        let map = match key.split_once('*') {
            Some((name, _)) => continued.entry(name.to_owned()).or_default(),
            None => &mut params,
        };
        if map.get(&key).is_some_and(|old| *old != value) {
            return None;
        }
        map.insert(key, value);
        rest = after;
    }
    for (name, pieces) in continued {
        if let Some(value) = pieces.get(&format!("{name}*")) {
            if let Some(decoded) = decode_2231(value) {
                params.insert(name, decoded);
            }
            continue;
        }
        let mut joined = Vec::new();
        let mut found = false;
        for n in 0_usize.. {
            let simple = format!("{name}*{n}");
            if let Some(value) = pieces.get(&simple) {
                found = true;
                joined.extend_from_slice(value);
                continue;
            }
            let Some(value) = pieces.get(&format!("{simple}*")) else {
                break;
            };
            found = true;
            let decoded = if n == 0 {
                decode_2231(value)
            } else {
                percent_unescape(value)
            };
            joined.extend(decoded.unwrap_or_default());
        }
        if found {
            params.insert(name, joined);
        }
    }
    Some((kind, params))
}

/// Go's `checkMediaTypeDisposition`: whether `kind` is a token, or two
/// joined by `/`.
fn is_media_type(kind: &[u8]) -> bool {
    let (main, rest) = consume_token(kind);
    if main.is_empty() {
        return false;
    }
    if rest.is_empty() {
        return true;
    }
    let Some(rest) = rest.strip_prefix(b"/") else {
        return false;
    };
    let (sub, rest) = consume_token(rest);
    !sub.is_empty() && rest.is_empty()
}

/// Go's `consumeMediaParam`: the `;` and parameter at the start of `rest`,
/// as its lower-case name, its value and what follows.
fn consume_param(rest: &[u8]) -> Option<(String, Vec<u8>, &[u8])> {
    let rest = trim_left_space(rest).strip_prefix(b";")?;
    let (name, rest) = consume_token(trim_left_space(rest));
    if name.is_empty() {
        return None;
    }
    let rest = trim_left_space(trim_left_space(rest).strip_prefix(b"=")?);
    let (value, after) = consume_value(rest);
    if value.is_empty() && after.len() == rest.len() {
        return None;
    }
    Some((lossy(name).to_ascii_lowercase(), value, after))
}

/// Go's `consumeValue`: the token or quoted string at the start of `rest`,
/// and what follows; empty, with all of `rest`, when there is none. In a
/// quoted string `\` escapes a special character only, and before any
/// other is kept, as Go keeps the `\` of a Windows path a browser sends.
fn consume_value(rest: &[u8]) -> (Vec<u8>, &[u8]) {
    let Some(mut quoted) = rest.strip_prefix(b"\"") else {
        let (token, after) = consume_token(rest);
        return (token.to_vec(), after);
    };
    let mut value = Vec::new();
    loop {
        match quoted {
            [b'"', after @ ..] => return (value, after),
            [b'\x5c', c, after @ ..] if is_special(*c) => {
                value.push(*c);
                quoted = after;
            }
            [] | [b'\r' | b'\n', ..] => return (Vec::new(), rest),
            [c, after @ ..] => {
                value.push(*c);
                quoted = after;
            }
        }
    }
}

/// Go's `consumeToken`: the token at the start of `rest`, and what
/// follows.
fn consume_token(rest: &[u8]) -> (&[u8], &[u8]) {
    let end = rest
        .iter()
        .position(|&b| !is_token_byte(b))
        .unwrap_or(rest.len());
    rest.split_at_checked(end).unwrap_or((rest, b""))
}

/// Whether `b` may be in a token: printable ASCII, not a space nor
/// special.
fn is_token_byte(b: u8) -> bool {
    b > 0x20 && b < 0x7f && !is_special(b)
}

/// Go's `isTSpecial`: whether `b` is one of `()<>@,;:\"/[]?=`.
fn is_special(b: u8) -> bool {
    b"()<>@,;:\x5c\"/[]?=".contains(&b)
}

/// Go's `decode2231Enc`: an RFC 2231 extended value,
/// `charset'language'value`, its charset US-ASCII or UTF-8 in any case, and
/// its value percent-decoded; the language is ignored.
fn decode_2231(value: &[u8]) -> Option<Vec<u8>> {
    let (charset, rest) = split_at_byte(value, b'\'')?;
    let (_language, encoded) = split_at_byte(rest, b'\'')?;
    match to_lower(&lossy(charset)).as_str() {
        "us-ascii" | "utf-8" => percent_unescape(encoded),
        _ => None,
    }
}

/// Go's `percentHexUnescape`: `value` with each `%` and the two hex digits
/// after it read as that byte; `None` when a `%` isn't followed by two.
fn percent_unescape(value: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(value.len());
    let mut bytes = value.iter();
    while let Some(&b) = bytes.next() {
        if b != b'%' {
            out.push(b);
            continue;
        }
        let mut digit = || bytes.next().and_then(|&d| char::from(d).to_digit(16));
        let (high, low) = (digit()?, digit()?);
        out.push(u8::try_from((high << 4) | low).ok()?);
    }
    Some(out)
}

/// `bytes` before and after the first `at`, if it holds one.
fn split_at_byte(bytes: &[u8], at: u8) -> Option<(&[u8], &[u8])> {
    let index = bytes.iter().position(|&b| b == at)?;
    Some((bytes.get(..index)?, bytes.get(index + 1..)?))
}

/// Go's `strings.TrimLeftFunc(s, unicode.IsSpace)`.
fn trim_left_space(mut bytes: &[u8]) -> &[u8] {
    while let Some((c, width)) = decode_rune(bytes)
        && c.is_whitespace()
    {
        bytes = bytes.get(width..).unwrap_or_default();
    }
    bytes
}

/// What a form parser's error means for the request.
fn form_error(error: &MultipartError) -> FormError {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        FormError::TooLarge
    } else {
        FormError::Invalid(error.body_text())
    }
}
