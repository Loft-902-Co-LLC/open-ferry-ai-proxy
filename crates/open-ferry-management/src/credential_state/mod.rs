// Ported from CLIProxyAPI internal/api/handlers/management/
// auth_files_fields.go (PatchAuthFileStatus, applyAuthDisabledState,
// PatchAuthFileFields, normalizeAuthFilePatchFields,
// decodeAuthFileRequestRetryPatch, rootAuthFileField,
// setAuthFileMetadataValue, applyAuthFileHeadersPatch,
// authFileHeadersStringMap, syncAuthFileMetadataFields and the attribute
// syncs it calls, authFileIntValue, authFileBoolValue),
// auth_files_refresh.go (RefreshAuthFiles) and auth_files.go
// (lookupAuthFile, matchesAuthFileLookup) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A credential's state: turning it on and off, changing its settings and
//! refreshing its tokens.
//!
//! - `PATCH /v0/management/auth-files/status` (also
//!   `/v8/management/credentials/status`) turns a credential on or off. The
//!   body names it (`name`, an ID or file name, and `auth_index` to pick one
//!   of several) and sets `disabled`.
//! - `PATCH /v0/management/auth-files/fields` (also
//!   `/v8/management/credentials/fields`) sets fields of a credential's
//!   metadata, a dotted name reaching into objects, and brings the settings
//!   read from them (prefix, proxy URL, headers, priority, weight, note,
//!   websockets, disabled, Codex plan type) up to date.
//! - `POST /v0/management/auth-files/refresh` (also
//!   `/v8/management/credentials/refresh`) refreshes one credential's tokens
//!   now, or every credential's with `all`, and answers with the refreshed
//!   credential, its tokens included, as upstream does: the route is behind
//!   the management key.
//!
//! A status or field change is saved to the credential's file by the
//! manager, then handed to the running service, which registers the
//! credential's models again: a credential turned back on gets its models
//! back. The credential lock is held while the change is made and saved,
//! never while the service applies it. A refresh goes through the manager,
//! which refreshes one credential at a time per ID, saves the result and
//! publishes it to the service itself; refreshing all of them runs at most
//! the manager's `refresh_workers` at once.
//!
//! As upstream, changing fields of a credential from a config API key
//! changes it in the running service only: such a credential has no file,
//! and the change is gone when the config is next loaded.
//!
//! Deviations from upstream:
//! - Turning a credential from a config API key on or off answers 409
//!   `{"error":"config API key credentials are managed in the config file,
//!   which is never written"}` and changes nothing. Upstream adds `*` to the
//!   key's `excluded-models` in the config file and saves it.
//! - Status and field changes answer 503 `{"error":"credential store
//!   unavailable"}` when the API has no credential store or service to hand
//!   the change to, and 503 when the service has stopped (the foundation's
//!   rule); upstream answers 500 when its hook fails.
//! - Fields are applied, and the first bad one reported, in body order.
//!   Upstream walks a Go map, so with two bad fields which one it reports
//!   varies, as does which of two headers that trim to the same name wins.
//! - A field change holds the credential lock, as a status change does;
//!   upstream takes no lock for it.
//! - A change to a credential removed meanwhile answers 404 `auth file not
//!   found`; upstream hands its stale copy to the hook, registering it
//!   again.
//! - A refresh body that fails to decode answers 400 `invalid request body`
//!   without Go's decoder error after it.
//! - When two credentials match a name, the first by ID is taken; upstream
//!   takes whichever its map yields first.
//! - A field body nested more than 127 deep is refused as an invalid
//!   request body, and so is a field whose dotted parts plus its value's
//!   own depth pass 127, which would nest the credential's file deeper
//!   than the store reads back. Go's decoder allows 10000, and upstream
//!   builds whatever a dotted name asks for, writing a file it can't read
//!   back past that.
//! - A query value that isn't UTF-8 is read with each bad byte replaced by
//!   U+FFFD.
//! - There is no plugin host, so there are no plugin virtual credentials
//!   and none of upstream's handling of them (409 for a virtual child,
//!   toggling every credential of a source file).
//! - The refreshed credential is written by [`auth_json`], with that
//!   module's deviations.

mod auth_json;

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{RawQuery, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{patch, post};
use chrono::Utc;
use http::StatusCode;
use open_ferry_core::auth::classification::{
    ATTRIBUTE_SOURCE_BACKEND, ATTRIBUTE_WEIGHT, AUTH_SOURCE_FILE,
};
use open_ferry_core::auth::metadata::{
    ATTRIBUTE_FILE_PRIORITY, canonical_credential_metadata_key,
    extract_custom_headers_from_metadata, normalize_credential_metadata,
};
use open_ferry_core::auth::weight::{parse_weight_str, parse_weight_value};
use open_ferry_core::auth::{Auth, AuthKind, AuthSource, Status};
use open_ferry_core::manager::Manager;
use open_ferry_providers::codex::jwt::{parse_jwt_token, plan_type_or_default};
use open_ferry_translate::go::quote;
use serde::Deserialize as _;
use serde::de::MapAccess;
use serde_json::{Map, Value};

pub(crate) use self::auth_json::auth_json;
use crate::Route;
use crate::auth_files::{auth_index, run_blocking};
use crate::bind::{self, GoStruct, set_string};
use crate::go::{atoi, equal_fold, lossy, parse_bool};
use crate::json::{self, Json};
use crate::query::Query;
use crate::state::ManagementState;

/// The answer to a status change on a credential from a config API key.
const CONFIG_API_KEY_REFUSAL: &str =
    "config API key credentials are managed in the config file, which is never written";

/// The status message of a credential turned off here.
const DISABLED_MESSAGE: &str = "disabled via management API";

/// The deepest the store reads a credential's file back: `serde_json`
/// refuses arrays and objects nested 128 deep.
const MAX_FILE_DEPTH: usize = 127;

/// The routes this module serves.
pub(crate) fn routes() -> Vec<Route> {
    vec![
        Route::key("/v0/management/auth-files/status", patch(status)),
        Route::key("/v8/management/credentials/status", patch(status)),
        Route::key("/v0/management/auth-files/fields", patch(fields)),
        Route::key("/v8/management/credentials/fields", patch(fields)),
        Route::key("/v0/management/auth-files/refresh", post(refresh)),
        Route::key("/v8/management/credentials/refresh", post(refresh)),
    ]
}

/// The body of a status change.
#[derive(Default)]
struct StatusRequest {
    name: String,
    auth_index: String,
    disabled: Option<bool>,
}

impl GoStruct for StatusRequest {
    const FIELDS: &'static [&'static str] = &["name", "auth_index", "disabled"];

    fn set<'de, A: MapAccess<'de>>(&mut self, index: usize, map: &mut A) -> Result<(), A::Error> {
        match index {
            0 => set_string(&mut self.name, map),
            1 => set_string(&mut self.auth_index, map),
            // A `*bool`: `null` sets it back to nil.
            _ => {
                self.disabled = map.next_value::<Option<bool>>()?;
                Ok(())
            }
        }
    }
}

/// The body of a refresh.
#[derive(Default)]
struct RefreshRequest {
    name: String,
    auth_index: String,
    all: bool,
}

impl GoStruct for RefreshRequest {
    const FIELDS: &'static [&'static str] = &["name", "auth_index", "all"];

    fn set<'de, A: MapAccess<'de>>(&mut self, index: usize, map: &mut A) -> Result<(), A::Error> {
        match index {
            0 => set_string(&mut self.name, map),
            1 => set_string(&mut self.auth_index, map),
            _ => {
                if let Some(all) = map.next_value::<Option<bool>>()? {
                    self.all = all;
                }
                Ok(())
            }
        }
    }
}

/// The credential `name` (an ID or a file name) and `index` name, after
/// trimming (upstream's `lookupAuthFile`). Without an index, the ID is
/// tried first.
fn lookup(manager: &Manager, name: &str, index: &str) -> Option<Arc<Auth>> {
    let name = name.trim();
    let index = index.trim();
    if name.is_empty() {
        return None;
    }
    if index.is_empty() {
        return manager.get(name).or_else(|| {
            manager
                .list()
                .into_iter()
                .find(|auth| auth.file_name.trim() == name)
        });
    }
    manager.list().into_iter().find(|auth| {
        (auth.id.trim() == name || auth.file_name.trim() == name) && auth_index(auth) == index
    })
}

/// Whether `auth` comes from an API key in the config (upstream's
/// `IsConfigAPIKeyAuth`).
fn is_config_api_key(auth: &Auth) -> bool {
    auth.auth_kind() == Some(AuthKind::ApiKey)
        && auth.auth_source_kind() == Some(AuthSource::Config)
}

/// Turns `auth` on or off (upstream's `applyAuthDisabledState`).
fn apply_disabled_state(auth: &mut Auth, disabled: bool) {
    auth.disabled = disabled;
    if disabled {
        auth.status = Status::Disabled;
        auth.status_message = DISABLED_MESSAGE.to_owned();
    } else {
        auth.status = Status::Active;
        auth.status_message.clear();
    }
    auth.updated_at = Some(Utc::now());
    auth.metadata
        .insert("disabled".into(), Value::Bool(disabled));
}

/// `PATCH /v0/management/auth-files/status` (upstream's
/// `PatchAuthFileStatus`).
async fn status(State(state): State<ManagementState>, body: Body) -> Response {
    let store = match state.credential_store() {
        Ok(store) => store,
        Err(unavailable) => return unavailable.into_response(),
    };
    let body = match bind::read_body(body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(request) = bind::decode::<StatusRequest>(&body) else {
        return json::error(StatusCode::BAD_REQUEST, "invalid request body");
    };
    if request.name.trim().is_empty() {
        return json::error(StatusCode::BAD_REQUEST, "name is required");
    }
    let Some(disabled) = request.disabled else {
        return json::error(StatusCode::BAD_REQUEST, "disabled is required");
    };

    let guard = state.credential_lock().lock().await;
    let manager = state.manager().clone();
    let Some(target) = lookup(&manager, &request.name, &request.auth_index) else {
        return json::error(StatusCode::NOT_FOUND, "auth file not found");
    };
    if is_config_api_key(&target) {
        return json::error(StatusCode::CONFLICT, CONFIG_API_KEY_REFUSAL);
    }
    let mut auth = Auth::clone(&target);
    apply_disabled_state(&mut auth, disabled);
    // The update saves the credential to its file.
    let updated = match run_blocking(move || manager.update(auth)).await {
        Err(error) => {
            return json::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("failed to update auth: {error}"),
            );
        }
        Ok(None) => return json::error(StatusCode::NOT_FOUND, "auth file not found"),
        Ok(Some(updated)) => updated,
    };
    // Made under the lock, so the change takes its revision there.
    let synced = store.sync.upsert(Auth::clone(&updated));
    drop(guard);

    if let Err(error) = synced.await {
        tracing::error!(auth_id = %updated.id, "post-auth persist hook failed for status update: {error}");
        return json::error(
            error.status(),
            &format!("failed to synchronize auth runtime: {error}"),
        );
    }
    json::response(
        StatusCode::OK,
        &Json::map([
            ("status", Json::Str("ok".into())),
            ("disabled", Json::Bool(disabled)),
        ]),
    )
}

/// One field of a field change: its path, as normalized, and its value.
struct Field {
    path: String,
    value: Value,
}

/// A field change's body: its fields in body order, or `None` where Go's
/// decode into a `map[string]json.RawMessage` fails. `null` holds no
/// fields.
fn decode_fields(body: &[u8]) -> Option<Map<String, Value>> {
    let text = lossy(body);
    let mut deserializer = serde_json::Deserializer::from_str(&text);
    match Value::deserialize(&mut deserializer) {
        Ok(Value::Object(fields)) => Some(fields),
        Ok(Value::Null) => Some(Map::new()),
        _ => None,
    }
}

/// How deep `value` nests: 0 for a scalar, 1 for an array or object of
/// scalars, and so on.
fn value_depth(value: &Value) -> usize {
    let mut deepest = 0;
    let mut pending = vec![(value, 0)];
    while let Some((value, depth)) = pending.pop() {
        let depth = depth + 1;
        match value {
            Value::Array(items) => pending.extend(items.iter().map(|item| (item, depth))),
            Value::Object(map) => pending.extend(map.values().map(|item| (item, depth))),
            _ => continue,
        }
        deepest = deepest.max(depth);
    }
    deepest
}

/// Whether one of `fields` would nest the credential's file deeper than
/// the store reads back. The file's object holds a field's first part, and
/// each further part is an object inside the one before, so the file nests
/// at least as deep as the parts plus the value's own depth.
fn nests_too_deep(fields: &Map<String, Value>) -> bool {
    fields
        .iter()
        .any(|(key, value)| key.split('.').count() + value_depth(value) > MAX_FILE_DEPTH)
}

/// The fields with their paths normalized: the key trimmed, each dotted
/// part trimmed and the root renamed from a legacy spelling (upstream's
/// `normalizeAuthFilePatchFields`). Where two keys reach one path, the one
/// spelled as normalized wins; two spelled alike are an error.
fn normalize_fields(fields: Map<String, Value>) -> Result<Vec<Field>, String> {
    struct Seen {
        original: String,
        canonical: bool,
    }
    let mut normalized: Vec<Field> = Vec::with_capacity(fields.len());
    let mut seen: HashMap<String, (usize, Seen)> = HashMap::with_capacity(fields.len());
    for (key, value) in fields {
        let mut parts = key.trim().split('.').map(str::trim);
        let original_root = parts.next().unwrap_or_default();
        let root = canonical_credential_metadata_key(original_root);
        let canonical = root == original_root;
        let mut path = root.to_owned();
        for part in parts {
            path.push('.');
            path.push_str(part);
        }
        if let Some((position, existing)) = seen.get_mut(&path) {
            if existing.canonical != canonical {
                if canonical && let Some(field) = normalized.get_mut(*position) {
                    field.value = value;
                    existing.original = key;
                    existing.canonical = true;
                }
                continue;
            }
            return Err(format!(
                "auth file fields {} and {} refer to the same field",
                quote(&existing.original),
                quote(&key)
            ));
        }
        seen.insert(
            path.clone(),
            (
                normalized.len(),
                Seen {
                    original: key,
                    canonical,
                },
            ),
        );
        normalized.push(Field { path, value });
    }
    Ok(normalized)
}

/// The part of a field path before its first dot, trimmed (upstream's
/// `rootAuthFileField`).
fn root_field(path: &str) -> &str {
    let path = path.trim();
    path.split_once('.').map_or(path, |(root, _)| root.trim())
}

/// The `request_retry` change among `fields`, taken out of them: `None`
/// when there is none, else the new value, `None` to remove it (upstream's
/// `decodeAuthFileRequestRetryPatch`).
fn take_request_retry(fields: &mut Vec<Field>) -> Result<Option<Option<i64>>, &'static str> {
    const NOT_INTEGER: &str = "request_retry must be an integer or null";
    if fields
        .iter()
        .any(|field| root_field(&field.path) == "request_retry" && field.path != "request_retry")
    {
        return Err("request_retry does not support nested fields");
    }
    let Some(position) = fields
        .iter()
        .position(|field| field.path == "request_retry")
    else {
        return Ok(None);
    };
    let field = fields.remove(position);
    match field.value {
        Value::Null => Ok(Some(None)),
        Value::Number(number) => match number.as_i64() {
            Some(retry) if retry < 0 => Ok(Some(None)),
            Some(retry) => Ok(Some(Some(retry))),
            None => Err(NOT_INTEGER),
        },
        _ => Err(NOT_INTEGER),
    }
}

/// Sets `value` at the dotted `path` of `metadata`, making each object on
/// the way, over any other value there (upstream's
/// `setAuthFileMetadataValue`).
fn set_metadata_value(
    metadata: &mut Map<String, Value>,
    path: &str,
    value: Value,
) -> Result<(), String> {
    let invalid = || format!("invalid field path: {path}");
    let parts: Vec<&str> = path.split('.').map(str::trim).collect();
    if parts.iter().any(|part| part.is_empty()) {
        return Err(invalid());
    }
    let Some((last, intermediate)) = parts.split_last() else {
        return Err(invalid());
    };
    let mut current = metadata;
    for part in intermediate {
        let entry = current
            .entry((*part).to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(Map::new());
        }
        let Value::Object(next) = entry else {
            return Err(invalid());
        };
        current = next;
    }
    current.insert((*last).to_owned(), value);
    Ok(())
}

/// Applies a `headers` change (upstream's `applyAuthFileHeadersPatch`): an
/// object of strings is merged into the headers there, an empty value
/// removing one; anything else replaces them.
fn apply_headers_patch(metadata: &mut Map<String, Value>, value: Value) {
    let patch = match value {
        Value::Object(patch) if patch.values().all(Value::is_string) => patch,
        other => {
            metadata.insert("headers".into(), other);
            return;
        }
    };
    let mut headers = extract_custom_headers_from_metadata(metadata);
    for (name, value) in &patch {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let value = value.as_str().unwrap_or_default().trim();
        if value.is_empty() {
            headers.remove(name);
        } else {
            headers.insert(name.to_owned(), value.to_owned());
        }
    }
    if headers.is_empty() {
        metadata.shift_remove("headers");
        return;
    }
    let headers = headers
        .into_iter()
        .map(|(name, value)| (name, Value::String(value)))
        .collect();
    metadata.insert("headers".into(), Value::Object(headers));
}

/// A whole number from a metadata value (upstream's `authFileIntValue`): a
/// number, as `json.Number.Int64` reads it, or a string holding one.
fn int_value(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => atoi(text.trim()),
        _ => None,
    }
}

/// A boolean from a metadata value (upstream's `authFileBoolValue`): a
/// boolean, or a string holding one.
fn bool_value(value: Option<&Value>) -> Option<bool> {
    match value? {
        Value::Bool(b) => Some(*b),
        Value::String(text) => parse_bool(text.trim()),
        _ => None,
    }
}

/// Brings the settings read from the metadata fields under `roots` up to
/// date (upstream's `syncAuthFileMetadataFields`).
fn sync_metadata_fields(auth: &mut Auth, roots: &BTreeSet<String>) {
    let touched = |root: &str| roots.contains(root);
    if touched("prefix")
        && let Some(Value::String(prefix)) = auth.metadata.get("prefix")
    {
        auth.prefix = prefix.trim().to_owned();
    }
    if touched("proxy_url")
        && let Some(Value::String(proxy_url)) = auth.metadata.get("proxy_url")
    {
        auth.proxy_url = proxy_url.trim().to_owned();
    }
    if touched("headers") {
        auth.attributes.retain(|key, _| !key.starts_with("header:"));
        for (name, value) in extract_custom_headers_from_metadata(&auth.metadata) {
            auth.attributes.insert(format!("header:{name}"), value);
        }
    }
    if touched("priority") {
        sync_priority(auth);
    }
    if touched(ATTRIBUTE_WEIGHT) {
        match auth.metadata.get(ATTRIBUTE_WEIGHT).map(parse_weight_value) {
            Some(Ok(weight)) => {
                auth.attributes
                    .insert(ATTRIBUTE_WEIGHT.to_owned(), weight.to_string());
            }
            _ => {
                auth.attributes.remove(ATTRIBUTE_WEIGHT);
            }
        }
    }
    if touched("note") {
        match auth.metadata.get("note") {
            Some(Value::String(note)) if !note.trim().is_empty() => {
                let note = note.trim().to_owned();
                auth.attributes.insert("note".into(), note);
            }
            _ => {
                auth.attributes.remove("note");
            }
        }
    }
    if touched("websockets") {
        match bool_value(auth.metadata.get("websockets")) {
            Some(websockets) => {
                auth.attributes
                    .insert("websockets".into(), websockets.to_string());
            }
            None => {
                auth.attributes.remove("websockets");
            }
        }
    }
    if touched("disabled") {
        sync_disabled(auth);
    }
    if touched("plan_type") || touched("id_token") {
        sync_plan_type(auth);
    }
}

/// Upstream's `syncAuthFilePriorityAttribute`.
fn sync_priority(auth: &mut Auth) {
    let Some(priority) = int_value(auth.metadata.get("priority")) else {
        auth.attributes.remove("priority");
        auth.attributes.remove(ATTRIBUTE_FILE_PRIORITY);
        return;
    };
    if auth
        .attributes
        .get(ATTRIBUTE_SOURCE_BACKEND)
        .is_some_and(|backend| backend == AUTH_SOURCE_FILE)
    {
        auth.attributes
            .insert(ATTRIBUTE_FILE_PRIORITY.to_owned(), "true".to_owned());
    }
    if priority == 0 {
        auth.attributes.remove("priority");
    } else {
        auth.attributes
            .insert("priority".into(), priority.to_string());
    }
}

/// Upstream's `syncAuthFileDisabledState`.
fn sync_disabled(auth: &mut Auth) {
    let Some(disabled) = bool_value(auth.metadata.get("disabled")) else {
        return;
    };
    auth.disabled = disabled;
    if disabled {
        auth.status = Status::Disabled;
        if auth.status_message.trim().is_empty() {
            auth.status_message = DISABLED_MESSAGE.to_owned();
        }
    } else {
        auth.status = Status::Active;
        auth.status_message.clear();
    }
}

/// Upstream's `syncAuthFilePlanTypeAttribute`: a Codex credential's plan
/// type from its metadata, else from its ID token.
fn sync_plan_type(auth: &mut Auth) {
    if !equal_fold(auth.provider.trim(), "codex") {
        return;
    }
    let text = |key: &str| match auth.metadata.get(key) {
        Some(Value::String(text)) if !text.trim().is_empty() => Some(text.as_str()),
        _ => None,
    };
    let plan_type = if let Some(plan_type) = text("plan_type") {
        Some(plan_type.trim().to_owned())
    } else {
        text("id_token")
            .map(|id_token| plan_type_or_default(parse_jwt_token(id_token).ok().as_ref()))
    };
    match plan_type {
        Some(plan_type) => {
            auth.attributes.insert("plan_type".into(), plan_type);
        }
        None => {
            auth.attributes.remove("plan_type");
        }
    }
}

/// Applies `fields` and the `request_retry` change to `auth`, then brings
/// the settings read from them up to date. Returns whether anything
/// changed, or the 400 answer's message.
fn apply_fields(
    auth: &mut Auth,
    fields: Vec<Field>,
    request_retry: Option<Option<i64>>,
) -> Result<bool, String> {
    let mut changed = false;
    let mut roots = BTreeSet::new();
    for Field { path, value } in fields {
        let path = path.trim();
        if path.is_empty() {
            return Err("field name is required".to_owned());
        }
        let root = root_field(path);
        if path == ATTRIBUTE_WEIGHT {
            match value {
                Value::Null => {
                    auth.metadata.shift_remove(ATTRIBUTE_WEIGHT);
                }
                Value::Number(number) => {
                    let weight =
                        parse_weight_str(&number.to_string()).map_err(|error| error.to_string())?;
                    auth.metadata
                        .insert(ATTRIBUTE_WEIGHT.to_owned(), Value::from(weight));
                }
                _ => return Err("weight must be an integer".to_owned()),
            }
        } else if root == ATTRIBUTE_WEIGHT {
            return Err("weight does not support nested fields".to_owned());
        } else if path == "headers" {
            apply_headers_patch(&mut auth.metadata, value);
        } else {
            set_metadata_value(&mut auth.metadata, path, value)?;
        }
        if !root.is_empty() {
            roots.insert(root.to_owned());
        }
        changed = true;
    }
    if let Some(retry) = request_retry {
        match retry {
            Some(retry) => {
                auth.metadata
                    .insert("request_retry".into(), Value::from(retry));
            }
            None => {
                auth.metadata.shift_remove("request_retry");
            }
        }
        changed = true;
    }
    if changed {
        sync_metadata_fields(auth, &roots);
    }
    Ok(changed)
}

/// `PATCH /v0/management/auth-files/fields` (upstream's
/// `PatchAuthFileFields`).
async fn fields(State(state): State<ManagementState>, body: Body) -> Response {
    let store = match state.credential_store() {
        Ok(store) => store,
        Err(unavailable) => return unavailable.into_response(),
    };
    let body = match bind::read_body(body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    // A field nesting the file deeper than the store reads back is refused
    // before anything is built from it.
    let Some(mut request) = decode_fields(&body).filter(|fields| !nests_too_deep(fields)) else {
        return json::error(StatusCode::BAD_REQUEST, "invalid request body");
    };
    let name = match request.shift_remove("name") {
        Some(Value::String(name)) if !name.trim().is_empty() => name.trim().to_owned(),
        _ => return json::error(StatusCode::BAD_REQUEST, "name is required"),
    };
    let mut fields = match normalize_fields(request) {
        Ok(fields) => fields,
        Err(message) => return json::error(StatusCode::BAD_REQUEST, &message),
    };
    let request_retry = match take_request_retry(&mut fields) {
        Ok(request_retry) => request_retry,
        Err(message) => return json::error(StatusCode::BAD_REQUEST, message),
    };

    let guard = state.credential_lock().lock().await;
    let manager = state.manager().clone();
    let target = manager.get(&name).or_else(|| {
        manager
            .list()
            .into_iter()
            .find(|auth| auth.file_name == name)
    });
    let Some(target) = target else {
        return json::error(StatusCode::NOT_FOUND, "auth file not found");
    };
    let mut auth = Auth::clone(&target);
    normalize_credential_metadata(&mut auth.metadata);
    match apply_fields(&mut auth, fields, request_retry) {
        Err(message) => return json::error(StatusCode::BAD_REQUEST, &message),
        Ok(false) => return json::error(StatusCode::BAD_REQUEST, "no fields to update"),
        Ok(true) => {}
    }
    auth.updated_at = Some(Utc::now());
    // The update saves the credential to its file.
    let updated = match run_blocking(move || manager.update(auth)).await {
        Err(error) => {
            return json::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("failed to update auth: {error}"),
            );
        }
        Ok(None) => return json::error(StatusCode::NOT_FOUND, "auth file not found"),
        Ok(Some(updated)) => updated,
    };
    // Made under the lock, so the change takes its revision there.
    let synced = store.sync.upsert(Auth::clone(&updated));
    drop(guard);

    if let Err(error) = synced.await {
        return json::error(
            error.status(),
            &format!("post-auth persist hook failed: {error}"),
        );
    }
    json::response(
        StatusCode::OK,
        &Json::map([("status", Json::Str("ok".into()))]),
    )
}

/// `POST /v0/management/auth-files/refresh` (upstream's
/// `RefreshAuthFiles`). The manager saves and publishes what a refresh
/// brings, so there is no hook to call, as upstream calls none.
async fn refresh(
    State(state): State<ManagementState>,
    RawQuery(raw): RawQuery,
    body: Body,
) -> Response {
    let body = match bind::read_body(body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    // An empty body, or one of only whitespace, is Go's `io.EOF`: no body.
    let mut request = if body
        .iter()
        .all(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
    {
        RefreshRequest::default()
    } else {
        match bind::decode::<RefreshRequest>(&body) {
            Some(request) => request,
            None => return json::error(StatusCode::BAD_REQUEST, "invalid request body"),
        }
    };
    let query = Query::parse(raw.as_deref());
    if query.value("all") == b"true" {
        request.all = true;
    }
    let query_name = lossy(query.value("name"));
    if !query_name.trim().is_empty() && request.name.is_empty() {
        request.name = query_name.trim().to_owned();
    }
    let query_index = lossy(query.value("auth_index"));
    if !query_index.trim().is_empty() && request.auth_index.is_empty() {
        request.auth_index = query_index.trim().to_owned();
    }

    let manager = state.manager();
    if request.all {
        let results = manager
            .force_refresh_all()
            .await
            .into_iter()
            .map(|result| {
                let mut fields = vec![
                    ("id", Json::Str(result.id)),
                    ("success", Json::Bool(result.success)),
                ];
                if let Some(error) = result.error.filter(|error| !error.is_empty()) {
                    fields.push(("error", Json::Str(error)));
                }
                Json::Struct(fields)
            })
            .collect();
        return json::response(
            StatusCode::OK,
            &Json::map([("ok", Json::Bool(true)), ("results", Json::Array(results))]),
        );
    }

    if request.name.trim().is_empty() {
        return json::error(StatusCode::BAD_REQUEST, "name or all=true is required");
    }
    let Some(target) = lookup(manager, &request.name, &request.auth_index) else {
        return json::error(StatusCode::NOT_FOUND, "auth file not found");
    };
    match manager.force_refresh(&target.id).await {
        Err(error) => json::error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
        Ok(refreshed) => json::response(
            StatusCode::OK,
            &Json::map([("auth", auth_json(&refreshed)), ("ok", Json::Bool(true))]),
        ),
    }
}
