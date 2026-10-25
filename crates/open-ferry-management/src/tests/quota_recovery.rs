//! A confirmed upstream usage reset ends a stale quota cooldown (issue #8).
//!
//! Every upstream here is a local fake on 127.0.0.1; nothing contacts a
//! provider or a live account. The 429 is recorded straight into the
//! manager, as an executor's result would be, and the usage read goes
//! through `POST /v0/management/quota/fetch` (or, for the races, straight
//! to the manager).

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use open_ferry_core::auth::{Auth, AuthError};
use open_ferry_core::config::Config;
use open_ferry_core::manager::cooldown_store;
use open_ferry_core::manager::{
    CallResult, QuotaRecoveryToken, UsageReading, UsageWindow, UsageWindowReading,
};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

use super::{Api, Upstream, http_response};

const PATH: &str = "/v0/management/quota/fetch";
const ID: &str = "claude-recovery";
const SONNET: &str = "claude-3-5-sonnet-20241022";
const OPUS: &str = "claude-3-opus-20240229";
const SEVEN_DAYS: Duration = Duration::from_secs(7 * 24 * 60 * 60);

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.insert(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    map
}

/// The headers of a 429 that rejects the 7-day window.
fn weekly_headers() -> HeaderMap {
    let reset = (Utc::now() + TimeDelta::days(7)).timestamp().to_string();
    headers(&[
        ("anthropic-ratelimit-unified-status", "rejected"),
        ("anthropic-ratelimit-unified-7d-status", "rejected"),
        ("anthropic-ratelimit-unified-7d-reset", &reset),
        ("anthropic-ratelimit-unified-7d-utilization", "1.0"),
        (
            "anthropic-ratelimit-unified-representative-claim",
            "seven_day",
        ),
    ])
}

/// The headers of a 429 that rejects the 5-hour window.
fn session_headers() -> HeaderMap {
    let reset = (Utc::now() + TimeDelta::hours(5)).timestamp().to_string();
    headers(&[
        ("anthropic-ratelimit-unified-status", "rejected"),
        ("anthropic-ratelimit-unified-5h-status", "rejected"),
        ("anthropic-ratelimit-unified-5h-reset", &reset),
        ("anthropic-ratelimit-unified-5h-utilization", "1.0"),
        (
            "anthropic-ratelimit-unified-representative-claim",
            "five_hour",
        ),
    ])
}

fn credential_429(model: &str, response_headers: HeaderMap, retry: Duration) -> CallResult {
    CallResult {
        auth_id: ID.into(),
        provider: "claude".into(),
        model: model.into(),
        success: false,
        retry_after: Some(retry),
        credential_scope: true,
        response_headers,
        error: Some(AuthError {
            http_status: 429,
            message: "rate limit rejected".into(),
            ..AuthError::default()
        }),
        ..CallResult::default()
    }
}

/// A 429 of one model alone, rejecting the windows `rejected` names (`5h`,
/// `7d`) and naming `claim` as the representative one.
fn model_429(model: &str, rejected: &[&str], claim: &str, retry: Duration) -> CallResult {
    let mut pairs = vec![("anthropic-ratelimit-unified-status", "rejected")];
    for window in ["5h", "7d"] {
        let name: &'static str = if window == "5h" {
            "anthropic-ratelimit-unified-5h-status"
        } else {
            "anthropic-ratelimit-unified-7d-status"
        };
        pairs.push((
            name,
            if rejected.contains(&window) {
                "rejected"
            } else {
                "allowed"
            },
        ));
    }
    pairs.push(("anthropic-ratelimit-unified-representative-claim", claim));
    CallResult {
        credential_scope: false,
        ..credential_429(model, headers(&pairs), retry)
    }
}

fn rfc3339(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// A usage window `percent` used, resetting `reset_in` from now.
fn window(percent: f64, reset_in: TimeDelta) -> Value {
    json!({ "utilization": percent, "resets_at": rfc3339(Utc::now() + reset_in) })
}

/// Raw OAuth usage with every window healthy.
fn healthy_usage() -> Value {
    json!({
        "five_hour": window(4.0, TimeDelta::hours(5)),
        "seven_day": window(9.0, TimeDelta::days(7)),
        "seven_day_oauth_apps": null,
        "seven_day_opus": null,
        "seven_day_sonnet": window(2.0, TimeDelta::days(7)),
    })
}

/// A response with `body`, answered at `date` if given, plus `extra`.
fn usage_at(
    status: &str,
    body: &Value,
    date: Option<DateTime<Utc>>,
    extra: &[(&str, String)],
) -> Vec<u8> {
    let mut all = vec![("Content-Type", "application/json".to_owned())];
    if let Some(date) = date {
        all.push(("Date", date.format("%a, %d %b %Y %H:%M:%S GMT").to_string()));
    }
    all.extend(extra.iter().map(|(name, value)| (*name, value.clone())));
    let pairs: Vec<(&str, &str)> = all.iter().map(|(n, v)| (*n, v.as_str())).collect();
    http_response(status, &pairs, body.to_string().as_bytes())
}

fn usage_response(body: &Value, extra: &[(&str, String)]) -> Vec<u8> {
    usage_at("200 OK", body, Some(Utc::now()), extra)
}

/// A Claude credential whose probe reads `url`, standing for the official
/// source when `official`.
fn credential(id: &str, url: &str, official: bool) -> Auth {
    let source = format!("{url}/api/oauth/usage");
    if official {
        crate::quota_fetch::allow_test_source(&source);
    }
    let mut auth = Auth {
        id: id.into(),
        file_name: format!("{id}.json"),
        provider: "claude".into(),
        ..Auth::default()
    };
    auth.metadata = match json!({
        "access_token": "synthetic-token",
        "quota_probe": {
            "url": source,
            "method": "GET",
            "header": { "Authorization": "Bearer $TOKEN$" },
            // Display only: it reads the raw windows as buckets, which is
            // all the pre-recovery handler could do with this body.
            "mapping": {
                "groups": [{
                    "display_name": "Claude",
                    "buckets": [
                        {
                            "window": "five_hour.resets_at",
                            "remaining_fraction": "five_hour.utilization",
                        },
                        {
                            "window": "seven_day.resets_at",
                            "remaining_fraction": "seven_day.utilization",
                        },
                    ],
                }],
            },
        },
    }) {
        Value::Object(map) => map,
        _ => unreachable!(),
    };
    auth
}

/// A registered Claude credential whose probe reads `upstream`.
fn claude_api(upstream: &Upstream) -> (Api, String) {
    let api = Api::new();
    let index = api.register(credential(ID, &upstream.url, true));
    (api, index)
}

/// An API with a credential and no upstream, for applying readings direct.
fn manager_api() -> Api {
    let api = Api::new();
    api.register(credential(ID, "http://127.0.0.1:9", false));
    api
}

async fn fetch(api: &Api, index: &str) -> super::Answer {
    api.post(PATH, &json!({ "auth_index": index }).to_string())
        .await
}

fn blocked(api: &Api) -> bool {
    let auth = api.manager.get(ID).expect("credential");
    auth.unavailable || auth.quota.exceeded
}

fn model_blocked(api: &Api, model: &str) -> bool {
    let auth = api.manager.get(ID).expect("credential");
    auth.model_states
        .iter()
        .any(|(name, state)| name == model && (state.quota.exceeded || state.unavailable))
}

fn reading(windows: &[(UsageWindow, f64)]) -> UsageReading {
    UsageReading {
        observed_at: Some(Utc::now()),
        windows: windows
            .iter()
            .map(|(window, percent)| {
                (
                    *window,
                    UsageWindowReading {
                        utilization_percent: *percent,
                        resets_at: Some(Utc::now() + TimeDelta::days(1)),
                    },
                )
            })
            .collect(),
    }
}

fn base(five: f64, seven: f64) -> Vec<(UsageWindow, f64)> {
    vec![
        (UsageWindow::FiveHour, five),
        (UsageWindow::SevenDay, seven),
    ]
}

/// Whether applying `reading` against `token` ended anything.
fn apply(api: &Api, token: &QuotaRecoveryToken, reading: &UsageReading) -> bool {
    !api.manager
        .apply_quota_usage_recovery(token, reading)
        .unwrap()
        .is_empty()
}

// A multi-day weekly cooldown, a fresh healthy usage read: the credential is
// back in rotation, and a later real 429 blocks it again.
#[tokio::test]
async fn a_healthy_usage_read_ends_a_stale_weekly_cooldown() {
    let upstream = Upstream::start(|_, _| usage_response(&healthy_usage(), &[])).await;
    let (api, index) = claude_api(&upstream);
    api.manager
        .mark_result(&credential_429(SONNET, weekly_headers(), SEVEN_DAYS));
    assert!(blocked(&api), "the 429 must block the credential");

    let answer = fetch(&api, &index).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert!(
        !blocked(&api),
        "a healthy usage read left the stale cooldown: {:?}",
        api.manager.get(ID).unwrap().quota
    );

    api.manager
        .mark_result(&credential_429(SONNET, weekly_headers(), SEVEN_DAYS));
    assert!(blocked(&api), "a new 429 must reblock the credential");
}

#[tokio::test]
async fn a_session_cooldown_ends_the_same_way() {
    let upstream = Upstream::start(|_, _| usage_response(&healthy_usage(), &[])).await;
    let (api, index) = claude_api(&upstream);
    api.manager.mark_result(&credential_429(
        SONNET,
        session_headers(),
        Duration::from_secs(5 * 3600),
    ));
    assert!(blocked(&api));
    fetch(&api, &index).await;
    assert!(!blocked(&api));
}

// A read whose status isn't exactly 200 proves nothing, whatever its body.
#[tokio::test]
async fn a_non_200_read_with_a_healthy_body_leaves_the_cooldown() {
    for status in [
        "206 Partial Content",
        "202 Accepted",
        "500 Internal Server Error",
    ] {
        let upstream =
            Upstream::start(move |_, _| usage_at(status, &healthy_usage(), Some(Utc::now()), &[]))
                .await;
        let (api, index) = claude_api(&upstream);
        api.manager
            .mark_result(&credential_429(SONNET, weekly_headers(), SEVEN_DAYS));
        fetch(&api, &index).await;
        assert!(blocked(&api), "{status} revived the credential");
    }
}

#[tokio::test]
async fn a_read_that_proves_nothing_leaves_the_cooldown() {
    let day = TimeDelta::days(1);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "weekly window at its limit",
            usage_response(
                &json!({
                    "five_hour": window(4.0, TimeDelta::hours(5)),
                    "seven_day": window(100.0, day),
                }),
                &[],
            ),
        ),
        ("plan only", usage_response(&json!({ "plan": "max" }), &[])),
        (
            "malformed",
            usage_at("200 OK", &json!("nope"), Some(Utc::now()), &[]),
        ),
        (
            "partial",
            usage_response(
                &json!({ "five_hour": window(4.0, TimeDelta::hours(5)) }),
                &[],
            ),
        ),
        (
            "stale date",
            usage_at(
                "200 OK",
                &healthy_usage(),
                Some(Utc::now() - TimeDelta::minutes(10)),
                &[],
            ),
        ),
        ("no date", usage_at("200 OK", &healthy_usage(), None, &[])),
        (
            "aged",
            usage_at(
                "200 OK",
                &healthy_usage(),
                Some(Utc::now()),
                &[("Age", "30".into())],
            ),
        ),
    ];
    for (name, response) in cases {
        let upstream = Upstream::answering(response).await;
        let (api, index) = claude_api(&upstream);
        api.manager
            .mark_result(&credential_429(SONNET, weekly_headers(), SEVEN_DAYS));
        fetch(&api, &index).await;
        assert!(blocked(&api), "{name} revived the credential");
    }
}

// A probe that isn't the official request is shown, nothing more.
#[tokio::test]
async fn an_unofficial_probe_never_ends_a_cooldown() {
    let upstream = Upstream::start(|_, _| usage_response(&healthy_usage(), &[])).await;
    let api = Api::new();
    let index = api.register(credential(ID, &upstream.url, false));
    api.manager
        .mark_result(&credential_429(SONNET, weekly_headers(), SEVEN_DAYS));
    fetch(&api, &index).await;
    assert!(blocked(&api));
}

#[tokio::test]
async fn a_redirect_to_another_source_never_ends_a_cooldown() {
    let other = Upstream::start(|_, _| usage_response(&healthy_usage(), &[])).await;
    let target = format!("{}/api/oauth/usage", other.url);
    let upstream = Upstream::start(move |_, _| {
        http_response("302 Found", &[("Location", target.as_str())], b"")
    })
    .await;
    let (api, index) = claude_api(&upstream);
    api.manager
        .mark_result(&credential_429(SONNET, weekly_headers(), SEVEN_DAYS));
    fetch(&api, &index).await;
    assert!(blocked(&api));
}

// Other credentials keep their cooldowns.
#[tokio::test]
async fn another_credentials_cooldown_stays() {
    let upstream = Upstream::start(|_, _| usage_response(&healthy_usage(), &[])).await;
    let (api, index) = claude_api(&upstream);
    api.register(credential("claude-other", &upstream.url, true));
    api.manager
        .mark_result(&credential_429(SONNET, weekly_headers(), SEVEN_DAYS));
    api.manager.mark_result(&CallResult {
        auth_id: "claude-other".into(),
        ..credential_429(SONNET, weekly_headers(), SEVEN_DAYS)
    });
    fetch(&api, &index).await;
    assert!(!blocked(&api));
    let other = api.manager.get("claude-other").unwrap();
    assert!(other.unavailable || other.quota.exceeded);
}

// The windows a 429 named, not just any healthy one, decide.
#[test]
fn a_weekly_cooldown_needs_the_weekly_window_healthy() {
    let api = manager_api();
    api.manager
        .mark_result(&credential_429(SONNET, weekly_headers(), SEVEN_DAYS));
    let token = api.manager.quota_recovery_token(ID).unwrap();
    assert!(!apply(&api, &token, &reading(&base(4.0, 100.0))));
    assert!(blocked(&api));
    assert!(apply(&api, &token, &reading(&base(4.0, 9.0))));
    assert!(!blocked(&api));
}

// A model's own cooldown ends alone; its sibling's stays.
#[test]
fn a_model_scope_recovery_leaves_the_sibling() {
    let api = manager_api();
    api.manager
        .mark_result(&model_429(OPUS, &["7d"], "seven_day_opus", SEVEN_DAYS));
    api.manager
        .mark_result(&model_429(SONNET, &["7d"], "seven_day_sonnet", SEVEN_DAYS));
    let token = api.manager.quota_recovery_token(ID).unwrap();
    let mut windows = base(4.0, 9.0);
    windows.push((UsageWindow::SevenDayOpus, 100.0));
    windows.push((UsageWindow::SevenDaySonnet, 3.0));
    assert!(apply(&api, &token, &reading(&windows)));
    assert!(model_blocked(&api, OPUS), "opus is still at its limit");
    assert!(!model_blocked(&api, SONNET), "sonnet read healthy");
}

// A model's claim survives another model's 429 (no whole-credential fence),
// and the credential's derived quota goes with the last of its models'.
#[test]
fn another_models_429_does_not_fence_a_model_claim() {
    let api = manager_api();
    api.manager
        .mark_result(&model_429(OPUS, &["7d"], "seven_day_opus", SEVEN_DAYS));
    api.manager
        .mark_result(&model_429(SONNET, &["7d"], "seven_day_sonnet", SEVEN_DAYS));
    let token = api.manager.quota_recovery_token(ID).unwrap();
    let mut windows = base(4.0, 9.0);
    windows.push((UsageWindow::SevenDayOpus, 2.0));
    windows.push((UsageWindow::SevenDaySonnet, 3.0));
    assert!(apply(&api, &token, &reading(&windows)));
    assert!(!model_blocked(&api, OPUS));
    assert!(!model_blocked(&api, SONNET));
    assert!(
        !blocked(&api),
        "the derived credential quota must clear too"
    );
}

// A later 429 that leaves the longer earlier cooldown standing keeps the
// earlier window owed.
#[test]
fn a_retained_earlier_window_stays_owed() {
    let api = manager_api();
    api.manager
        .mark_result(&model_429(OPUS, &["7d"], "seven_day_opus", SEVEN_DAYS));
    api.manager.mark_result(&model_429(
        OPUS,
        &["5h"],
        "five_hour",
        Duration::from_secs(3600),
    ));
    let token = api.manager.quota_recovery_token(ID).unwrap();
    let mut windows = base(4.0, 9.0);
    windows.push((UsageWindow::SevenDayOpus, 100.0));
    assert!(!apply(&api, &token, &reading(&windows)));
    assert!(model_blocked(&api, OPUS));
}

// A read begun before a newer failure, a rejected token or a replacement of
// the credential is refused.
#[test]
fn a_read_racing_a_newer_429_is_refused() {
    let api = manager_api();
    api.manager
        .mark_result(&credential_429(SONNET, weekly_headers(), SEVEN_DAYS));
    let token = api.manager.quota_recovery_token(ID).unwrap();
    api.manager
        .mark_result(&credential_429(SONNET, weekly_headers(), SEVEN_DAYS));
    assert!(!apply(&api, &token, &reading(&base(4.0, 9.0))));
    assert!(blocked(&api), "a newer 429 must keep its cooldown");
}

#[test]
fn a_read_racing_an_unauthorized_failure_is_refused() {
    let api = manager_api();
    api.manager
        .mark_result(&credential_429(SONNET, weekly_headers(), SEVEN_DAYS));
    let token = api.manager.quota_recovery_token(ID).unwrap();
    api.manager.mark_result(&CallResult {
        auth_id: ID.into(),
        provider: "claude".into(),
        model: SONNET.into(),
        success: false,
        error: Some(AuthError {
            http_status: 401,
            message: "unauthorized".into(),
            ..AuthError::default()
        }),
        ..CallResult::default()
    });
    assert!(!apply(&api, &token, &reading(&base(4.0, 9.0))));
}

#[test]
fn a_failure_on_the_models_own_scope_fences_its_claim() {
    let api = manager_api();
    api.manager
        .mark_result(&model_429(OPUS, &["7d"], "seven_day_opus", SEVEN_DAYS));
    api.manager.mark_result(&CallResult {
        auth_id: ID.into(),
        provider: "claude".into(),
        model: OPUS.into(),
        success: false,
        error: Some(AuthError {
            http_status: 503,
            message: "unavailable".into(),
            ..AuthError::default()
        }),
        ..CallResult::default()
    });
    let token = api.manager.quota_recovery_token(ID).unwrap();
    let mut windows = base(4.0, 9.0);
    windows.push((UsageWindow::SevenDayOpus, 2.0));
    assert!(!apply(&api, &token, &reading(&windows)));
}

#[test]
fn a_read_racing_a_replacement_is_refused() {
    let api = manager_api();
    api.manager
        .mark_result(&credential_429(SONNET, weekly_headers(), SEVEN_DAYS));
    let token = api.manager.quota_recovery_token(ID).unwrap();
    api.manager
        .register(credential(ID, "http://127.0.0.1:9", false))
        .unwrap();
    assert!(!apply(&api, &token, &reading(&base(4.0, 9.0))));
}

fn ok(model: &str) -> CallResult {
    CallResult {
        auth_id: ID.into(),
        provider: "claude".into(),
        model: model.into(),
        success: true,
        ..CallResult::default()
    }
}

fn unavailable(model: &str) -> CallResult {
    CallResult {
        auth_id: ID.into(),
        provider: "claude".into(),
        model: model.into(),
        success: false,
        retry_after: Some(Duration::from_secs(600)),
        error: Some(AuthError {
            http_status: 503,
            message: "unavailable".into(),
            ..AuthError::default()
        }),
        ..CallResult::default()
    }
}

fn opus_healthy() -> Vec<(UsageWindow, f64)> {
    let mut windows = base(4.0, 9.0);
    windows.push((UsageWindow::SevenDayOpus, 2.0));
    windows
}

// A model's cooldown first, then a credential-wide one over it: the model's
// own claim still stands, and one healthy read ends both.
#[test]
fn a_credential_wrap_keeps_a_models_earlier_claim() {
    let api = manager_api();
    api.manager
        .mark_result(&model_429(OPUS, &["7d"], "seven_day_opus", SEVEN_DAYS));
    api.manager
        .mark_result(&credential_429(SONNET, weekly_headers(), SEVEN_DAYS));
    let token = api.manager.quota_recovery_token(ID).unwrap();
    assert!(apply(&api, &token, &reading(&opus_healthy())));
    assert!(!blocked(&api));
    assert!(!model_blocked(&api, OPUS));
}

// A cooldown no usage read can speak for (a 503, a 429 of unknown cause)
// under a later proven 429 is not recovered with it.
#[test]
fn a_mixed_in_blocker_is_not_recovered() {
    let api = manager_api();
    api.manager.mark_result(&unavailable(OPUS));
    api.manager
        .mark_result(&model_429(OPUS, &["7d"], "seven_day_opus", SEVEN_DAYS));
    let token = api.manager.quota_recovery_token(ID).unwrap();
    assert!(!apply(&api, &token, &reading(&opus_healthy())));
    assert!(model_blocked(&api, OPUS));

    let api = manager_api();
    api.manager.mark_result(&CallResult {
        credential_scope: false,
        ..credential_429(OPUS, HeaderMap::new(), Duration::from_secs(600))
    });
    api.manager
        .mark_result(&model_429(OPUS, &["7d"], "seven_day_opus", SEVEN_DAYS));
    let token = api.manager.quota_recovery_token(ID).unwrap();
    assert!(!apply(&api, &token, &reading(&opus_healthy())));
}

// An old blocker that genuinely ended doesn't poison a later proven 429.
#[test]
fn an_ended_blocker_does_not_poison_a_later_claim() {
    let api = manager_api();
    api.manager.mark_result(&unavailable(OPUS));
    api.manager.mark_result(&ok(OPUS));
    api.manager.mark_result(&ok(OPUS));
    api.manager
        .mark_result(&model_429(OPUS, &["7d"], "seven_day_opus", SEVEN_DAYS));
    let token = api.manager.quota_recovery_token(ID).unwrap();
    assert!(apply(&api, &token, &reading(&opus_healthy())));
    assert!(!model_blocked(&api, OPUS));
}

// The token is of the credential the request resolved, not a replacement.
#[test]
fn a_token_is_not_of_a_replaced_credential() {
    let api = manager_api();
    let before = api.manager.get(ID).unwrap();
    api.manager
        .register(credential(ID, "http://127.0.0.1:9", false))
        .unwrap();
    let token = api.manager.quota_recovery_token(ID).unwrap();
    assert!(!token.matches(&before));
    assert!(token.matches(&api.manager.get(ID).unwrap()));
}

// Only a bearer the executor sends for the credential may speak for it.
#[tokio::test]
async fn a_conflicting_bearer_never_ends_a_cooldown() {
    for conflict in ["api_key", "accessToken"] {
        let upstream = Upstream::start(|_, _| usage_response(&healthy_usage(), &[])).await;
        let api = Api::new();
        let mut auth = credential(ID, &upstream.url, true);
        match conflict {
            "api_key" => {
                auth.attributes
                    .insert("api_key".into(), "another-key".into());
            }
            _ => {
                auth.metadata
                    .insert("accessToken".into(), json!("another-token"));
            }
        }
        let index = api.register(auth);
        api.manager
            .mark_result(&credential_429(SONNET, weekly_headers(), SEVEN_DAYS));
        fetch(&api, &index).await;
        assert!(blocked(&api), "{conflict} override ended the cooldown");
    }
}

// A custom header can replace the credential's authentication after the
// executor sets it, so none that could carry an identity may stand.
#[tokio::test]
async fn an_authentication_header_override_never_ends_a_cooldown() {
    for (name, value) in [
        ("header:Authorization", "Bearer another-token"),
        ("header:authorization", "Bearer another-token"),
        ("header: AUTHORIZATION ", "Bearer another-token"),
        ("HEADER:Authorization", "Bearer another-token"),
        ("header:x-api-key", "another-key"),
        ("header:X-Api-Key", "another-key"),
        ("header:X-Source", "$X-Api-Key"),
        ("header:Cookie", "session=another"),
    ] {
        let upstream = Upstream::start(|_, _| usage_response(&healthy_usage(), &[])).await;
        let api = Api::new();
        let mut auth = credential(ID, &upstream.url, true);
        auth.attributes.insert(name.into(), value.into());
        let index = api.register(auth);
        api.manager
            .mark_result(&credential_429(SONNET, weekly_headers(), SEVEN_DAYS));
        fetch(&api, &index).await;
        assert!(blocked(&api), "{name:?} override ended the cooldown");
    }
}

// The probe's own extra headers are sent with the bearer: an `X-Api-Key` or
// a cookie beside it leaves open which identity the endpoint reads, so the
// read is shown and ends nothing. The display fetch itself still works.
#[tokio::test]
async fn a_second_authentication_header_on_the_probe_never_ends_a_cooldown() {
    for (name, value) in [
        ("X-Api-Key", "another-key"),
        ("x-api-key", "another-key"),
        ("X-API-KEY", "another-key"),
        ("Cookie", "session=another"),
        ("cookie", "session=another"),
        ("Proxy-Authorization", "Basic YW5vdGhlcg=="),
        ("X-Auth-Token", "another-token"),
        ("X-Source", "$X-Api-Key"),
    ] {
        let upstream = Upstream::start(|_, _| usage_response(&healthy_usage(), &[])).await;
        let api = Api::new();
        let mut auth = credential(ID, &upstream.url, true);
        match auth.metadata.get_mut("quota_probe") {
            Some(Value::Object(probe)) => match probe.get_mut("header") {
                Some(Value::Object(headers)) => {
                    headers.insert(name.into(), json!(value));
                }
                _ => unreachable!(),
            },
            _ => unreachable!(),
        }
        let index = api.register(auth);
        api.manager
            .mark_result(&credential_429(SONNET, weekly_headers(), SEVEN_DAYS));
        let answer = fetch(&api, &index).await;
        assert_eq!(answer.status, StatusCode::OK, "{name}: {}", answer.body);
        assert!(blocked(&api), "{name:?} on the probe ended the cooldown");
    }
}

// An unrelated custom header does not stop a recovery.
#[tokio::test]
async fn an_unrelated_custom_header_still_allows_a_recovery() {
    let upstream = Upstream::start(|_, _| usage_response(&healthy_usage(), &[])).await;
    let api = Api::new();
    let mut auth = credential(ID, &upstream.url, true);
    auth.attributes
        .insert("header:X-Region".into(), "eu".into());
    let index = api.register(auth);
    api.manager
        .mark_result(&credential_429(SONNET, weekly_headers(), SEVEN_DAYS));
    fetch(&api, &index).await;
    assert!(!blocked(&api));
}

// A report naming a window or a field twice is ambiguous: a parser may keep
// either, so it proves nothing.
#[tokio::test]
async fn a_report_with_a_repeated_name_never_ends_a_cooldown() {
    let ok = window(4.0, TimeDelta::hours(5)).to_string();
    let week = window(9.0, TimeDelta::days(7)).to_string();
    let full = window(100.0, TimeDelta::days(1)).to_string();
    let reset = rfc3339(Utc::now() + TimeDelta::days(7));
    let cases = [
        (
            "repeated window",
            format!(r#"{{"five_hour":{ok},"seven_day":{full},"seven_day":{week}}}"#),
        ),
        (
            "repeated window, healthy last",
            format!(r#"{{"five_hour":{ok},"seven_day":{week},"seven_day":{week}}}"#),
        ),
        (
            "repeated field",
            format!(
                r#"{{"five_hour":{ok},"seven_day":{{"utilization":100.0,"utilization":9.0,"resets_at":"{reset}"}}}}"#
            ),
        ),
    ];
    for (name, body) in cases {
        let response = http_response(
            "200 OK",
            &[
                ("Content-Type", "application/json"),
                (
                    "Date",
                    &Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string(),
                ),
            ],
            body.as_bytes(),
        );
        let upstream = Upstream::answering(response).await;
        let (api, index) = claude_api(&upstream);
        api.manager
            .mark_result(&credential_429(SONNET, weekly_headers(), SEVEN_DAYS));
        fetch(&api, &index).await;
        assert!(blocked(&api), "{name} revived the credential");
    }
}

// Only an explicit null is an unused window; a missing reset time proves
// nothing.
#[tokio::test]
async fn an_unused_window_needs_an_explicit_null_reset() {
    let unused = json!({ "utilization": 0.0, "resets_at": null });
    let missing = json!({ "utilization": 0.0 });
    for (name, five, ends) in [("null", unused, true), ("missing", missing, false)] {
        let body = json!({ "five_hour": five, "seven_day": window(9.0, TimeDelta::days(7)) });
        let upstream = Upstream::start(move |_, _| usage_response(&body, &[])).await;
        let (api, index) = claude_api(&upstream);
        api.manager
            .mark_result(&credential_429(SONNET, weekly_headers(), SEVEN_DAYS));
        fetch(&api, &index).await;
        assert_eq!(!blocked(&api), ends, "{name} reset time");
    }
}

fn blocked_id(api: &Api, id: &str) -> bool {
    let auth = api.manager.get(id).expect("credential");
    auth.unavailable || auth.quota.exceeded
}

// A manual reset, by either route, clears its own credential alone, and a
// read begun before it can't end a cooldown that came after.
#[tokio::test]
async fn a_manual_reset_clears_its_own_credential_on_both_routes() {
    for route in [
        "/v0/management/reset-quota",
        "/v8/management/routing/cooldown/reset",
    ] {
        let api = Api::new();
        let index = api.register(credential(ID, "http://127.0.0.1:9", false));
        let other = "claude-other";
        api.register(credential(other, "http://127.0.0.1:9", false));
        for id in [ID, other] {
            api.manager.mark_result(&CallResult {
                auth_id: id.into(),
                ..credential_429(SONNET, weekly_headers(), SEVEN_DAYS)
            });
        }
        assert!(blocked_id(&api, ID) && blocked_id(&api, other));
        let stale = api.manager.quota_recovery_token(ID).unwrap();

        let body = json!({ "auth_index": index }).to_string();
        let answer = api.post(route, &body).await;
        assert_eq!(answer.status, StatusCode::OK, "{route}: {}", answer.body);
        assert!(!blocked_id(&api, ID), "{route}: the credential stays");
        assert!(blocked_id(&api, other), "{route}: another one was reset");

        api.manager
            .mark_result(&credential_429(SONNET, weekly_headers(), SEVEN_DAYS));
        assert!(
            !apply(&api, &stale, &reading(&base(4.0, 9.0))),
            "{route}: a read from before the reset ended the new cooldown"
        );
        assert!(blocked_id(&api, ID));
    }
}

// A window whose reset has passed proves nothing.
#[test]
fn a_reading_with_a_passed_reset_is_refused() {
    let api = manager_api();
    api.manager
        .mark_result(&credential_429(SONNET, weekly_headers(), SEVEN_DAYS));
    let token = api.manager.quota_recovery_token(ID).unwrap();
    let mut late = reading(&base(4.0, 9.0));
    late.windows
        .get_mut(&UsageWindow::SevenDay)
        .unwrap()
        .resets_at = Some(Utc::now() - TimeDelta::seconds(5));
    assert!(!apply(&api, &token, &late));
    assert!(blocked(&api));
}

// A reading whose Date predates its token (a cached answer) is refused.
#[test]
fn a_reading_older_than_its_token_is_refused() {
    let api = manager_api();
    api.manager
        .mark_result(&credential_429(SONNET, weekly_headers(), SEVEN_DAYS));
    let stale = UsageReading {
        observed_at: Some(Utc::now() - TimeDelta::seconds(30)),
        ..reading(&base(4.0, 9.0))
    };
    let token = api.manager.quota_recovery_token(ID).unwrap();
    assert!(!apply(&api, &token, &stale));
    assert!(blocked(&api));
}

/// A usage upstream that tells the test when it has read the request and
/// answers it only when released, so a change can land while a read is in
/// flight. The answer is made when it is sent, so its `Date` is of that
/// moment.
struct Gated {
    url: String,
    received: tokio::sync::mpsc::UnboundedReceiver<()>,
    release: Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl Gated {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (sender, received) = tokio::sync::mpsc::unbounded_channel();
        let release = Arc::new(tokio::sync::Notify::new());
        let gate = Arc::clone(&release);
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                if super::read_request(&mut stream).await.is_none() {
                    continue;
                }
                let _ = sender.send(());
                gate.notified().await;
                let _ = stream
                    .write_all(&usage_response(&healthy_usage(), &[]))
                    .await;
                let _ = stream.shutdown().await;
            }
        });
        Self {
            url,
            received,
            release,
            task,
        }
    }
}

impl Drop for Gated {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Starts a fetch of `index`, waits until the upstream has the request, runs
/// `change`, then lets the upstream answer. The fetch's status.
async fn fetch_across(
    api: &Api,
    upstream: &mut Gated,
    index: &str,
    change: impl FnOnce(),
) -> StatusCode {
    let fetching = fetch(api, index);
    tokio::pin!(fetching);
    tokio::select! {
        Some(()) = upstream.received.recv() => {}
        answer = &mut fetching => panic!("the fetch ended before the upstream saw it: {}", answer.status),
    }
    change();
    upstream.release.notify_one();
    fetching.await.status
}

// A credential removed while its usage is being read stays removed.
#[tokio::test]
async fn a_read_in_flight_does_not_outlive_the_credentials_removal() {
    let mut upstream = Gated::start().await;
    let api = Api::new();
    let index = api.register(credential(ID, &upstream.url, true));
    api.manager
        .mark_result(&credential_429(SONNET, weekly_headers(), SEVEN_DAYS));
    fetch_across(&api, &mut upstream, &index, || api.manager.remove(ID)).await;
    assert!(api.manager.get(ID).is_none());
}

// A credential registered again while its usage is being read keeps the
// cooldown the new registration got, whatever the old read shows.
#[tokio::test]
async fn a_read_in_flight_does_not_end_a_reregistered_credentials_cooldown() {
    let mut upstream = Gated::start().await;
    let api = Api::new();
    let url = upstream.url.clone();
    let index = api.register(credential(ID, &url, true));
    api.manager
        .mark_result(&credential_429(SONNET, weekly_headers(), SEVEN_DAYS));
    let status = fetch_across(&api, &mut upstream, &index, || {
        api.register(credential(ID, &url, true));
        // A call made with the new registration says so, or is dropped.
        let new = api.manager.get(ID).unwrap();
        api.manager.mark_result(&CallResult {
            registration_epoch: new.registration_epoch,
            credential_version: new.credential_version,
            ..credential_429(SONNET, weekly_headers(), SEVEN_DAYS)
        });
        assert!(blocked(&api), "the new registration's 429 did not block it");
    })
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(blocked(&api), "a read of the old registration ended it");
}

// Saved cooldowns put back while a usage read is in flight are not ones the
// read speaks for.
#[tokio::test]
async fn a_read_in_flight_does_not_end_a_cooldown_restored_meanwhile() {
    let dir = super::scratch_dir();
    let mut upstream = Gated::start().await;
    let api = Api::new();
    let mut claude = credential(ID, &upstream.url, true);
    claude.attributes.insert(
        "path".into(),
        dir.path()
            .join("claude.json")
            .to_string_lossy()
            .into_owned(),
    );
    let index = api.register(claude);
    let config = |on: bool| {
        let mut config = Config::default();
        config.auth_dir = dir.path().to_string_lossy().into_owned();
        config.save_cooldown_status = on;
        config
    };
    let (on, off) = (config(true), config(false));
    cooldown_store::reconfigure(&api.manager, None, &on);
    cooldown_store::restore(&api.manager, &on);
    api.manager
        .mark_result(&credential_429(SONNET, weekly_headers(), SEVEN_DAYS));
    cooldown_store::flush(&api.manager);
    assert!(blocked(&api));

    let status = fetch_across(&api, &mut upstream, &index, || {
        cooldown_store::reconfigure(&api.manager, Some(&on), &off);
        cooldown_store::reconfigure(&api.manager, Some(&off), &on);
        cooldown_store::restore(&api.manager, &on);
    })
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(blocked(&api), "a read from before the restore ended it");
}
