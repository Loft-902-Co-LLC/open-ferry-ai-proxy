// Ported from CLIProxyAPI sdk/cliproxy/auth/retry_deadline_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! When every credential is only temporarily unavailable, the call fails
//! with a retryable `auth_unavailable` whose `Retry-After` is the earliest
//! recovery; a disabled credential's deadline never counts, and a quota
//! cooldown stays a 429.
//!
//! Deviations from upstream:
//! - Upstream calls `getAvailableAuths` and
//!   `availableAuthsForRouteModelWithPriorityMode` on the credentials
//!   directly. The port has one such check, the legacy pick's
//!   `available_auths_for_route_model`, so each case runs a call through the
//!   legacy pick (credentials with prefix `team`, model `team/gpt-5.6-luna`)
//!   and through the scheduler pick (plain `gpt-5.6-luna`). The legacy pick
//!   leaves disabled credentials out before the check, where upstream checks
//!   them and skips their deadline; the outcome is the same.
//! - `TestSchedulerPreservesTemporaryDeadlineWithoutChangingQuotaClassification`:
//!   the scheduler shard is not an index here, so the summary counts are
//!   checked through the error they produce (not a cooldown, so quota 0;
//!   `Retry-After` 60, so the earliest is a minute away). The shard leaves
//!   disabled credentials out, so upstream's total of 2 has no equivalent.
//!   "Deleting the disabled entry and setting the temporary one to cooldown"
//!   is removing the disabled credential and re-registering the temporary
//!   one with an exceeded quota.

use std::time::Duration;

use http::HeaderValue;

use super::support::*;
use crate::auth::Auth;
use crate::exec::{Dispatcher, ErrorKind, ExecError};
use crate::manager::Settings;
use crate::manager::classify::{ErrView, result_error_from_error};
use crate::manager::cooldown::add;

const MODEL: &str = "gpt-5.6-luna";

#[derive(Clone, Copy, Debug)]
enum Pick {
    Legacy,
    Scheduler,
}

fn temporary(h: &Harness, pick: Pick) -> Auth {
    let mut credential = auth("temporary", "codex");
    credential.unavailable = true;
    credential.next_retry_after = Some(add(h.now(), Duration::from_secs(60)));
    if let Pick::Legacy = pick {
        credential.prefix = "team".into();
    }
    credential
}

fn disabled(h: &Harness, pick: Pick) -> Auth {
    let mut credential = auth("disabled", "codex");
    credential.disabled = true;
    credential.next_retry_after = Some(add(h.now(), Duration::from_secs(1)));
    if let Pick::Legacy = pick {
        credential.prefix = "team".into();
    }
    credential
}

fn route_model(pick: Pick) -> &'static str {
    match pick {
        Pick::Legacy => "team/gpt-5.6-luna",
        Pick::Scheduler => MODEL,
    }
}

async fn call(h: &Harness, pick: Pick) -> ExecError {
    h.manager
        .execute(
            &providers(&["codex"]),
            request(route_model(pick)),
            options(),
        )
        .await
        .expect_err("call succeeded with no available credential")
}

fn harness() -> Harness {
    let h = Harness::new(Settings::default());
    h.executor(&FakeExecutor::new("codex"));
    h
}

#[tokio::test(start_paused = true)]
async fn temporary_unavailability_preserves_recovery_deadline() {
    for with_disabled in [false, true] {
        for pick in [Pick::Legacy, Pick::Scheduler] {
            let h = harness();
            h.add(temporary(&h, pick), &[MODEL]);
            if with_disabled {
                h.add(disabled(&h, pick), &[MODEL]);
            }
            let label = format!("{pick:?}, with disabled: {with_disabled}");

            let err = call(&h, pick).await;
            let result = result_error_from_error(ErrView::Exec(&err));
            assert!(
                err.kind == ErrorKind::AuthUnavailable
                    && result.code == "auth_unavailable"
                    && result.retryable,
                "{label}: expected recoverable auth_unavailable, got {err:?}"
            );
            assert_eq!(
                err.retry_after_header(),
                Some(HeaderValue::from_static("60")),
                "{label}: Retry-After"
            );
        }
    }

    for pick in [Pick::Legacy, Pick::Scheduler] {
        let h = harness();
        h.add(disabled(&h, pick), &[MODEL]);
        let err = call(&h, pick).await;
        assert_eq!(
            err.retry_after_header(),
            None,
            "{pick:?}: disabled credential must not advertise recovery"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn scheduler_preserves_temporary_deadline_without_changing_quota_classification() {
    let h = harness();
    h.add(temporary(&h, Pick::Scheduler), &[MODEL]);
    h.add(disabled(&h, Pick::Scheduler), &[MODEL]);

    let err = call(&h, Pick::Scheduler).await;
    assert_eq!(
        err.kind,
        ErrorKind::AuthUnavailable,
        "unexpected summary: {err:?}"
    );
    assert_eq!(
        err.retry_after_header(),
        Some(HeaderValue::from_static("60")),
        "scheduler Retry-After"
    );

    h.manager.remove("disabled");
    let mut cooling = temporary(&h, Pick::Scheduler);
    cooling.quota.exceeded = true;
    cooling.quota.next_recover_at = cooling.next_retry_after;
    h.add(cooling, &[MODEL]);
    let err = call(&h, Pick::Scheduler).await;
    assert!(
        err.kind == ErrorKind::ModelCooldown && err.http_status() == 429,
        "quota cooldown contract changed: {err:?}"
    );
}
