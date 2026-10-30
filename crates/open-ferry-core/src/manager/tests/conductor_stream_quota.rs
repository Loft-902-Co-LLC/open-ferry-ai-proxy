// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_stream_quota_test.go (v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A 429 that ends a stream after its first chunk reaches the client as is,
//! without a replay on another credential, and still cools the credential
//! down with the provider's retry hint and scope.
//!
//! Deviations from upstream:
//! - Upstream checks the stream error is the very error value the executor
//!   sent; [`ExecError`] has no identity, so its kind, status, body, retry
//!   hint and scope are compared.
//! - Upstream's `RoundRobinSelector` and `WeightedRoundRobinSelector` are
//!   the round-robin and weighted routing strategies.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use chrono::TimeDelta;
use futures_core::future::BoxFuture;
use futures_util::StreamExt;
use http::HeaderMap;
use tokio::sync::mpsc;

use super::support::*;
use crate::auth::{Auth, Status};
use crate::exec::{Dispatcher, ExecError, Options, Request, Response, StreamResponse};
use crate::executor::ProviderExecutor;
use crate::manager::models::Resolver;
use crate::manager::select::{PickArgs, Selection, is_auth_blocked_for_model};
use crate::manager::{RoutingStrategy, Settings};

// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_issue6503_test.go
// (v8.0.32, MIT). Cancellation drops a pending future here, rather than
// setting Go's request context while ExecuteStream returns.
struct CanceledBootstrapExecutor {
    source: Mutex<Option<mpsc::UnboundedReceiver<Result<Bytes, ExecError>>>>,
    refresh_first: bool,
    calls: AtomicUsize,
}

impl ProviderExecutor for CanceledBootstrapExecutor {
    fn id(&self) -> &str {
        "codex"
    }

    fn execute(
        &self,
        _auth: Arc<Auth>,
        _request: Request,
        _options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        Box::pin(async { Err(ExecError::upstream(501, "not used")) })
    }

    fn count_tokens(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        self.execute(auth, request, options)
    }

    fn execute_stream(
        &self,
        _auth: Arc<Auth>,
        _request: Request,
        _options: Options,
    ) -> BoxFuture<'_, Result<StreamResponse, ExecError>> {
        Box::pin(async move {
            let chunks = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 && self.refresh_first {
                futures_util::stream::iter([Err(ExecError::upstream(401, "token invalidated"))])
                    .boxed()
            } else {
                let source = crate::manager::lock(&self.source)
                    .take()
                    .expect("one source");
                futures_util::stream::unfold(source, |mut source| async move {
                    source.recv().await.map(|chunk| (chunk, source))
                })
                .boxed()
            };
            Ok(StreamResponse {
                headers: HeaderMap::new(),
                chunks,
            })
        })
    }

    fn refresh(&self, auth: Arc<Auth>) -> BoxFuture<'_, Result<Auth, ExecError>> {
        Box::pin(async move {
            let mut refreshed = (*auth).clone();
            refreshed
                .metadata
                .insert("access_token".into(), serde_json::json!("rotated"));
            Ok(refreshed)
        })
    }
}

#[tokio::test(start_paused = true)]
async fn canceled_stream_bootstrap_keeps_ready_upstream_cooldown() {
    for refresh_first in [false, true] {
        for with_retry_after in [false, true] {
            let h = Harness::new(Settings::default());
            let mut candidate = auth("limited", "codex");
            candidate.metadata =
                serde_json::json!({"access_token":"stale", "refresh_token":"refresh"})
                    .as_object()
                    .unwrap()
                    .clone();
            h.add(candidate, &[MODEL]);
            let (tx, rx) = mpsc::unbounded_channel();
            let executor = Arc::new(CanceledBootstrapExecutor {
                source: Mutex::new(Some(rx)),
                refresh_first,
                calls: AtomicUsize::new(0),
            });
            h.manager.register_executor(executor.clone());
            let manager = h.manager.clone();
            let task = tokio::spawn(async move {
                let mut opts = options();
                opts.stream = true;
                manager
                    .execute_stream(&providers(&["codex"]), request(MODEL), opts)
                    .await
            });
            settle().await;
            assert_eq!(
                executor.calls.load(Ordering::SeqCst),
                if refresh_first { 2 } else { 1 }
            );
            let error = if with_retry_after {
                quota_error(true, Duration::from_secs(600))
            } else {
                ExecError::canceled()
            };
            tx.send(Ok(Bytes::new())).unwrap();
            tx.send(Err(error)).unwrap();
            // Abort before the ready chunk gets another normal poll.
            task.abort();
            assert!(task.await.is_err_and(|err| err.is_cancelled()));
            let current = h.get("limited");
            if with_retry_after {
                assert!(current.unavailable);
                assert!(
                    current
                        .next_retry_after
                        .is_some_and(|t| t >= h.now() + TimeDelta::minutes(5))
                );
                assert_eq!(current.quota.reason, "credential_quota");
            } else {
                assert!(!current.unavailable);
                assert!(current.next_retry_after.is_none());
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn stream_executor_error_records_retry_hint_before_returning() {
    let h = Harness::new(Settings::default());
    h.add(auth("limited", "codex"), &[MODEL]);
    let executor = FakeExecutor::with("codex", |_| {
        Reply::Err(quota_error(true, Duration::from_secs(600)))
    });
    h.executor(&executor);
    let _ = h
        .manager
        .execute_stream(&providers(&["codex"]), request(MODEL), options())
        .await;
    let current = h.get("limited");
    assert!(current.unavailable);
    assert!(
        current
            .next_retry_after
            .is_some_and(|t| t >= h.now() + TimeDelta::minutes(5))
    );
}

const MODEL: &str = "stream-quota-model";
const SIBLING_MODEL: &str = "stream-quota-sibling";

/// Upstream's `pickNextMixed(ctx, providers, model, Options{}, nil)`: the
/// credential picked for `model` now.
fn pick(h: &Harness, provider: &str, model: &str) -> String {
    let providers = providers(&[provider]);
    let tried = HashSet::new();
    let now = h.manager.now();
    let mut guard = h.manager.lock();
    let state = &mut *guard;
    let selection = Selection {
        auths: &state.auths,
        executors: &state.executors,
        models: h.manager.models(),
        resolver: Resolver {
            settings: &state.settings,
            oauth: &state.oauth,
        },
        strategy: state.settings.routing_strategy,
        now,
    };
    let args = PickArgs {
        model,
        pinned: "",
        downstream_websocket: false,
        eligibility: Default::default(),
        tried: &tried,
    };
    match selection.pick_next_mixed(&mut state.selector, &providers, &args) {
        Ok(picked) => picked.auth.id.clone(),
        Err(err) => panic!("pick_next_mixed({model}) error = {err}"),
    }
}

fn quota_error(credential_scoped: bool, retry_after: Duration) -> ExecError {
    let body = if credential_scoped {
        r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":3600}}"#
    } else {
        r#"{"error":{"type":"rate_limit_error","message":"Model rate limit exceeded"}}"#
    };
    let mut err = ExecError::upstream(429, body);
    err.retry_after = Some(retry_after);
    err.credential_scoped = credential_scoped;
    err
}

#[tokio::test(start_paused = true)]
async fn execute_stream_quota_failure_preserves_cooldown_and_scope() {
    for credential_scoped in [true, false] {
        for weighted in [false, true] {
            run_case(credential_scoped, weighted).await;
        }
    }
}

async fn run_case(credential_scoped: bool, weighted: bool) {
    let name = format!("credential_scope={credential_scoped}/weighted={weighted}");
    let h = Harness::new(Settings {
        request_retry: 3,
        max_retry_interval: Duration::from_secs(30),
        max_retry_credentials: 0,
        routing_strategy: if weighted {
            RoutingStrategy::Weighted
        } else {
            RoutingStrategy::RoundRobin
        },
        ..Settings::default()
    });
    let high_id = format!("stream-quota-high-{name}");
    let low_id = format!("stream-quota-low-{name}");
    for (id, priority) in [(&high_id, "4"), (&low_id, "3")] {
        let mut candidate = auth(id, "codex");
        candidate.status = Status::Active;
        candidate
            .attributes
            .insert("priority".into(), priority.into());
        candidate.attributes.insert("weight".into(), "1".into());
        h.add(candidate, &[MODEL, SIBLING_MODEL]);
    }

    let retry_after = Duration::from_secs(3600);
    let executor = FakeExecutor::with("codex", move |_: &Call| {
        Reply::chunks(vec![
            Ok(Bytes::from_static(
                b"data: {\"type\":\"response.created\"}\n\n",
            )),
            Err(quota_error(credential_scoped, retry_after)),
        ])
    });
    h.executor(&executor);

    let before = h.now();
    let mut opts = options();
    opts.stream = true;
    let stream = h
        .manager
        .execute_stream(&providers(&["codex"]), request(MODEL), opts)
        .await
        .unwrap_or_else(|err| panic!("{name}: execute_stream() error = {err}"));
    let (chunks, end) = collect(stream).await;
    settle().await;

    let payloads = chunks.iter().filter(|chunk| !chunk.is_empty()).count();
    let failures = usize::from(end.is_some());
    let attempts = executor.ids(Kind::Stream);
    assert!(
        payloads == 1 && failures == 1 && attempts == [high_id.clone()],
        "{name}: started stream must retain its payload and error without replay: \
         payloads={payloads} failures={failures} attempts={attempts:?}"
    );
    let end = end.expect("stream error");
    let want = quota_error(credential_scoped, retry_after);
    assert!(
        end.kind == want.kind
            && end.status == want.status
            && end.message == want.message
            && end.retry_after == want.retry_after
            && end.credential_scoped == want.credential_scoped,
        "{name}: stream error = {end:?}, want original quota error"
    );

    let high = h.get(&high_id);
    let one_hour = TimeDelta::hours(1);
    let state = high.model_states.get(MODEL);
    assert!(
        state
            .and_then(|state| state.next_retry_after)
            .is_some_and(|t| t >= before + one_hour),
        "{name}: model cooldown lost upstream retry hint: state={state:?}"
    );
    if credential_scoped {
        assert!(
            high.quota.reason == "credential_quota"
                && high
                    .quota
                    .next_recover_at
                    .is_some_and(|t| t >= before + one_hour),
            "{name}: credential cooldown lost scope or retry hint: quota={:?}",
            high.quota
        );
    }

    for requested_model in [MODEL, SIBLING_MODEL] {
        let want_id = if requested_model == SIBLING_MODEL && !credential_scoped {
            &high_id
        } else {
            &low_id
        };
        assert_eq!(
            &pick(&h, "codex", requested_model),
            want_id,
            "{name}: pick_next_mixed({requested_model})"
        );
    }
    assert!(
        is_auth_blocked_for_model(&high, MODEL, before + TimeDelta::minutes(1)).0,
        "{name}: exhausted model becomes selectable before the upstream reset"
    );
}
