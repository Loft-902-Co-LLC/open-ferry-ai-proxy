// Ported from CLIProxyAPI oauth_session_proxy_test.go, native Claude/Codex cases.
// Plugin and Antigravity sessions are not served by open-ferry.

use super::*;
use axum::body::Body;
use axum::extract::{RawQuery, State};
use open_ferry_core::auth::{AuthStore, FileStore};
use open_ferry_core::executor::ProviderExecutor;
use tower::ServiceExt as _;

const START: &str = "/v8/management/oauth/auth-url";

/// Exercise the production handler with the POST wiring held for integration.
async fn start_request(api: &Api, method: Method, query: &str, body: &str) -> Answer {
    use http_body_util::BodyExt as _;
    let response = crate::oauth::test_auth_url(
        State(api.state.clone()),
        RawQuery((!query.is_empty()).then(|| query.to_owned())),
        method,
        Body::from(body.to_owned()),
    )
    .await;
    let (parts, body) = response.into_parts();
    Answer {
        status: parts.status,
        headers: parts.headers,
        body: String::from_utf8(body.collect().await.unwrap().to_bytes().to_vec()).unwrap(),
    }
}

async fn start_proxy(api: &Api, provider: &str, proxy: &str) -> String {
    let body = json!({"provider": provider, "proxy_url": proxy}).to_string();
    let response = api.post(START, &body).await;
    assert_eq!(response.status, StatusCode::OK);
    let value: Value = serde_json::from_str(&response.body).unwrap();
    assert!(value["status"] == "ok");
    assert!(!response.body.contains("proxy_url"));
    value["state"].as_str().unwrap().to_owned()
}

fn tokens(provider: &str, email: &str) -> Vec<u8> {
    if provider == "codex" {
        codex_tokens(&codex_id_token(Some(email), "plus"))
    } else {
        let body = CLAUDE_TOKENS.replace("claude-user@example.test", email);
        http_response(
            "200 OK",
            &[("Content-Type", "application/json")],
            body.as_bytes(),
        )
    }
}

fn provider_name(provider: &str) -> &str {
    if provider == "claude" {
        "anthropic"
    } else {
        provider
    }
}

#[tokio::test]
async fn session_proxy_validates_body_aliases_conflicts_and_query_refusal() {
    let api = Api::new();
    let cases = [
        ("missing provider", "", "{}", "provider is required"),
        ("provider type", "", r#"{"provider":1}"#, "invalid provider"),
        (
            "provider conflict",
            "provider=codex",
            r#"{"provider":"claude"}"#,
            "conflicting provider parameter",
        ),
        (
            "snake type",
            "provider=codex",
            r#"{"proxy_url":true}"#,
            "invalid proxy_url",
        ),
        (
            "kebab type",
            "provider=codex",
            r#"{"proxy-url":3}"#,
            "invalid proxy_url",
        ),
        (
            "proxy conflict",
            "provider=codex",
            r#"{"proxy_url":"http://a.test","proxy-url":"http://b.test"}"#,
            "invalid proxy_url",
        ),
        (
            "webui type",
            "provider=codex",
            r#"{"is_webui":1}"#,
            "invalid is_webui",
        ),
        (
            "webui text",
            "provider=codex",
            r#"{"is_webui":"maybe"}"#,
            "invalid is_webui",
        ),
        ("array", "provider=codex", "[]", "invalid request body"),
        ("null", "provider=codex", "null", "invalid request body"),
        (
            "trailing json",
            "provider=codex",
            "{} {}",
            "invalid request body",
        ),
        (
            "invalid json",
            "provider=codex",
            "{",
            "invalid request body",
        ),
    ];
    for (name, query, body, error) in cases {
        let response = start_request(&api, Method::POST, query, body).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{name}");
        let value: Value = serde_json::from_str(&response.body).unwrap();
        assert!(value["error"] == error, "{name}");
    }
    let invalid = [
        "://bad-url",
        "not-a-url",
        "ftp://127.0.0.1:21",
        "http://:8080",
        "http://127.0.0.1:99999",
        "http://127.0.0.1:0",
        "http://127.0.0.1:",
        "socks5://127.0.0.1",
        "socks5://127.0.0.1:1080",
        "socks5h://127.0.0.1:1080",
        "direct",
        "none",
        "http://proxy.test/path",
        "http://proxy.test/a/..",
        "http://proxy.test?route=1",
        "http://proxy.test#fragment",
    ];
    for (index, proxy) in invalid.iter().enumerate() {
        let body = json!({"provider":"codex", "proxy_url":proxy}).to_string();
        let response = start_request(&api, Method::POST, "", &body).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "case {index}");
        assert!(response.body == r#"{"error":"invalid proxy_url"}"#);
    }
    for method in [Method::GET, Method::POST] {
        for query in [
            "provider=codex&proxy_url",
            "provider=codex&proxy-url=",
            "provider=codex&proxy%5Furl=http%3A%2F%2Fproxy.test",
        ] {
            let response = start_request(&api, method.clone(), query, "").await;
            assert_eq!(response.status, StatusCode::BAD_REQUEST);
            assert!(
                response
                    .body
                    .contains("must be provided in the request body")
            );
        }
    }
    let response = start_request(&api, Method::GET, "provider=codex", "{}").await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(response.body.contains("request body requires POST"));

    for body in [
        r#"{"provider":"CODEX","proxy_url":null}"#,
        r#"{"provider":"codex","proxy_url":"   ","proxy-url":null}"#,
        r#"{"provider":"codex","proxy-url":" http://proxy.test:8080/ "}"#,
        r#"{"provider":"codex","proxy_url":"https://proxy.test","proxy-url":" https://proxy.test "}"#,
        r#"{"provider":"","proxy_url":"http://proxy.test:80"}"#,
    ] {
        let response = start_request(&api, Method::POST, "provider=codex", body).await;
        assert_eq!(response.status, StatusCode::OK);
        let value: Value = serde_json::from_str(&response.body).unwrap();
        assert!(
            api.state
                .oauth_sessions()
                .store()
                .cancel(value["state"].as_str().unwrap())
        );
    }
    for provider in ["plugin-sso", "antigravity", "kimi", "devin"] {
        let body = json!({"provider":provider}).to_string();
        let response = start_request(&api, Method::POST, "", &body).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND);
    }
    api.state.shutdown().await;
}

#[tokio::test]
async fn session_proxy_blank_body_uses_go_trim_space() {
    let api = Api::new();
    for method in [Method::GET, Method::POST] {
        for body in ["", " \t\r\n", "\u{000b}", "\u{00a0}", "\u{0085}\u{3000}"] {
            let response = start_request(&api, method.clone(), "provider=codex", body).await;
            assert_eq!(response.status, StatusCode::OK);
            let value: Value = serde_json::from_str(&response.body).unwrap();
            assert!(
                api.state
                    .oauth_sessions()
                    .store()
                    .cancel(value["state"].as_str().unwrap())
            );
        }
    }
    api.state.shutdown().await;
}

#[tokio::test]
async fn session_proxy_domain_and_channel_types_are_checked_before_dispatch() {
    let api = Api::new();
    for provider in [
        "claude",
        "codex",
        "plugin-sso",
        "antigravity",
        "kimi",
        "devin",
    ] {
        for (field, error) in [("domain", "invalid domain"), ("channel", "invalid channel")] {
            for invalid in [json!(false), json!(1), json!([]), json!({})] {
                let mut body = json!({"provider":provider});
                body[field] = invalid;
                let response = start_request(&api, Method::POST, "", &body.to_string()).await;
                assert_eq!(response.status, StatusCode::BAD_REQUEST);
                let value: Value = serde_json::from_str(&response.body).unwrap();
                assert!(value["error"] == error);
            }
        }
    }
    for provider in ["claude", "codex"] {
        for fields in [
            json!({"domain":null, "channel":null}),
            json!({"domain":" example.test "}),
            json!({"channel":" native-test "}),
            // As upstream, a non-null domain shadows the channel alias.
            json!({"domain":"", "channel":false}),
        ] {
            let response = start_request(
                &api,
                Method::POST,
                &format!("provider={provider}"),
                &fields.to_string(),
            )
            .await;
            assert_eq!(response.status, StatusCode::OK);
            let value: Value = serde_json::from_str(&response.body).unwrap();
            assert!(
                api.state
                    .oauth_sessions()
                    .store()
                    .cancel(value["state"].as_str().unwrap())
            );
        }
    }
    let response = start_request(
        &api,
        Method::POST,
        "provider=codex",
        r#"{"domain":null,"channel":false}"#,
    )
    .await;
    assert!(response.body == r#"{"error":"invalid channel"}"#);
    api.state.shutdown().await;
}

#[tokio::test]
async fn session_proxy_retains_bounded_body_and_json_depth_guards() {
    let api = Api::new();
    let body = " ".repeat(crate::bind::MAX_BODY + 1);
    let response = start_request(&api, Method::POST, "provider=codex", &body).await;
    assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE);
    let body = format!("{{\"extra\":{}0{}}}", "[".repeat(140), "]".repeat(140));
    let response = start_request(&api, Method::POST, "provider=codex", &body).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(api.state.oauth_sessions().store().count(), 0);
}

#[tokio::test]
async fn session_proxy_webui_body_defaults_and_query_precedence() {
    let api = Api::with(served_config(), None);
    for (query, webui, expected) in [
        ("", json!(true), true),
        ("", json!(" YES "), true),
        ("is_webui=", json!(true), true),
        ("is_webui=no", json!(true), false),
        ("is_webui=on", json!(false), true),
        ("", json!("off"), false),
    ] {
        let body = json!({"provider":"codex", "is_webui":webui}).to_string();
        let response = start_request(&api, Method::POST, query, &body).await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(
            api.state
                .oauth_sessions()
                .forwarder_addr(Provider::Codex)
                .is_some(),
            expected
        );
        let value: Value = serde_json::from_str(&response.body).unwrap();
        assert!(
            api.state
                .oauth_sessions()
                .store()
                .cancel(value["state"].as_str().unwrap())
        );
        // The lease owns the forwarder until the login observes cancellation.
        tokio::time::timeout(Duration::from_secs(5), async {
            while api.state.oauth_sessions().running() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    api.state.shutdown().await;
}

#[tokio::test]
async fn session_proxy_parallel_native_logins_persist_isolated_proxies_out_of_order() {
    for provider in ["claude", "codex"] {
        let dir = AuthDir::new();
        let unrelated = r#"{"type":"codex","email":"other@example.test","access_token":"other-test","refresh_token":"other-refresh-test"}"#;
        dir.write("unrelated.json", unrelated);
        let direct = Upstream::answering(tokens(provider, "must-not-be-called@example.test")).await;
        let a = Upstream::answering(tokens(provider, "proxy-a@example.test")).await;
        let b = Upstream::answering(tokens(provider, "proxy-b@example.test")).await;
        let global = Upstream::answering(tokens(provider, "global@example.test")).await;
        let mut config = dir.config();
        config.proxy_url.clone_from(&global.url);
        let api = Api::over_with(&dir, config, None);
        api.state.oauth_sessions().overrides().endpoints = Some(direct.url.clone());
        let before = api.state.config();
        let (first, second) = tokio::join!(
            start_proxy(&api, provider, &a.url),
            start_proxy(&api, provider, &b.url)
        );
        deliver(&api, &second, provider_name(provider), code("b-test"));
        completed(&api, &second).await;
        assert_eq!(b.requests().len(), 1);
        assert!(a.requests().is_empty());
        deliver(&api, &first, provider_name(provider), code("a-test"));
        completed(&api, &first).await;
        assert_eq!(a.requests().len(), 1);
        assert!(direct.requests().is_empty());
        assert!(Arc::ptr_eq(&before, &api.state.config()));
        assert!(api.state.config().proxy_url == global.url);
        assert!(global.requests().is_empty());
        assert!(std::fs::read_to_string(dir.path().join("unrelated.json")).unwrap() == unrelated);
        let records = dir.store.list().unwrap();
        for (email, proxy) in [
            ("proxy-a@example.test", &a.url),
            ("proxy-b@example.test", &b.url),
        ] {
            let record = records
                .iter()
                .find(|record| record.metadata_str("email") == Some(email))
                .unwrap();
            assert!(record.proxy_url == *proxy);
            assert!(record.metadata["proxy_url"] == *proxy);
            let runtime = api
                .manager
                .list()
                .into_iter()
                .find(|record| record.metadata_str("email") == Some(email))
                .unwrap();
            assert!(runtime.proxy_url == *proxy);
            assert!(runtime.metadata["proxy_url"] == *proxy);
        }
        api.state.shutdown().await;
    }
}

#[tokio::test]
async fn session_proxy_failures_never_fall_back_and_redact_userinfo() {
    for provider in ["claude", "codex"] {
        for failure_kind in ["auth", "dropped", "echo"] {
            let (logs, _capture) = Logs::capture();
            let dir = AuthDir::new();
            let direct =
                Upstream::answering(tokens(provider, "must-not-be-called@example.test")).await;
            let user = "oauth-proxy-user-test";
            let password = "oauth-proxy-pass-test";
            let response = if failure_kind == "auth" {
                http_response(
                    "407 Proxy Authentication Required",
                    &[],
                    b"proxy authentication refused",
                )
            } else if failure_kind == "echo" {
                http_response(
                    "400 Bad Request",
                    &[],
                    format!("{user}:{password}").as_bytes(),
                )
            } else {
                Vec::new()
            };
            let proxy = Upstream::answering(response).await;
            let mut url = url::Url::parse(&proxy.url).unwrap();
            url.set_username(user).unwrap();
            url.set_password(Some(password)).unwrap();
            let proxy_url = url.to_string();
            let global = Upstream::answering(tokens(provider, "global@example.test")).await;
            let mut config = dir.config();
            config.proxy_url.clone_from(&global.url);
            let api = Api::over_with(&dir, config, None);
            api.state.oauth_sessions().overrides().endpoints = Some(direct.url.clone());
            let state = start_proxy(&api, provider, &proxy_url).await;
            deliver(
                &api,
                &state,
                provider_name(provider),
                code("failure-code-test"),
            );
            let status = failure(&api, &state).await;
            assert!(!status.contains(user));
            assert!(!status.contains(password));
            let text = logs.text();
            assert!(!text.contains(user));
            assert!(!text.contains(password));
            assert!(direct.requests().is_empty());
            assert!(global.requests().is_empty());
            assert!(files(&dir).is_empty());
            api.state.shutdown().await;
        }
    }
}

#[tokio::test]
async fn session_proxy_uses_authenticated_transport_and_keeps_redirect_guard() {
    let dir = AuthDir::new();
    let direct = Upstream::answering(tokens("codex", "direct@example.test")).await;
    let direct_url = direct.url.clone();
    let proxy = Upstream::start(move |_, _| {
        http_response("307 Temporary Redirect", &[("Location", &direct_url)], b"")
    })
    .await;
    let mut url = url::Url::parse(&proxy.url).unwrap();
    url.set_username("oauth-user-test").unwrap();
    url.set_password(Some("oauth-pass-test")).unwrap();
    let api = login_api(&dir, "http://127.0.0.1:9");
    let state = start_proxy(&api, "codex", url.as_str()).await;
    deliver(&api, &state, "codex", code("redirect-code-test"));
    let _ = failure(&api, &state).await;
    let requests = proxy.requests();
    assert_eq!(requests.len(), 1);
    let expected = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("oauth-user-test:oauth-pass-test")
    );
    assert!(requests[0].lines().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("proxy-authorization") && value.trim() == expected
        })
    }));
    assert!(direct.requests().is_empty());
    api.state.shutdown().await;
}

#[tokio::test]
async fn session_proxy_missing_override_preserves_global_and_existing_account_settings() {
    for global in [false, true] {
        let dir = AuthDir::new();
        let direct = Upstream::answering(tokens("codex", "default@example.test")).await;
        let proxy = Upstream::answering(tokens("codex", "default@example.test")).await;
        let mut config = dir.config();
        if global {
            config.proxy_url.clone_from(&proxy.url);
        }
        let api = Api::over_with(&dir, config, None);
        api.state.oauth_sessions().overrides().endpoints = Some(direct.url.clone());
        let state = start_proxy(&api, "codex", "").await;
        deliver(&api, &state, "codex", code("default-code-test"));
        completed(&api, &state).await;
        assert_eq!(direct.requests().len(), usize::from(!global));
        assert_eq!(proxy.requests().len(), usize::from(global));
        let name = saved_file(&dir, "codex-");
        assert!(dir.read_json(&name).get("proxy_url").is_none());
        // A later default login retains the account setting, rather than
        // persisting the global transport choice or erasing an older proxy.
        let mut old = dir.read_json(&name);
        old["proxy_url"] = json!(proxy.url);
        old["custom_setting"] = json!("keep-test");
        dir.write(&name, &old.to_string());
        let state = start_proxy(&api, "codex", "").await;
        deliver(&api, &state, "codex", code("default-again-test"));
        completed(&api, &state).await;
        let saved = dir.read_json(&name);
        assert!(saved["proxy_url"] == proxy.url);
        assert!(saved["custom_setting"] == "keep-test");
        api.state.shutdown().await;
    }
}

#[tokio::test]
async fn session_proxy_explicit_override_replaces_only_the_account_proxy() {
    for provider in ["claude", "codex"] {
        let dir = AuthDir::new();
        let a = Upstream::answering(tokens(provider, "replace@example.test")).await;
        let b = Upstream::answering(tokens(provider, "replace@example.test")).await;
        let api = login_api(&dir, "http://127.0.0.1:9");
        let state = start_proxy(&api, provider, &a.url).await;
        deliver(&api, &state, provider_name(provider), code("first-test"));
        completed(&api, &state).await;
        let name = saved_file(&dir, &format!("{provider}-"));
        let mut old = dir.read_json(&name);
        old["custom_setting"] = json!("keep-test");
        dir.write(&name, &old.to_string());
        let state = start_proxy(&api, provider, &b.url).await;
        deliver(&api, &state, provider_name(provider), code("second-test"));
        completed(&api, &state).await;
        let saved = dir.read_json(&name);
        assert!(saved["proxy_url"] == b.url);
        assert!(saved["custom_setting"] == "keep-test");
        assert_eq!(files(&dir).len(), 1);
        assert_eq!(a.requests().len(), 1);
        assert_eq!(b.requests().len(), 1);
        api.state.shutdown().await;
    }
}

#[tokio::test]
async fn session_proxy_validation_errors_never_echo_proxy_credentials() {
    let (logs, _capture) = Logs::capture();
    let api = Api::new();
    let user = "invalid-user-test";
    let password = "invalid-password-test";
    let proxy = format!("http://{user}:{password}@127.0.0.1:0");
    for (query, body) in [
        (
            String::new(),
            json!({"provider":"codex", "proxy_url":proxy}).to_string(),
        ),
        (format!("provider=codex&proxy_url={proxy}"), String::new()),
    ] {
        let response = start_request(&api, Method::POST, &query, &body).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert!(!response.body.contains(user));
        assert!(!response.body.contains(password));
    }
    let text = logs.text();
    assert!(!text.contains(user));
    assert!(!text.contains(password));
    assert_eq!(api.state.oauth_sessions().store().count(), 0);
}

#[tokio::test]
async fn session_proxy_survives_restart_and_production_native_refresh() {
    for provider in ["claude", "codex"] {
        let dir = AuthDir::new();
        let direct = Upstream::answering(tokens(provider, "must-not-be-called@example.test")).await;
        let proxy = Upstream::start(move |n, _| {
            if n == 0 {
                tokens(provider, "restart@example.test")
            } else {
                http_response("200 OK", &[("Content-Type", "application/json")], br#"{"access_token":"refreshed-access-test","refresh_token":"refreshed-grant-test","expires_in":3600}"#)
            }
        }).await;
        let mut url = url::Url::parse(&proxy.url).unwrap();
        url.set_username("restart-user-test").unwrap();
        url.set_password(Some("restart-password-test")).unwrap();
        let proxy_url = url.to_string();
        let api = login_api(&dir, &direct.url);
        let state = start_proxy(&api, provider, &proxy_url).await;
        deliver(
            &api,
            &state,
            provider_name(provider),
            code("restart-code-test"),
        );
        completed(&api, &state).await;
        api.state.shutdown().await;
        let fresh = Arc::new(FileStore::new(dir.path()));
        let loaded = fresh.list().unwrap().remove(0);
        assert!(loaded.proxy_url == proxy_url);
        let before = loaded.metadata.clone();
        let executor: Box<dyn ProviderExecutor> = if provider == "codex" {
            Box::new(
                open_ferry_providers::codex::CodexExecutor::new("").with_oauth_endpoints(
                    open_ferry_providers::codex::oauth::Endpoints::with_base(&direct.url),
                ),
            )
        } else {
            Box::new(
                open_ferry_providers::claude::ClaudeExecutor::new("").with_oauth_endpoints(
                    open_ferry_providers::claude::oauth::Endpoints::with_base(&direct.url),
                ),
            )
        };
        let refreshed = executor
            .refresh(Arc::new(loaded))
            .await
            .unwrap_or_else(|_| panic!("native refresh failed"));
        assert!(refreshed.proxy_url == proxy_url);
        assert!(refreshed.metadata["proxy_url"] == proxy_url);
        if provider == "codex" {
            let patch = open_ferry_core::auth::codex_refresh::CredentialRefreshPatch::new(
                &before,
                &refreshed.metadata,
                true,
                false,
            );
            fresh.apply_codex_refresh(refreshed, patch).await.unwrap();
        } else {
            fresh.save(&refreshed).unwrap();
        }
        let reloaded = FileStore::new(dir.path()).list().unwrap().remove(0);
        assert!(reloaded.proxy_url == proxy_url);
        assert!(reloaded.metadata["access_token"] == "refreshed-access-test");
        assert_eq!(proxy.requests().len(), 2);
        assert!(direct.requests().is_empty());
    }
}

#[tokio::test]
async fn session_proxy_commit_claim_beats_cancel_and_poll_while_save_waits() {
    let dir = AuthDir::new();
    let proxy = Upstream::answering(tokens("codex", "commit@example.test")).await;
    let api = login_api(&dir, "http://127.0.0.1:9");
    let state = start_proxy(&api, "codex", &proxy.url).await;
    let claimed = Arc::new(tokio::sync::Notify::new());
    api.state.oauth_sessions().overrides().login_commit_claimed = Some(claimed.clone());
    let guard = api.state.credential_lock().lock().await;
    deliver(&api, &state, "codex", code("commit-code-test"));
    tokio::time::timeout(Duration::from_secs(5), claimed.notified())
        .await
        .unwrap();
    let response = api
        .call(
            Method::DELETE,
            &format!("/v8/management/oauth/session?state={state}"),
            "",
        )
        .await;
    assert!(response.expect(StatusCode::OK)["cancelled"] == false);
    assert!(
        api.get(&format!("/v8/management/oauth/status?state={state}"))
            .await
            .expect(StatusCode::OK)["status"]
            == "wait"
    );
    api.state
        .oauth_sessions()
        .store()
        .set_error(&state, "late failure-test");
    assert!(
        api.state
            .oauth_sessions()
            .store()
            .get(&state)
            .unwrap()
            .status
            .is_empty()
    );
    assert!(files(&dir).is_empty());
    drop(guard);
    completed(&api, &state).await;
    assert_eq!(files(&dir).len(), 1);
    api.state.shutdown().await;
}

#[tokio::test]
async fn session_proxy_claimed_saves_finalize_the_real_outcome_after_shutdown() {
    use open_ferry_core::auth::file_guard::FileGuard;
    for provider in ["claude", "codex"] {
        for outcome in ["saved", "sync-stopped", "save-failed"] {
            let dir = if outcome == "save-failed" {
                AuthDir::guarded(Some(FileGuard::new(|_| Err("test save refused".into()))))
            } else {
                AuthDir::new()
            };
            let proxy = Upstream::answering(tokens(provider, "shutdown@example.test")).await;
            let api = login_api(&dir, "http://127.0.0.1:9");
            let state = start_proxy(&api, provider, &proxy.url).await;
            let claimed = Arc::new(tokio::sync::Notify::new());
            api.state.oauth_sessions().overrides().login_commit_claimed = Some(claimed.clone());
            // Hold the real file lock before delivering the callback. The
            // notification proves the claim passed, without timing sleeps.
            let guard = api.state.credential_lock().lock().await;
            let store = api.state.oauth_sessions().store();
            let delivered = store
                .deliver(&state, provider_name(provider), code("shutdown-code-test"))
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), claimed.notified())
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), api.state.shutdown())
                .await
                .unwrap();
            assert_eq!(api.state.oauth_sessions().running(), 0);
            assert!(store.is_committing(&state));
            assert!(files(&dir).is_empty());
            assert!(
                api.get(&format!("/v8/management/oauth/status?state={state}"))
                    .await
                    .expect(StatusCode::OK)["status"]
                    == "wait"
            );
            assert!(!store.cancel(&state));
            if outcome == "sync-stopped" {
                api.sync.stop();
            }
            drop(guard);
            let ended = store.ended(&state, delivered, Duration::from_secs(5)).await;
            if outcome == "save-failed" {
                assert_eq!(
                    ended,
                    Some(Ended::Failed("Failed to save authentication tokens".into()))
                );
                assert!(files(&dir).is_empty());
            } else {
                assert_eq!(ended, Some(Ended::Completed));
                let records = dir.store.list().unwrap();
                assert_eq!(records.len(), 1);
                assert!(records[0].proxy_url == proxy.url);
                assert!(records[0].metadata["proxy_url"] == proxy.url);
                assert_eq!(api.sync.calls().len(), usize::from(outcome == "saved"));
            }
            assert!(!store.is_committing(&state));
            assert_eq!(proxy.requests().len(), 1);
        }
    }
}

#[tokio::test]
async fn session_proxy_cancel_before_exchange_saves_nothing() {
    let dir = AuthDir::new();
    let proxy = Upstream::answering(tokens("codex", "cancel@example.test")).await;
    let api = login_api(&dir, "http://127.0.0.1:9");
    let state = start_proxy(&api, "codex", &proxy.url).await;
    assert!(api.state.oauth_sessions().store().cancel(&state));
    assert!(
        api.state
            .oauth_sessions()
            .store()
            .deliver(&state, "codex", code("cancel-code-test"))
            .is_err()
    );
    api.state.shutdown().await;
    assert!(proxy.requests().is_empty());
    assert!(files(&dir).is_empty());
}

#[tokio::test]
async fn session_proxy_does_not_weaken_existing_siwc_bind_selection() {
    let api = Api::new();
    let response = start_request(
        &api,
        Method::POST,
        "provider=codex&mode=siwc-bind&name=test.json",
        r#"{"proxy_url":"http://proxy.test"}"#,
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(response.body.contains("not supported for SIWC bind"));
    assert_eq!(api.state.oauth_sessions().store().count(), 0);
}

#[tokio::test]
async fn session_proxy_dedicated_siwc_bind_keeps_ignoring_proxy_url() {
    let dir = AuthDir::new();
    let original = json!({
        "type":"codex", "access_token":"outer-access-test", "refresh_token":"outer-refresh-test",
        "siwc":{"device_id":"123e4567-e89b-42d3-a456-426614174000"}
    });
    dir.write("codex-bind-test.json", &original.to_string());
    let api = Api::over(&dir);
    api.manager.load().unwrap();
    for proxy in [
        json!(false),
        json!(null),
        json!("invalid"),
        json!("http://proxy.test:8080"),
    ] {
        let response = api
            .post(
                "/v8/management/oauth/siwc/bind",
                &json!({"name":"codex-bind-test.json", "proxy_url":proxy}).to_string(),
            )
            .await;
        let value = response.expect(StatusCode::OK);
        assert!(value["flow"] == "siwc_bind");
        assert!(
            api.state
                .oauth_sessions()
                .store()
                .cancel(value["state"].as_str().unwrap())
        );
    }
    api.state.shutdown().await;
    assert!(dir.read_json("codex-bind-test.json") == original);
}

#[tokio::test]
async fn session_proxy_post_handler_accepts_proposed_integration_wiring() {
    let api = Api::new();
    // The unchanged GET registration still works with the new handler signature.
    let state = start_login(&api, &format!("{START}?provider=codex")).await;
    assert!(api.state.oauth_sessions().store().cancel(&state));
    // The exact proposed POST wiring accepts the same handler.
    let router = axum::Router::new()
        .route(START, axum::routing::post(crate::oauth::test_auth_url))
        .with_state(api.state.clone());
    let response = router
        .oneshot(keyed(Method::POST, START, r#"{"provider":"codex"}"#))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    api.state.shutdown().await;
}

#[tokio::test]
async fn oauth_post_start_route_delivers_proxy_body() {
    let api = Api::new();
    for provider in ["claude", "codex"] {
        let invalid = api
            .post(
                START,
                &json!({"provider":provider,"proxy_url":false}).to_string(),
            )
            .await;
        assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
        assert!(invalid.body.contains("invalid proxy_url"));
        let state = start_proxy(&api, provider, "http://proxy.test:8080").await;
        assert!(api.state.oauth_sessions().store().cancel(&state));
    }
    api.state.shutdown().await;
}
