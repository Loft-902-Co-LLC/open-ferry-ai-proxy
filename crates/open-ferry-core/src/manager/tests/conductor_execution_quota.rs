// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_execution_quota_test.go
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Each attempt of a call is observed with its own response's headers,
//! never with an earlier attempt's.
//!
//! Deviations from upstream:
//! - `execution_attempts_do_not_reuse_quota_response_headers`: the second
//!   attempt fails at the provider with no headers, where upstream's fails
//!   in `PrepareRequestAuth`, which isn't ported; and there is no
//!   request-wide header holder to check is left empty.
//! - `TestSyncMetadataSessionToContext` and the
//!   `TestApplyRequestAfterAuthInterceptor*` and `TestGhostParentElimination`
//!   tests aren't ported: request interceptors and session metadata sync
//!   aren't.

use http::{HeaderMap, HeaderName, HeaderValue};

use super::support::*;
use crate::exec::{Dispatcher, ExecError};
use crate::manager::Settings;

fn quota_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("x-codex-plan-type", "pro"),
        ("x-codex-primary-used-percent", "91"),
        ("x-codex-primary-window-minutes", "10080"),
        ("x-codex-primary-reset-after-seconds", "3600"),
    ] {
        headers.insert(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
    headers
}

/// Upstream's `TestExecutionAttemptsDoNotReuseQuotaResponseHeaders`.
#[tokio::test(start_paused = true)]
async fn execution_attempts_do_not_reuse_quota_response_headers() {
    for stream in [false, true] {
        let name = if stream { "stream" } else { "non-stream" };
        let h = Harness::new(Settings::default());
        let executor = FakeExecutor::with("codex", |call: &Call| {
            if call.auth_id.ends_with("-a") {
                let mut err = ExecError::upstream(500, "first upstream attempt failed");
                err.headers = quota_headers();
                Reply::Err(err)
            } else {
                Reply::Err(ExecError::upstream(500, "second attempt failed"))
            }
        });
        h.executor(&executor);
        let model = format!("gpt-quota-attempt-isolation-{name}");
        let first = format!("quota-attempt-{name}-a");
        let second = format!("quota-attempt-{name}-b");
        for id in [&first, &second] {
            h.add(auth(id, "codex"), &[model.as_str()]);
        }

        let names = providers(&["codex"]);
        let failed = if stream {
            h.manager
                .execute_stream(&names, request(&model), options())
                .await
                .is_err()
        } else {
            h.manager
                .execute(&names, request(&model), options())
                .await
                .is_err()
        };
        assert!(failed, "{name}: the call should fail");
        let kind = if stream { Kind::Stream } else { Kind::Execute };
        assert_eq!(
            executor.ids(kind),
            [first.clone(), second.clone()],
            "{name}"
        );

        let first = h.get(&first);
        assert_eq!(
            first
                .quota
                .signals
                .get("X-Codex-Primary-Used-Percent")
                .map(String::as_str),
            Some("91"),
            "{name}: {:?}",
            first.quota
        );
        let second = h.get(&second);
        assert!(
            second.quota.signals.is_empty(),
            "{name}: {:?}",
            second.quota
        );
        assert!(second.quota.observed_at.is_none(), "{name}");
    }
}
