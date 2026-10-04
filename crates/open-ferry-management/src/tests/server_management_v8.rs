// Ported from CLIProxyAPI internal/api/server_management_v8_test.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The v8 management names.
//!
//! Deviations from upstream:
//! - `TestManagementV8RoutesShareAccessControl` requests the credential
//!   list under both names here; its config routes are requested in
//!   `config_read` (`management_v8_config_routes_share_access_control`),
//!   and its plugin routes aren't ported. Its Home mode case is dropped:
//!   Home mode isn't ported.
//! - `TestManagementV8IndependentContract` keeps here its legacy names and
//!   removed quota routes, which answer 404 under `/v8/management`, and its
//!   import checks, and checks that each ported route answers under both
//!   names. Its OAuth checks are in `oauth`
//!   (`management_v8_oauth_contract`), and its config reads and writes in
//!   `config_read` (`management_v8_independent_contract_config`), the
//!   writes answering 404 as this port never writes the config. Its route
//!   table check is dropped, the router having no list of routes to read,
//!   as are its plugin routes (not ported). The Vertex import answers `file
//!   required` only with a credential store, which upstream doesn't need,
//!   so the import checks run with one.
//! - `TestManagementV8PluginOperationMigratesConfiguration` is dropped: the
//!   plugin routes aren't ported, and this port never writes the config.

use http::{Method, StatusCode};

use super::{Api, AuthDir, KEY, LOCAL, keyed, keyed_config, request_from};

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

    // The import routes, sent no form. Imports need a credential store.
    let auth_dir = AuthDir::new();
    let api_with_store = Api::over(&auth_dir);
    for (path, status, want) in [
        (
            "/v8/management/oauth/import",
            StatusCode::BAD_REQUEST,
            r#"{"error":"provider is required"}"#,
        ),
        (
            "/v8/management/oauth/import?provider=codex",
            StatusCode::NOT_FOUND,
            r#"{"error":"provider_not_found"}"#,
        ),
        (
            "/v8/management/oauth/import?provider=vertex",
            StatusCode::BAD_REQUEST,
            r#"{"error":"file required"}"#,
        ),
        (
            "/v0/management/vertex/import",
            StatusCode::BAD_REQUEST,
            r#"{"error":"file required"}"#,
        ),
        (
            "/v8/management/oauth/providers/vertex/import",
            StatusCode::NOT_FOUND,
            "",
        ),
    ] {
        let answer = api_with_store.send(keyed(Method::POST, path, "")).await;
        assert_eq!(
            (answer.status, answer.body.as_str()),
            (status, want),
            "{path}"
        );
    }

    // The key works under both names, through either header.
    let mut request = request_from(LOCAL, Method::GET, "/v8/management/credentials", "");
    request
        .headers_mut()
        .insert("x-management-key", KEY.parse().unwrap());
    assert_eq!(api.send(request).await.status, StatusCode::OK);
}
