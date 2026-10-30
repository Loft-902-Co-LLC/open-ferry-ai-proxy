//! Native issue7017 cases from conductor_issue7017, execution and review r1/r3/r4/r5.
//! Home/plugin-only tests are deliberately excluded.
use super::support::*;
use crate::auth::{ModelState, QuotaState, Status};
use crate::exec::{Dispatcher, ExecError};
use crate::manager::classify::{ErrView, result_error_from_error};
use crate::manager::cooldown::MIN_QUOTA_COOLDOWN_FLOOR;
use crate::manager::models::Resolver;
use crate::manager::policy::Eligibility;
use crate::manager::retry::{RetryQuery, closest_cooldown_wait};
use crate::manager::select::Selection;
use crate::manager::{CallResult, Settings};
use chrono::TimeDelta;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

fn error(hint: Option<Duration>) -> ExecError {
    ExecError::transient_capacity("turn_rate_limit_exceeded", hint)
}
fn result(model: &str, hint: Option<Duration>) -> CallResult {
    CallResult {
        auth_id: "a".into(),
        provider: "codex".into(),
        model: model.into(),
        error: Some(result_error_from_error(ErrView::Exec(&error(hint)))),
        retry_after: hint,
        ..Default::default()
    }
}

#[tokio::test(start_paused = true)]
async fn p47_issue7017_model_and_auth_hints_never_create_quota() {
    for model in ["", "m"] {
        for hint in [
            None,
            Some(Duration::ZERO),
            Some(Duration::from_secs(8)),
            Some(Duration::from_secs(256)),
        ] {
            let h = Harness::new(Settings::default());
            h.add(auth("a", "codex"), &["m", "sibling"]);
            h.manager.mark_result(&result(model, hint));
            let live = h.get("a");
            let (quota, next, unavailable) = if model.is_empty() {
                (&live.quota, live.next_retry_after, live.unavailable)
            } else {
                let state = &live.model_states[model];
                (&state.quota, state.next_retry_after, state.unavailable)
            };
            assert!(!quota.exceeded);
            assert_eq!(quota.backoff_level, 0);
            match hint {
                Some(hint) if hint.is_zero() => {
                    assert!(next.is_none());
                    assert!(!unavailable);
                }
                Some(hint) => assert_eq!(next, Some(h.now() + TimeDelta::from_std(hint).unwrap())),
                None => assert!(next.unwrap() >= h.now() + TimeDelta::seconds(30)),
            }
            assert!(
                !live
                    .model_states
                    .get("sibling")
                    .is_some_and(|state| state.unavailable)
            );
            let views = crate::manager::cooldown_snapshot_for_auth(&live, h.now());
            assert_eq!(views.len(), usize::from(hint != Some(Duration::ZERO)));
            for view in views {
                assert_eq!(view.reason, "transient_rate_limit");
                assert_eq!(
                    view.scope,
                    if model.is_empty() {
                        "credential"
                    } else {
                        "model"
                    }
                );
                assert_eq!(view.model_key, model);
                assert_eq!(Some(view.retry_at), next);
                assert!(view.remaining_seconds > 0);
                assert_eq!(view.http_status, 429);
                assert!(view.backoff_level.is_none());
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn p47_issue7017_zero_hint_and_disabled_transient_keep_live_quota() {
    for model in ["", "m"] {
        for unknown in [false, true] {
            for hint in [None, Some(Duration::ZERO), Some(Duration::from_secs(8))] {
                let h = Harness::new(Settings::default());
                let future = (!unknown).then(|| h.now() + TimeDelta::hours(2));
                let quota = QuotaState {
                    exceeded: true,
                    reason: "quota".into(),
                    next_recover_at: future,
                    backoff_level: 2,
                    ..Default::default()
                };
                let mut a = auth("a", "codex");
                if model.is_empty() {
                    a.quota = quota.clone();
                    a.unavailable = true;
                    a.next_retry_after = future;
                } else {
                    a.model_states.insert(
                        model.into(),
                        ModelState {
                            quota: quota.clone(),
                            unavailable: true,
                            next_retry_after: future,
                            status: Status::Error,
                            ..Default::default()
                        },
                    );
                }
                h.add(a, &["m"]);
                h.manager.mark_result(&result(model, hint));
                let live = h.get("a");
                let (actual, next, unavailable) = if model.is_empty() {
                    (&live.quota, live.next_retry_after, live.unavailable)
                } else {
                    let state = &live.model_states[model];
                    (&state.quota, state.next_retry_after, state.unavailable)
                };
                assert!(actual.exceeded);
                assert_eq!(actual.backoff_level, 2);
                assert_eq!(actual.next_recover_at, future);
                assert_eq!(next, future);
                assert!(unavailable);
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn p47_issue7017_wait_floor_uses_own_live_quota_only() {
    for kind in [
        "none",
        "sibling",
        "expired-own",
        "live-own",
        "credential",
        "unknown-own",
    ] {
        for hint in [0, 8] {
            let h = Harness::new(Settings {
                request_retry: 1,
                ..Default::default()
            });
            let now = h.now();
            let mut a = auth("a", "codex");
            let quota = QuotaState {
                exceeded: true,
                reason: "quota".into(),
                next_recover_at: Some(now + TimeDelta::seconds(3)),
                backoff_level: 1,
                ..Default::default()
            };
            let mut state = ModelState {
                unavailable: hint > 0,
                next_retry_after: (hint > 0).then(|| now + TimeDelta::seconds(hint)),
                last_error: Some(result_error_from_error(ErrView::Exec(&error(Some(
                    Duration::ZERO,
                ))))),
                status: Status::Error,
                ..Default::default()
            };
            match kind {
                "sibling" => {
                    a.quota = quota.clone();
                    a.model_states.insert(
                        "sibling".into(),
                        ModelState {
                            unavailable: true,
                            next_retry_after: quota.next_recover_at,
                            quota: quota.clone(),
                            ..Default::default()
                        },
                    );
                }
                "expired-own" => {
                    state.quota = quota.clone();
                    state.quota.next_recover_at = Some(now - TimeDelta::hours(1));
                }
                "live-own" => {
                    state.quota = quota.clone();
                    state.unavailable = true;
                    state.next_retry_after = quota.next_recover_at;
                }
                "unknown-own" => {
                    state.quota = quota.clone();
                    state.quota.next_recover_at = None;
                    state.unavailable = true;
                    state.next_retry_after = None;
                }
                "credential" => {
                    a.quota = quota.clone();
                    a.quota.reason = "credential_quota".into();
                    a.unavailable = true;
                    a.next_retry_after = quota.next_recover_at;
                }
                _ => {}
            }
            a.model_states.insert("m".into(), state);
            h.add(a, &["m", "sibling"]);
            let executor = FakeExecutor::new("codex");
            h.executor(&executor);
            let locked = h.manager.lock();
            let selection = Selection {
                auths: &locked.auths,
                executors: &locked.executors,
                models: h.manager.models(),
                resolver: Resolver {
                    settings: &locked.settings,
                    oauth: &locked.oauth,
                },
                strategy: locked.settings.routing_strategy,
                now,
            };
            let attempted = HashSet::from(["a".into()]);
            let providers = providers(&["codex"]);
            let query = RetryQuery {
                providers: &providers,
                model: "m",
                pinned: "",
                attempt: 0,
                default_retry: 1,
                attempted: &attempted,
                eligibility: Eligibility::for_request(&options()),
            };
            let wait = closest_cooldown_wait(&selection, &query, 429);
            match kind {
                "unknown-own" => assert!(wait.is_none()),
                "live-own" | "credential" => assert!(wait.unwrap() >= MIN_QUOTA_COOLDOWN_FLOOR),
                _ => assert_eq!(wait, Some(Duration::from_secs(hint as u64)), "{kind}"),
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn p47_issue7017_execute_count_stream_and_bootstrap_keep_trusted_headers() {
    for path in [0, 1, 2, 3, 4] {
        for credentials in [1, 2] {
            let h = Harness::new(Settings {
                disable_cooling: true,
                ..Default::default()
            });
            let executor = FakeExecutor::with("codex", move |_| {
                let mut e = error(Some(Duration::from_secs(256)));
                e.headers.insert("retry-after", "9999".parse().unwrap());
                e.headers.insert("x-request-id", "dummy".parse().unwrap());
                if path == 4 {
                    Reply::chunks(vec![Ok("accepted".into()), Err(e)])
                } else if path == 3 {
                    Reply::chunks(vec![Err(e)])
                } else {
                    Reply::Err(e)
                }
            });
            h.executor(&executor);
            h.add(auth("a", "codex"), &["m", "sibling"]);
            if credentials == 2 {
                h.add(auth("b", "codex"), &["m", "sibling"]);
            }
            let providers = providers(&["codex"]);
            let e = match path {
                0 => h
                    .manager
                    .execute(&providers, request("m"), options())
                    .await
                    .unwrap_err(),
                1 => h
                    .manager
                    .count_tokens(&providers, request("m"), options())
                    .await
                    .unwrap_err(),
                _ => match h
                    .manager
                    .execute_stream(&providers, request("m"), options())
                    .await
                {
                    Err(e) if path == 2 => e,
                    Err(_) => panic!("bootstrap and accepted failures must return a stream"),
                    Ok(stream) => {
                        assert_ne!(path, 2, "a pre-stream error is not a bootstrap stream");
                        collect(stream).await.1.unwrap()
                    }
                },
            };
            assert_eq!(e.http_status(), 429);
            assert_eq!(e.failure_code(), Some("transient_rate_limit"));
            assert!(e.has_trusted_retry_deadline());
            assert_eq!(e.retry_after_header().unwrap(), "256");
            assert_eq!(e.headers["x-request-id"], "dummy");
            assert_eq!(
                executor
                    .ids(match path {
                        0 => Kind::Execute,
                        1 => Kind::Count,
                        _ => Kind::Stream,
                    })
                    .len(),
                if path == 4 { 1 } else { credentials }
            );
            assert!(!h.get("a").model_states["m"].quota.exceeded);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn p47_issue7017_explicit_zero_allows_one_immediate_retry() {
    let h = Harness::new(Settings {
        request_retry: 1,
        max_retry_interval: Duration::ZERO,
        ..Default::default()
    });
    let n = Arc::new(AtomicUsize::new(0));
    let count = n.clone();
    let executor = FakeExecutor::with("codex", move |_| {
        if count.fetch_add(1, Ordering::SeqCst) == 0 {
            Reply::Err(error(Some(Duration::ZERO)))
        } else {
            Reply::ok("ok")
        }
    });
    h.executor(&executor);
    h.add(auth("a", "codex"), &["m"]);
    h.manager
        .execute(&providers(&["codex"]), request("m"), options())
        .await
        .unwrap();
    assert_eq!(n.load(Ordering::SeqCst), 2);
}

#[test]
fn p47_issue7017_trusted_deadline_identity_precedence_decay_and_expiry() {
    let mut cause = error(Some(Duration::from_secs(256)));
    cause.headers.insert("retry-after", "9999".parse().unwrap());
    let original = cause.to_string();
    assert!(
        !cause
            .clone()
            .with_trusted_retry_after(Duration::ZERO)
            .has_trusted_retry_deadline()
    );
    assert!(cause.has_retry_after_override());
    assert_eq!(cause.retry_after_header().unwrap(), "256");
    let mut zero = error(Some(Duration::ZERO));
    zero.headers.insert("retry-after", "9999".parse().unwrap());
    assert!(!zero.has_retry_after_override());
    assert!(zero.retry_after_header().is_none());
    let mut e = cause.with_trusted_retry_after(Duration::from_secs(8));
    assert_eq!(e.to_string(), original);
    assert_eq!(e.retry_after, Some(Duration::from_secs(256)));
    assert_eq!(e.http_status(), 429);
    assert_eq!(e.headers["retry-after"], "9999");
    assert_eq!(e.retry_after_header().unwrap(), "8");
    assert!(e.effective_retry_after().unwrap() <= Duration::from_secs(8));
    e.trusted_retry_deadline = Some(Instant::now() - Duration::from_secs(1));
    assert!(e.has_trusted_retry_deadline());
    assert!(e.retry_after_header().is_none());
    assert_eq!(e.effective_retry_after(), Some(Duration::from_secs(256)));
    assert_eq!(
        e.clone()
            .with_trusted_retry_after(Duration::from_secs(30))
            .trusted_retry_deadline,
        e.trusted_retry_deadline
    );
    e.retry_after = None;
    assert!(e.effective_retry_after().is_none());
    for e in [
        ExecError::auth_unavailable(Duration::from_secs(3)),
        ExecError::model_cooldown("m", None, Duration::from_secs(3), None),
    ] {
        assert!(e.has_trusted_retry_deadline());
        assert!(e.retry_after_header().is_some());
    }
}

#[tokio::test(start_paused = true)]
async fn p47_issue7017_credential_scope_restores_quota_classification() {
    let h = Harness::new(Settings::default());
    h.add(auth("a", "codex"), &["m"]);
    let e = error(Some(Duration::from_secs(60))).with_credential_scoped();
    assert!(e.failure_code().is_none());
    let mut r = result("m", e.retry_after);
    r.error = Some(result_error_from_error(ErrView::Exec(&e)));
    r.credential_scope = true;
    h.manager.mark_result(&r);
    assert!(h.get("a").quota.exceeded);
    assert_eq!(h.get("a").quota.reason, "credential_quota");
}

#[tokio::test(start_paused = true)]
async fn p47_issue7017_stale_capacity_cannot_undo_newer_state_or_refresh_merge() {
    let h = Harness::new(Settings::default());
    h.add(auth("a", "codex"), &["m"]);
    let base = h.get("a");
    let older_seq = h.manager.next_execution_seq();
    let newer_seq = h.manager.next_execution_seq();
    let mut newer = result("m", Some(Duration::from_secs(256)));
    newer.execution_seq = newer_seq;
    h.manager.mark_result(&newer);
    let before = h.get("a");
    let mut stale = result("m", Some(Duration::ZERO));
    stale.execution_seq = older_seq;
    h.manager.mark_result(&stale);
    let live = h.get("a");
    assert_eq!(
        live.model_states["m"].next_retry_after,
        before.model_states["m"].next_retry_after
    );
    assert_eq!(live.model_states["m"].cooldown_recorded_seq, newer_seq);
    let merged = crate::manager::merge::merge_refreshed_auth(&base, &live, &base, h.now());
    assert_eq!(
        merged.model_states["m"].next_retry_after,
        live.model_states["m"].next_retry_after
    );
}

#[tokio::test(start_paused = true)]
async fn p47_issue7017_disabled_transient_default_preserves_quota() {
    let h = Harness::new(Settings {
        transient_error_cooldown_seconds: -1,
        ..Default::default()
    });
    let mut a = auth("a", "codex");
    let future = h.now() + TimeDelta::hours(2);
    a.model_states.insert(
        "m".into(),
        ModelState {
            unavailable: true,
            status: Status::Error,
            next_retry_after: Some(future),
            quota: QuotaState {
                exceeded: true,
                reason: "quota".into(),
                next_recover_at: Some(future),
                backoff_level: 2,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    h.add(a, &["m"]);
    h.manager.mark_result(&result("m", None));
    assert_eq!(h.get("a").model_states["m"].next_retry_after, Some(future));
    assert!(h.get("a").model_states["m"].unavailable);
    h.manager.mark_result(&result("fresh", None));
    assert!(!h.get("a").model_states["fresh"].unavailable);
    assert!(!h.get("a").model_states["fresh"].quota.exceeded);
}

#[tokio::test(start_paused = true)]
async fn p47r2_retry_deadline_uses_only_the_errors_own_positive_hint() {
    for path in 0..5 {
        for cooling_disabled in [false, true] {
            for (status, hint) in [
                (429, Some(Duration::from_secs(3))),
                (429, None),
                (503, Some(Duration::from_secs(3))),
                (503, None),
            ] {
                let h = Harness::new(Settings {
                    disable_cooling: cooling_disabled,
                    ..Default::default()
                });
                let executor = FakeExecutor::with("codex", move |_| {
                    let mut error = ExecError::upstream(status, "dummy upstream failure");
                    error.retry_after = hint;
                    error.headers.insert("retry-after", "9999".parse().unwrap());
                    match path {
                        3 => Reply::chunks(vec![Err(error)]),
                        4 => Reply::chunks(vec![Ok("accepted".into()), Err(error)]),
                        _ => Reply::Err(error),
                    }
                });
                h.executor(&executor);
                h.add(auth("a", "codex"), &["m"]);
                let providers = providers(&["codex"]);
                let error = match path {
                    0 => h
                        .manager
                        .execute(&providers, request("m"), options())
                        .await
                        .unwrap_err(),
                    1 => h
                        .manager
                        .count_tokens(&providers, request("m"), options())
                        .await
                        .unwrap_err(),
                    _ => {
                        let stream = h
                            .manager
                            .execute_stream(&providers, request("m"), options())
                            .await;
                        if path == 2 {
                            stream.err().expect("pre-stream failure")
                        } else {
                            collect(stream.expect("bootstrap/accepted failure stream"))
                                .await
                                .1
                                .unwrap()
                        }
                    }
                };
                assert_eq!(error.http_status(), status);
                assert_eq!(
                    error.has_trusted_retry_deadline(),
                    hint.is_some(),
                    "path={path}, status={status}, cooling_disabled={cooling_disabled}"
                );
                if hint.is_some() {
                    assert_eq!(
                        error.retry_after_header().unwrap(),
                        "3",
                        "must not advertise the cooldown floor"
                    );
                    assert!(error.effective_retry_after().unwrap() <= Duration::from_secs(3));
                }
                assert_eq!(executor.calls().len(), 1);
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn p47r2_cooling_followup_has_a_deadline_without_another_attempt() {
    let h = Harness::new(Settings::default());
    let executor = FakeExecutor::with("codex", |_| {
        Reply::Err(error(Some(Duration::from_secs(256))))
    });
    h.executor(&executor);
    h.add(auth("a", "codex"), &["m"]);
    let providers = providers(&["codex"]);
    let first = h
        .manager
        .execute(&providers, request("m"), options())
        .await
        .unwrap_err();
    assert_eq!(first.retry_after_header().unwrap(), "256");
    let followup = h
        .manager
        .execute(&providers, request("m"), options())
        .await
        .unwrap_err();
    assert!(followup.has_trusted_retry_deadline());
    assert!(followup.retry_after_header().is_some());
    assert_eq!(executor.calls().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn p47r2_capacity_after_an_expired_newer_cooldown_is_recorded() {
    for model in ["", "m"] {
        let h = Harness::new(Settings::default());
        h.add(auth("a", "codex"), &["m"]);
        let seq = h.manager.next_execution_seq();
        let mut outage = result(model, Some(Duration::from_secs(30)));
        outage.error = Some(result_error_from_error(ErrView::Exec(
            &ExecError::upstream(503, "dummy outage"),
        )));
        outage.execution_seq = seq;
        h.manager.mark_result(&outage);
        h.clock.advance(Duration::from_secs(31));
        let deadline = h.now() + TimeDelta::seconds(60);
        let mut capacity = result(model, Some(Duration::from_secs(60)));
        capacity.execution_seq = seq;
        h.manager.mark_result(&capacity);
        let live = h.get("a");
        let next = if model.is_empty() {
            live.next_retry_after
        } else {
            live.model_states[model].next_retry_after
        };
        assert_eq!(
            next,
            Some(deadline),
            "an expired watermark must not discard capacity"
        );
    }
}
