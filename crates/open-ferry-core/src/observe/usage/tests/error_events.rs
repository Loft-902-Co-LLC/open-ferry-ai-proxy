// Ported from CLIProxyAPI sdk/cliproxy/auth/error_events_test.go
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests of the error events the manager's failed calls publish.
//!
//! Dropped: TestManagerMarkResultSkipsErrorEventInHomeMode, as open-ferry
//! has no Home mode.
//!
//! Deviations from upstream: the events reach the queue through the hook
//! the usage statistics give the manager, and each test has its own queue.

use std::sync::Arc;

use serde_json::{Map, Value, json};
use tokio::sync::mpsc::error::TryRecvError;

use super::super::Usage;
use super::support::{bool_at, int_at, str_field};
use crate::auth::{Auth, AuthError};
use crate::manager::{CallResult, ClientModels, Manager, Settings};

/// A model registry that knows no credentials.
struct NoModels;

impl ClientModels for NoModels {
    fn models_for_client(&self, _client_id: &str) -> Vec<String> {
        Vec::new()
    }
}

/// Usage statistics with the queue on, and a manager telling them about
/// its failed calls.
fn manager_with_events() -> (Usage, Manager) {
    let usage = Usage::default();
    usage.inner.queue.set_enabled(true);
    let manager = Manager::new(Settings::default(), Arc::new(NoModels), None);
    manager.set_error_events(usage.error_events());
    (usage, manager)
}

/// A Codex credential with `id`.
fn codex_auth(id: &str) -> Auth {
    let mut metadata = Map::new();
    metadata.insert("type".to_owned(), json!("codex"));
    Auth {
        id: id.to_owned(),
        provider: "codex".to_owned(),
        metadata,
        ..Auth::default()
    }
}

/// Ports TestManagerMarkResultPublishesErrorEventAfterAuthStateUpdate.
#[test]
fn mark_result_publishes_error_event_after_auth_state_update() {
    let (usage, manager) = manager_with_events();
    let (mut subscriber, _subscription) = usage.subscribe_errors();
    let auth = manager
        .register_unsaved(codex_auth("auth-error-event"))
        .expect("register");

    manager.mark_result(&CallResult {
        auth_id: auth.id.clone(),
        provider: "codex".to_owned(),
        model: "gpt-5".to_owned(),
        success: false,
        error: Some(AuthError {
            code: "rate_limit".to_owned(),
            message: r#"{"error":"quota"}"#.to_owned(),
            retryable: true,
            http_status: 429,
        }),
        ..CallResult::default()
    });

    let payload = subscriber.try_recv().expect("an error event");
    let event: Value = serde_json::from_slice(&payload).expect("JSON");
    assert_eq!(str_field(&event, "provider"), "codex");
    assert_eq!(str_field(&event, "model"), "gpt-5");
    assert_eq!(str_field(&event, "auth_id"), auth.id);
    assert!(!str_field(&event, "auth_index").is_empty(), "{event}");
    assert_eq!(int_at(&event, "/status_code"), 429);
    assert_eq!(str_field(&event, "body"), r#"{"error":"quota"}"#);
    assert_eq!(str_field(&event, "code"), "rate_limit");
    assert!(bool_at(&event, "/retryable"));

    let status = &event["auth_status"];
    assert_eq!(str_field(status, "status"), "error", "{event}");
    assert!(bool_at(status, "/unavailable"));
    assert!(bool_at(status, "/quota/exceeded"), "{event}");
    assert_eq!(status.pointer("/quota/reason"), Some(&json!("quota")));
    assert_eq!(str_field(&status["model"], "name"), "gpt-5");
    assert_eq!(str_field(&status["model"], "status"), "error");
    assert!(bool_at(status, "/model/unavailable"));
    assert!(bool_at(status, "/model/quota/exceeded"));
    assert_eq!(status.pointer("/model/quota/reason"), Some(&json!("quota")));
}

/// Not upstream's: a call that succeeds publishes nothing, nor does a
/// failed one while the queue is off.
#[test]
fn no_error_event_for_a_success_or_while_off() {
    let (usage, manager) = manager_with_events();
    let (mut subscriber, _subscription) = usage.subscribe_errors();
    let auth = manager
        .register_unsaved(codex_auth("auth-quiet"))
        .expect("register");
    manager.mark_result(&CallResult {
        auth_id: auth.id.clone(),
        provider: "codex".to_owned(),
        model: "gpt-5".to_owned(),
        success: true,
        ..CallResult::default()
    });
    assert_eq!(subscriber.try_recv(), Err(TryRecvError::Empty));

    let (usage, manager) = manager_with_events();
    let auth = manager
        .register_unsaved(codex_auth("auth-off"))
        .expect("register");
    let (mut subscriber, _subscription) = usage.subscribe_errors();
    usage.inner.queue.set_enabled(false);
    manager.mark_result(&CallResult {
        auth_id: auth.id.clone(),
        provider: "codex".to_owned(),
        model: "gpt-5".to_owned(),
        success: false,
        ..CallResult::default()
    });
    assert!(subscriber.try_recv().is_err());
}

/// Not upstream's: the event's body is scrubbed of the credential's key,
/// and an error without a message or status reads `request failed` with
/// 500.
#[test]
fn error_event_body_is_scrubbed_and_defaults() {
    let (usage, manager) = manager_with_events();
    let (mut subscriber, _subscription) = usage.subscribe_errors();
    let mut credential = codex_auth("auth-keyed");
    credential.metadata.insert(
        "access_token".to_owned(),
        json!("tok-secret-0123456789abcdef"),
    );
    let auth = manager.register_unsaved(credential).expect("register");
    manager.mark_result(&CallResult {
        auth_id: auth.id.clone(),
        provider: "codex".to_owned(),
        model: "gpt-5".to_owned(),
        success: false,
        error: Some(AuthError {
            message: "bad token tok-secret-0123456789abcdef".to_owned(),
            http_status: 401,
            ..AuthError::default()
        }),
        ..CallResult::default()
    });
    let event: Value =
        serde_json::from_slice(&subscriber.try_recv().expect("an error event")).expect("JSON");
    let body = str_field(&event, "body");
    assert!(!body.contains("tok-secret-0123456789abcdef"), "{body}");
    assert!(body.starts_with("bad token "), "{body}");

    manager.mark_result(&CallResult {
        auth_id: auth.id.clone(),
        provider: "codex".to_owned(),
        model: "gpt-5".to_owned(),
        success: false,
        ..CallResult::default()
    });
    let event: Value =
        serde_json::from_slice(&subscriber.try_recv().expect("an error event")).expect("JSON");
    assert_eq!(str_field(&event, "body"), "request failed");
    assert_eq!(int_at(&event, "/status_code"), 500);
}

/// Not upstream's: the credential's and the model's status messages, which
/// repeat the error's message, are scrubbed of the credential's tokens as
/// the body is.
#[test]
fn error_event_status_messages_are_scrubbed() {
    let (usage, manager) = manager_with_events();
    let (mut subscriber, _subscription) = usage.subscribe_errors();
    let mut credential = codex_auth("auth-status");
    credential
        .metadata
        .insert("access_token".to_owned(), json!("access-secret-123456789"));
    let auth = manager.register_unsaved(credential).expect("register");
    manager.mark_result(&CallResult {
        auth_id: auth.id.clone(),
        provider: "codex".to_owned(),
        model: "gpt-5".to_owned(),
        success: false,
        error: Some(AuthError {
            message: "invalid access-secret-123456789".to_owned(),
            http_status: 401,
            ..AuthError::default()
        }),
        ..CallResult::default()
    });
    let event: Value =
        serde_json::from_slice(&subscriber.try_recv().expect("an error event")).expect("JSON");
    let text = event.to_string();
    assert!(!text.contains("access-secret-123456789"), "{text}");
    assert_eq!(str_field(&event, "body"), "invalid [redacted]");
    for pointer in [
        "/auth_status/status_message",
        "/auth_status/model/status_message",
    ] {
        assert_eq!(
            event.pointer(pointer).and_then(Value::as_str),
            Some("invalid [redacted]"),
            "{pointer} in {text}"
        );
    }
}

/// Not upstream's: a credential's token shorter than the client errors'
/// minimum is still scrubbed from the event, as from a file.
#[test]
fn error_event_scrubs_short_secrets() {
    let (usage, manager) = manager_with_events();
    let (mut subscriber, _subscription) = usage.subscribe_errors();
    let mut credential = codex_auth("auth-short");
    credential
        .metadata
        .insert("access_token".to_owned(), json!("tk-42"));
    let auth = manager.register_unsaved(credential).expect("register");
    manager.mark_result(&CallResult {
        auth_id: auth.id.clone(),
        provider: "codex".to_owned(),
        model: "gpt-5".to_owned(),
        success: false,
        error: Some(AuthError {
            message: "invalid tk-42".to_owned(),
            http_status: 401,
            ..AuthError::default()
        }),
        ..CallResult::default()
    });
    let event: Value =
        serde_json::from_slice(&subscriber.try_recv().expect("an error event")).expect("JSON");
    let text = event.to_string();
    assert!(!text.contains("tk-42"), "{text}");
}
