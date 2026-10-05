// Ported from CLIProxyAPI internal/api/server_test.go
// (TestCodexAlphaSearchForwardsRequest,
// TestCodexAlphaSearchSanitizesResponsesOnlyFields,
// TestCodexAlphaSearchCredentialPolicy,
// TestCodexAlphaSearchOptInAPIKeyUsesConfiguredEndpoint,
// TestCodexAlphaSearchOptInAPIKeyStripsCredentialPrefix,
// TestCodexAlphaSearchOptInAPIKeyResolvesModelAlias,
// TestCodexAlphaSearchOptInAPIKeyWithoutBaseURLFailsClosed,
// TestCodexAlphaSearchRecordsRequestLog) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Codex Alpha Search through the whole router: the client key, the
//! credential manager, the Codex executor, and a mock Codex on 127.0.0.1
//! standing in for ChatGPT's Codex API and for API keys' base URLs.
//!
//! Deviations from upstream:
//! - Upstream hands the request to a capturing executor; these send it
//!   through the Codex executor to the mock, so they check what goes on the
//!   wire, and an API key's base URL is the mock's.
//! - ForwardsRequest checks that no `Originator` goes out, where upstream
//!   checks for `codex_cli_rs`.
//! - RecordsRequestLog goes through the router with a request logger writing
//!   to a directory of its own, and reads the log file that is written,
//!   where upstream reads the `API_REQUEST` and `API_RESPONSE` the handler
//!   left in gin's context. The upstream URL is the mock's, and the log is
//!   also checked for the credential's token and the client key.
//! - CredentialPolicy counts on the API key sorting first, so round robin
//!   would pick it without the policy, where upstream sets a selector that
//!   picks API keys first: custom selectors aren't ported.
//! - TestRewriteCodexAlphaSearchModel is in the core manager's
//!   `alpha_search` module.
//! - Dropped, with what they cover:
//!   - TestCodexAlphaSearchUsesPluginProviderTargetModel,
//!     TestCodexAlphaSearchFallsBackWhenPluginDoesNotHandleRoute and
//!     TestCodexAlphaSearchRejectsUnsupportedPluginRouteTarget: the plugin
//!     model router isn't ported.
//!   - TestCodexAlphaSearchPassesGinContextToAuthSelection: custom selectors
//!     and gin's context aren't ported.
//!   - TestCodexAlphaSearchUsesRequestIDForSessionAffinity: session affinity
//!     isn't ported.
//!   - TestAuditHomeCodexSearchBusyReturnsTrustedRetryAfter,
//!     TestAuditHomeCodexSearchBodyCloseBeforeRelease and the four
//!     TestHomeCodexAlphaSearch* tests: Home isn't ported.

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::body::Body;
use axum::http::Uri;
use bytes::Bytes;
use http::{HeaderMap, Request, StatusCode, header};
use http_body_util::BodyExt;
use open_ferry_core::auth::Auth;
use open_ferry_core::auth::classification::ATTRIBUTE_CODEX_ALPHA_SEARCH;
use open_ferry_core::config::Config;
use open_ferry_core::manager::{ApiKeyEntry, Manager, ModelAlias, Settings};
use open_ferry_core::models::ModelInfo;
use open_ferry_core::observe::Observability;
use open_ferry_core::observe::request_log::{CPA_TRACE_ID_HEADER, RequestLogger};
use open_ferry_core::registry::ModelRegistry;
use open_ferry_providers::codex::CodexExecutor;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

use crate::config::ServerConfig;
use crate::router;
use crate::state::AppState;
use crate::testing::{FakeCatalog, FakeDispatcher, TempDir, state};

/// What the mock answers every request with.
const RESULTS: &str = r#"{"results":[{"url":"https://example.com"}]}"#;

/// One request the mock received.
#[derive(Clone, Debug)]
struct Seen {
    path: String,
    headers: HeaderMap,
    body: Bytes,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|value| value.to_str().unwrap())
    }

    fn json(&self) -> Map<String, Value> {
        serde_json::from_slice(&self.body).unwrap()
    }
}

/// A mock Codex answering every request with `status`, `content_type`
/// (when it isn't empty) and `body`.
struct Mock {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Mock {
    async fn start(status: u16, content_type: &'static str, body: &'static str) -> Self {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let app = Router::new().fallback(move |uri: Uri, headers: HeaderMap, received: Bytes| {
            let recorder = Arc::clone(&recorder);
            async move {
                recorder
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(Seen {
                        path: uri.path().to_owned(),
                        headers,
                        body: received,
                    });
                let mut response = axum::response::Response::builder().status(status);
                if !content_type.is_empty() {
                    response = response.header(header::CONTENT_TYPE, content_type);
                }
                response.body(Body::from(body)).unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { url, seen }
    }

    async fn ok() -> Self {
        Self::start(200, "application/json", RESULTS).await
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn only(&self) -> Seen {
        let requests = self.requests();
        assert_eq!(requests.len(), 1, "requests: {requests:?}");
        requests.into_iter().next().unwrap()
    }
}

/// The state with client key `test-key`, over a manager with `settings`,
/// `credentials` and a Codex executor whose ChatGPT base is `chatgpt`.
/// `models` registers each credential's models. The manager comes too.
fn build(
    settings: Settings,
    chatgpt: &str,
    credentials: Vec<Auth>,
    models: &[(&str, &str)],
) -> (AppState, Arc<Manager>) {
    let registry = Arc::new(ModelRegistry::new());
    let manager = Arc::new(Manager::new(settings, registry.clone(), None));
    manager.register_executor(Arc::new(
        CodexExecutor::new("direct").with_base_url(format!("{chatgpt}/backend-api/codex")),
    ));
    for credential in credentials {
        manager.register_unsaved(credential).unwrap();
    }
    for (id, model) in models {
        registry.register_client(
            id,
            "codex",
            &[ModelInfo {
                id: (*model).to_owned(),
                ..ModelInfo::default()
            }],
        );
    }
    let config = ServerConfig {
        api_keys: vec!["test-key".into()],
        ..ServerConfig::default()
    };
    let state = AppState::new(config, manager.clone(), Arc::new(FakeCatalog::new()));
    (state, manager)
}

/// The router over [`build`]'s state.
fn proxy(
    settings: Settings,
    chatgpt: &str,
    credentials: Vec<Auth>,
    models: &[(&str, &str)],
) -> Router {
    router(build(settings, chatgpt, credentials, models).0)
}

/// A Codex sign-in with `metadata`.
fn oauth(id: &str, metadata: Value) -> Auth {
    let Value::Object(metadata) = metadata else {
        panic!("metadata isn't an object");
    };
    Auth {
        id: id.into(),
        provider: "codex".into(),
        metadata,
        ..Auth::default()
    }
}

/// A Codex API key with `attributes` besides its key.
fn api_key(id: &str, attributes: &[(&str, &str)]) -> Auth {
    let mut auth = Auth {
        id: id.into(),
        provider: "codex".into(),
        ..Auth::default()
    };
    auth.attributes
        .insert("api_key".into(), "codex-alpha-key".into());
    for (key, value) in attributes {
        auth.attributes
            .insert((*key).to_owned(), (*value).to_owned());
    }
    auth
}

/// An API key that opted in to Alpha Search, with base URL `base_url`.
fn opted_in(id: &str, base_url: &str) -> Auth {
    api_key(
        id,
        &[
            (ATTRIBUTE_CODEX_ALPHA_SEARCH, "true"),
            ("base_url", base_url),
        ],
    )
}

/// A search on `path` with the client key and `headers`.
fn search(path: &str, body: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut request = Request::post(path)
        .header(header::AUTHORIZATION, "Bearer test-key")
        .header(header::CONTENT_TYPE, "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.body(Body::from(body.to_owned())).unwrap()
}

/// The status, headers and body of `request`'s response.
async fn send(app: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
    let response = app.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (
        parts.status,
        parts.headers,
        String::from_utf8(bytes.to_vec()).unwrap(),
    )
}

fn content_type(headers: &HeaderMap) -> &str {
    headers
        .get(header::CONTENT_TYPE)
        .map_or("", |value| value.to_str().unwrap())
}

/// Checks that `stamp` is a `yyyymmddHHMMSS` time, as Go's
/// `time.Parse("20060102150405", ..)` takes it.
fn assert_timestamp(stamp: &str) {
    assert!(
        stamp.len() == 14 && stamp.bytes().all(|b| b.is_ascii_digit()),
        "trace timestamp = {stamp:?}"
    );
    let part = |range: std::ops::Range<usize>| stamp[range].parse::<u32>().unwrap();
    assert!((1..=12).contains(&part(4..6)), "month of {stamp:?}");
    assert!((1..=31).contains(&part(6..8)), "day of {stamp:?}");
    assert!(part(8..10) < 24, "hour of {stamp:?}");
    assert!(part(10..12) < 60, "minute of {stamp:?}");
    assert!(part(12..14) < 60, "second of {stamp:?}");
}

/// The part of `log` from the line `title` to the next section.
fn section<'a>(log: &'a str, title: &str) -> &'a str {
    let start = log
        .find(title)
        .unwrap_or_else(|| panic!("no {title}: {log}"));
    let rest = &log[start..];
    let end = rest[title.len()..]
        .find("\n=== ")
        .map_or(rest.len(), |end| title.len() + end + 1);
    &rest[..end]
}

// Ports TestCodexAlphaSearchForwardsRequest.
#[tokio::test]
async fn codex_alpha_search_forwards_request() {
    let mock = Mock::ok().await;
    let credential = oauth(
        "codex-auth",
        json!({"access_token": "codex-token", "account_id": "account-123"}),
    );
    let (state, manager) = build(Settings::default(), &mock.url, vec![credential], &[]);
    let app = router(state);
    let request = search(
        "/v1/alpha/search",
        r#"{"query":"GPT-5.6"}"#,
        &[("Session_id", "session-123")],
    );
    let (status, headers, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, RESULTS);
    assert_eq!(content_type(&headers), "application/json");
    // The trace ID is `<timestamp>-<credential index>-<request ID>`.
    let index = manager.get("codex-auth").unwrap().index.clone();
    assert!(!index.is_empty());
    let trace = headers.get(CPA_TRACE_ID_HEADER).unwrap().to_str().unwrap();
    let mut parts = trace.splitn(3, '-');
    let (stamp, trace_index, request_id) = (
        parts.next().unwrap(),
        parts.next().unwrap(),
        parts.next().unwrap(),
    );
    assert_eq!(trace_index, index, "{trace}");
    assert!(uuid::Uuid::parse_str(request_id).is_ok(), "{trace}");
    assert_timestamp(stamp);

    let upstream = mock.only();
    assert_eq!(upstream.path, "/backend-api/codex/alpha/search");
    assert_eq!(upstream.body, r#"{"query":"GPT-5.6"}"#);
    assert_eq!(upstream.header("authorization"), Some("Bearer codex-token"));
    assert_eq!(upstream.header("chatgpt-account-id"), Some("account-123"));
    assert_eq!(upstream.header("session_id"), Some("session-123"));
    assert_eq!(upstream.header("content-type"), Some("application/json"));
    assert_eq!(upstream.header("accept"), Some("application/json"));
    // Not upstream's: no client identity is made up.
    assert_eq!(upstream.header("originator"), None);
    assert!(
        upstream
            .header("user-agent")
            .unwrap()
            .starts_with("open-ferry/"),
        "{:?}",
        upstream.header("user-agent")
    );
}

// Ports TestCodexAlphaSearchRecordsRequestLog.
#[tokio::test]
async fn codex_alpha_search_records_request_log() {
    let mock = Mock::ok().await;
    let credential = oauth(
        "codex-auth",
        json!({"access_token": "codex-token", "account_id": "account-123"}),
    );
    let dir = TempDir::new();
    let mut config = Config::default();
    config.request_log = true;
    config.error_logs_max_files = 10;
    let logger = RequestLogger::new(&config, dir.path(), Path::new(""));
    let (state, _) = build(Settings::default(), &mock.url, vec![credential], &[]);
    let app = router(state.with_observability(Observability {
        log_dir: Some(dir.path().to_path_buf()),
        request_log: logger.clone(),
        ..Observability::default()
    }));

    let request = search("/v1/alpha/search", r#"{"query":"GPT-5.6"}"#, &[]);
    let (status, _, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    logger.flush();
    let files: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    let log = fs::read_to_string(&files[0]).unwrap();

    let upstream = section(&log, "=== API REQUEST 1 ===");
    let url = format!("{}/backend-api/codex/alpha/search", mock.url);
    assert!(upstream.contains(&url), "missing upstream URL: {upstream}");
    assert!(
        upstream.contains(r#"{"query":"GPT-5.6"}"#),
        "missing body: {upstream}"
    );
    let answer = section(&log, "=== API RESPONSE 1 ===");
    assert!(answer.contains(RESULTS), "missing body: {answer}");
    // Not upstream's: no secret is written to the file.
    for secret in ["codex-token", "test-key"] {
        assert!(!log.contains(secret), "{secret} leaked: {log}");
    }
}

// Ports TestCodexAlphaSearchSanitizesResponsesOnlyFields.
#[tokio::test]
async fn codex_alpha_search_sanitizes_responses_only_fields() {
    let mock = Mock::ok().await;
    let credential = oauth("codex-auth", json!({"access_token": "codex-token"}));
    let app = proxy(
        Settings::default(),
        &mock.url,
        vec![credential],
        &[("codex-auth", "gpt-5.6-sol")],
    );
    let payload = r#"{"id":"session-123","model":"gpt-5.6-sol","commands":{"search_query":[{"q":"golang channels"}]},"prompt_cache_key":"cache-123","prompt_cache_retention":"24h"}"#;
    for path in ["/v1/alpha/search", "/backend-api/codex/alpha/search"] {
        let (status, _, body) = send(&app, search(path, payload, &[])).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        let upstream = mock.requests().pop().unwrap().json();
        for field in ["prompt_cache_key", "prompt_cache_retention"] {
            assert!(!upstream.contains_key(field), "{path}: {upstream:?}");
        }
        for field in ["id", "model", "commands"] {
            assert!(upstream.contains_key(field), "{path}: {upstream:?}");
        }
    }
    assert_eq!(mock.requests().len(), 2);
}

// Ports TestCodexAlphaSearchCredentialPolicy.
#[tokio::test]
async fn codex_alpha_search_credential_policy() {
    let ordinary = || api_key("codex-api-key", &[]);

    // Mixed credentials.
    let mock = Mock::ok().await;
    let credentials = vec![
        ordinary(),
        oauth("codex-oauth", json!({"access_token": "codex-token"})),
    ];
    let app = proxy(Settings::default(), &mock.url, credentials, &[]);
    let request = search("/v1/alpha/search", r#"{"query":"GPT-5.6"}"#, &[]);
    let (status, _, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        mock.only().header("authorization"),
        Some("Bearer codex-token")
    );

    // An ordinary API key only.
    let mock = Mock::ok().await;
    let app = proxy(Settings::default(), &mock.url, vec![ordinary()], &[]);
    let request = search("/v1/alpha/search", r#"{"query":"GPT-5.6"}"#, &[]);
    let (status, headers, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body, r#"{"error":"auth_not_found: no auth available"}"#);
    assert_eq!(content_type(&headers), "application/json; charset=utf-8");
    assert!(mock.requests().is_empty());
}

// Ports TestCodexAlphaSearchOptInAPIKeyUsesConfiguredEndpoint.
#[tokio::test]
async fn codex_alpha_search_opt_in_api_key_uses_configured_endpoint() {
    let chatgpt = Mock::ok().await;
    let endpoint = Mock::ok().await;
    let credential = opted_in("codex-alpha-api-key", &format!("{}/v1/", endpoint.url));
    let app = proxy(Settings::default(), &chatgpt.url, vec![credential], &[]);
    let payload = r#"{"query":"golang","prompt_cache_key":"cache","prompt_cache_retention":"24h"}"#;
    let (status, _, body) = send(&app, search("/v1/alpha/search", payload, &[])).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let upstream = endpoint.only();
    assert_eq!(upstream.path, "/v1/alpha/search");
    assert_eq!(
        upstream.header("authorization"),
        Some("Bearer codex-alpha-key")
    );
    let sent = upstream.json();
    for field in ["prompt_cache_key", "prompt_cache_retention"] {
        assert!(!sent.contains_key(field), "{sent:?}");
    }
    assert!(chatgpt.requests().is_empty());
}

// Ports TestCodexAlphaSearchOptInAPIKeyStripsCredentialPrefix.
#[tokio::test]
async fn codex_alpha_search_opt_in_api_key_strips_credential_prefix() {
    let endpoint = Mock::ok().await;
    let mut credential = opted_in(
        "codex-alpha-api-key-prefix",
        &format!("{}/v1", endpoint.url),
    );
    credential.prefix = "vendor".into();
    let app = proxy(
        Settings::default(),
        &endpoint.url,
        vec![credential],
        &[("codex-alpha-api-key-prefix", "vendor/gpt-5.6-sol")],
    );
    let payload = r#"{"id":"00000000-0000-4000-8000-000000000003","model":"vendor/gpt-5.6-sol","commands":{"search_query":[{"q":"Go programming language official website"}]}}"#;
    let (status, _, body) = send(&app, search("/v1/alpha/search", payload, &[])).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let upstream = endpoint.only();
    assert_eq!(upstream.path, "/v1/alpha/search");
    assert_eq!(upstream.json()["model"], "gpt-5.6-sol");
}

// Ports TestCodexAlphaSearchOptInAPIKeyResolvesModelAlias.
#[tokio::test]
async fn codex_alpha_search_opt_in_api_key_resolves_model_alias() {
    let endpoint = Mock::ok().await;
    let base_url = format!("{}/v1", endpoint.url);
    let settings = Settings {
        api_keys: [(
            "codex".to_owned(),
            vec![ApiKeyEntry {
                api_key: "codex-alpha-key".into(),
                prefix: "vendor".into(),
                base_url: base_url.clone(),
                models: vec![ModelAlias {
                    name: "gpt-5.6-sol".into(),
                    alias: "sol-alias".into(),
                    force_mapping: false,
                }],
                ..ApiKeyEntry::default()
            }],
        )]
        .into(),
        ..Settings::default()
    };
    let mut credential = opted_in("codex-alpha-api-key-alias", &base_url);
    credential.prefix = "vendor".into();
    let app = proxy(
        settings,
        &endpoint.url,
        vec![credential],
        &[("codex-alpha-api-key-alias", "vendor/sol-alias")],
    );
    let payload = r#"{"model":"vendor/sol-alias","commands":{"search_query":[{"q":"golang"}]}}"#;
    let (status, _, body) = send(&app, search("/v1/alpha/search", payload, &[])).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(endpoint.only().json()["model"], "gpt-5.6-sol");
}

// Ports TestCodexAlphaSearchOptInAPIKeyWithoutBaseURLFailsClosed.
#[tokio::test]
async fn codex_alpha_search_opt_in_api_key_without_base_url_fails_closed() {
    let mock = Mock::ok().await;
    let credential = api_key(
        "codex-alpha-api-key",
        &[(ATTRIBUTE_CODEX_ALPHA_SEARCH, "true")],
    );
    let app = proxy(Settings::default(), &mock.url, vec![credential], &[]);
    let request = search("/v1/alpha/search", r#"{"query":"GPT-5.6"}"#, &[]);
    let (status, _, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(
        body,
        r#"{"error":"Codex Alpha Search API key base URL unavailable"}"#
    );
    assert!(mock.requests().is_empty());
}

// Not upstream's: the route model is the payload's `model` as Go's
// `json.Unmarshal` reads it into upstream's routing struct. It reads the
// members in order, repeated keys included, takes any case of `model`, and
// keeps the last string; `null` or another value leaves the model as it
// was, and no other letters fold to those of `model`. Go 1.26.4 reads these
// payloads' models as "missing", "allowed", "allowed", "allowed",
// "missing", "" and "".
#[tokio::test]
async fn codex_alpha_search_routes_on_the_model_go_reads() {
    let mock = Mock::ok().await;
    let credential = oauth("codex-auth", json!({"access_token": "codex-token"}));
    let app = proxy(
        Settings::default(),
        &mock.url,
        vec![credential],
        &[("codex-auth", "allowed")],
    );
    let mut sent = 0;
    for (payload, allowed) in [
        (
            r#"{"model":"missing","MODEL":"allowed","model":"missing"}"#,
            false,
        ),
        (
            r#"{"model":"missing","MODEL":"allowed","model":null}"#,
            true,
        ),
        (r#"{"model":"missing","MODEL":"allowed","model":7}"#, true),
        (r#"{"MoDeL":"allowed"}"#, true),
        (r#"{"model":"allowed","Model":"missing"}"#, false),
        ("{\"\u{ff4d}odel\":\"missing\"}", true),
        ("{\"\u{1d0d}odel\":\"missing\"}", true),
    ] {
        let (status, _, body) = send(&app, search("/v1/alpha/search", payload, &[])).await;
        if allowed {
            sent += 1;
            assert_eq!(status, StatusCode::OK, "{payload}: {body}");
            assert_eq!(mock.requests().pop().unwrap().body, payload);
        } else {
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{payload}: {body}");
            assert_eq!(body, r#"{"error":"auth_not_found: no auth available"}"#);
        }
        assert_eq!(mock.requests().len(), sent, "{payload}");
    }
}

// Not upstream's: a base URL with an ASCII control character fails before
// anything is sent, as Go's `url.Parse` fails it, with a 502 and Go's
// message, but without the URL Go quotes (Go 1.26.4 answers `parse
// "<url>": net/url: invalid control character in URL`), which may hold a
// secret.
#[tokio::test]
async fn codex_alpha_search_refuses_a_base_url_with_a_control_character() {
    let chatgpt = Mock::ok().await;
    let endpoint = Mock::ok().await;
    for base_url in [
        format!("{}/v1\n/x?key=secret", endpoint.url),
        format!("{}/v1\u{7f}", endpoint.url),
    ] {
        let credential = opted_in("codex-alpha-api-key", &base_url);
        let app = proxy(Settings::default(), &chatgpt.url, vec![credential], &[]);
        let request = search("/v1/alpha/search", r#"{"query":"golang"}"#, &[]);
        let (status, headers, body) = send(&app, request).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert_eq!(
            body,
            r#"{"error":"net/url: invalid control character in URL"}"#
        );
        assert_eq!(content_type(&headers), "application/json; charset=utf-8");
        assert!(headers.get(header::RETRY_AFTER).is_none());
    }
    assert!(endpoint.requests().is_empty());
    assert!(chatgpt.requests().is_empty());
}

// Not upstream's: pins a deviation. A base URL is read as a WHATWG URL, so
// its percent-encoded dot segments are resolved, where Go 1.26.4 sends
// `/v1/%2e%2e/alpha/search` as written.
#[tokio::test]
async fn codex_alpha_search_resolves_a_base_urls_dot_segments() {
    let chatgpt = Mock::ok().await;
    let endpoint = Mock::ok().await;
    let credential = opted_in(
        "codex-alpha-api-key",
        &format!("{}/v1/%2e%2e", endpoint.url),
    );
    let app = proxy(Settings::default(), &chatgpt.url, vec![credential], &[]);
    let request = search("/v1/alpha/search", r#"{"query":"golang"}"#, &[]);
    let (status, _, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(endpoint.only().path, "/alpha/search");
    assert!(chatgpt.requests().is_empty());
}

// Not upstream's: both routes need a client key, and a refused one never
// reaches Codex.
#[tokio::test]
async fn codex_alpha_search_requires_a_client_key() {
    let mock = Mock::ok().await;
    let credential = oauth("codex-auth", json!({"access_token": "codex-token"}));
    let app = proxy(Settings::default(), &mock.url, vec![credential], &[]);
    for (sent, path) in ["/v1/alpha/search", "/backend-api/codex/alpha/search"]
        .into_iter()
        .enumerate()
    {
        let request = |key: Option<&str>| {
            let mut request = Request::post(path);
            if let Some(key) = key {
                request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
            }
            request.body(Body::from(r#"{"query":"x"}"#)).unwrap()
        };
        let (status, _, body) = send(&app, request(None)).await;
        assert_eq!(
            (status, body.as_str()),
            (StatusCode::UNAUTHORIZED, r#"{"error":"Missing API key"}"#),
            "{path}"
        );
        let (status, _, body) = send(&app, request(Some("wrong-key"))).await;
        assert_eq!(
            (status, body.as_str()),
            (StatusCode::UNAUTHORIZED, r#"{"error":"Invalid API key"}"#),
            "{path}"
        );
        assert_eq!(mock.requests().len(), sent, "{path}");

        let (status, _, body) = send(&app, request(Some("test-key"))).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        // The client's key stays here.
        let upstream = mock.requests().pop().unwrap();
        assert_eq!(upstream.header("authorization"), Some("Bearer codex-token"));
    }
    let (status, _, _) = send(
        &app,
        Request::get("/v1/alpha/search")
            .header(header::AUTHORIZATION, "Bearer test-key")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// Not upstream's: only the client's own `Version`, `User-Agent`,
// `Session_id` and `X-Client-Request-Id` go along, trimmed; nothing else it
// sends does, `Originator` included.
#[tokio::test]
async fn codex_alpha_search_sends_only_the_clients_own_headers() {
    let mock = Mock::ok().await;
    let credential = oauth("codex-auth", json!({"access_token": "codex-token"}));
    let app = proxy(Settings::default(), &mock.url, vec![credential], &[]);
    let request = search(
        "/backend-api/codex/alpha/search",
        r#"{"query":"x"}"#,
        &[
            ("User-Agent", " my-client/1.0 "),
            ("Version", "0.99.0"),
            ("X-Client-Request-Id", "request-1"),
            ("Session_id", "  "),
            ("Originator", "codex_cli_rs"),
            ("Chatgpt-Account-Id", "someone-else"),
            ("X-Custom", "nope"),
        ],
    );
    let (status, _, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let upstream = mock.only();
    assert_eq!(upstream.header("user-agent"), Some("my-client/1.0"));
    assert_eq!(upstream.header("version"), Some("0.99.0"));
    assert_eq!(upstream.header("x-client-request-id"), Some("request-1"));
    for name in ["session_id", "originator", "chatgpt-account-id", "x-custom"] {
        assert_eq!(upstream.header(name), None, "{name}");
    }
}

// Not upstream's: Codex's status, `Content-Type` and body come back as they
// are, and a missing `Content-Type` stays missing.
#[tokio::test]
async fn codex_alpha_search_copies_the_answer() {
    let mock = Mock::start(429, "text/plain", "slow down").await;
    let credential = oauth("codex-auth", json!({"access_token": "codex-token"}));
    let app = proxy(Settings::default(), &mock.url, vec![credential], &[]);
    let (status, headers, body) = send(&app, search("/v1/alpha/search", "{}", &[])).await;
    assert_eq!(
        (status, content_type(&headers), body.as_str()),
        (StatusCode::TOO_MANY_REQUESTS, "text/plain", "slow down")
    );

    let mock = Mock::start(201, "", "plain").await;
    let credential = oauth("codex-auth", json!({"access_token": "codex-token"}));
    let app = proxy(Settings::default(), &mock.url, vec![credential], &[]);
    let (status, headers, body) = send(&app, search("/v1/alpha/search", "{}", &[])).await;
    assert_eq!((status, body.as_str()), (StatusCode::CREATED, "plain"));
    assert!(headers.get(header::CONTENT_TYPE).is_none(), "{headers:?}");
}

// Not upstream's: an unreachable Codex is a 502, and an answer that breaks
// off is a 502 that says so.
#[tokio::test]
async fn codex_alpha_search_reports_failed_calls() {
    // Bound but never listening, so a connection is refused, and the port
    // stays ours: no other test's server can be given it.
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let closed = format!("http://{}", socket.local_addr().unwrap());
    let credential = oauth("codex-auth", json!({"access_token": "codex-token"}));
    let app = proxy(Settings::default(), &closed, vec![credential], &[]);
    let (status, headers, body) = send(&app, search("/v1/alpha/search", "{}", &[])).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(content_type(&headers), "application/json; charset=utf-8");
    assert!(body.starts_with(r#"{"error":"#), "{body}");
    assert!(!body.contains("codex-token"), "{body}");

    // A server that promises more than it sends.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let broken = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0; 64 << 10];
        let _ = stream.read(&mut buffer).await;
        let head = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{\"res";
        stream.write_all(head.as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
    });
    let credential = oauth("codex-auth", json!({"access_token": "codex-token"}));
    let app = proxy(Settings::default(), &broken, vec![credential], &[]);
    let (status, _, body) = send(&app, search("/v1/alpha/search", "{}", &[])).await;
    assert_eq!(
        (status, body.as_str()),
        (
            StatusCode::BAD_GATEWAY,
            r#"{"error":"Failed to read Codex search response"}"#
        )
    );
}

// Not upstream's: a payload over the limit is refused before a credential
// is picked.
#[tokio::test]
async fn codex_alpha_search_refuses_a_payload_over_the_limit() {
    let mock = Mock::ok().await;
    let registry = Arc::new(ModelRegistry::new());
    let manager = Arc::new(Manager::new(Settings::default(), registry, None));
    manager.register_executor(Arc::new(
        CodexExecutor::new("direct").with_base_url(format!("{}/backend-api/codex", mock.url)),
    ));
    manager
        .register_unsaved(oauth("codex-auth", json!({"access_token": "t"})))
        .unwrap();
    let config = ServerConfig {
        api_keys: vec!["test-key".into()],
        body_limit: 16,
        ..ServerConfig::default()
    };
    let app = router(AppState::new(config, manager, Arc::new(FakeCatalog::new())));
    let request = search("/v1/alpha/search", r#"{"query":"far too long"}"#, &[]);
    let (status, _, _) = send(&app, request).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(mock.requests().is_empty());
}

// Not upstream's: a dispatcher without Codex's credential manager says so
// (upstream's nil auth manager).
#[tokio::test]
async fn codex_alpha_search_needs_the_credential_manager() {
    let dispatcher = FakeDispatcher::new([]);
    let config = ServerConfig {
        api_keys: vec!["test-key".into()],
        ..ServerConfig::default()
    };
    let app = router(state(config, FakeCatalog::new(), &dispatcher));
    let (status, _, body) = send(&app, search("/v1/alpha/search", "{}", &[])).await;
    assert_eq!(
        (status, body.as_str()),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"error":"Codex auth manager unavailable"}"#
        )
    );
    assert!(dispatcher.calls().is_empty());
}
