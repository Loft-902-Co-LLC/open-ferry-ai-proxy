// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_retry_round_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Retry rounds: each credential takes as many rounds as its own
//! `request_retry` allows, `max_retry_credentials` caps every round, and a
//! call keeps the `request_retry` it started with.
//!
//! Deviations from upstream:
//! - `TestExecuteHomeRetryRoundCredentialWindows` is dropped: the Home
//!   dispatcher isn't ported.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::json;

use super::support::*;
use crate::exec::{Dispatcher, ExecError};
use crate::manager::Settings;

#[derive(Clone, Copy, Debug)]
enum Path {
    Execute,
    Count,
    Stream,
}

const PATHS: [Path; 3] = [Path::Execute, Path::Count, Path::Stream];

impl Path {
    fn kind(self) -> Kind {
        match self {
            Path::Execute => Kind::Execute,
            Path::Count => Kind::Count,
            Path::Stream => Kind::Stream,
        }
    }
}

async fn invoke(h: &Harness, path: Path, provider: &str, model: &str) -> Result<(), ExecError> {
    let provs = providers(&[provider]);
    match path {
        Path::Execute => h
            .manager
            .execute(&provs, request(model), options())
            .await
            .map(|_| ()),
        Path::Count => h
            .manager
            .count_tokens(&provs, request(model), options())
            .await
            .map(|_| ()),
        Path::Stream => {
            let mut opts = options();
            opts.stream = true;
            h.manager
                .execute_stream(&provs, request(model), opts)
                .await
                .map(|_| ())
        }
    }
}

fn retry_round_executor() -> Arc<FakeExecutor> {
    FakeExecutor::with("retry-round-test", |_| {
        Reply::status(500, "retry-round test failure")
    })
}

/// Upstream's `registerRetryRoundLocalAuths`: credentials in ID order, each
/// with its own `request_retry` and cooling off.
fn register_retry_round_local_auths(
    h: &Harness,
    provider: &str,
    model: &str,
    limits: &[(&str, i64)],
) {
    let mut sorted = limits.to_vec();
    sorted.sort_by_key(|(id, _)| *id);
    for (id, limit) in sorted {
        h.add(
            auth_with_metadata(
                id,
                provider,
                json!({"request_retry": limit, "disable_cooling": true}),
            ),
            &[model],
        );
    }
}

fn count_ids(ids: &[String]) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for id in ids {
        *counts.entry(id.clone()).or_insert(0) += 1;
    }
    counts
}

fn count(counts: &HashMap<String, usize>, id: &str) -> usize {
    counts.get(id).copied().unwrap_or(0)
}

#[tokio::test(start_paused = true)]
async fn execute_retry_round_credential_windows() {
    for path in PATHS {
        let h = Harness::new(Settings {
            request_retry: 3,
            ..Settings::default()
        });
        let executor = retry_round_executor();
        h.executor(&executor);
        register_retry_round_local_auths(
            &h,
            "retry-round-test",
            "retry-round-model",
            &[
                ("retry-round-a", 3),
                ("retry-round-b", 2),
                ("retry-round-c", 2),
            ],
        );

        let result = invoke(&h, path, "retry-round-test", "retry-round-model").await;
        assert!(
            result.is_err(),
            "{path:?}: execution error = nil, want terminal retry error"
        );
        let ids = executor.ids(path.kind());
        let counts = count_ids(&ids);
        assert!(
            count(&counts, "retry-round-a") == 4
                && count(&counts, "retry-round-b") == 3
                && count(&counts, "retry-round-c") == 3,
            "{path:?}: credential call counts = {counts:?}, want A=4 B=3 C=3; calls={ids:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn execute_retry_round_max_credentials_ages_skipped_auths() {
    let h = Harness::new(Settings {
        request_retry: 2,
        max_retry_credentials: 3,
        ..Settings::default()
    });
    let executor = retry_round_executor();
    h.executor(&executor);
    register_retry_round_local_auths(
        &h,
        "retry-round-test",
        "retry-round-model-cap",
        &[
            ("retry-cap-a", 1),
            ("retry-cap-b", 1),
            ("retry-cap-c", 1),
            ("retry-cap-d", 2),
        ],
    );

    let result = invoke(
        &h,
        Path::Execute,
        "retry-round-test",
        "retry-round-model-cap",
    )
    .await;
    assert!(
        result.is_err(),
        "execution error = nil, want terminal retry error"
    );
    let calls = executor.ids(Kind::Execute);
    assert_eq!(
        calls.len(),
        7,
        "credential calls = {calls:?}, want three initial, three round-1, and one round-2 call"
    );
    let counts = count_ids(&calls);
    assert!(
        count(&counts, "retry-cap-a") <= 2
            && count(&counts, "retry-cap-b") <= 2
            && count(&counts, "retry-cap-c") <= 2
            && count(&counts, "retry-cap-d") <= 3,
        "credential call counts exceed their round windows: {counts:?}"
    );
    assert_eq!(
        calls[calls.len() - 1],
        "retry-cap-d",
        "last retry call; calls={calls:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn execute_snapshots_default_request_retry() {
    // (name, initial, next, want calls, want success)
    let scenarios = [
        ("decrease after request starts", 1, 0, 2, true),
        ("increase after request starts", 0, 1, 1, false),
    ];
    for path in PATHS {
        for (name, initial, next, want_calls, want_success) in scenarios {
            let h = Harness::new(Settings {
                request_retry: initial,
                ..Settings::default()
            });
            let calls = Arc::new(AtomicUsize::new(0));
            let manager = h.manager.clone();
            let handler_calls = calls.clone();
            // The first call changes the default request_retry, then fails;
            // later calls succeed (a stream with no chunks, as upstream's
            // closed channel).
            let executor = FakeExecutor::with("retry-config-snapshot", move |call| {
                if handler_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    manager.set_settings(Settings {
                        request_retry: next,
                        ..Settings::default()
                    });
                    return Reply::status(500, "retry config changed");
                }
                match call.kind {
                    Kind::Stream => Reply::chunks(vec![]),
                    _ => Reply::ok("ok"),
                }
            });
            h.executor(&executor);
            h.add(
                auth_with_metadata(
                    "retry-config-snapshot-auth",
                    "retry-config-snapshot",
                    json!({"disable_cooling": true}),
                ),
                &["retry-config-snapshot-model"],
            );

            let result = invoke(
                &h,
                path,
                "retry-config-snapshot",
                "retry-config-snapshot-model",
            )
            .await;
            if want_success {
                assert!(
                    result.is_ok(),
                    "{path:?}/{name}: execution error = {:?}, want success",
                    result.err()
                );
            } else {
                let status = result.as_ref().err().map(ExecError::http_status);
                assert_eq!(
                    status,
                    Some(500),
                    "{path:?}/{name}: execution error = {:?}, want HTTP 500",
                    result.err()
                );
            }
            assert_eq!(
                calls.load(Ordering::SeqCst),
                want_calls,
                "{path:?}/{name}: executor calls"
            );
        }
    }
}
