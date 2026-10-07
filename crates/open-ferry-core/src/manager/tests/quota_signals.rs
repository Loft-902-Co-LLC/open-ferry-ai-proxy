// Ported from CLIProxyAPI sdk/cliproxy/auth/quota_signals_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Passive quota observations: which response headers a snapshot keeps,
//! that each response's snapshot replaces the last, and that cooldown
//! changes leave it alone.
//!
//! Deviations from upstream:
//! - Upstream's tests hand the manager a call's headers through the
//!   request's response-header holder; here they come in the
//!   [`CallResult`].
//! - `provider_supports_quota_observation`: Devin isn't observed, as it
//!   isn't ported, so `devin` is among the providers that aren't.
//! - `observe_response_headers_rejects_control_character_values`: a header
//!   map can't hold a CR or an LF, so the value holds a tab.
//! - `cooldown_equality_ignores_observation_signals` compares the cooldown
//!   records two credentials save, which upstream's `cooldownQuotaEqual`
//!   compares; `cooldown_state_record_omits_observation` takes the record
//!   from the store's snapshot, as `authCooldownStateRecord` isn't ported
//!   on its own.
//! - `quota_state_clone_copies_signals` is kept, though a Rust clone can't
//!   share its map.
//! - open-ferry's `claude-cli` provider is observed as Claude is, so it is
//!   among the providers that are.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{TimeDelta, TimeZone as _, Utc};
use http::{HeaderMap, HeaderName, HeaderValue};

use super::support::*;
use crate::auth::{Auth, AuthError, ModelState, QuotaState, Status, Timestamp};
use crate::exec::{Dispatcher, Response};
use crate::manager::cooldown::{
    clear_cooldown_state_for_auth, merge_model_state, reset_model_state,
};
use crate::manager::cooldown_store::{
    Record, StateStore, StoreError, install_store, restore_now, set_debounce, snapshot,
};
use crate::manager::quota_signals::{
    MAX_QUOTA_SIGNAL_HEADERS, MAX_QUOTA_SIGNAL_VALUE, apply_cooldown_fields,
};
use crate::manager::{CallResult, Settings, provider_supports_quota_observation};

fn unix(secs: i64) -> Timestamp {
    Utc.timestamp_opt(secs, 0).unwrap()
}

/// A header map of `pairs`, each value as bytes.
fn headers(pairs: &[(&str, &[u8])]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.append(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_bytes(value).unwrap(),
        );
    }
    map
}

/// A header map of text `pairs`.
fn text_headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let bytes: Vec<(&str, &[u8])> = pairs
        .iter()
        .map(|(name, value)| (*name, value.as_bytes()))
        .collect();
    headers(&bytes)
}

fn signals(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

fn observation(at: i64, pairs: &[(&str, &str)]) -> QuotaState {
    QuotaState {
        observed_at: Some(unix(at)),
        signals: signals(pairs),
        ..QuotaState::default()
    }
}

fn signal<'a>(quota: &'a QuotaState, name: &str) -> Option<&'a str> {
    quota.signals.get(name).map(String::as_str)
}

/// Upstream's `TestQuotaStateObserveResponseHeadersKeepsProviderScopedSignals`.
#[test]
fn observe_response_headers_keeps_provider_scoped_signals() {
    let observed_at = unix(123);
    let mut quota = QuotaState::default();
    assert!(quota.observe_response_headers_for_provider(
        "codex",
        &text_headers(&[
            ("X-Codex-Active-Limit", "codex_bengalfox"),
            ("X-Codex-Primary-Used-Percent", "2"),
            ("X-Codex-Turn-State", "opaque-state"),
            ("X-Codex-Safety-Buffering-Enabled", "true"),
            ("Retry-After", "120"),
            ("Authorization", "Bearer dummy"),
        ]),
        observed_at,
    ));
    assert_eq!(quota.observed_at, Some(observed_at));
    assert_eq!(
        quota.signals,
        signals(&[
            ("Retry-After", "120"),
            ("X-Codex-Active-Limit", "codex_bengalfox"),
            ("X-Codex-Primary-Used-Percent", "2"),
        ])
    );
}

/// Upstream's `TestQuotaStateObserveResponseHeadersBoundsAndCanonicalizesValues`.
#[test]
fn observe_response_headers_bounds_and_canonicalizes_values() {
    let mut quota = QuotaState::default();
    let long = "x".repeat(MAX_QUOTA_SIGNAL_VALUE + 1);
    assert!(!quota.observe_response_headers_for_provider(
        "codex",
        &text_headers(&[("X-Codex-Empty", ""), ("X-Codex-Long", &long)]),
        unix(123),
    ));
    assert_eq!(quota, QuotaState::default());
    assert!(quota.observe_response_headers_for_provider(
        "codex",
        &text_headers(&[("x-codex-plan-type", "pro")]),
        unix(124),
    ));
    assert_eq!(signal(&quota, "X-Codex-Plan-Type"), Some("pro"));
}

/// Not upstream's: a value is kept up to the length limit, counted after
/// trimming, and one past it in a quota header is dropped.
#[test]
fn observe_response_headers_counts_the_trimmed_length() {
    let at_limit = format!("  {}  ", "7".repeat(MAX_QUOTA_SIGNAL_VALUE));
    let past_limit = "7".repeat(MAX_QUOTA_SIGNAL_VALUE + 1);
    let mut quota = QuotaState::default();
    assert!(quota.observe_response_headers_for_provider(
        "codex",
        &text_headers(&[
            ("X-Codex-Limit-Name", &at_limit),
            ("X-Codex-Primary-Used-Percent", &past_limit),
        ]),
        unix(1),
    ));
    assert_eq!(
        quota.signals,
        signals(&[("X-Codex-Limit-Name", &"7".repeat(MAX_QUOTA_SIGNAL_VALUE))])
    );
}

/// Upstream's
/// `TestQuotaStateObserveResponseHeadersRetainsMeasuredClaudeAndCodexWatermarks`.
#[test]
fn observe_response_headers_retains_measured_claude_and_codex_watermarks() {
    let claude_signals = [
        ("Anthropic-Ratelimit-Unified-5h-Status", "allowed"),
        ("Anthropic-Ratelimit-Unified-5h-Utilization", "0.0"),
        ("Anthropic-Ratelimit-Unified-5h-Reset", "1787296800"),
        ("Anthropic-Ratelimit-Unified-7d-Status", "allowed"),
        ("Anthropic-Ratelimit-Unified-7d-Utilization", "0.53"),
        ("Anthropic-Ratelimit-Unified-7d-Reset", "1787695200"),
        ("Anthropic-Ratelimit-Unified-Fallback-Percentage", "0.5"),
        (
            "Anthropic-Ratelimit-Unified-Overage-Disabled-Reason",
            "member_zero_credit_limit",
        ),
        ("Anthropic-Ratelimit-Unified-Overage-Status", "rejected"),
        (
            "Anthropic-Ratelimit-Unified-Representative-Claim",
            "five_hour",
        ),
        ("Anthropic-Ratelimit-Unified-Reset", "1787296800"),
        ("Anthropic-Ratelimit-Unified-Status", "allowed"),
    ];
    let mut claude_headers = claude_signals.to_vec();
    claude_headers.push((
        "Anthropic-Workspace-Id",
        "workspace-must-not-be-quota-signal",
    ));
    let mut claude = QuotaState::default();
    assert!(claude.observe_response_headers_for_provider(
        "claude",
        &text_headers(&claude_headers),
        unix(1_787_279_282),
    ));
    assert_eq!(claude.signals, signals(&claude_signals));

    let codex_signals = [
        ("X-Codex-Plan-Type", "pro"),
        ("X-Codex-Primary-Used-Percent", "51"),
        ("X-Codex-Primary-Window-Minutes", "10080"),
        ("X-Codex-Primary-Reset-After-Seconds", "309718"),
        ("X-Codex-Primary-Reset-At", "1787588999"),
        ("X-Codex-Bengalfox-Limit-Name", "GPT-5.3-Codex-Spark"),
        ("X-Codex-Bengalfox-Secondary-Used-Percent", "35"),
        ("X-Codex-Credits-Has-Credits", "False"),
    ];
    let mut codex = QuotaState::default();
    assert!(codex.observe_response_headers_for_provider(
        "codex",
        &text_headers(&codex_signals),
        unix(1_787_279_282),
    ));
    assert_eq!(codex.signals, signals(&codex_signals));
}

/// Upstream's
/// `TestQuotaStateObserveResponseHeadersDropsKimiGrokAndAntigravitySignals`.
#[test]
fn observe_response_headers_drops_kimi_grok_and_antigravity_signals() {
    for provider in [
        "kimi",
        "xai",
        "grok",
        "antigravity",
        "gemini",
        "vertex",
        "aistudio",
    ] {
        let mut quota = observation(1_786_082_736, &[("old", "value")]);
        let changed = quota.observe_response_headers_for_provider(
            provider,
            &text_headers(&[
                ("X-Ratelimit-Remaining-Requests", "0"),
                ("X-Ratelimit-Remaining-Tokens", "0"),
                ("Retry-After", "60"),
                ("Anthropic-Ratelimit-Unified-5h-Utilization", "0.5"),
            ]),
            unix(1_786_082_800),
        );
        assert!(changed, "{provider}");
        assert_eq!(quota, QuotaState::default(), "{provider}");
    }
}

/// Upstream's `TestCooldownEqualityIgnoresObservationSignals`.
#[tokio::test(start_paused = true)]
async fn cooldown_equality_ignores_observation_signals() {
    let record_of = |quota: QuotaState| {
        let h = Harness::new(Settings::default());
        let now = h.now();
        let mut auth = auth("auth-1", "codex");
        auth.unavailable = true;
        auth.next_retry_after = Some(now + TimeDelta::hours(1));
        auth.quota = quota;
        h.add(auth, &[]);
        snapshot(&h.manager, now)
    };
    let base = QuotaState {
        exceeded: true,
        reason: "quota".into(),
        next_recover_at: Some(unix(1_787_588_999)),
        backoff_level: 2,
        ..QuotaState::default()
    };
    let mut observed = base.clone();
    observed.observed_at = Some(unix(1_787_279_282));
    observed.signals = signals(&[
        ("X-Codex-Primary-Used-Percent", "51"),
        ("X-Codex-Primary-Reset-At", "1787588999"),
    ]);
    let base_records = record_of(base);
    assert_eq!(base_records.len(), 1);
    assert_eq!(base_records, record_of(observed));
}

/// Upstream's `TestManagerMarkResultRecordsResponseQuotaSignalsInMemory`.
#[tokio::test(start_paused = true)]
async fn manager_mark_result_records_response_quota_signals_in_memory() {
    let h = Harness::new(Settings::default());
    h.add(auth("quota-signal-auth", "codex"), &[]);
    h.manager.mark_result(&CallResult {
        auth_id: "quota-signal-auth".into(),
        provider: "codex".into(),
        model: "gpt-5.3-codex".into(),
        success: true,
        response_headers: text_headers(&[
            ("X-Codex-Active-Limit", "codex_bengalfox"),
            ("X-Codex-Primary-Used-Percent", "2"),
            ("X-Codex-Primary-Window-Minutes", "10080"),
            ("X-Codex-Primary-Reset-At", "1782951970"),
        ]),
        ..CallResult::default()
    });
    let updated = h.get("quota-signal-auth");
    assert_eq!(
        signal(&updated.quota, "X-Codex-Active-Limit"),
        Some("codex_bengalfox")
    );
    assert_eq!(
        signal(&updated.quota, "X-Codex-Primary-Used-Percent"),
        Some("2")
    );
    assert_eq!(updated.quota.observed_at, Some(h.now()));
}

/// Upstream's `TestMarkResultCountTokensDoesNotReplaceObservation`.
#[tokio::test(start_paused = true)]
async fn mark_result_count_tokens_does_not_replace_observation() {
    let h = Harness::new(Settings::default());
    let mut credential = auth("quota-count-tokens-auth", "claude");
    credential.quota = observation(10, &[("Anthropic-Ratelimit-Unified-Status", "allowed")]);
    h.add(credential, &[]);
    h.manager.mark_result(&CallResult {
        auth_id: "quota-count-tokens-auth".into(),
        provider: "claude".into(),
        model: "claude-opus-4-6".into(),
        success: true,
        response_headers: text_headers(&[("Anthropic-Ratelimit-Unified-Status", "rejected")]),
        skip_quota_observation: true,
        ..CallResult::default()
    });
    let updated = h.get("quota-count-tokens-auth");
    assert_eq!(
        updated.quota,
        observation(10, &[("Anthropic-Ratelimit-Unified-Status", "allowed")])
    );
}

/// Not upstream's: a token count through the manager keeps the snapshot
/// of the last generation, and a generation replaces it.
#[tokio::test(start_paused = true)]
async fn a_token_count_keeps_the_last_generation_snapshot() {
    let h = Harness::new(Settings::default());
    let executor = FakeExecutor::with("claude", |call: &Call| {
        let utilization = if call.kind == Kind::Count {
            "0.99"
        } else {
            "0.25"
        };
        Reply::Ok(Response {
            payload: "ok".into(),
            headers: text_headers(&[("Anthropic-Ratelimit-Unified-5h-Utilization", utilization)]),
        })
    });
    h.executor(&executor);
    h.add(auth("claude-a", "claude"), &["claude-opus-4-6"]);
    let names = providers(&["claude"]);
    h.manager
        .execute(&names, request("claude-opus-4-6"), options())
        .await
        .unwrap();
    let generated_at = h.now();
    tokio::time::advance(Duration::from_secs(5)).await;
    h.manager
        .count_tokens(&names, request("claude-opus-4-6"), options())
        .await
        .unwrap();
    let updated = h.get("claude-a");
    assert_eq!(updated.quota.observed_at, Some(generated_at));
    assert_eq!(
        signal(&updated.quota, "Anthropic-Ratelimit-Unified-5h-Utilization"),
        Some("0.25")
    );
}

/// Not upstream's: a stream is observed with its headers when it ends, and
/// a stream that fails part way with the headers of its error over them,
/// as upstream merges an error's headers into the request's.
#[tokio::test(start_paused = true)]
async fn a_stream_is_observed_with_its_headers_and_its_error_headers() {
    let h = Harness::new(Settings::default());
    let executor = FakeExecutor::with("codex", |call: &Call| {
        let stream_headers = text_headers(&[
            ("X-Codex-Plan-Type", "pro"),
            ("X-Codex-Primary-Used-Percent", "30"),
        ]);
        if call.auth_id == "codex-a" {
            return Reply::Stream {
                headers: stream_headers,
                chunks: vec![Ok("one".into())],
            };
        }
        let mut err = crate::exec::ExecError::upstream(429, "usage limit reached");
        err.headers = text_headers(&[
            ("X-Codex-Primary-Used-Percent", "100"),
            ("Retry-After", "60"),
        ]);
        Reply::Stream {
            headers: stream_headers,
            chunks: vec![Ok("one".into()), Err(err)],
        }
    });
    h.executor(&executor);
    h.add(auth("codex-a", "codex"), &["gpt-5"]);
    h.add(auth("codex-b", "codex"), &["gpt-5"]);
    let names = providers(&["codex"]);

    let stream = h
        .manager
        .execute_stream(&names, request("gpt-5"), pinned("codex-a"))
        .await
        .unwrap();
    assert!(h.get("codex-a").quota.signals.is_empty(), "before the end");
    let (_, err) = collect(stream).await;
    assert!(err.is_none());
    settle().await;
    assert_eq!(
        h.get("codex-a").quota.signals,
        signals(&[
            ("X-Codex-Plan-Type", "pro"),
            ("X-Codex-Primary-Used-Percent", "30"),
        ])
    );

    let stream = h
        .manager
        .execute_stream(&names, request("gpt-5"), pinned("codex-b"))
        .await
        .unwrap();
    let (_, err) = collect(stream).await;
    assert!(err.is_some());
    settle().await;
    assert_eq!(
        h.get("codex-b").quota.signals,
        signals(&[
            ("Retry-After", "60"),
            ("X-Codex-Plan-Type", "pro"),
            ("X-Codex-Primary-Used-Percent", "100"),
        ])
    );
}

/// Not upstream's: a call that fails is observed with its error's headers.
#[tokio::test(start_paused = true)]
async fn a_failed_call_is_observed_with_its_error_headers() {
    let h = Harness::new(Settings::default());
    let executor = FakeExecutor::with("claude", |_: &Call| {
        let mut err = crate::exec::ExecError::upstream(429, "rate limited");
        err.headers = text_headers(&[
            ("Anthropic-Ratelimit-Unified-Status", "rejected"),
            (
                "Anthropic-Ratelimit-Unified-Representative-Claim",
                "seven_day",
            ),
        ]);
        Reply::Err(err)
    });
    h.executor(&executor);
    h.add(auth("claude-a", "claude"), &["claude-opus-4-6"]);
    let failed = h
        .manager
        .execute(
            &providers(&["claude"]),
            request("claude-opus-4-6"),
            options(),
        )
        .await;
    assert!(failed.is_err());
    let updated = h.get("claude-a");
    assert_eq!(
        updated.quota.signals,
        signals(&[
            (
                "Anthropic-Ratelimit-Unified-Representative-Claim",
                "seven_day"
            ),
            ("Anthropic-Ratelimit-Unified-Status", "rejected"),
        ])
    );
    assert_eq!(
        updated.model_states["claude-opus-4-6"].quota.signals,
        updated.quota.signals
    );
}

/// Upstream's `TestResetModelStatePreservesObservationSignals`.
#[test]
fn reset_model_state_preserves_observation_signals() {
    let mut state = ModelState {
        status: Status::Error,
        unavailable: true,
        status_message: "quota".into(),
        quota: QuotaState {
            exceeded: true,
            reason: "credential_quota".into(),
            next_recover_at: Some(unix(20)),
            backoff_level: 2,
            ..observation(10, &[("X-Codex-Active-Limit", "premium")])
        },
        ..ModelState::default()
    };
    reset_model_state(&mut state, unix(30));
    assert_eq!(
        state.quota,
        observation(10, &[("X-Codex-Active-Limit", "premium")])
    );
}

/// Upstream's `TestMergeModelStateKeepsNewestObservationSnapshot`.
#[test]
fn merge_model_state_keeps_newest_observation_snapshot() {
    let mut target = ModelState {
        updated_at: Some(unix(20)),
        quota: observation(20, &[("X-Codex-Plan-Type", "pro")]),
        ..ModelState::default()
    };
    let source = ModelState {
        updated_at: Some(unix(30)),
        quota: observation(30, &[("X-Codex-Active-Limit", "codex_bengalfox")]),
        ..ModelState::default()
    };
    merge_model_state(&mut target, &source);
    // Uniting snapshots taken at different times would bring back the
    // older watermark, so the stale key must be gone.
    assert_eq!(
        target.quota,
        observation(30, &[("X-Codex-Active-Limit", "codex_bengalfox")])
    );
}

/// Not upstream's: a merge keeps the newer snapshot even when it is on the
/// older state.
#[test]
fn merge_model_state_keeps_a_newer_snapshot_of_the_older_state() {
    let mut target = ModelState {
        updated_at: Some(unix(20)),
        quota: observation(40, &[("X-Codex-Plan-Type", "pro")]),
        ..ModelState::default()
    };
    let source = ModelState {
        updated_at: Some(unix(30)),
        quota: observation(30, &[("X-Codex-Active-Limit", "codex_bengalfox")]),
        ..ModelState::default()
    };
    merge_model_state(&mut target, &source);
    assert_eq!(
        target.quota,
        observation(40, &[("X-Codex-Plan-Type", "pro")])
    );
}

/// Upstream's `TestObserveResponseHeadersReplacesStaleWatermarks`: a
/// watermark such as `Retry-After` is only on the response that produced
/// it, and later responses must not keep showing it.
#[test]
fn observe_response_headers_replaces_stale_watermarks() {
    let mut quota = QuotaState::default();
    assert!(quota.observe_response_headers_for_provider(
        "codex",
        &text_headers(&[
            ("Retry-After", "120"),
            ("X-Codex-Primary-Used-Percent", "99"),
        ]),
        unix(100),
    ));
    assert_eq!(signal(&quota, "Retry-After"), Some("120"));
    assert!(quota.observe_response_headers_for_provider(
        "codex",
        &text_headers(&[("X-Codex-Primary-Used-Percent", "5")]),
        unix(200),
    ));
    assert_eq!(
        quota,
        observation(200, &[("X-Codex-Primary-Used-Percent", "5")])
    );
}

/// Upstream's `TestObserveResponseHeadersKeepsSnapshotWhenResponseCarriesNoSignal`:
/// a response with no quota header, as a transport failure or a 5xx, must
/// not erase the last snapshot.
#[test]
fn observe_response_headers_keeps_snapshot_when_response_carries_no_signal() {
    let mut quota = observation(100, &[("X-Codex-Primary-Used-Percent", "5")]);
    assert!(!quota.observe_response_headers_for_provider(
        "codex",
        &text_headers(&[("Content-Type", "application/json")]),
        unix(200),
    ));
    assert_eq!(
        quota,
        observation(100, &[("X-Codex-Primary-Used-Percent", "5")])
    );
}

/// Upstream's `TestObserveResponseHeadersAdvancesObservedAtOnRepeatedValues`:
/// without it, a fresh reading can't be told from a stale one.
#[test]
fn observe_response_headers_advances_observed_at_on_repeated_values() {
    let mut quota = QuotaState::default();
    let map = text_headers(&[("X-Codex-Primary-Used-Percent", "5")]);
    quota.observe_response_headers_for_provider("codex", &map, unix(100));
    quota.observe_response_headers_for_provider("codex", &map, unix(200));
    assert_eq!(quota.observed_at, Some(unix(200)));
}

/// Upstream's `TestObserveResponseHeadersRejectsControlCharacterValues`:
/// the values reach the plain-text request log, so a control character is
/// never kept.
#[test]
fn observe_response_headers_rejects_control_character_values() {
    let mut quota = QuotaState::default();
    assert!(!quota.observe_response_headers_for_provider(
        "codex",
        &headers(&[("X-Codex-Bengalfox-Limit-Name", b"evil\tX-Injected: 1")]),
        unix(100),
    ));
    assert!(quota.signals.is_empty(), "{quota:?}");
}

/// Not upstream's: bytes that aren't UTF-8 are kept as U+FFFD, as Go's JSON
/// encoder writes them.
#[test]
fn observe_response_headers_keeps_invalid_bytes_as_replacement_characters() {
    let mut quota = QuotaState::default();
    assert!(quota.observe_response_headers_for_provider(
        "codex",
        &headers(&[("X-Codex-Limit-Name", b"spark\xff\xfe")]),
        unix(100),
    ));
    assert_eq!(
        signal(&quota, "X-Codex-Limit-Name"),
        Some("spark\u{fffd}\u{fffd}")
    );
}

/// Not upstream's: of a header sent twice, the last value is kept.
#[test]
fn observe_response_headers_keeps_the_last_value() {
    let mut quota = QuotaState::default();
    assert!(quota.observe_response_headers_for_provider(
        "claude",
        &text_headers(&[("Retry-After", "10"), ("retry-after", "20"),]),
        unix(100),
    ));
    assert_eq!(quota.signals, signals(&[("Retry-After", "20")]));
}

/// Upstream's `TestObserveResponseHeadersTruncatesDeterministically`.
#[test]
fn observe_response_headers_truncates_deterministically() {
    let pairs: Vec<(String, String)> = (0..MAX_QUOTA_SIGNAL_HEADERS * 2)
        .map(|i| {
            (
                format!("X-Codex-L{i:03}-Primary-Used-Percent"),
                i.to_string(),
            )
        })
        .collect();
    let borrowed: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    let map = text_headers(&borrowed);
    let mut first = QuotaState::default();
    first.observe_response_headers_for_provider("codex", &map, unix(100));
    assert_eq!(first.signals.len(), MAX_QUOTA_SIGNAL_HEADERS);
    assert_eq!(
        first.signals.keys().next_back().map(String::as_str),
        Some("X-Codex-L063-Primary-Used-Percent")
    );
    for _ in 0..5 {
        let mut next = QuotaState::default();
        next.observe_response_headers_for_provider("codex", &map, unix(100));
        assert_eq!(first.signals, next.signals);
    }
}

/// Upstream's `TestProviderSupportsQuotaObservation`.
#[test]
fn provider_supports_quota_observation_for_claude_and_codex() {
    for provider in [
        "",
        "kimi",
        "xai",
        "grok",
        "antigravity",
        "gemini",
        "gemini-interactions",
        "vertex",
        "aistudio",
        "openai",
        "openai-compatibility",
        "third-party-plugin",
        "XAI",
        " Grok ",
        " Gemini ",
        "devin",
        " Devin ",
    ] {
        assert!(
            !provider_supports_quota_observation(provider),
            "{provider:?}"
        );
    }
    for provider in ["codex", "claude", "CODEX", " Claude ", "claude-cli"] {
        assert!(
            provider_supports_quota_observation(provider),
            "{provider:?}"
        );
    }
}

/// Not upstream's: a `claude-cli` response keeps Claude's unified headers
/// and `Retry-After`, and nothing else.
#[test]
fn observe_response_headers_keeps_claude_signals_for_claude_cli() {
    let kept = [
        ("Anthropic-Ratelimit-Unified-5h-Utilization", "0.42"),
        ("Anthropic-Ratelimit-Unified-5h-Reset", "1787296800"),
        ("Anthropic-Ratelimit-Unified-7d-Utilization", "0.1"),
        ("Anthropic-Ratelimit-Unified-Status", "allowed"),
        ("Retry-After", "30"),
    ];
    let mut all = kept.to_vec();
    all.push(("X-Codex-Plan-Type", "pro"));
    all.push(("Anthropic-Workspace-Id", "not-a-signal"));
    let mut quota = QuotaState::default();
    assert!(quota.observe_response_headers_for_provider(
        "claude-cli",
        &text_headers(&all),
        unix(1_787_279_282),
    ));
    assert_eq!(quota.signals, signals(&kept));
}

/// Upstream's `TestQuotaStateCloneCopiesSignals`.
#[test]
fn quota_state_clone_copies_signals() {
    let original = observation(1, &[("X-Codex-Plan-Type", "pro")]);
    let mut clone = original.clone();
    clone
        .signals
        .insert("X-Codex-Plan-Type".into(), "team".into());
    assert_eq!(signal(&original, "X-Codex-Plan-Type"), Some("pro"));
}

/// Upstream's `TestApplyCooldownFieldsPreservesObservation`.
#[test]
fn apply_cooldown_fields_preserves_observation() {
    let mut quota = QuotaState {
        exceeded: true,
        reason: "quota".into(),
        next_recover_at: Some(unix(20)),
        backoff_level: 1,
        ..observation(10, &[("X-Codex-Primary-Used-Percent", "51")])
    };
    apply_cooldown_fields(
        &mut quota,
        QuotaState {
            exceeded: true,
            reason: "credential_quota".into(),
            next_recover_at: Some(unix(40)),
            backoff_level: 2,
            ..QuotaState::default()
        },
    );
    assert_eq!(
        quota,
        QuotaState {
            exceeded: true,
            reason: "credential_quota".into(),
            next_recover_at: Some(unix(40)),
            backoff_level: 2,
            ..observation(10, &[("X-Codex-Primary-Used-Percent", "51")])
        }
    );
}

/// Upstream's `TestClearCooldownStateForAuthPreservesObservation`.
#[test]
fn clear_cooldown_state_for_auth_preserves_observation() {
    let mut auth = Auth {
        unavailable: true,
        next_retry_after: Some(unix(40)),
        quota: QuotaState {
            exceeded: true,
            reason: "credential_quota".into(),
            next_recover_at: Some(unix(40)),
            ..observation(10, &[("X-Codex-Primary-Used-Percent", "51")])
        },
        model_states: BTreeMap::from([(
            "gpt-5.3-codex".to_owned(),
            ModelState {
                unavailable: true,
                next_retry_after: Some(unix(40)),
                quota: QuotaState {
                    exceeded: true,
                    reason: "quota".into(),
                    next_recover_at: Some(unix(40)),
                    ..observation(11, &[("X-Codex-Plan-Type", "pro")])
                },
                ..ModelState::default()
            },
        )]),
        ..Auth::default()
    };
    assert!(clear_cooldown_state_for_auth(&mut auth, unix(50)));
    assert!(!auth.unavailable);
    assert_eq!(
        auth.quota,
        observation(10, &[("X-Codex-Primary-Used-Percent", "51")])
    );
    let state = &auth.model_states["gpt-5.3-codex"];
    assert!(!state.unavailable);
    assert_eq!(
        state.quota,
        observation(11, &[("X-Codex-Plan-Type", "pro")])
    );
}

/// Upstream's `TestCooldownStateRecordOmitsObservation`.
#[tokio::test(start_paused = true)]
async fn cooldown_state_record_omits_observation() {
    let h = Harness::new(Settings::default());
    let now = h.now();
    let mut credential = auth("auth-1", "codex");
    credential.unavailable = true;
    credential.next_retry_after = Some(now + TimeDelta::hours(1));
    credential.quota = QuotaState {
        exceeded: true,
        reason: "quota".into(),
        next_recover_at: Some(now + TimeDelta::hours(1)),
        backoff_level: 2,
        ..observation(10, &[("X-Codex-Primary-Used-Percent", "51")])
    };
    h.add(credential, &[]);
    let records = snapshot(&h.manager, now);
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].quota,
        QuotaState {
            exceeded: true,
            reason: "quota".into(),
            next_recover_at: Some(now + TimeDelta::hours(1)),
            backoff_level: 2,
            ..QuotaState::default()
        }
    );
}

/// Upstream's `TestMarkResultQuotaFailureDoesNotEraseSiblingObservation`.
#[tokio::test(start_paused = true)]
async fn mark_result_quota_failure_does_not_erase_sibling_observation() {
    let h = Harness::new(Settings::default());
    let mut credential = auth("quota-sibling-auth", "codex");
    credential.model_states = BTreeMap::from([
        (
            "gpt-5.3-codex".to_owned(),
            ModelState {
                status: Status::Active,
                quota: observation(10, &[("X-Codex-Primary-Used-Percent", "40")]),
                ..ModelState::default()
            },
        ),
        (
            "gpt-5.4".to_owned(),
            ModelState {
                status: Status::Active,
                quota: observation(11, &[("X-Codex-Primary-Used-Percent", "41")]),
                ..ModelState::default()
            },
        ),
    ]);
    h.add(credential, &[]);
    h.manager.mark_result(&CallResult {
        auth_id: "quota-sibling-auth".into(),
        provider: "codex".into(),
        model: "gpt-5.3-codex".into(),
        success: false,
        credential_scope: true,
        error: Some(AuthError {
            http_status: 429,
            message: "quota".into(),
            ..AuthError::default()
        }),
        response_headers: text_headers(&[
            ("Retry-After", "120"),
            ("X-Codex-Primary-Used-Percent", "99"),
        ]),
        ..CallResult::default()
    });

    let updated = h.get("quota-sibling-auth");
    let current = &updated.model_states["gpt-5.3-codex"];
    assert!(current.quota.exceeded);
    assert_eq!(current.quota.reason, "quota");
    assert_eq!(
        current.quota.signals,
        signals(&[
            ("Retry-After", "120"),
            ("X-Codex-Primary-Used-Percent", "99"),
        ])
    );
    let sibling = &updated.model_states["gpt-5.4"];
    assert!(sibling.quota.exceeded);
    assert_eq!(sibling.quota.reason, "credential_quota");
    assert_eq!(
        sibling.quota.signals,
        signals(&[("X-Codex-Primary-Used-Percent", "41")])
    );
    assert_eq!(sibling.quota.observed_at, Some(unix(11)));
}

/// Upstream's `TestMarkResultRetainedCredentialQuotaStillObservesModel`.
#[tokio::test(start_paused = true)]
async fn mark_result_retained_credential_quota_still_observes_model() {
    let h = Harness::new(Settings::default());
    let recover_at = h.now() + TimeDelta::hours(1);
    let held = QuotaState {
        exceeded: true,
        reason: "credential_quota".into(),
        next_recover_at: Some(recover_at),
        ..observation(10, &[("X-Codex-Primary-Used-Percent", "10")])
    };
    let mut credential = auth("quota-retain-auth", "codex");
    credential.quota = held.clone();
    credential.model_states = BTreeMap::from([(
        "gpt-5.3-codex".to_owned(),
        ModelState {
            status: Status::Error,
            unavailable: true,
            next_retry_after: Some(recover_at),
            quota: held,
            ..ModelState::default()
        },
    )]);
    h.add(credential, &[]);
    h.manager.mark_result(&CallResult {
        auth_id: "quota-retain-auth".into(),
        provider: "codex".into(),
        model: "gpt-5.3-codex".into(),
        success: true,
        response_headers: text_headers(&[("X-Codex-Primary-Used-Percent", "20")]),
        ..CallResult::default()
    });

    let updated = h.get("quota-retain-auth");
    assert!(updated.quota.exceeded, "{:?}", updated.quota);
    assert_eq!(updated.quota.reason, "credential_quota");
    assert_eq!(
        signal(&updated.quota, "X-Codex-Primary-Used-Percent"),
        Some("20")
    );
    let state = &updated.model_states["gpt-5.3-codex"];
    assert!(state.quota.exceeded, "{state:?}");
    assert_eq!(state.quota.reason, "credential_quota");
    assert_eq!(
        signal(&state.quota, "X-Codex-Primary-Used-Percent"),
        Some("20")
    );
}

/// A store that loads `records` and saves nothing.
struct LoadOnly(Vec<Record>);

impl StateStore for LoadOnly {
    fn load(&self) -> Result<Vec<Record>, StoreError> {
        Ok(self.0.clone())
    }

    fn save(&self, _records: &[Record], _now: Timestamp) -> Result<(), StoreError> {
        Ok(())
    }
}

/// Upstream's `TestRestoreCooldownRecordDoesNotOverwriteNewerObservation`.
#[tokio::test(start_paused = true)]
async fn restore_cooldown_record_does_not_overwrite_newer_observation() {
    let h = Harness::new(Settings::default());
    set_debounce(&h.manager, Duration::from_secs(3600));
    let next_retry = h.now() + TimeDelta::hours(1);
    let mut credential = auth("auth-restore-obs", "codex");
    credential.quota = observation(50, &[("X-Codex-Primary-Used-Percent", "77")]);
    h.manager.register_unsaved(credential).unwrap();
    let store = LoadOnly(vec![Record {
        provider: "codex".into(),
        auth_id: "auth-restore-obs".into(),
        status: "cooling".into(),
        next_retry_after: Some(next_retry),
        reason: "quota".into(),
        quota: QuotaState {
            exceeded: true,
            reason: "quota".into(),
            next_recover_at: Some(next_retry),
            ..observation(5, &[("X-Codex-Primary-Used-Percent", "1")])
        },
        updated_at: Some(next_retry - TimeDelta::minutes(1)),
        ..Record::default()
    }]);
    install_store(&h.manager, std::sync::Arc::new(store));
    restore_now(&h.manager);

    let restored = h.get("auth-restore-obs");
    assert!(restored.unavailable);
    assert!(restored.quota.exceeded);
    assert_eq!(restored.quota.reason, "quota");
    assert_eq!(
        restored.quota.signals,
        signals(&[("X-Codex-Primary-Used-Percent", "77")])
    );
    assert_eq!(restored.quota.observed_at, Some(unix(50)));
}

/// Not upstream's: a restored record's snapshot replaces an older one.
#[tokio::test(start_paused = true)]
async fn restore_cooldown_record_takes_a_newer_observation() {
    let h = Harness::new(Settings::default());
    set_debounce(&h.manager, Duration::from_secs(3600));
    let next_retry = h.now() + TimeDelta::hours(1);
    let mut credential = auth("auth-restore-obs", "codex");
    credential.quota = observation(5, &[("X-Codex-Plan-Type", "pro")]);
    h.manager.register_unsaved(credential).unwrap();
    let store = LoadOnly(vec![Record {
        provider: "codex".into(),
        auth_id: "auth-restore-obs".into(),
        status: "cooling".into(),
        next_retry_after: Some(next_retry),
        quota: observation(50, &[("X-Codex-Primary-Used-Percent", "1")]),
        updated_at: Some(next_retry - TimeDelta::minutes(1)),
        ..Record::default()
    }]);
    install_store(&h.manager, std::sync::Arc::new(store));
    restore_now(&h.manager);

    let restored = h.get("auth-restore-obs");
    assert!(restored.unavailable);
    assert_eq!(
        restored.quota,
        observation(50, &[("X-Codex-Primary-Used-Percent", "1")])
    );
}

/// Upstream's `TestObserveResponseHeadersKeepsPrimaryWhenTruncatingAdditional`.
#[test]
fn observe_response_headers_keeps_primary_when_truncating_additional() {
    let mut pairs: Vec<(String, String)> = vec![
        ("X-Codex-Plan-Type".into(), "pro".into()),
        ("X-Codex-Primary-Used-Percent".into(), "81".into()),
        ("X-Codex-Credits-Balance".into(), "0".into()),
    ];
    pairs.extend((0..MAX_QUOTA_SIGNAL_HEADERS).map(|i| {
        (
            format!("X-Codex-Additional-L{i:03}-Primary-Used-Percent"),
            i.to_string(),
        )
    }));
    let borrowed: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    let mut quota = QuotaState::default();
    quota.observe_response_headers_for_provider("codex", &text_headers(&borrowed), unix(100));
    assert_eq!(signal(&quota, "X-Codex-Plan-Type"), Some("pro"));
    assert_eq!(signal(&quota, "X-Codex-Primary-Used-Percent"), Some("81"));
    assert_eq!(signal(&quota, "X-Codex-Credits-Balance"), Some("0"));
    assert_eq!(quota.signals.len(), MAX_QUOTA_SIGNAL_HEADERS);
}
