// Ported from CLIProxyAPI internal/api/server_management_v8_test.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The v8 management names.
//!
//! Deviations from upstream:
//! - `TestManagementV8RoutesShareAccessControl` requests the credential
//!   list under both names instead of the unported config and plugin
//!   routes, and drops its Home mode case: Home mode isn't ported.
//! - `TestManagementV8IndependentContract` keeps what concerns the routes
//!   ported here: the legacy names and the removed quota routes answer 404
//!   under `/v8/management`, and each ported route answers under both
//!   names. Its route table, OAuth, config and migration checks concern
//!   unported routes and are dropped.
//! - `TestManagementV8PluginOperationMigratesConfiguration` is dropped: the
//!   plugin routes aren't ported, and this port never writes the config.

use http::{Method, StatusCode};

use super::{Api, KEY, LOCAL, keyed, keyed_config, request_from};

#[tokio::test]
async fn management_v8_routes_share_access_control() {
    for (name, enabled, authorized, want) in [
        ("authorized", true, true, StatusCode::OK),
        ("missing key", true, false, StatusCode::UNAUTHORIZED),
        ("disabled", false, true, StatusCode::NOT_FOUND),
    ] {
        let config = if enabled {
            keyed_config()
        } else {
            Default::default()
        };
        let api = Api::with(config, None);
        for route in ["/v0/management/auth-files", "/v8/management/credentials"] {
            let request = if authorized {
                keyed(Method::GET, route, "")
            } else {
                request_from(LOCAL, Method::GET, route, "")
            };
            let answer = api.send(request).await;
            assert_eq!(answer.status, want, "{name}: {route}: {}", answer.body);
            if !enabled {
                assert_eq!(answer.body, "", "{name}: {route}");
            }
        }
    }
}

#[tokio::test]
async fn management_v8_independent_contract() {
    let api = Api::new();
    for legacy in [
        "debug",
        "request-retry",
        "api-keys",
        "codex-api-key",
        "auth-files",
        "codex-auth-url",
        "oauth/providers/codex/auth-url",
        "plugins/test-plugin/config",
        "reset-quota",
        "api-call",
    ] {
        for method in [Method::GET, Method::POST] {
            let path = format!("/v8/management/{legacy}");
            let answer = api.send(keyed(method.clone(), &path, "")).await;
            answer.assert(StatusCode::NOT_FOUND, "");
        }
    }
    for (method, path) in [
        (Method::GET, "/v8/management/credentials/quota/providers"),
        (Method::POST, "/v8/management/credentials/quota/fetch"),
        (Method::POST, "/v8/management/credentials/quota/reset"),
    ] {
        api.send(keyed(method, path, ""))
            .await
            .assert(StatusCode::NOT_FOUND, "");
    }

    for (method, v0, v8, body, status, want) in [
        (
            Method::GET,
            "/v0/management/auth-files/models",
            "/v8/management/credentials/models",
            "",
            StatusCode::BAD_REQUEST,
            r#"{"error":"name is required"}"#,
        ),
        (
            Method::POST,
            "/v0/management/api-call",
            "/v8/management/requests/api-call",
            "{}",
            StatusCode::BAD_REQUEST,
            r#"{"error":"missing method"}"#,
        ),
        (
            Method::POST,
            "/v0/management/reset-quota",
            "/v8/management/routing/cooldown/reset",
            "{}",
            StatusCode::BAD_REQUEST,
            r#"{"error":"auth_index is required"}"#,
        ),
    ] {
        for path in [v0, v8] {
            api.send(keyed(method.clone(), path, body))
                .await
                .assert(status, want);
        }
    }
    let v0 = api.get("/v0/management/auth-files").await;
    let v8 = api.get("/v8/management/credentials").await;
    assert_eq!(v0.status, StatusCode::OK);
    assert_eq!(v8.status, StatusCode::OK);
    assert!(
        v0.body.starts_with(r#"{"files":[],"observed_at":""#),
        "{}",
        v0.body
    );
    assert!(
        v8.body.starts_with(r#"{"files":[],"observed_at":""#),
        "{}",
        v8.body
    );

    // The key works under both names, through either header.
    let mut request = request_from(LOCAL, Method::GET, "/v8/management/credentials", "");
    request
        .headers_mut()
        .insert("x-management-key", KEY.parse().unwrap());
    assert_eq!(api.send(request).await.status, StatusCode::OK);
}
