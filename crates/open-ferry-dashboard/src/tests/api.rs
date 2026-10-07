//! The dashboard API's common rules: access, the errors, and the answers.

use http::{Method, StatusCode, header};
use open_ferry_core::config::Config;

use super::{Dash, KEY, LOCAL, REMOTE, keyed, keyed_config, request};
use crate::CONTENT_SECURITY_POLICY;

/// A route every test can call.
const LEDGER: &str = "/open-ferry/api/v1/usage/ledger";

/// The summary route.
const SUMMARY: &str = "/open-ferry/api/v1/usage/summary";

/// Not upstream's: the key is taken in each of the management API's
/// header forms, and every answer is JSON not to be stored, with the
/// dashboard's security headers.
#[tokio::test]
async fn the_key_is_taken_as_the_management_api_takes_it() {
    let dash = Dash::new();
    for (name, value) in [
        ("authorization", format!("Bearer {KEY}")),
        ("authorization", format!("bearer {KEY}")),
        ("authorization", KEY.to_owned()),
        ("x-management-key", KEY.to_owned()),
    ] {
        let mut call = request(LOCAL, Method::GET, LEDGER, "");
        call.headers_mut().insert(name, value.parse().unwrap());
        let answer = dash.send(call).await;
        answer.json(StatusCode::OK);
        assert_eq!(answer.header("cache-control"), Some("no-store"), "{name}");
        assert_eq!(
            answer.header("content-type"),
            Some("application/json; charset=utf-8")
        );
        assert_eq!(
            answer.header("content-security-policy"),
            Some(CONTENT_SECURITY_POLICY)
        );
        assert_eq!(answer.header("x-frame-options"), Some("DENY"));
        assert!(answer.header("set-cookie").is_none());
    }
}

/// Not upstream's: each refusal of the management API's checks is the
/// contract's error, with upstream's message; failed attempts at the
/// dashboard API count toward the same ban.
#[tokio::test]
async fn refusals_are_the_contracts_errors() {
    let dash = Dash::new();
    let message = dash
        .send(request(LOCAL, Method::GET, LEDGER, ""))
        .await
        .error(StatusCode::UNAUTHORIZED, "missing_management_key");
    assert_eq!(message, "missing management key");

    let mut wrong = request(LOCAL, Method::GET, LEDGER, "");
    wrong
        .headers_mut()
        .insert(header::AUTHORIZATION, "Bearer wrong".parse().unwrap());
    let message = dash
        .send(wrong)
        .await
        .error(StatusCode::UNAUTHORIZED, "invalid_management_key");
    assert_eq!(message, "invalid management key");

    let mut remote = request(REMOTE, Method::GET, LEDGER, "");
    remote.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {KEY}").parse().unwrap(),
    );
    let message = dash
        .send(remote)
        .await
        .error(StatusCode::FORBIDDEN, "remote_management_disabled");
    assert_eq!(message, "remote management disabled");

    // Two failures so far; three more ban the address, even with the key.
    for _ in 0..3 {
        dash.send(request(LOCAL, Method::GET, SUMMARY, ""))
            .await
            .error(StatusCode::UNAUTHORIZED, "missing_management_key");
    }
    let message = dash
        .get(LEDGER)
        .await
        .error(StatusCode::FORBIDDEN, "ip_banned");
    assert!(
        message.starts_with("IP banned due to too many failed attempts. Try again in"),
        "{message}"
    );
}

/// Not upstream's: a remote client is let in when remote management is
/// allowed, and nothing is served while no key is set.
#[tokio::test]
async fn remote_clients_and_an_unset_key() {
    let mut config = keyed_config();
    config.remote_management.allow_remote = true;
    let dash = Dash::with_config(config);
    let mut remote = request(REMOTE, Method::GET, LEDGER, "");
    remote.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {KEY}").parse().unwrap(),
    );
    dash.send(remote).await.json(StatusCode::OK);

    let dash = Dash::with_config(Config::default());
    for call in [
        keyed(Method::GET, LEDGER, ""),
        request(LOCAL, Method::GET, LEDGER, ""),
    ] {
        let message = dash
            .send(call)
            .await
            .error(StatusCode::NOT_FOUND, "management_disabled");
        assert_eq!(
            message,
            "no management key is set; set management.secret-key or MANAGEMENT_PASSWORD"
        );
    }
}

/// Not upstream's: the local management password is a key from 127.0.0.1
/// and ::1 only, as the management API takes it, and doesn't stand in for
/// a management key.
#[tokio::test]
async fn the_local_password_is_for_local_clients() {
    let with_password = |peer: &str| {
        let mut call = request(peer, Method::GET, LEDGER, "");
        call.headers_mut()
            .insert(header::AUTHORIZATION, "Bearer local-pass".parse().unwrap());
        call
    };
    let mut config = keyed_config();
    config.remote_management.allow_remote = true;
    let dash = Dash::with_local_password(config, "local-pass");
    for peer in [LOCAL, "[::1]:50000"] {
        dash.send(with_password(peer)).await.json(StatusCode::OK);
    }
    // From elsewhere it is a wrong key, whatever the client claims.
    let mut forwarded = with_password(REMOTE);
    forwarded
        .headers_mut()
        .insert("x-forwarded-for", "127.0.0.1".parse().unwrap());
    for call in [with_password(REMOTE), forwarded] {
        dash.send(call)
            .await
            .error(StatusCode::UNAUTHORIZED, "invalid_management_key");
    }
    let mut remote = request(REMOTE, Method::GET, LEDGER, "");
    remote.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {KEY}").parse().unwrap(),
    );
    dash.send(remote).await.json(StatusCode::OK);

    // Without allow-remote a remote client is refused before its key.
    let dash = Dash::with_local_password(keyed_config(), "local-pass");
    dash.send(with_password(REMOTE))
        .await
        .error(StatusCode::FORBIDDEN, "remote_management_disabled");

    // Without a management key the password lets nothing in.
    let dash = Dash::with_local_password(Config::default(), "local-pass");
    dash.send(with_password(LOCAL))
        .await
        .error(StatusCode::NOT_FOUND, "management_disabled");
}

/// Not upstream's: a path under `/open-ferry/` that isn't a route, and a
/// method a route doesn't take, are answered before the key is checked,
/// and aren't counted as failed attempts.
#[tokio::test]
async fn unknown_routes_and_methods() {
    let dash = Dash::new();
    for path in [
        "/open-ferry",
        "/open-ferry/",
        "/open-ferry/api",
        "/open-ferry/api/v2/usage/summary",
        "/open-ferry/api/v1/usage",
        "/open-ferry/api/v1/nothing",
    ] {
        for method in [Method::GET, Method::POST] {
            dash.send(request(LOCAL, method, path, ""))
                .await
                .error(StatusCode::NOT_FOUND, "not_found");
        }
    }
    for (method, path) in [
        (Method::POST, SUMMARY),
        (Method::DELETE, LEDGER),
        (Method::PUT, LEDGER),
        (Method::GET, "/open-ferry/api/v1/usage/records"),
        (Method::POST, "/open-ferry/api/v1/usage/prices"),
        (Method::DELETE, "/open-ferry/api/v1/request-logs"),
        (
            Method::POST,
            "/open-ferry/api/v1/request-logs/v1-2026-10-05T115802-1234abcd.log",
        ),
        (Method::PUT, "/open-ferry/api/v1/client-setup"),
        (Method::DELETE, "/open-ferry/api/v1/claude-cli/entries"),
    ] {
        let answer = dash.send(request(LOCAL, method.clone(), path, "")).await;
        answer.error(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed");
        assert_eq!(
            answer.header("content-security-policy"),
            Some(CONTENT_SECURITY_POLICY),
            "{method} {path}"
        );
    }
    dash.get(LEDGER).await.json(StatusCode::OK);
}

/// Not upstream's: a body must be a JSON object of known fields, of at
/// most 64 KiB.
#[tokio::test]
async fn bodies_are_checked() {
    let dash = Dash::new();
    for (body, code) in [
        ("", "invalid_json"),
        ("not json", "invalid_json"),
        ("[1]", "invalid_json"),
        ("\"text\"", "invalid_json"),
        (r#"{"retention_days": "30"}"#, "invalid_request"),
    ] {
        dash.call(Method::PATCH, LEDGER, body)
            .await
            .error(StatusCode::BAD_REQUEST, code);
    }
    let message = dash
        .call(
            Method::PATCH,
            LEDGER,
            r#"{"retention_days": 30, "retention": 1}"#,
        )
        .await
        .error(StatusCode::BAD_REQUEST, "invalid_request");
    assert!(message.contains("retention"), "{message}");

    let big = format!(r#"{{"currency": "{}"}}"#, "a".repeat(64 * 1024));
    dash.call(Method::PATCH, LEDGER, &big)
        .await
        .error(StatusCode::PAYLOAD_TOO_LARGE, "body_too_large");
    // Nothing was changed.
    let state = dash.get(LEDGER).await.json(StatusCode::OK);
    assert_eq!(state["retention_days"], 90);
}

/// Not upstream's: a query parameter given twice, a malformed or
/// out-of-range one and an empty range are refused, naming the parameter;
/// unknown parameters are ignored.
#[tokio::test]
async fn queries_are_checked() {
    let dash = Dash::new();
    let long = format!("{SUMMARY}?model={}", "a".repeat(9000));
    for (path, needle) in [
        (
            format!("{SUMMARY}?model=a&model=b"),
            "model is given more than once",
        ),
        (
            format!("{SUMMARY}?from=2026-10-05T12:00:00Z&to=2026-10-05T12:00:00Z"),
            "from must be before to",
        ),
        (
            format!("{SUMMARY}?from=2026-10-05T14:00:00%2B02:00&to=2026-10-05T12:00:00Z"),
            "from must be before to",
        ),
        (
            format!("{SUMMARY}?from=yesterday"),
            "from must be an RFC 3339 time",
        ),
        (
            format!("{SUMMARY}?limit=0"),
            "limit must be a whole number from 1 to 500",
        ),
        (format!("{SUMMARY}?limit=x"), "limit must be a whole number"),
        (format!("{SUMMARY}?group_by=day"), "group_by must be"),
        (long, "the query is too long"),
    ] {
        let message = dash
            .get(&path)
            .await
            .error(StatusCode::BAD_REQUEST, "invalid_request");
        assert!(message.contains(needle), "{path}: {message}");
    }
    dash.get(&format!(
        "{SUMMARY}?unknown=1&from=2020-01-01T00:00:00%2B02:00"
    ))
    .await
    .json(StatusCode::OK);
}
