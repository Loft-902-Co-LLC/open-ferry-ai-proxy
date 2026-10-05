// Ported from CLIProxyAPI sdk/cliproxy/auth/types_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Credential readers: the `request_retry` override, and expiry read from
//! the access token's JWT `exp` ahead of the metadata.
//!
//! Deviations from upstream:
//! - The nil-auth case of `TestRequestRetryOverride` is dropped (no nil
//!   credential in Rust); upstream's `(0, false)` is `None`.
//! - `TestAuth_ExpirationTime_JWTExp` uses a fixed now, where upstream reads
//!   the wall clock.
//! - `TestToolPrefixDisabled` is in `auth/metadata.rs`, with
//!   `Auth::tool_prefix_disabled`.
//! - The two `EnsureIndex` tests are in `auth/index.rs`, and the three
//!   `RecentRequestsSnapshot` tests in `auth/recent.rs`, beside the code.
//! - Dropped: `TestAuthClone_EmptyMapsIsolation` (Go map aliasing; Rust's
//!   `Clone` copies the maps).

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{SecondsFormat, TimeDelta};
use serde_json::{Value, json};

use crate::auth::{Auth, Timestamp};
use crate::manager::credential;

fn with_metadata(metadata: Value) -> Auth {
    let Value::Object(metadata) = metadata else {
        unreachable!()
    };
    Auth {
        metadata,
        ..Auth::default()
    }
}

#[test]
fn request_retry_override() {
    let cases: [(&str, Value, Option<i64>); 8] = [
        ("empty auth", json!({}), None),
        ("request_retry=0", json!({"request_retry": 0}), Some(0)),
        ("request_retry=3", json!({"request_retry": 3}), Some(3)),
        ("request_retry=-1", json!({"request_retry": -1}), None),
        (
            "legacy request-retry=2",
            json!({"request-retry": 2}),
            Some(2),
        ),
        (
            "legacy request-retry=-2",
            json!({"request-retry": -2}),
            None,
        ),
        (
            "canonical request_retry precedence",
            json!({"request_retry": 0, "request-retry": 2}),
            Some(0),
        ),
        (
            "request_retry string 0",
            json!({"request_retry": "0"}),
            Some(0),
        ),
    ];
    for (name, metadata, want) in cases {
        assert_eq!(
            credential::request_retry_override(&with_metadata(metadata)),
            want,
            "{name} override"
        );
    }
}

fn make_test_jwt(exp_unix: i64) -> String {
    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"none","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(format!(
        r#"{{"exp":{exp_unix},"email":"test@example.com"}}"#
    ));
    format!("{header}.{payload}.sig")
}

fn rfc3339(t: Timestamp) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[test]
fn auth_expiration_time_jwt_exp() {
    let now = chrono::DateTime::from_timestamp(1_780_272_000, 0).expect("now");
    let future_time = now + TimeDelta::hours(48);
    let past_time = now - TimeDelta::hours(24);

    let future_jwt = make_test_jwt(future_time.timestamp());
    let past_jwt = make_test_jwt(past_time.timestamp());

    // 1. No expired key: the expiry is the access token's JWT exp.
    let only_jwt = with_metadata(json!({"access_token": future_jwt}));
    let exp = only_jwt
        .expiration_time()
        .expect("ExpirationTime() should be set when access_token has valid JWT exp");
    assert_eq!(exp.timestamp(), future_time.timestamp());

    // 2. A past expired key loses to a future JWT exp.
    let stale_expired = with_metadata(json!({
        "expired": rfc3339(past_time),
        "access_token": future_jwt,
    }));
    let exp = stale_expired
        .expiration_time()
        .expect("ExpirationTime() for stale expired");
    assert_eq!(exp.timestamp(), future_time.timestamp());

    // 3. Both in the past: the expiry is in the past.
    let past = with_metadata(json!({
        "expired": rfc3339(past_time),
        "access_token": past_jwt,
    }));
    let exp = past
        .expiration_time()
        .expect("ExpirationTime() for past JWT");
    assert!(exp < now, "ExpirationTime() = {exp}, want past time");

    // 4. An expired access token isn't valid, whatever the ID token says.
    let expired_access_future_id = with_metadata(json!({
        "access_token": past_jwt,
        "id_token": future_jwt,
    }));
    assert!(
        !expired_access_future_id.has_valid_access_token(now),
        "HasValidAccessToken() should be false when access_token JWT is expired even if id_token is future"
    );
    let exp = expired_access_future_id
        .access_token_expiration_time()
        .expect("AccessTokenExpirationTime() should be set");
    assert!(
        exp < now,
        "AccessTokenExpirationTime() = {exp}, want past time"
    );

    // 5. No access token, no valid access token.
    let no_access = with_metadata(json!({
        "id_token": future_jwt,
        "expired": rfc3339(future_time),
    }));
    assert!(
        !no_access.has_valid_access_token(now),
        "HasValidAccessToken() should be false when access_token is missing"
    );
}
