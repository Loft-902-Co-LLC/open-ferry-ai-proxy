//! Tests of the routes of `crate::observability_settings`. Upstream has no
//! tests of these getters.

use http::{Method, StatusCode};

use super::{Api, KEY, LOCAL, keyed, keyed_config, request_from};

/// Not upstream's: each getter answers its setting as upstream writes it.
#[tokio::test]
async fn getters_answer_the_settings() {
    let api = Api::new();
    for (path, body) in [
        (
            "usage-statistics-enabled",
            r#"{"usage-statistics-enabled":false}"#,
        ),
        ("logs-max-total-size-mb", r#"{"logs-max-total-size-mb":0}"#),
        ("error-logs-max-files", r#"{"error-logs-max-files":10}"#),
    ] {
        let answer = api.get(&format!("/v0/management/{path}")).await;
        answer.assert(StatusCode::OK, body);
        assert_eq!(
            answer.header("content-type"),
            Some("application/json; charset=utf-8")
        );
    }

    let mut config = keyed_config();
    config.usage_statistics_enabled = true;
    config.logs_max_total_size_mb = 250;
    config.error_logs_max_files = 3;
    let api = Api::with(config, None);
    for (path, body) in [
        (
            "usage-statistics-enabled",
            r#"{"usage-statistics-enabled":true}"#,
        ),
        (
            "logs-max-total-size-mb",
            r#"{"logs-max-total-size-mb":250}"#,
        ),
        ("error-logs-max-files", r#"{"error-logs-max-files":3}"#),
    ] {
        let answer = api.get(&format!("/v0/management/{path}")).await;
        answer.assert(StatusCode::OK, body);
    }
}

/// Not upstream's: the getters need the management key, and their writes,
/// which would change the config file, answer with the empty 404.
#[tokio::test]
async fn getters_need_the_key_and_writes_are_unported() {
    let api = Api::new();
    for name in [
        "usage-statistics-enabled",
        "logs-max-total-size-mb",
        "error-logs-max-files",
    ] {
        let path = format!("/v0/management/{name}");
        let answer = api.send(request_from(LOCAL, Method::GET, &path, "")).await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{path}");
        assert!(!answer.body.contains(KEY), "{path}");

        for method in [Method::PUT, Method::PATCH] {
            let answer = api
                .send(keyed(method.clone(), &path, r#"{"value":1}"#))
                .await;
            assert_eq!(
                (answer.status, answer.body.as_str()),
                (StatusCode::NOT_FOUND, ""),
                "{method} {path}"
            );
        }
    }
}
