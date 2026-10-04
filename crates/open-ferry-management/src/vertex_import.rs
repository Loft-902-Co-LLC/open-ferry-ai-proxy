// Ported from CLIProxyAPI internal/api/handlers/management/
// vertex_import.go (ImportVertexCredential, valueAsString,
// sanitizeVertexFilePart, labelForVertex) and auth_files_v8.go
// (ImportOAuthV8) (v8.0.10, MIT), with Go's fmt print.go (Sprint of a
// decoded JSON value) and strconv ftoa.go (%v of a float64) (go1.26,
// BSD-3-Clause).
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/golang/go

//! Importing a Vertex AI service account as a credential, with the
//! management key: `POST /v0/management/vertex/import`, and `POST
//! /v8/management/oauth/import?provider=vertex`.
//!
//! The account's JSON key file is sent as field `file` of a
//! `multipart/form-data` form, with the region in field `location`, else
//! in `?location=`, else `us-central1`. Its `private_key` must be an RSA
//! key, which is saved as PKCS #1 (see
//! [`normalize_service_account`]), and it must have a `project_id`. The
//! credential is saved as `vertex-<project_id>.json` in the auth directory
//! (with `/`, `\` and `:` made `_` and spaces `-`), as a login's is (see
//! [`save_token_record`]), and served once the route answers
//! `{"auth-file":<path>,"email":...,"location":...,"project_id":...,
//! "status":"ok"}`.
//!
//! On the v8 route, `provider` (trimmed, case aside) picks the importer:
//! none answers 400 `provider is required`, and any but `vertex` 404
//! `provider_not_found`.
//!
//! Failures answer `{"error":<what>}`, with a `message` saying why for
//! `invalid json`, `invalid service account` and `save_failed`. The
//! private key is never quoted or logged.
//!
//! Deviations from upstream:
//! - A form over 32 MiB answers 413 `request body too large`, and a file
//!   over 8 MiB (the most the service reads) 413 `auth file too large`;
//!   upstream takes any size.
//! - The `message` of `invalid json` is the JSON parser's, which reads
//!   differently from Go's.
//! - The key file is read with a JSON parser that takes at most 127 levels
//!   of nesting, the account's own object the first: a file nested 128 or
//!   more deep, as one with a field holding 127 nested arrays, answers 400
//!   `invalid json` (`recursion limit exceeded at line ... column ...`),
//!   where Go reads up to 10000 levels. The saved file holds the account a
//!   level deeper, under `service_account`, and the service reads it with
//!   the same limit, so an account exactly 127 deep is saved and answers
//!   200 but isn't served (the service logs the file); Go serves it.
//! - A `project_id` that gives a file name Windows can't hold (with a
//!   control character or one of `<>"|?*`) is refused with 400 `invalid
//!   service account`, on every system. A symlink where the file would go
//!   is refused with 500 `save_failed`, checked just before the save while
//!   the credential lock is held, as [`save_token_record`] describes;
//!   upstream writes through it and takes no lock.
//! - Without a credential store or sync the route answers 503 `credential
//!   store unavailable` before reading the form; a credential saved that
//!   the service can't be told of (it has stopped) answers 503
//!   `save_failed`. Upstream answers 500 `save_failed` when it has no token
//!   store.
//! - The file is written as the store writes every credential, compact and
//!   atomically, rather than indented by upstream's Vertex storage; the
//!   service account's numbers are written as they came, where Go writes
//!   them as float64. The key's PEM headers are dropped.

use axum::extract::{DefaultBodyLimit, RawQuery, Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use http::StatusCode;
use open_ferry_core::auth::Auth;
use open_ferry_core::auth::file_store::MAX_AUTH_FILE_SIZE;
use open_ferry_providers::gemini::normalize_service_account;
use open_ferry_translate::go::{to_lower, trim_space};
use serde_json::{Map, Value};

use crate::Route;
use crate::credential_files::{FormError, MAX_FORM, is_unsafe_name, read_form};
use crate::go::lossy;
use crate::json::{self, Json};
use crate::query::Query;
use crate::state::ManagementState;
use crate::token_record::save_token_record;

/// The region when none is given.
const DEFAULT_LOCATION: &str = "us-central1";

/// The routes this module serves.
pub(crate) fn routes() -> Vec<Route> {
    vec![
        Route::key(
            "/v0/management/vertex/import",
            post(import_vertex).layer(DefaultBodyLimit::max(MAX_FORM)),
        ),
        Route::key(
            "/v8/management/oauth/import",
            post(import_v8).layer(DefaultBodyLimit::max(MAX_FORM)),
        ),
    ]
}

/// `POST /v8/management/oauth/import` (upstream's `ImportOAuthV8`): the
/// importer for `?provider=`.
async fn import_v8(
    State(state): State<ManagementState>,
    RawQuery(raw): RawQuery,
    request: Request,
) -> Response {
    let query = Query::parse(raw.as_deref());
    let provider = to_lower(&lossy(trim_space(query.value("provider"))));
    match provider.as_str() {
        "" => json::error(StatusCode::BAD_REQUEST, "provider is required"),
        "vertex" => import(&state, raw.as_deref(), request).await,
        _ => json::error(StatusCode::NOT_FOUND, "provider_not_found"),
    }
}

/// `POST /v0/management/vertex/import` (upstream's
/// `ImportVertexCredential`).
async fn import_vertex(
    State(state): State<ManagementState>,
    RawQuery(raw): RawQuery,
    request: Request,
) -> Response {
    import(&state, raw.as_deref(), request).await
}

/// Imports the service account `request` sends.
async fn import(state: &ManagementState, raw: Option<&str>, request: Request) -> Response {
    let store = match state.credential_store() {
        Ok(store) => store,
        Err(unavailable) => return unavailable.into_response(),
    };
    if store.files.base_dir().as_os_str().is_empty() {
        return json::error(
            StatusCode::SERVICE_UNAVAILABLE,
            "auth directory not configured",
        );
    }
    let form = match read_form(request).await {
        Ok(form) => form,
        Err(FormError::TooLarge) => {
            return json::error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
        }
        Err(FormError::Invalid(_)) => {
            return json::error(StatusCode::BAD_REQUEST, "file required");
        }
    };
    let Some(file) = form.file("file") else {
        return json::error(StatusCode::BAD_REQUEST, "file required");
    };
    if file.data.len() as u64 > MAX_AUTH_FILE_SIZE {
        return json::error(StatusCode::PAYLOAD_TOO_LARGE, "auth file too large");
    }
    let account = match parse_account(&file.data) {
        Ok(account) => account,
        Err(response) => return response,
    };
    let account = match normalize_service_account(&account) {
        Ok(account) => account,
        Err(reason) => return failure("invalid service account", &reason),
    };

    let project_id = trimmed(&value_as_string(account.get("project_id")));
    if project_id.is_empty() {
        return json::error(StatusCode::BAD_REQUEST, "project_id missing");
    }
    let email = trimmed(&value_as_string(account.get("client_email")));
    let location = [
        form.value("location").unwrap_or_default(),
        Query::parse(raw).value("location"),
    ]
    .into_iter()
    .map(|value| lossy(trim_space(value)))
    .find(|value| !value.is_empty())
    .unwrap_or_else(|| DEFAULT_LOCATION.to_owned());

    let file_name = format!("vertex-{}.json", sanitize_file_part(&project_id));
    if is_unsafe_name(&file_name) {
        return failure(
            "invalid service account",
            "project_id holds characters a file name can't",
        );
    }
    let label = label_for_vertex(&project_id, &email);
    let metadata = Map::from_iter([
        ("service_account".to_owned(), Value::Object(account)),
        ("project_id".to_owned(), Value::String(project_id.clone())),
        ("email".to_owned(), Value::String(email.clone())),
        ("location".to_owned(), Value::String(location.clone())),
        ("type".to_owned(), Value::String("vertex".to_owned())),
        ("label".to_owned(), Value::String(label.clone())),
    ]);
    let record = Auth {
        id: file_name.clone(),
        provider: "vertex".to_owned(),
        file_name,
        label,
        metadata,
        ..Auth::default()
    };
    let saved = match save_token_record(state, record).await {
        Ok(path) => path,
        Err(error) => {
            return json::response(
                error.status(),
                &Json::map([
                    ("error", Json::Str("save_failed".to_owned())),
                    ("message", Json::Str(error.to_string())),
                ]),
            );
        }
    };
    json::response(
        StatusCode::OK,
        &Json::map([
            ("auth-file", Json::Str(saved)),
            ("email", Json::Str(email)),
            ("location", Json::Str(location)),
            ("project_id", Json::Str(project_id)),
            ("status", Json::Str("ok".to_owned())),
        ]),
    )
}

/// The service account in `data`, as Go's `json.Unmarshal` reads it into a
/// map; else the answer.
fn parse_account(data: &[u8]) -> Result<Map<String, Value>, Response> {
    let kind = match serde_json::from_str::<Value>(&lossy(data)) {
        Ok(Value::Object(account)) => return Ok(account),
        Ok(Value::Null) => {
            return Err(failure(
                "invalid service account",
                "service account payload is empty",
            ));
        }
        Err(error) => return Err(failure("invalid json", &error.to_string())),
        Ok(Value::Bool(_)) => "bool",
        Ok(Value::Number(_)) => "number",
        Ok(Value::String(_)) => "string",
        Ok(Value::Array(_)) => "array",
    };
    Err(failure(
        "invalid json",
        &format!("json: cannot unmarshal {kind} into Go value of type map[string]interface {{}}"),
    ))
}

/// A 400 with `error` and its `message`.
fn failure(error: &str, message: &str) -> Response {
    json::response(
        StatusCode::BAD_REQUEST,
        &Json::map([
            ("error", Json::Str(error.to_owned())),
            ("message", Json::Str(message.to_owned())),
        ]),
    )
}

/// Go's `strings.TrimSpace`, owned.
fn trimmed(text: &str) -> String {
    lossy(trim_space(text.as_bytes()))
}

/// `valueAsString`: a string as it is, nothing as empty, and anything else
/// as Go's `fmt.Sprint` prints it.
fn value_as_string(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(value) => sprint(value),
    }
}

/// Go's `fmt.Sprint` of a JSON value decoded into an `any`.
fn sprint(value: &Value) -> String {
    match value {
        Value::Null => "<nil>".to_owned(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.as_f64().map(go_float).unwrap_or_default(),
        Value::String(text) => text.clone(),
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(sprint).collect();
            format!("[{}]", items.join(" "))
        }
        Value::Object(fields) => {
            let mut entries: Vec<(&String, &Value)> = fields.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let entries: Vec<String> = entries
                .into_iter()
                .map(|(key, value)| format!("{key}:{}", sprint(value)))
                .collect();
            format!("map[{}]", entries.join(" "))
        }
    }
}

/// Go's `%v` of a float64: the shortest digits that read back as `value`,
/// with an exponent of at least two digits when it is below -4 or 6 and
/// over.
fn go_float(value: f64) -> String {
    let exponential = format!("{value:e}");
    let Some((mantissa, exponent)) = exponential.split_once('e') else {
        return exponential;
    };
    let Ok(exponent) = exponent.parse::<i32>() else {
        return exponential;
    };
    if value == 0.0 || (-4..6).contains(&exponent) {
        return format!("{value}");
    }
    let sign = if exponent < 0 { '-' } else { '+' };
    format!("{mantissa}e{sign}{:02}", exponent.unsigned_abs())
}

/// `sanitizeVertexFilePart`: `s` trimmed, with `/`, `\` and `:` made `_`
/// and spaces `-`; `vertex` if that leaves nothing.
fn sanitize_file_part(s: &str) -> String {
    let out: String = trimmed(s)
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' => '_',
            ' ' => '-',
            c => c,
        })
        .collect();
    if out.is_empty() {
        "vertex".to_owned()
    } else {
        out
    }
}

/// `labelForVertex`: `<project> (<email>)`, else either, else `vertex`.
fn label_for_vertex(project_id: &str, email: &str) -> String {
    match (trimmed(project_id), trimmed(email)) {
        (p, e) if !p.is_empty() && !e.is_empty() => format!("{p} ({e})"),
        (p, _) if !p.is_empty() => p,
        (_, e) if !e.is_empty() => e,
        _ => "vertex".to_owned(),
    }
}
