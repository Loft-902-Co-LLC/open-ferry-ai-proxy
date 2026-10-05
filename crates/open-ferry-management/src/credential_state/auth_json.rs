// Ported from CLIProxyAPI sdk/cliproxy/auth/types.go (Auth, QuotaState,
// ModelState) and errors.go (Error), as encoding/json writes them from
// their tags (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A credential as upstream's management API writes one: the JSON of Go's
//! `coreauth.Auth`, field by field in declaration order, `omitempty`
//! fields left out when empty, maps with their keys sorted and strings
//! escaped for HTML, as gin's `c.JSON` writes them.
//!
//! The refresh answer carries it. It holds the credential's tokens, as
//! upstream's does: the route is behind the management key.
//!
//! Deviations from upstream:
//! - The quota's `observed_at` is always Go's zero time, and its `signals`
//!   never appear: the port doesn't track either.
//! - A credential whose status isn't known writes `"unknown"`, where Go's
//!   zero status writes `""`.

use open_ferry_core::auth::{Auth, AuthError, ModelState, QuotaState, Timestamp};
use serde_json::Value;

use crate::go::is_zero;
use crate::json::Json;

/// Go's zero `time.Time`, as `MarshalJSON` writes it.
const ZERO_TIME: &str = "0001-01-01T00:00:00Z";

/// `auth` as Go's `json.Marshal` writes a `*coreauth.Auth`.
pub(crate) fn auth_json(auth: &Auth) -> Json {
    let mut fields: Vec<(&'static str, Json)> = vec![("id", Json::Str(auth.id.clone()))];
    if auth.registration_epoch != 0 {
        fields.push(("registration_epoch", Json::Uint(auth.registration_epoch)));
    }
    if auth.generation != 0 {
        fields.push(("generation", Json::Uint(auth.generation)));
    }
    fields.push(("provider", Json::Str(auth.provider.clone())));
    push_nonempty(&mut fields, "prefix", &auth.prefix);
    push_nonempty(&mut fields, "label", &auth.label);
    fields.push(("status", Json::Str(auth.status.as_str().to_owned())));
    push_nonempty(&mut fields, "status_message", &auth.status_message);
    fields.push(("disabled", Json::Bool(auth.disabled)));
    fields.push(("unavailable", Json::Bool(auth.unavailable)));
    push_nonempty(&mut fields, "proxy_url", &auth.proxy_url);
    if !auth.attributes.is_empty() {
        let attributes = auth
            .attributes
            .iter()
            .map(|(key, value)| (key.clone(), Json::Str(value.clone())))
            .collect();
        fields.push(("attributes", Json::Map(attributes)));
    }
    if !auth.metadata.is_empty() {
        fields.push(("metadata", Json::Any(Value::Object(auth.metadata.clone()))));
    }
    fields.push(("quota", quota_json(&auth.quota)));
    if let Some(error) = &auth.last_error {
        fields.push(("last_error", error_json(error)));
    }
    fields.push(("created_at", time_json(auth.created_at)));
    fields.push(("updated_at", time_json(auth.updated_at)));
    fields.push(("last_refreshed_at", time_json(auth.last_refreshed_at)));
    fields.push(("next_refresh_after", time_json(auth.next_refresh_after)));
    fields.push(("next_retry_after", time_json(auth.next_retry_after)));
    if !auth.model_states.is_empty() {
        let states = auth
            .model_states
            .iter()
            .map(|(model, state)| (model.clone(), model_state_json(state)))
            .collect();
        fields.push(("model_states", Json::Map(states)));
    }
    Json::Struct(fields)
}

/// A `ModelState`.
fn model_state_json(state: &ModelState) -> Json {
    let mut fields: Vec<(&'static str, Json)> =
        vec![("status", Json::Str(state.status.as_str().to_owned()))];
    push_nonempty(&mut fields, "status_message", &state.status_message);
    fields.push(("unavailable", Json::Bool(state.unavailable)));
    fields.push(("next_retry_after", time_json(state.next_retry_after)));
    if let Some(error) = &state.last_error {
        fields.push(("last_error", error_json(error)));
    }
    fields.push(("quota", quota_json(&state.quota)));
    fields.push(("updated_at", time_json(state.updated_at)));
    Json::Struct(fields)
}

/// A `QuotaState`.
fn quota_json(quota: &QuotaState) -> Json {
    let mut fields: Vec<(&'static str, Json)> = vec![("exceeded", Json::Bool(quota.exceeded))];
    push_nonempty(&mut fields, "reason", &quota.reason);
    fields.push(("next_recover_at", time_json(quota.next_recover_at)));
    if quota.backoff_level != 0 {
        fields.push(("backoff_level", Json::Int(i64::from(quota.backoff_level))));
    }
    fields.push(("observed_at", Json::Str(ZERO_TIME.to_owned())));
    Json::Struct(fields)
}

/// An `auth.Error`.
fn error_json(error: &AuthError) -> Json {
    let mut fields: Vec<(&'static str, Json)> = Vec::new();
    push_nonempty(&mut fields, "code", &error.code);
    fields.push(("message", Json::Str(error.message.clone())));
    fields.push(("retryable", Json::Bool(error.retryable)));
    if error.http_status != 0 {
        fields.push(("http_status", Json::Int(i64::from(error.http_status))));
    }
    Json::Struct(fields)
}

/// A `time.Time`: Go's zero time when unset.
fn time_json(time: Option<Timestamp>) -> Json {
    match time {
        Some(time) if !is_zero(Some(time)) => Json::Time(time),
        _ => Json::Str(ZERO_TIME.to_owned()),
    }
}

/// Adds an `omitempty` string field unless it is empty.
fn push_nonempty(fields: &mut Vec<(&'static str, Json)>, name: &'static str, value: &str) {
    if !value.is_empty() {
        fields.push((name, Json::Str(value.to_owned())));
    }
}
