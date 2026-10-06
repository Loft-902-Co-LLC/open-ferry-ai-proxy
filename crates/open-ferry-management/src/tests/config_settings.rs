// Ported from CLIProxyAPI internal/api/handlers/management/
// config_basic_weight_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The routes of `crate::config_settings`, which change one setting.
//!
//! Upstream's tests of the file a setting is saved to (in
//! config_v8_test.go and config_v8_compatibility_test.go) are ported in
//! `config_v8_write`; here the config the
//! [`FakeWriter`](super::FakeWriter) is asked to save is checked.
//!
//! Deviations from upstream:
//! - `TestNormalizeRoutingStrategyWeightedRoundRobin` calls the helper;
//!   here the names go through the route.

use http::{Method, StatusCode};
use open_ferry_core::config::Config;

use super::{Api, keyed_config};

const OK: &str = r#"{"status":"ok"}"#;

/// Reads a boolean setting from the config.
type BoolField = fn(&Config) -> bool;

/// Reads an integer setting from the config.
type IntField = fn(&Config) -> i64;

/// Sets `path` to `value` with `method`, checking the answer.
async fn set(api: &Api, method: Method, path: &str, value: &str) {
    let path = format!("/v0/management/{path}");
    let answer = api
        .call(method, &path, &format!(r#"{{"value":{value}}}"#))
        .await;
    assert_eq!(
        (answer.status, answer.body.as_str()),
        (StatusCode::OK, OK),
        "{path} {value}"
    );
}

// Not upstream's: each boolean setting, by PUT and PATCH, saved without
// migrating the file.
#[tokio::test]
async fn booleans() {
    let settings: [(&str, BoolField); 8] = [
        ("debug", |config| config.debug),
        ("usage-statistics-enabled", |config| {
            config.usage_statistics_enabled
        }),
        ("logging-to-file", |config| config.logging_to_file),
        ("request-log", |config| config.request_log),
        ("ws-auth", |config| config.ws_auth),
        ("force-model-prefix", |config| config.force_model_prefix),
        ("quota-exceeded/switch-project", |config| {
            config.quota_exceeded.switch_project
        }),
        ("quota-exceeded/switch-preview-model", |config| {
            config.quota_exceeded.switch_preview_model
        }),
    ];
    let api = Api::writing(keyed_config());
    for (path, field) in settings {
        set(&api, Method::PUT, path, "true").await;
        assert!(field(&api.saved()), "{path}");
        set(&api, Method::PATCH, path, "false").await;
        assert!(!field(&api.saved()), "{path}");
    }
    assert!(api.writer.saved().iter().all(|(_, migrate)| !migrate));
    assert_eq!(api.reload.count(), 16);
}

// Not upstream's: each integer setting, the log limits mapped as upstream
// maps them.
#[tokio::test]
async fn integers() {
    let settings: [(&str, &str, i64, IntField); 9] = [
        ("request-retry", "5", 5, |config| config.request_retry),
        ("max-retry-credentials", "2", 2, |config| {
            config.max_retry_credentials
        }),
        ("max-retry-interval", "30", 30, |config| {
            config.max_retry_interval
        }),
        ("logs-max-total-size-mb", "100", 100, |config| {
            config.logs_max_total_size_mb
        }),
        ("logs-max-total-size-mb", "-5", 0, |config| {
            config.logs_max_total_size_mb
        }),
        ("error-logs-max-files", "3", 3, |config| {
            config.error_logs_max_files
        }),
        ("error-logs-max-files", "0", 0, |config| {
            config.error_logs_max_files
        }),
        ("error-logs-max-files", "-1", 10, |config| {
            config.error_logs_max_files
        }),
        ("request-retry", "-2", -2, |config| config.request_retry),
    ];
    let api = Api::writing(keyed_config());
    for (path, value, want, field) in settings {
        set(&api, Method::PUT, path, value).await;
        assert_eq!(field(&api.saved()), want, "{path} {value}");
    }
}

// Not upstream's: the proxy URL is stored as given, and deleted.
#[tokio::test]
async fn proxy_url() {
    let api = Api::writing(keyed_config());
    set(
        &api,
        Method::PUT,
        "proxy-url",
        r#"" socks5://127.0.0.1:1 ""#,
    )
    .await;
    assert_eq!(api.saved().proxy_url, " socks5://127.0.0.1:1 ");
    set(&api, Method::PATCH, "proxy-url", r#""http://127.0.0.1:2""#).await;
    assert_eq!(api.saved().proxy_url, "http://127.0.0.1:2");

    api.call(Method::DELETE, "/v0/management/proxy-url", "")
        .await
        .assert(StatusCode::OK, OK);
    assert_eq!(api.saved().proxy_url, "");
    assert_eq!(api.writer.saved().len(), 3);
}

// Not upstream's: the routing strategy is stored by its canonical name, and
// another name is refused.
#[tokio::test]
async fn routing_strategy() {
    let api = Api::writing(keyed_config());
    for (value, want) in [
        (r#"" FF ""#, "fill-first"),
        (r#""""#, "round-robin"),
        (r#""fill-first""#, "fill-first"),
        (r#""roundrobin""#, "round-robin"),
    ] {
        set(&api, Method::PUT, "routing/strategy", value).await;
        assert_eq!(api.saved().routing.strategy, want, "{value}");
    }
    let saves = api.writer.saved().len();
    api.call(
        Method::PUT,
        "/v0/management/routing/strategy",
        r#"{"value":"random"}"#,
    )
    .await
    .assert(StatusCode::BAD_REQUEST, r#"{"error":"invalid strategy"}"#);
    assert_eq!(api.writer.saved().len(), saves);
    assert_eq!(api.state.config().routing.strategy, "round-robin");
}

/// Ported from upstream's config_basic_weight_test.go
/// (TestNormalizeRoutingStrategyWeightedRoundRobin).
#[tokio::test]
async fn routing_strategy_weighted_round_robin() {
    let api = Api::writing(keyed_config());
    for input in ["weighted-round-robin", "weightedroundrobin", "wrr"] {
        set(
            &api,
            Method::PUT,
            "routing/strategy",
            &format!(r#""{input}""#),
        )
        .await;
        assert_eq!(
            api.saved().routing.strategy,
            "weighted-round-robin",
            "{input}"
        );
    }
}

// Not upstream's: a body that isn't an object with a value of the setting's
// type answers 400 and saves nothing.
#[tokio::test]
async fn invalid_bodies() {
    let api = Api::writing(keyed_config());
    let cases: [(&str, &[&str]); 3] = [
        (
            "debug",
            &[
                "",
                "{",
                "[]",
                "{}",
                r#"{"value":null}"#,
                r#"{"value":"true"}"#,
                r#"{"value":1}"#,
            ],
        ),
        (
            "request-retry",
            &[r#"{"value":1.5}"#, r#"{"value":true}"#, r#"{"value":"3"}"#],
        ),
        ("proxy-url", &[r#"{"value":1}"#, r#"{"value":null}"#]),
    ];
    for (path, bodies) in cases {
        for body in bodies {
            let answer = api
                .call(Method::PUT, &format!("/v0/management/{path}"), body)
                .await;
            assert_eq!(
                (answer.status, answer.body.as_str()),
                (StatusCode::BAD_REQUEST, r#"{"error":"invalid body"}"#),
                "{path} {body}"
            );
        }
    }
    assert!(api.writer.written().is_empty());
    assert_eq!(api.reload.count(), 0);
}
