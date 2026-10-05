// Ported from CLIProxyAPI sdk/cliproxy/auth/cooldown_view_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The cooldown views the management API lists: which scopes show, the
//! seconds left, the reason codes, and how two states of one model fold.
//!
//! Deviations from upstream:
//! - The nil-auth case is dropped (no nil credential in Rust), and the nil
//!   model state cases use a default state, which blocks nothing either.
//! - The "unknown is sanitized" check reads the view's `Debug` text; the
//!   JSON encoding is the management API's, tested there.
//! - The deduplication test's now is in UTC, not +01:00, and its check that
//!   `retry_at` is in UTC is dropped: [`Timestamp`] is always UTC. Writing
//!   to a returned view can't reach the credential in Rust, so only the
//!   credential's model states and metadata are compared before and after
//!   (an [`Auth`] has no equality).

use std::collections::BTreeMap;

use chrono::{TimeDelta, TimeZone, Utc};
use serde_json::json;

use crate::auth::{Auth, AuthError, ModelState, QuotaState, Status, Timestamp};
use crate::manager::classify::CODE_FORCE_COOLDOWN;
use crate::manager::cooldown_snapshot_for_auth;
use crate::manager::select::{BlockReason, is_auth_blocked_for_model};

fn base() -> Timestamp {
    Utc.with_ymd_and_hms(2026, 7, 17, 10, 0, 0).unwrap()
}

fn http_error(status: u16) -> Option<AuthError> {
    Some(AuthError {
        http_status: status,
        ..AuthError::default()
    })
}

fn states(entries: Vec<(&str, ModelState)>) -> BTreeMap<String, ModelState> {
    entries
        .into_iter()
        .map(|(key, state)| (key.to_owned(), state))
        .collect()
}

fn metadata(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    match value {
        serde_json::Value::Object(map) => map,
        _ => unreachable!(),
    }
}

/// `scope:model` for each view.
fn keys(auth: &Auth, now: Timestamp) -> Vec<String> {
    cooldown_snapshot_for_auth(auth, now)
        .iter()
        .map(|view| format!("{}:{}", view.scope, view.model_key))
        .collect()
}

#[test]
fn cooldown_snapshot_for_auth_scopes() {
    let now = base();
    let model_state = |after: TimeDelta| ModelState {
        unavailable: true,
        next_retry_after: Some(now + after),
        quota: QuotaState {
            exceeded: true,
            reason: "quota".into(),
            next_recover_at: Some(now + after),
            backoff_level: 6,
        },
        last_error: http_error(429),
        ..ModelState::default()
    };
    let cases: Vec<(&str, Auth, Vec<&str>)> = vec![
        ("empty", Auth::default(), vec![]),
        (
            "aggregate is not credential cooldown",
            Auth {
                unavailable: true,
                next_retry_after: Some(now + TimeDelta::minutes(1)),
                quota: QuotaState {
                    exceeded: true,
                    reason: "quota".into(),
                    next_recover_at: Some(now + TimeDelta::minutes(1)),
                    ..QuotaState::default()
                },
                model_states: states(vec![
                    ("b", model_state(TimeDelta::minutes(1))),
                    ("a", model_state(TimeDelta::seconds(32))),
                    ("ready", ModelState::default()),
                ]),
                ..Auth::default()
            },
            vec!["model:a", "model:b"],
        ),
        (
            "credential quota and longer model coexist",
            Auth {
                next_retry_after: Some(now + TimeDelta::hours(1)),
                quota: QuotaState {
                    exceeded: true,
                    reason: "credential_quota".into(),
                    next_recover_at: Some(now + TimeDelta::seconds(20)),
                    backoff_level: 9,
                },
                model_states: states(vec![("a", model_state(TimeDelta::minutes(1)))]),
                ..Auth::default()
            },
            vec!["credential:", "model:a"],
        ),
        (
            "credential fallback",
            Auth {
                unavailable: true,
                next_retry_after: Some(now + TimeDelta::minutes(1)),
                last_error: http_error(503),
                ..Auth::default()
            },
            vec!["credential:"],
        ),
        (
            "nil model states still suppress fallback like selector",
            Auth {
                unavailable: true,
                next_retry_after: Some(now + TimeDelta::minutes(1)),
                model_states: states(vec![("a", ModelState::default())]),
                ..Auth::default()
            },
            vec![],
        ),
        (
            "disabled credential retains timer",
            Auth {
                disabled: true,
                status: Status::Disabled,
                unavailable: true,
                next_retry_after: Some(now + TimeDelta::minutes(1)),
                ..Auth::default()
            },
            vec!["credential:"],
        ),
        (
            "expired token does not erase timer",
            Auth {
                metadata: metadata(json!({
                    "expired": (now - TimeDelta::hours(1)).to_rfc3339(),
                })),
                model_states: states(vec![("a", model_state(TimeDelta::minutes(1)))]),
                ..Auth::default()
            },
            vec!["model:a"],
        ),
        (
            "disabled without timer",
            Auth {
                disabled: true,
                model_states: states(vec![(
                    "a",
                    ModelState {
                        status: Status::Disabled,
                        ..ModelState::default()
                    },
                )]),
                ..Auth::default()
            },
            vec![],
        ),
        (
            "forced timer survives disable cooling override",
            Auth {
                metadata: metadata(json!({"disable_cooling": true})),
                unavailable: true,
                next_retry_after: Some(now + TimeDelta::minutes(1)),
                last_error: Some(AuthError {
                    code: CODE_FORCE_COOLDOWN.into(),
                    ..AuthError::default()
                }),
                ..Auth::default()
            },
            vec!["credential:"],
        ),
    ];
    for (name, auth, want) in cases {
        assert_eq!(keys(&auth, now), want, "{name}");
        if name == "credential quota and longer model coexist" {
            let got = cooldown_snapshot_for_auth(&auth, now);
            assert_eq!(got[0].remaining_seconds, 20, "{name}");
            assert_eq!(got[0].backoff_level, None, "{name}");
            assert_eq!(got[0].http_status, 0, "{name}");
        }
    }
}

#[test]
fn cooldown_snapshot_for_auth_time_boundaries() {
    let now = base();
    let ns = TimeDelta::nanoseconds;
    let s = TimeDelta::seconds;
    let blocked = |after: TimeDelta| ModelState {
        unavailable: true,
        next_retry_after: Some(now + after),
        ..ModelState::default()
    };
    let quota = |exceeded: bool, recover: Option<TimeDelta>, level: u32| QuotaState {
        exceeded,
        next_recover_at: recover.map(|d| now + d),
        backoff_level: level,
        ..QuotaState::default()
    };
    let cases: Vec<(&str, ModelState, i64)> = vec![
        ("just before expiry", blocked(ns(1)), 1),
        (
            "fraction rounds up",
            blocked(TimeDelta::milliseconds(1500)),
            2,
        ),
        ("exact expiry", blocked(TimeDelta::zero()), 0),
        ("past expiry", blocked(ns(-1)), 0),
        (
            "historical error and backoff",
            ModelState {
                status: Status::Error,
                quota: quota(false, None, 6),
                ..ModelState::default()
            },
            0,
        ),
        (
            "no deadline",
            ModelState {
                unavailable: true,
                quota: quota(true, None, 0),
                ..ModelState::default()
            },
            0,
        ),
        (
            "inactive future timestamp",
            ModelState {
                next_retry_after: Some(now + TimeDelta::minutes(1)),
                ..ModelState::default()
            },
            0,
        ),
        (
            "later quota time",
            ModelState {
                quota: quota(true, Some(s(3)), 0),
                ..blocked(s(1))
            },
            3,
        ),
        (
            "later retry time",
            ModelState {
                quota: quota(true, Some(s(1)), 0),
                ..blocked(s(4))
            },
            4,
        ),
        (
            "quota only",
            ModelState {
                quota: quota(true, Some(s(5)), 0),
                ..ModelState::default()
            },
            5,
        ),
        (
            "expired quota and retry",
            ModelState {
                quota: quota(true, Some(TimeDelta::zero()), 6),
                ..blocked(TimeDelta::zero())
            },
            0,
        ),
        (
            "retry hint independent of backoff",
            ModelState {
                quota: QuotaState {
                    exceeded: true,
                    reason: "quota".into(),
                    backoff_level: 2,
                    ..QuotaState::default()
                },
                ..blocked(s(97))
            },
            97,
        ),
    ];
    for (name, state, seconds) in cases {
        let auth = Auth {
            model_states: states(vec![("a", state)]),
            ..Auth::default()
        };
        let got = cooldown_snapshot_for_auth(&auth, now);
        if seconds == 0 {
            assert!(got.is_empty(), "{name}: {got:?}");
            continue;
        }
        assert_eq!(got.len(), 1, "{name}: {got:?}");
        assert_eq!(got[0].remaining_seconds, seconds, "{name}");
    }
}

#[test]
fn cooldown_snapshot_for_auth_reasons() {
    let now = base();
    struct Case {
        name: &'static str,
        quota: QuotaState,
        err: Option<AuthError>,
        message: &'static str,
        reason: &'static str,
        status: u16,
        backoff: bool,
    }
    let quota = |reason: &str, recover: Option<TimeDelta>, level: u32| QuotaState {
        exceeded: true,
        reason: reason.into(),
        next_recover_at: recover.map(|d| now + d),
        backoff_level: level,
    };
    let message_error = |status: u16, message: &str| {
        Some(AuthError {
            http_status: status,
            message: message.into(),
            ..AuthError::default()
        })
    };
    let case = |name, quota, err, reason, status, backoff| Case {
        name,
        quota,
        err,
        message: "",
        reason,
        status,
        backoff,
    };
    let cases = vec![
        case(
            "quota",
            quota("quota", None, 6),
            http_error(429),
            "quota",
            429,
            true,
        ),
        case(
            "propagated quota hides old error",
            quota("credential_quota", None, 6),
            http_error(401),
            "credential_quota",
            0,
            false,
        ),
        case(
            "quota hides unrelated error",
            quota("quota", None, 0),
            http_error(503),
            "quota",
            0,
            true,
        ),
        case(
            "challenge",
            quota("cloudflare challenge", None, 0),
            message_error(403, "cf-mitigated: challenge"),
            "cloudflare_challenge",
            403,
            true,
        ),
        case(
            "model unsupported",
            QuotaState::default(),
            message_error(400, "model not supported"),
            "model_not_supported",
            400,
            false,
        ),
        case(
            "invalid grant",
            QuotaState::default(),
            message_error(400, "invalid_grant"),
            "invalid_grant",
            400,
            false,
        ),
        case(
            "unauthorized",
            QuotaState::default(),
            http_error(401),
            "unauthorized",
            401,
            false,
        ),
        case(
            "payment",
            QuotaState::default(),
            http_error(402),
            "payment_required",
            402,
            false,
        ),
        case(
            "forbidden",
            QuotaState::default(),
            http_error(403),
            "payment_required",
            403,
            false,
        ),
        case(
            "not found",
            QuotaState::default(),
            http_error(404),
            "not_found",
            404,
            false,
        ),
        case(
            "gateway not challenge",
            QuotaState::default(),
            message_error(520, "cloudflare challenge"),
            "transient_error",
            520,
            false,
        ),
        case(
            "shorter active quota does not label longer retry",
            quota("quota", Some(TimeDelta::seconds(20)), 6),
            http_error(503),
            "transient_error",
            503,
            false,
        ),
        case(
            "longer quota supplies deadline",
            quota("quota", Some(TimeDelta::minutes(2)), 6),
            http_error(503),
            "quota",
            0,
            true,
        ),
        case(
            "shorter propagated quota preserves longer retry reason",
            quota("credential_quota", Some(TimeDelta::seconds(20)), 6),
            http_error(401),
            "unauthorized",
            0,
            false,
        ),
        case(
            "expired quota does not label new failure",
            quota("quota", Some(TimeDelta::zero()), 6),
            http_error(503),
            "transient_error",
            503,
            false,
        ),
        Case {
            message: "transient upstream error",
            ..case(
                "known marker",
                QuotaState::default(),
                None,
                "transient_error",
                0,
                false,
            )
        },
        Case {
            message: "secret-message",
            ..case(
                "unknown is sanitized",
                quota("secret-quota", None, 0),
                Some(AuthError {
                    code: "secret-code".into(),
                    message: "secret-body".into(),
                    ..AuthError::default()
                }),
                "unknown",
                0,
                false,
            )
        },
        case(
            "unknown HTTP error",
            QuotaState::default(),
            http_error(418),
            "unknown",
            418,
            false,
        ),
    ];
    for tc in cases {
        let state = ModelState {
            unavailable: true,
            next_retry_after: Some(now + TimeDelta::minutes(1)),
            quota: tc.quota,
            last_error: tc.err,
            status_message: tc.message.into(),
            ..ModelState::default()
        };
        let auth = Auth {
            model_states: states(vec![("a", state)]),
            ..Auth::default()
        };
        let got = cooldown_snapshot_for_auth(&auth, now);
        assert_eq!(got.len(), 1, "{}: {got:?}", tc.name);
        let view = &got[0];
        assert_eq!(view.reason, tc.reason, "{}", tc.name);
        assert_eq!(view.http_status, tc.status, "{}", tc.name);
        assert_eq!(view.backoff_level.is_some(), tc.backoff, "{}", tc.name);
        assert!(!format!("{view:?}").contains("secret"), "{}", tc.name);
    }
}

#[test]
fn cooldown_snapshot_for_auth_deduplicates_without_mutation() {
    let now = base();
    let auth = Auth {
        metadata: metadata(json!({"access_token": "secret-token"})),
        model_states: states(vec![
            (
                " model-a(high) ",
                ModelState {
                    unavailable: true,
                    next_retry_after: Some(now + TimeDelta::minutes(1)),
                    last_error: http_error(503),
                    ..ModelState::default()
                },
            ),
            (
                "model-a",
                ModelState {
                    unavailable: true,
                    next_retry_after: Some(now + TimeDelta::seconds(32)),
                    quota: QuotaState {
                        exceeded: true,
                        reason: "quota".into(),
                        backoff_level: 6,
                        ..QuotaState::default()
                    },
                    last_error: http_error(429),
                    ..ModelState::default()
                },
            ),
            (
                "model-b",
                ModelState {
                    unavailable: true,
                    next_retry_after: Some(now + TimeDelta::minutes(1)),
                    quota: QuotaState {
                        exceeded: true,
                        reason: "quota".into(),
                        backoff_level: 6,
                        ..QuotaState::default()
                    },
                    ..ModelState::default()
                },
            ),
            (
                " ",
                ModelState {
                    unavailable: true,
                    next_retry_after: Some(now + TimeDelta::minutes(1)),
                    ..ModelState::default()
                },
            ),
            ("nil", ModelState::default()),
        ]),
        ..Auth::default()
    };
    let before = auth.clone();
    let got = cooldown_snapshot_for_auth(&auth, now);
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[0].model_key, "model-a");
    assert_eq!(got[0].reason, "transient_error");
    assert_eq!(got[0].http_status, 503);
    assert_eq!(got[0].backoff_level, None);
    assert_eq!(got[0].remaining_seconds, 60);
    for _ in 0..20 {
        assert_eq!(cooldown_snapshot_for_auth(&auth, now), got);
    }
    assert_eq!(auth.model_states, before.model_states);
    assert_eq!(auth.metadata, before.metadata);
}

#[test]
fn cooldown_snapshot_for_auth_equal_deadline_is_stable() {
    let now = base();
    for (quota_key, other_key) in [("a(high)", "a(low)"), ("a(low)", "a(high)")] {
        let auth = Auth {
            model_states: states(vec![
                (
                    quota_key,
                    ModelState {
                        unavailable: true,
                        next_retry_after: Some(now + TimeDelta::minutes(1)),
                        last_error: http_error(429),
                        quota: QuotaState {
                            exceeded: true,
                            reason: "quota".into(),
                            next_recover_at: Some(now + TimeDelta::minutes(1)),
                            ..QuotaState::default()
                        },
                        ..ModelState::default()
                    },
                ),
                (
                    other_key,
                    ModelState {
                        unavailable: true,
                        next_retry_after: Some(now + TimeDelta::minutes(1)),
                        last_error: http_error(503),
                        ..ModelState::default()
                    },
                ),
            ]),
            ..Auth::default()
        };
        let (blocked, reason, next) = is_auth_blocked_for_model(&auth, "a", now);
        assert!(blocked && reason == BlockReason::Cooldown, "{quota_key}");
        for _ in 0..20 {
            let got = cooldown_snapshot_for_auth(&auth, now);
            assert_eq!(got.len(), 1, "{quota_key}: {got:?}");
            assert_eq!(got[0].reason, "quota", "{quota_key}");
            assert_eq!(Some(got[0].retry_at), next, "{quota_key}");
            assert_eq!(got[0].http_status, 429, "{quota_key}");
        }
    }
}

#[test]
fn cooldown_snapshot_for_auth_longer_retry_reason_survives_quota_expiry() {
    let now = base();
    let auth = Auth {
        model_states: states(vec![(
            "a",
            ModelState {
                unavailable: true,
                next_retry_after: Some(now + TimeDelta::hours(1)),
                last_error: http_error(503),
                quota: QuotaState {
                    exceeded: true,
                    reason: "quota".into(),
                    next_recover_at: Some(now + TimeDelta::minutes(5)),
                    backoff_level: 6,
                },
                ..ModelState::default()
            },
        )]),
        ..Auth::default()
    };
    for observed in [
        now,
        now + TimeDelta::minutes(5),
        now + TimeDelta::minutes(6),
    ] {
        let got = cooldown_snapshot_for_auth(&auth, observed);
        assert_eq!(got.len(), 1, "{observed}: {got:?}");
        assert_eq!(got[0].reason, "transient_error", "{observed}");
        assert_eq!(got[0].http_status, 503, "{observed}");
        assert_eq!(got[0].backoff_level, None, "{observed}");
        assert_eq!(got[0].retry_at, now + TimeDelta::hours(1), "{observed}");
    }
}
