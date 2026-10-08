// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_oauth_request_scoped_errors_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The `oauth-request-scoped-errors` rules apply to OAuth credentials of
//! their provider, including an OAuth credential that also carries an API
//! key, and never to API-key credentials.
//!
//! Deviations from upstream:
//! - None.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::support::*;
use crate::auth::{Auth, Status};
use crate::exec::Dispatcher;
use crate::manager::{RequestScopedErrorRule, Settings};

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn settings_with_oauth_rules(provider: &str, rules: Vec<RequestScopedErrorRule>) -> Settings {
    Settings {
        oauth_request_scoped_errors: BTreeMap::from([(provider.to_owned(), rules)]),
        ..Settings::default()
    }
}

fn active(id: &str, provider: &str, attributes: &[(&str, &str)]) -> Auth {
    let mut credential = auth(id, provider);
    credential.status = Status::Active;
    for (key, value) in attributes {
        credential
            .attributes
            .insert((*key).to_owned(), (*value).to_owned());
    }
    credential
}

fn assert_remains_active(h: &Harness, id: &str) {
    let got = h.get(id);
    assert!(
        got.status == Status::Active && !got.unavailable,
        "expected auth1 to remain active, got status={:?} unavailable={}",
        got.status,
        got.unavailable
    );
}

#[tokio::test(start_paused = true)]
async fn oauth_request_scoped_errors_applies_to_oauth_auth() {
    const MODEL: &str = "claude-3-5-sonnet";
    let h = Harness::new(settings_with_oauth_rules(
        "vertex",
        vec![RequestScopedErrorRule {
            status: 400,
            matches: strings(&["maximum_context_length", "context_length_exceeded"]),
            match_regex: strings(&["maximum_context_length$", "^context_length_exceeded"]),
            action: "stop".into(),
        }],
    ));
    h.add(
        active(
            "auth-vertex-oauth",
            "vertex",
            &[("auth_kind", "oauth"), ("priority", "10")],
        ),
        &[MODEL],
    );
    h.add(
        active(
            "auth-vertex-oauth-2",
            "vertex",
            &[("auth_kind", "oauth"), ("priority", "5")],
        ),
        &[MODEL],
    );
    let executor = FakeExecutor::with("vertex", |_| {
        Reply::status(400, r#"{"error": "maximum_context_length"}"#)
    });
    h.executor(&executor);

    let result = h
        .manager
        .execute(&providers(&["vertex"]), request(MODEL), options())
        .await;
    assert!(result.is_err(), "expected error, got nil");
    // A stop ends the call without trying auth2.
    assert_eq!(
        executor.calls().len(),
        1,
        "expected execCount = 1 (stopped)"
    );
    // A stop without cooldown leaves auth1 active.
    assert_remains_active(&h, "auth-vertex-oauth");
}

#[tokio::test(start_paused = true)]
async fn oauth_request_scoped_errors_does_not_apply_to_api_key() {
    const MODEL: &str = "claude-3-5-sonnet";
    let h = Harness::new(settings_with_oauth_rules(
        "vertex",
        vec![RequestScopedErrorRule {
            status: 500,
            matches: strings(&["internal_server_error"]),
            match_regex: Vec::new(),
            action: "stop".into(),
        }],
    ));
    h.add(
        active(
            "auth-vertex-apikey",
            "vertex",
            &[
                ("auth_kind", "apikey"),
                ("api_key", "test-key"),
                ("priority", "10"),
            ],
        ),
        &[MODEL],
    );
    h.add(
        active(
            "auth-vertex-apikey-2",
            "vertex",
            &[
                ("auth_kind", "apikey"),
                ("api_key", "test-key-2"),
                ("priority", "5"),
            ],
        ),
        &[MODEL],
    );
    let count = Arc::new(AtomicUsize::new(0));
    let counter = count.clone();
    let executor = FakeExecutor::with("vertex", move |_| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            Reply::status(500, r#"{"error": "internal_server_error"}"#)
        } else {
            Reply::ok(r#"{"success": true}"#)
        }
    });
    h.executor(&executor);

    let response = h
        .manager
        .execute(&providers(&["vertex"]), request(MODEL), options())
        .await
        .unwrap_or_else(|err| panic!("unexpected Execute error: {err}"));
    assert_eq!(
        &response.payload[..],
        br#"{"success": true}"#,
        "unexpected payload"
    );
    // The OAuth rule is skipped for an API key, so the call moved to auth2.
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "expected execCount = 2 (rotated because OAuth rule skipped for API key)"
    );
}

#[tokio::test(start_paused = true)]
async fn oauth_request_scoped_errors_applies_to_meta_oauth() {
    const MODEL: &str = "muse-spark-1.3";
    let h = Harness::new(settings_with_oauth_rules(
        "meta",
        vec![RequestScopedErrorRule {
            status: 400,
            matches: strings(&["context_length_exceeded"]),
            match_regex: Vec::new(),
            action: "stop".into(),
        }],
    ));
    h.add(
        active(
            "auth-meta-oauth",
            "meta",
            &[
                ("auth_kind", "oauth"),
                ("api_key", "LLM|minted"),
                ("priority", "10"),
            ],
        ),
        &[MODEL],
    );
    h.add(
        active(
            "auth-meta-oauth-2",
            "meta",
            &[
                ("auth_kind", "oauth"),
                ("api_key", "LLM|minted-2"),
                ("priority", "5"),
            ],
        ),
        &[MODEL],
    );
    let executor = FakeExecutor::with("meta", |_| {
        Reply::status(400, r#"{"error": "context_length_exceeded"}"#)
    });
    h.executor(&executor);

    let result = h
        .manager
        .execute(&providers(&["meta"]), request(MODEL), options())
        .await;
    assert!(result.is_err(), "expected error, got nil");
    assert_eq!(
        executor.calls().len(),
        1,
        "expected execCount = 1 (stopped)"
    );
    assert_remains_active(&h, "auth-meta-oauth");
}
