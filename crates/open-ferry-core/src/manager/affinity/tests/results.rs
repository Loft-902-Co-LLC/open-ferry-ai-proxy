// Ported from CLIProxyAPI sdk/cliproxy/auth/session_affinity_metadata_test.go
// (TestSessionAffinityDelayedSuccessDoesNotOverwriteReboundAuth and
// TestSessionAffinityOnResultWithMismatchedNamespaceFailsToUnbind)
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests of the results that refresh or drop a session's bindings.
//!
//! Deviations from upstream:
//! - The provider scope is an argument and the model the result's, where
//!   upstream reads both from the metadata its `Pick` wrote.

use std::time::Duration;

use super::{at, base, headers};
use crate::auth::AuthError;
use crate::manager::CallResult;
use crate::manager::affinity::{Affinity, Session};
use crate::manager::classify::CODE_REQUEST_SCOPED;
use crate::session::Payload;

const HOUR: Duration = Duration::from_secs(60 * 60);

/// The session of a call with headers `pairs` and body `payload`.
fn session(pairs: &[(&str, &str)], payload: &str) -> Session {
    Session::with_derived(&headers(pairs), &Payload::parse(payload.as_bytes()), "", "")
        .expect("a session")
}

/// The outcome of a call on `auth` for `model`.
fn result(auth: &str, model: &str, error: Option<AuthError>) -> CallResult {
    CallResult {
        auth_id: auth.into(),
        provider: "provider-a".into(),
        model: model.into(),
        success: error.is_none(),
        error,
        ..CallResult::default()
    }
}

/// A failure with HTTP status `status`.
fn failure(status: u16) -> Option<AuthError> {
    Some(AuthError {
        http_status: status,
        ..AuthError::default()
    })
}

// TestSessionAffinityDelayedSuccessDoesNotOverwriteReboundAuth.
#[test]
fn delayed_success_does_not_overwrite_rebound_auth() {
    let now = base();
    let mut affinity = Affinity::new(HOUR, true);
    let key = "mixed::header:sess-delay-success::model-x";
    affinity.cache.set(key, "auth-A", now);
    // The session moves to auth-B.
    affinity.cache.set(key, "auth-B", now);

    // A success of auth-A arrives late.
    let session = session(&[("X-Session-Id", "sess-delay-success")], "");
    affinity.on_result(&session, &result("auth-A", "model-x", None), "mixed", now);

    assert_eq!(affinity.cache.get(key, now).as_deref(), Some("auth-B"));
}

// TestSessionAffinityOnResultWithMismatchedNamespaceFailsToUnbind.
#[test]
fn on_result_in_the_bound_scope_unbinds() {
    let now = base();
    let mut affinity = Affinity::new(HOUR, true);
    let key = "mixed::header:sess-ns-1::test-model";
    affinity.cache.set(key, "auth-1", now);

    let session = session(&[("X-Session-Id", "sess-ns-1")], "");
    let failed = result("auth-1", "test-model", failure(500));
    affinity.on_result(&session, &failed, "mixed", now);

    assert_eq!(affinity.cache.get(key, now), None);
}

// Not upstream's: the client's model, when the result has one, names the
// binding, not the model the call ran on.
#[test]
fn on_result_reads_the_route_model() {
    let now = base();
    let mut affinity = Affinity::new(HOUR, true);
    let key = "mixed::header:sess-route::claude-sonnet-4-5";
    affinity.cache.set(key, "auth-1", now);

    let session = session(&[("X-Session-Id", "sess-route")], "");
    let mut failed = result("auth-1", "upstream-model", failure(500));
    failed.route_model = "claude-sonnet-4-5(high)".into();
    affinity.on_result(&session, &failed, "mixed", now);

    assert_eq!(affinity.cache.get(key, now), None);
}

// Not upstream's: a failure the request is to blame for keeps the binding.
#[test]
fn request_scoped_failure_keeps_the_binding() {
    let now = base();
    let mut affinity = Affinity::new(HOUR, true);
    let key = "mixed::header:sess-scoped::test-model";
    affinity.cache.set(key, "auth-1", now);

    let session = session(&[("X-Session-Id", "sess-scoped")], "");
    let error = AuthError {
        code: CODE_REQUEST_SCOPED.into(),
        http_status: 400,
        ..AuthError::default()
    };
    let failed = result("auth-1", "test-model", Some(error));
    affinity.on_result(&session, &failed, "mixed", now);

    assert_eq!(affinity.cache.get(key, now).as_deref(), Some("auth-1"));
}

// Not upstream's: a success keeps the session and the conversation it
// falls back to bound for another TTL.
#[test]
fn success_refreshes_the_session_and_its_fallback() {
    let mut affinity = Affinity::new(Duration::from_millis(100), true);
    let primary = "openai::pck:cache-1::gpt-5";
    let conversation = "openai::conv:conv-1::gpt-5";
    affinity.cache.set(primary, "auth-1", at(0));
    affinity.cache.set(conversation, "auth-1", at(0));

    let session = session(
        &[],
        r#"{"prompt_cache_key":"cache-1","conversation":{"id":"conv-1"}}"#,
    );
    assert_eq!(session.primary(), "pck:cache-1");
    assert_eq!(session.fallback(), "conv:conv-1");
    affinity.on_result(&session, &result("auth-1", "gpt-5", None), "openai", at(80));

    assert_eq!(
        affinity.cache.get(primary, at(150)).as_deref(),
        Some("auth-1")
    );
    assert_eq!(
        affinity.cache.get(conversation, at(150)).as_deref(),
        Some("auth-1")
    );
}

// Not upstream's: a subagent's failure drops its own binding, never its
// parent's.
#[test]
fn subagent_failure_keeps_the_parent_binding() {
    let now = base();
    let mut affinity = Affinity::new(HOUR, true);
    let session = session(
        &[
            ("X-Claude-Code-Session-Id", "root-1"),
            ("X-Claude-Code-Agent-Id", "checker"),
        ],
        "",
    );
    assert_eq!(session.primary(), "claude:root-1:agent:checker");
    assert_eq!(session.fallback(), "claude:root-1");
    let own = "claude::claude:root-1:agent:checker::model";
    let parent = "claude::claude:root-1::model";
    affinity.cache.set(own, "auth-1", now);
    affinity.cache.set(parent, "auth-1", now);

    affinity.on_result(
        &session,
        &result("auth-1", "model", failure(500)),
        "claude",
        now,
    );

    assert_eq!(affinity.cache.get(own, now), None);
    assert_eq!(affinity.cache.get(parent, now).as_deref(), Some("auth-1"));
}

// Not upstream's: removing a credential drops its bindings.
#[test]
fn invalidate_auth_drops_the_bindings() {
    let now = base();
    let mut affinity = Affinity::new(HOUR, true);
    affinity.cache.set("mixed::header:a::m", "auth-1", now);
    affinity.cache.set("mixed::header:b::m", "auth-2", now);
    affinity.invalidate_auth("auth-1");
    assert_eq!(affinity.cache.get("mixed::header:a::m", now), None);
    assert_eq!(
        affinity.cache.get("mixed::header:b::m", now).as_deref(),
        Some("auth-2")
    );
}
