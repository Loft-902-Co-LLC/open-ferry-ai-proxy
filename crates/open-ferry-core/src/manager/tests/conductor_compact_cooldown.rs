// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_compact_cooldown_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! On a `responses/compact` request a transient failure leaves credentials
//! available, a request fault stops the fallback, and 401, 403 and quota
//! 429 still cool the credential down.
//!
//! Deviations from upstream:
//! - `compactTestExecutor` is a `FakeExecutor` that fails only compact calls;
//!   its call counter is the number of recorded calls.
//! - "No cooldown wait" is measured in paused Tokio time, which would jump
//!   forward over any wait.

use super::support::*;

use std::sync::Arc;
use std::time::Duration;

use crate::auth::{Auth, Status};
use crate::exec::{Dispatcher, ExecError, Options};
use crate::manager::Settings;

const PROVIDER: &str = "compact-test-provider";
const MODEL: &str = "gpt-5.6-sol";
const PAYLOAD: &str = r#"{"input":"hello"}"#;

/// A manager with `compactTestExecutor` failing compact calls with `status`
/// and `message`, and active credentials `ids` serving [`MODEL`].
fn compact_manager(
    status: u16,
    message: &'static str,
    ids: &[&str],
) -> (Harness, Arc<FakeExecutor>) {
    let h = Harness::new(Settings::default());
    let executor = FakeExecutor::with(PROVIDER, move |call: &Call| {
        if call.options.alt == "responses/compact" {
            Reply::Err(ExecError::upstream(status, message))
        } else {
            Reply::ok(r#"{"status":"ok"}"#)
        }
    });
    h.executor(&executor);
    for id in ids {
        let mut a: Auth = auth(id, PROVIDER);
        a.status = Status::Active;
        h.add(a, &[MODEL]);
    }
    (h, executor)
}

fn compact_options() -> Options {
    let mut opts = options();
    opts.alt = "responses/compact".into();
    opts
}

async fn execute_compact(h: &Harness) -> Result<crate::exec::Response, ExecError> {
    h.manager
        .execute(
            &providers(&[PROVIDER]),
            request_with(MODEL, PAYLOAD),
            compact_options(),
        )
        .await
}

/// Asserts the credential's state for [`MODEL`] is unavailable with a future
/// retry time, as upstream's 401/403 checks do.
fn assert_cooled(h: &Harness, id: &str, what: &str) {
    let a = h.get(id);
    let state = a
        .model_states
        .get(MODEL)
        .unwrap_or_else(|| panic!("{id} model state should be recorded for {what}"));
    assert!(
        state.unavailable,
        "{id} model state should be unavailable after {what}"
    );
    assert!(
        state.next_retry_after.is_some_and(|t| t > h.now()),
        "{id} next_retry_after not set in future for {what}: {:?}",
        state.next_retry_after
    );
}

#[tokio::test(start_paused = true)]
async fn manager_responses_compact_transient_failure_availability_neutral() {
    let (h, executor) = compact_manager(500, "upstream compact 500", &["auth1", "auth2"]);

    let start = tokio::time::Instant::now();
    let result = execute_compact(&h).await;
    let elapsed = start.elapsed();

    assert!(result.is_err(), "execute expected error, got ok");
    assert!(
        elapsed <= Duration::from_secs(2),
        "execute took {elapsed:?}, should not pause for cooldown wait"
    );
    assert_eq!(
        executor.calls().len(),
        2,
        "want 2 calls (fallback across candidate auths)"
    );

    for id in ["auth1", "auth2"] {
        let a = h.get(id);
        if let Some(state) = a.model_states.get(MODEL) {
            assert!(
                !state.unavailable,
                "auth {id} marked unavailable after compact failure"
            );
            assert!(
                !state.next_retry_after.is_some_and(|t| t > h.now()),
                "auth {id} has next_retry_after set in future: {:?}",
                state.next_retry_after
            );
        }
    }

    // A normal request succeeds at once.
    let resp = h
        .manager
        .execute(
            &providers(&[PROVIDER]),
            request_with(MODEL, PAYLOAD),
            options(),
        )
        .await
        .expect("normal execute failed");
    assert_eq!(&resp.payload[..], br#"{"status":"ok"}"#);
}

#[tokio::test(start_paused = true)]
async fn manager_responses_compact_request_fault_stops_fallback() {
    let (h, executor) = compact_manager(404, "404 endpoint not found", &["auth1", "auth2"]);

    let result = execute_compact(&h).await;
    assert!(result.is_err(), "execute expected error, got ok");
    assert_eq!(
        executor.calls().len(),
        1,
        "want 1 call (fallback stopped on request fault)"
    );

    for id in ["auth1", "auth2"] {
        let a = h.get(id);
        if let Some(state) = a.model_states.get(MODEL) {
            assert!(
                !state.unavailable,
                "auth {id} marked unavailable after compact 404 fault"
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn manager_responses_compact_unauthorized_cools_credential() {
    let (h, _executor) = compact_manager(401, "401 unauthorized", &["auth1"]);

    let result = execute_compact(&h).await;
    assert!(result.is_err(), "execute expected error, got ok");
    assert_cooled(&h, "auth1", "401 unauthorized");
}

#[tokio::test(start_paused = true)]
async fn manager_responses_compact_forbidden_cools_credential() {
    let (h, _executor) = compact_manager(403, "403 forbidden", &["auth1"]);

    let result = execute_compact(&h).await;
    assert!(result.is_err(), "execute expected error, got ok");
    assert_cooled(&h, "auth1", "403 forbidden");
}

#[tokio::test(start_paused = true)]
async fn manager_responses_compact_quota429_cools_credential() {
    let (h, _executor) = compact_manager(
        429,
        r#"{"error":{"type":"usage_limit_reached","message":"quota exceeded"}}"#,
        &["auth1"],
    );

    let result = execute_compact(&h).await;
    assert!(result.is_err(), "execute expected error, got ok");

    let a = h.get("auth1");
    let state = a
        .model_states
        .get(MODEL)
        .expect("auth1 model state should be recorded for 429 quota");
    assert!(
        state.quota.exceeded,
        "auth1 quota should be marked exceeded after 429 quota"
    );
    assert!(
        state.next_retry_after.is_some_and(|t| t > h.now()),
        "auth1 next_retry_after not set in future for 429 quota: {:?}",
        state.next_retry_after
    );
}
