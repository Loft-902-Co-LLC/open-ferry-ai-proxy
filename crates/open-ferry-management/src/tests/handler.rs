// Ported from CLIProxyAPI internal/api/handlers/management/handler_test.go
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The management key check, through the router.
//!
//! Deviations from upstream:
//! - `TestMiddlewareSetsSupportPluginHeader` checks `X-CPA-VERSION`,
//!   `X-CPA-COMMIT` and `X-CPA-BUILD-DATE` instead of
//!   `X-CPA-SUPPORT-PLUGIN`, which comes from the plugin host this port
//!   doesn't have.

use http::{Method, StatusCode};

use super::{Api, KEY, LOCAL, request_from};

#[tokio::test]
async fn authenticate_management_key_localhost_ip_ban_blocks_correct_key_during_ban() {
    let api = Api::with(Default::default(), Some(KEY));
    let attempt = |key: &str| {
        let mut request = request_from(LOCAL, Method::GET, "/v0/management/auth-files", "");
        request
            .headers_mut()
            .insert("x-management-key", key.parse().unwrap());
        api.send(request)
    };

    for i in 0..5 {
        let answer = attempt("wrong-secret").await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "attempt {}", i + 1);
        assert_eq!(answer.body, r#"{"error":"invalid management key"}"#);
    }

    let answer = attempt(KEY).await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    let prefix = r#"{"error":"IP banned due to too many failed attempts. Try again in "#;
    assert!(answer.body.starts_with(prefix), "{}", answer.body);
}

#[tokio::test]
async fn middleware_sets_build_headers() {
    let api = Api::with(Default::default(), Some(KEY));
    for (key, status) in [
        ("wrong-secret", StatusCode::UNAUTHORIZED),
        (KEY, StatusCode::OK),
    ] {
        let mut request = request_from(LOCAL, Method::GET, "/v0/management/config", "");
        request
            .headers_mut()
            .insert("x-management-key", key.parse().unwrap());
        let answer = api.send(request).await;
        assert_eq!(answer.status, status, "{key}");
        assert_eq!(
            answer.header("x-cpa-version"),
            Some(env!("CARGO_PKG_VERSION"))
        );
        assert!(answer.header("x-cpa-commit").is_some_and(|v| !v.is_empty()));
        assert!(
            answer
                .header("x-cpa-build-date")
                .is_some_and(|v| !v.is_empty())
        );
        assert_eq!(answer.header("x-cpa-support-plugin"), None);
    }
}
