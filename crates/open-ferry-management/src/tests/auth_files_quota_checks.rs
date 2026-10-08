//! Not upstream's: the `quota_checks` of a credential entry, the quota
//! rests open-ferry's `routing.quota.check-after` capped. CLIProxyAPI has
//! no such cap or field, so there is no parity suite for them.

use std::time::Duration;

use chrono::{TimeZone, Utc};
use open_ferry_core::auth::{AuthError, Timestamp};
use open_ferry_core::manager::{CallResult, QuotaCheck, Settings};
use serde_json::{Value, json};

use super::{Api, object, time};
use crate::auth_files::quota_check_json;

const HOUR: Duration = Duration::from_secs(3600);
const DAY: Duration = Duration::from_secs(86_400);

/// The API with the cap on quota rests at `cap`, and a runtime-only Codex
/// credential `a`.
fn api(cap: Duration) -> Api {
    let api = Api::new();
    api.manager.set_settings(Settings {
        quota_check_after: cap,
        ..Settings::default()
    });
    let mut auth = super::runtime_auth("a");
    auth.file_name = "a.json".into();
    api.register(auth);
    api
}

/// A quota answer for `model` on `a` that resets in `reset_in`.
fn quota(model: &str, reset_in: Duration, credential_scope: bool) -> CallResult {
    CallResult {
        auth_id: "a".into(),
        provider: "codex".into(),
        model: model.into(),
        success: false,
        error: Some(AuthError {
            message: "rate limited".into(),
            http_status: 429,
            ..AuthError::default()
        }),
        retry_after: Some(reset_in),
        credential_scope,
        ..CallResult::default()
    }
}

/// The entry of `a` as the list shows it.
async fn entry(api: &Api) -> Value {
    let mut files = api.files("?name=a.json").await;
    assert_eq!(files.len(), 1, "{files:?}");
    files.remove(0)
}

fn seconds(d: chrono::TimeDelta) -> i64 {
    d.num_seconds()
}

#[tokio::test]
async fn a_capped_rest_shows_when_it_is_next_checked() {
    let api = api(HOUR);
    let before = Utc::now();
    api.manager.mark_result(&quota("gpt-5", 5 * DAY, false));
    let after = Utc::now();
    let file = entry(&api).await;
    let checks = file["quota_checks"].as_array().expect("quota_checks");
    assert_eq!(checks.len(), 1, "{checks:?}");
    let check = object(&checks[0]);
    assert_eq!(check.len(), 6, "{check:?}");
    assert_eq!(check["scope"], json!("model"));
    assert_eq!(check["model_key"], json!("gpt-5"));
    assert_eq!(check["state"], json!("resting"));
    assert_eq!(check["wait_seconds"], json!(3600));
    let next = time(&check["next_check_at"]);
    let reset = time(&check["provider_reset_at"]);
    assert!(next >= before + HOUR && next <= after + HOUR, "{next}");
    assert_eq!(seconds(reset - next), 5 * 86_400 - 3600);

    // The cooldown the list shows ends at the check too.
    let cooldowns = file["cooldowns"].as_array().expect("cooldowns");
    assert_eq!(time(&cooldowns[0]["retry_at"]), next, "{cooldowns:?}");

    // The dashboard's entry is the same.
    let (_, value) = crate::credential_entry(&api.state, "a", Utc::now()).expect("entry");
    assert_eq!(value["quota_checks"], file["quota_checks"]);

    // A credential-wide quota rests the whole credential.
    api.manager.mark_result(&quota("gpt-5", 7 * DAY, true));
    let file = entry(&api).await;
    let checks = file["quota_checks"].as_array().expect("quota_checks");
    assert!(
        checks
            .iter()
            .any(|check| check["scope"] == json!("credential") && check.get("model_key").is_none()),
        "{checks:?}"
    );
}

#[tokio::test]
async fn no_rest_no_field() {
    let api = api(HOUR);
    let file = entry(&api).await;
    assert!(file.get("quota_checks").is_none(), "{file}");

    // A rest within the cap isn't capped.
    api.manager.mark_result(&quota("gpt-5", HOUR / 2, false));
    let file = entry(&api).await;
    assert!(file.get("quota_checks").is_none(), "{file}");

    // Nor is any with the cap off.
    let api = self::api(Duration::ZERO);
    api.manager.mark_result(&quota("gpt-5", 5 * DAY, false));
    let file = entry(&api).await;
    assert!(file.get("quota_checks").is_none(), "{file}");
    assert_eq!(file["cooldowns"].as_array().map(Vec::len), Some(1));
}

#[test]
fn each_state_is_named() {
    let at = |hours: i64| -> Timestamp {
        Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
            .single()
            .expect("time")
            + chrono::TimeDelta::hours(hours)
    };
    let check = QuotaCheck {
        model_key: String::new(),
        next_check_at: at(2),
        provider_reset_at: at(96),
        wait: 2 * HOUR,
        checking: false,
    };
    let encode = |check: &QuotaCheck, now: Timestamp| -> Value {
        serde_json::from_str(&quota_check_json(check, now).encode()).expect("json")
    };
    assert_eq!(
        encode(&check, at(1)),
        json!({
            "scope": "credential",
            "state": "resting",
            "next_check_at": "2026-06-01T02:00:00Z",
            "provider_reset_at": "2026-06-05T00:00:00Z",
            "wait_seconds": 7200,
        })
    );
    assert_eq!(encode(&check, at(2))["state"], json!("due"));
    let checking = QuotaCheck {
        checking: true,
        ..check
    };
    assert_eq!(encode(&checking, at(3))["state"], json!("checking"));
}
