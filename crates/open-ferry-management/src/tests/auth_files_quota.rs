// Ported from CLIProxyAPI
// internal/api/handlers/management/auth_files_quota_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! What `quota` and `model_quotas` show in the credential list.
//!
//! Deviations from upstream:
//! - `TestModelQuotaObservationPayloadSkipsNilAndEmptyStates` has no nil
//!   state: the port's model states are values, never nil.

use std::collections::BTreeMap;

use chrono::{DateTime, TimeZone as _, Utc};
use http::StatusCode;
use open_ferry_core::auth::{ModelState, QuotaState};
use serde_json::{Value, json};

use super::{Api, runtime_auth};
use crate::auth_files::{model_quota_observations, quota_observation};

fn unix(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(secs, 0).unwrap()
}

fn signals<const N: usize>(pairs: [(&str, &str); N]) -> BTreeMap<String, String> {
    pairs
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

fn observed(at: i64, pairs: &[(&str, &str)]) -> ModelState {
    ModelState {
        quota: QuotaState {
            observed_at: Some(unix(at)),
            signals: pairs
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            ..QuotaState::default()
        },
        ..ModelState::default()
    }
}

/// Upstream's `TestModelQuotaObservationPayloadOmitsUnsupportedProviders`.
#[test]
fn model_quota_observation_payload_omits_unsupported_providers() {
    let states = BTreeMap::from([(
        "grok-4".to_owned(),
        observed(10, &[("X-Ratelimit-Remaining-Requests", "1")]),
    )]);
    for provider in [
        "grok",
        "gemini",
        "gemini-interactions",
        "openai",
        "openai-compatibility",
        "plugin-provider",
    ] {
        let got = model_quota_observations(provider, &states);
        assert!(got.is_empty(), "provider {provider} returned {got:?}");
    }
}

/// Upstream's `TestModelQuotaObservationPayloadSkipsNilAndEmptyStates`.
#[test]
fn model_quota_observation_payload_skips_empty_states() {
    let states = BTreeMap::from([
        ("empty".to_owned(), ModelState::default()),
        (
            "observed".to_owned(),
            observed(10, &[("X-Codex-Plan-Type", "pro")]),
        ),
    ]);
    let got = model_quota_observations("codex", &states);
    assert_eq!(got.keys().collect::<Vec<_>>(), ["observed"]);
}

/// Upstream's `TestQuotaObservationPayloadExcludesCooldownState`.
#[test]
fn quota_observation_payload_excludes_cooldown_state() {
    let quota = QuotaState {
        exceeded: true,
        reason: "credential_quota".into(),
        next_recover_at: Some(unix(20)),
        backoff_level: 3,
        observed_at: Some(unix(10)),
        signals: signals([("X-Codex-Plan-Type", "pro")]),
    };
    let payload: Value =
        serde_json::from_str(&quota_observation("codex", &quota).encode()).unwrap();
    assert_eq!(
        payload,
        json!({
            "observed_at": "1970-01-01T00:00:10Z",
            "signals": { "X-Codex-Plan-Type": "pro" },
        })
    );
}

/// Not upstream's: the listing shows a credential's own snapshot without
/// its cooldown, and a provider that isn't observed shows none.
#[tokio::test]
async fn listing_shows_only_observed_providers() {
    let api = Api::new();
    for (id, provider) in [("codex-auth", "codex"), ("gemini-auth", "gemini")] {
        let mut auth = runtime_auth(id);
        auth.provider = provider.into();
        auth.quota = QuotaState {
            exceeded: true,
            reason: "credential_quota".into(),
            next_recover_at: Some(unix(20)),
            backoff_level: 3,
            observed_at: Some(unix(10)),
            signals: signals([("Retry-After", "30")]),
        };
        auth.model_states =
            BTreeMap::from([("model-a".to_owned(), observed(11, &[("Retry-After", "40")]))]);
        api.register(auth);
    }

    let payload = api
        .get("/v0/management/auth-files")
        .await
        .expect(StatusCode::OK);
    let files = payload["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    let codex = &files[0];
    assert_eq!(codex["id"], json!("codex-auth"));
    assert_eq!(
        codex["quota"],
        json!({ "observed_at": "1970-01-01T00:00:10Z", "signals": { "Retry-After": "30" } })
    );
    assert_eq!(
        codex["model_quotas"],
        json!({ "model-a": { "observed_at": "1970-01-01T00:00:11Z", "signals": { "Retry-After": "40" } } })
    );
    let gemini = &files[1];
    assert_eq!(gemini["id"], json!("gemini-auth"));
    assert_eq!(gemini["quota"], json!({ "signals": {} }));
    assert!(gemini.get("model_quotas").is_none(), "{gemini}");
}
