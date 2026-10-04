// Ported from CLIProxyAPI sdk/cliproxy/auth/error_events.go
// (publishErrorEvent, buildErrorEventPayload, buildErrorEventAuthStatus,
// errorEventModelStatusFrom, errorEventQuotaStatusFrom,
// errorEventStatusCode, errorEventBody, timePtrIfSet) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The error events of failed calls, for the usage queue's error
//! subscribers: one JSON object per failure the manager records, with the
//! credential's state after it.
//!
//! The fields come in upstream's order. An event's status is the error's,
//! else 500; its body the error's message, else its text, else
//! `request failed`. The credential's quota is written only when it says
//! something, and the model's state only when the credential has one for
//! the call's model.
//!
//! Deviations from upstream:
//! - Times are in UTC; upstream writes the local time with its offset.
//! - The body and the credential's and model's status messages are
//!   scrubbed of the credential's keys and tokens (see
//!   [`Secrets::add_auth`]).
//! - Upstream skips the events in its Home mode, which open-ferry doesn't
//!   have.

use bytes::Bytes;
use chrono::Utc;
use open_ferry_translate::go::json_string;

use super::Usage;
use super::queue::Queue;
use super::record_json::go_time;
use crate::auth::{Auth, AuthError, QuotaState, Timestamp};
use crate::manager::{CallResult, ErrorEvents};
use crate::observe::redact::{Policy, Secrets};

/// Publishes the manager's failed calls to the usage queue.
pub(super) struct UsageErrorEvents {
    queue: Queue,
}

impl UsageErrorEvents {
    pub(super) fn new(usage: &Usage) -> Self {
        Self {
            queue: usage.inner.queue.clone(),
        }
    }
}

impl std::fmt::Debug for UsageErrorEvents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsageErrorEvents").finish_non_exhaustive()
    }
}

impl ErrorEvents for UsageErrorEvents {
    fn publish(&self, result: &CallResult, auth: &Auth) {
        if result.success || !self.queue.enabled() {
            return;
        }
        self.queue
            .enqueue_error(Bytes::from(error_event_payload(result, auth)));
    }
}

/// A JSON object written field by field.
struct Object(String);

impl Object {
    fn new() -> Self {
        Self(String::from("{"))
    }

    fn key(&mut self, name: &str) -> &mut String {
        if self.0.len() > 1 {
            self.0.push(',');
        }
        self.0.push('"');
        self.0.push_str(name);
        self.0.push_str("\":");
        &mut self.0
    }

    fn str(&mut self, name: &str, value: &str) {
        let encoded = json_string(value);
        self.key(name).push_str(&encoded);
    }

    fn str_omitempty(&mut self, name: &str, value: &str) {
        if !value.is_empty() {
            self.str(name, value);
        }
    }

    fn time_omitempty(&mut self, name: &str, value: Option<Timestamp>) {
        if let Some(value) = value {
            self.str(name, &go_time(value));
        }
    }

    fn raw(&mut self, name: &str, value: &str) {
        self.key(name).push_str(value);
    }

    fn bool(&mut self, name: &str, value: bool) {
        self.raw(name, if value { "true" } else { "false" });
    }

    fn finish(mut self) -> String {
        self.0.push('}');
        self.0
    }
}

/// The event of the failed `result`, with `auth`'s state after it
/// (upstream's `buildErrorEventPayload`).
pub(crate) fn error_event_payload(result: &CallResult, auth: &Auth) -> String {
    let error = result.error.as_ref();
    let mut event = Object::new();
    event.str("timestamp", &go_time(Utc::now()));
    event.str_omitempty("provider", result.provider.trim());
    event.str_omitempty("model", result.model.trim());
    event.str_omitempty("auth_id", result.auth_id.trim());
    event.str("auth_index", auth.index.trim());
    event.raw("status_code", &status_code(error).to_string());
    let secrets = credential_secrets(auth);
    event.str("body", &secrets.text(body(error), Policy::Client));
    if let Some(error) = error {
        event.str_omitempty("code", error.code.trim());
        if error.retryable {
            event.bool("retryable", true);
        }
    }
    event.raw("auth_status", &auth_status(&result.model, auth, &secrets));
    event.finish()
}

/// The error's status, else 500 (upstream's `errorEventStatusCode`).
fn status_code(error: Option<&AuthError>) -> u16 {
    match error {
        Some(error) if error.http_status > 0 => error.http_status,
        _ => 500,
    }
}

/// The error's message, else its text, else `request failed` (upstream's
/// `errorEventBody`).
fn body(error: Option<&AuthError>) -> String {
    let Some(error) = error else {
        return "request failed".to_owned();
    };
    let message = error.message.trim();
    if !message.is_empty() {
        return message.to_owned();
    }
    let text = if error.code.is_empty() {
        error.message.clone()
    } else {
        format!("{}: {}", error.code, error.message)
    };
    match text.trim() {
        "" => "request failed".to_owned(),
        text => text.to_owned(),
    }
}

/// The secrets to scrub from `auth`'s error events: its keys and tokens.
fn credential_secrets(auth: &Auth) -> Secrets {
    let mut secrets = Secrets::new();
    secrets.add_auth(auth);
    secrets
}

/// The credential's state (upstream's `buildErrorEventAuthStatus`), its
/// status messages scrubbed of `secrets`.
fn auth_status(model: &str, auth: &Auth, secrets: &Secrets) -> String {
    let mut status = Object::new();
    status.str("status", auth.status.as_str());
    status.str_omitempty(
        "status_message",
        &secrets.str(auth.status_message.trim(), Policy::Client),
    );
    status.bool("disabled", auth.disabled);
    status.bool("unavailable", auth.unavailable);
    status.time_omitempty("next_retry_after", auth.next_retry_after);
    if let Some(quota) = quota_status(&auth.quota) {
        status.raw("quota", &quota);
    }
    let model = model.trim();
    if !model.is_empty()
        && let Some(state) = auth.model_states.get(model)
    {
        let mut object = Object::new();
        object.str("name", model);
        object.str("status", state.status.as_str());
        object.str_omitempty(
            "status_message",
            &secrets.str(state.status_message.trim(), Policy::Client),
        );
        object.bool("unavailable", state.unavailable);
        object.time_omitempty("next_retry_after", state.next_retry_after);
        if let Some(quota) = quota_status(&state.quota) {
            object.raw("quota", &quota);
        }
        status.raw("model", &object.finish());
    }
    status.finish()
}

/// The quota's state, or none when it says nothing (upstream's
/// `errorEventQuotaStatusFrom`).
fn quota_status(quota: &QuotaState) -> Option<String> {
    let reason = quota.reason.trim();
    if !quota.exceeded
        && reason.is_empty()
        && quota.next_recover_at.is_none()
        && quota.backoff_level == 0
    {
        return None;
    }
    let mut object = Object::new();
    object.bool("exceeded", quota.exceeded);
    object.str_omitempty("reason", reason);
    object.time_omitempty("next_recover_at", quota.next_recover_at);
    if quota.backoff_level != 0 {
        object.raw("backoff_level", &quota.backoff_level.to_string());
    }
    Some(object.finish())
}
