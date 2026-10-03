// Ported from CLIProxyAPI internal/api/handlers/management/api_tools_test.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! `POST /v0/management/api-call`, against servers on 127.0.0.1.
//!
//! Deviations from upstream:
//! - The `TestAPICallTransport*` tests are covered by the tests of
//!   `crate::proxy`, which pick the same routes without building a client.
//! - `TestAPICallResolvesMetaToken`, `TestResolveMetaToken*`,
//!   `TestAPICallRefreshesExpiredXAIOAuthToken*`,
//!   `TestAPICallConcurrentXAITokenRefresh` and
//!   `TestAPICallRefreshesWhenOnlyIDTokenPresentWithRefreshToken` are
//!   dropped: this port never refreshes or mints a token here.
//! - `TestAPICallUsesXAITokenStorageWithoutRefresh` and
//!   `TestAPICallPrioritizesStorageAccessTokenOverMetadataIDToken` are
//!   dropped: the core's credentials hold no typed token storage.
//! - `TestAPICallRejectsWhenOnlyIDTokenPresentWithoutRefreshToken` is
//!   changed: with the plain lookup, an xAI credential holding only an
//!   `id_token` sends it, where upstream answers `auth token not found`.
//! - `TestAPICallRejectsUnresolvedTokenPlaceholder` expects the one answer
//!   this port gives for an unmatched index, `auth credential not found for
//!   auth_index`; upstream accepts that or `auth token not found`.
//! - `TestAPICallEndToEndWithAuthFilesList` lists and calls through the
//!   management router, key and all, rather than a bare gin engine.
//! - The tests from `api_call_answers_as_upstream_for_bad_requests` on are
//!   this port's: what upstream's tests leave unchecked.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::sync::Arc;

use http::StatusCode;
use open_ferry_core::auth::{Auth, FileStore};
use serde_json::{Value, json};
use tokio::net::TcpListener;

use super::{Api, Upstream, http_response, keyed_config};

/// The request's head lines, the request line first.
fn head(request: &str) -> Vec<&str> {
    let end = request.find("\r\n\r\n").unwrap();
    request[..end].split("\r\n").collect()
}

/// The request's body.
fn body(request: &str) -> &str {
    let end = request.find("\r\n\r\n").unwrap();
    &request[end + 4..]
}

/// The value of the request's header `name`, which must appear once.
fn header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    let mut values = head(request)
        .into_iter()
        .skip(1)
        .filter_map(|line| line.split_once(": "))
        .filter(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value);
    let value = values.next();
    assert_eq!(values.next(), None, "{name} sent twice");
    value
}

/// An upstream answering every request with 200 and `{"ok":true}`.
async fn ok_upstream() -> Upstream {
    Upstream::answering(http_response(
        "200 OK",
        &[("Content-Type", "application/json")],
        br#"{"ok":true}"#,
    ))
    .await
}

fn xai_auth(id: &str, metadata: Value) -> Auth {
    let mut auth = Auth {
        id: id.into(),
        file_name: id.into(),
        provider: "xai".into(),
        attributes: BTreeMap::from([
            ("auth_kind".into(), "oauth".into()),
            ("base_url".into(), "https://api.x.ai/v1".into()),
        ]),
        ..Auth::default()
    };
    let Value::Object(metadata) = metadata else {
        panic!("metadata isn't an object");
    };
    auth.metadata = metadata;
    auth
}

fn expires_in_an_hour() -> String {
    (chrono::Utc::now() + chrono::Duration::hours(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[tokio::test]
async fn api_call_uses_request_proxy_url() {
    let proxy = Upstream::answering(http_response("201 Created", &[], b"proxied")).await;
    let mut config = keyed_config();
    config.proxy_url = "http://127.0.0.1:1".into();
    let api = Api::with(config, None);

    let answer = api
        .api_call(&json!({
            "method": "GET",
            "url": "http://upstream.invalid/test",
            "proxy_url": proxy.url,
        }))
        .await;
    let response = answer.expect(StatusCode::OK);
    assert_eq!(response["status_code"], json!(201));
    assert_eq!(response["body"], json!("proxied"));
    let requests = proxy.requests();
    assert_eq!(
        head(&requests[0])[0],
        "GET http://upstream.invalid/test HTTP/1.1"
    );
}

#[tokio::test]
async fn auth_by_index_distinguishes_shared_api_keys_across_providers() {
    let api = Api::new();
    let gemini = Auth {
        id: "gemini:apikey:123".into(),
        provider: "gemini".into(),
        attributes: BTreeMap::from([("api_key".into(), "shared-key".into())]),
        ..Auth::default()
    };
    let compat = Auth {
        id: "openai-compatibility:bohe:456".into(),
        provider: "bohe".into(),
        label: "bohe".into(),
        attributes: BTreeMap::from([
            ("api_key".into(), "shared-key".into()),
            ("compat_name".into(), "bohe".into()),
            ("provider_key".into(), "bohe".into()),
        ]),
        ..Auth::default()
    };
    let gemini_index = api.register(gemini);
    let compat_index = api.register(compat);
    assert_ne!(gemini_index, compat_index, "shared api key, shared index");

    let found = crate::quota::auth_by_index(&api.manager, &gemini_index).unwrap();
    assert_eq!(found.id, "gemini:apikey:123");
    let found = crate::quota::auth_by_index(&api.manager, &compat_index).unwrap();
    assert_eq!(found.id, "openai-compatibility:bohe:456");
}

#[tokio::test]
async fn api_call_replaces_token_in_body_data() {
    let upstream = ok_upstream().await;
    let api = Api::new();
    let mut devin = Auth {
        id: "devin-test.json".into(),
        provider: "devin".into(),
        attributes: BTreeMap::from([("api_key".into(), "secret-session-token-xyz".into())]),
        ..Auth::default()
    };
    devin.metadata.insert("type".into(), json!("devin"));
    devin
        .metadata
        .insert("api_key".into(), json!("secret-session-token-xyz"));
    let index = api.register(devin);

    let answer = api
        .api_call(&json!({
            "method": "POST",
            "url": upstream.url,
            "auth_index": index,
            "header": { "Content-Type": "application/json" },
            "data": r#"{"metadata":{"apiKey":"$TOKEN$","ideName":"chisel"}}"#,
        }))
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(
        body(&upstream.requests()[0]),
        r#"{"metadata":{"apiKey":"secret-session-token-xyz","ideName":"chisel"}}"#
    );
}

#[tokio::test]
async fn api_call_rejects_unresolved_token_placeholder() {
    for (name, mut request, want) in [
        (
            "omitted auth_index in header",
            json!({
                "method": "GET",
                "header": { "Authorization": "Bearer $TOKEN$" },
            }),
            "auth token not found",
        ),
        (
            "omitted auth_index in data",
            json!({ "method": "POST", "data": r#"{"apiKey":"$TOKEN$"}"# }),
            "auth token not found",
        ),
        (
            "unmatched auth_index",
            json!({
                "auth_index": "missing-auth-index",
                "authIndex": "missing-auth-index",
                "method": "GET",
                "header": { "Authorization": "Bearer $TOKEN$" },
            }),
            "auth credential not found for auth_index",
        ),
    ] {
        let upstream = ok_upstream().await;
        let api = Api::new();
        request["url"] = json!(format!("{}/test", upstream.url));
        let answer = api.api_call(&request).await;
        answer.assert(
            StatusCode::BAD_REQUEST,
            &json!({ "error": want }).to_string(),
        );
        assert!(upstream.requests().is_empty(), "{name}: upstream was hit");
    }
}

#[tokio::test]
async fn api_call_replaces_xai_oauth_access_token() {
    let upstream = ok_upstream().await;
    let api = Api::new();
    let index = api.register(xai_auth(
        "xai-user@example.com.json",
        json!({
            "type": "xai",
            "auth_kind": "oauth",
            "access_token": "xai-oauth-access-token",
            "refresh_token": "xai-refresh-token",
            "base_url": "https://api.x.ai/v1",
            "expired": expires_in_an_hour(),
        }),
    ));

    let answer = api
        .api_call(&json!({
            "authIndex": index,
            "method": "GET",
            "url": format!("{}/billing", upstream.url),
            "header": {
                "Authorization": "Bearer $TOKEN$",
                "x-xai-token-auth": "xai-grok-cli",
            },
        }))
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let request = &upstream.requests()[0];
    assert_eq!(
        header(request, "authorization"),
        Some("Bearer xai-oauth-access-token")
    );
    assert_eq!(header(request, "x-xai-token-auth"), Some("xai-grok-cli"));
}

#[tokio::test]
async fn api_call_end_to_end_with_auth_files_list() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("xai-user.json"),
        r#"{
  "type": "xai",
  "auth_kind": "oauth",
  "base_url": "https://api.x.ai/v1",
  "access_token": "xai-e2e-access-token",
  "refresh_token": "xai-e2e-refresh-token",
  "id_token": "xai-e2e-id-token",
  "email": "user@example.com",
  "disabled": false
}"#,
    )
    .unwrap();
    let store = Arc::new(FileStore::new(dir.path()));
    let api = Api::with_store(keyed_config(), None, Some(store));
    api.manager.load().unwrap();

    let files = api.files("").await;
    assert!(!files.is_empty(), "expected at least one auth file in list");
    let index = files[0]["auth_index"].as_str().unwrap().trim().to_owned();
    assert!(!index.is_empty());

    let upstream = Upstream::answering(http_response(
        "200 OK",
        &[("Content-Type", "application/json")],
        br#"{"billing":"ok"}"#,
    ))
    .await;
    let answer = api
        .api_call(&json!({
            "authIndex": index,
            "method": "GET",
            "url": format!("{}/v1/billing?format=credits", upstream.url),
            "header": {
                "Authorization": "Bearer $TOKEN$",
                "x-xai-token-auth": "xai-grok-cli",
            },
        }))
        .await;
    let response = answer.expect(StatusCode::OK);
    assert_eq!(response["body"], json!(r#"{"billing":"ok"}"#));
    let request = &upstream.requests()[0];
    assert_eq!(head(request)[0], "GET /v1/billing?format=credits HTTP/1.1");
    assert_eq!(
        header(request, "authorization"),
        Some("Bearer xai-e2e-access-token")
    );
}

#[tokio::test]
async fn api_call_replaces_xai_oauth_access_token_in_data() {
    let upstream = ok_upstream().await;
    let api = Api::new();
    let index = api.register(xai_auth(
        "xai-body-data@example.com.json",
        json!({
            "type": "xai",
            "auth_kind": "oauth",
            "access_token": "xai-body-data-token",
            "refresh_token": "xai-refresh-token",
            "base_url": "https://api.x.ai/v1",
            "expired": expires_in_an_hour(),
        }),
    ));

    let answer = api
        .api_call(&json!({
            "authIndex": index,
            "method": "POST",
            "url": format!("{}/probe", upstream.url),
            "data": r#"{"token":"$TOKEN$","client":"grok"}"#,
        }))
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(
        body(&upstream.requests()[0]),
        r#"{"token":"xai-body-data-token","client":"grok"}"#
    );
}

#[tokio::test]
async fn api_call_sends_id_token_when_only_id_token_present() {
    let upstream = ok_upstream().await;
    let api = Api::new();
    let index = api.register(xai_auth(
        "xai-id-no-refresh@example.com.json",
        json!({
            "type": "xai",
            "auth_kind": "oauth",
            "id_token": "xai-raw-id-token",
            "base_url": "https://api.x.ai/v1",
        }),
    ));

    let answer = api
        .api_call(&json!({
            "authIndex": index,
            "method": "GET",
            "url": format!("{}/billing", upstream.url),
            "header": { "Authorization": "Bearer $TOKEN$" },
        }))
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(
        header(&upstream.requests()[0], "authorization"),
        Some("Bearer xai-raw-id-token")
    );
}

#[tokio::test]
async fn api_call_answers_as_upstream_for_bad_requests() {
    let api = Api::new();
    for (body, want) in [
        ("", "invalid body"),
        ("[]", "invalid body"),
        (r#"{"method":1}"#, "invalid body"),
        ("{}", "missing method"),
        (r#"{"method":" "}"#, "missing method"),
        (r#"{"method":"get"}"#, "missing url"),
        (r#"{"method":"GET","url":"/relative"}"#, "invalid url"),
        (r#"{"method":"GET","url":"http://"}"#, "invalid url"),
        (r#"{"method":"GET","url":"http://a b"}"#, "invalid url"),
        (
            r#"{"method":"GET","url":"http://127.0.0.1/","proxy_url":"ftp://proxy"}"#,
            "invalid proxy_url",
        ),
        (
            r#"{"method":"GE T","url":"http://127.0.0.1/"}"#,
            "failed to build request",
        ),
    ] {
        let answer = api.post("/v0/management/api-call", body).await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(answer.body, json!({ "error": want }).to_string(), "{body}");
    }
}

#[tokio::test]
async fn api_call_sends_only_what_the_caller_and_http_need() {
    let upstream = ok_upstream().await;
    let api = Api::new();

    let answer = api
        .api_call(&json!({
            "method": "post",
            "url": format!("{}/v1/probe?x=1", upstream.url),
            "header": { "x-custom-header": " value ", "accept": "application/json" },
        }))
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let request = &upstream.requests()[0];
    let mut lines = head(request);
    assert_eq!(lines.remove(0), "POST /v1/probe?x=1 HTTP/1.1");
    lines.sort_unstable();
    let host = upstream.url.trim_start_matches("http://");
    let mut want = vec![
        "Accept-Encoding: gzip".to_owned(),
        "Accept: application/json".to_owned(),
        "Content-Length: 0".to_owned(),
        format!("Host: {host}"),
        format!("User-Agent: {}", open_ferry_providers::codex::USER_AGENT),
        "X-Custom-Header: value".to_owned(),
    ];
    want.sort_unstable();
    assert_eq!(lines, want);
    assert_eq!(body(request), "");

    // An empty User-Agent sends none; a Host header names the host.
    let answer = api
        .api_call(&json!({
            "method": "GET",
            "url": upstream.url,
            "header": { "User-Agent": "", "Host": "example.test" },
        }))
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let request = &upstream.requests()[1];
    assert_eq!(head(request)[0], "GET / HTTP/1.1");
    assert_eq!(header(request, "user-agent"), None);
    assert_eq!(header(request, "host"), Some("example.test"));
    assert_eq!(header(request, "content-length"), None);
}

#[tokio::test]
async fn api_call_answers_with_the_response() {
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gzip.write_all(b"unzipped \xff").unwrap();
    let gzip = gzip.finish().unwrap();
    let upstream = Upstream::start(move |n, _| match n {
        0 => http_response(
            "418 I'm a teapot",
            &[
                ("x-multi", "a"),
                ("X-Multi", "b"),
                ("content-type", "text/plain"),
                ("Pragma", "no-cache"),
            ],
            b"short and stout",
        ),
        1 => http_response("200 OK", &[("Content-Encoding", "gzip")], &gzip),
        _ => b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTrailer: X-Sum\r\n\
               Connection: keep-alive, Close\r\n\r\n4\r\nbody\r\n0\r\nX-Sum: 1\r\n\r\n"
            .to_vec(),
    })
    .await;
    let api = Api::new();
    let call = json!({ "method": "GET", "url": upstream.url });

    // Go's client adds a Cache-Control for a Pragma, and takes out a
    // Connection asking to close.
    let answer = api.api_call(&call).await;
    answer.assert(
        StatusCode::OK,
        r#"{"status_code":418,"header":{"Cache-Control":["no-cache"],"Content-Length":["15"],"Content-Type":["text/plain"],"Pragma":["no-cache"],"X-Multi":["a","b"]},"body":"short and stout"}"#,
    );

    // A gzip body asked for on the caller's behalf is decompressed, and
    // its encoding and length dropped; bytes outside UTF-8 are replaced as
    // Go's JSON encoder replaces them.
    let answer = api.api_call(&call).await;
    answer.assert(
        StatusCode::OK,
        "{\"status_code\":200,\"header\":{},\"body\":\"unzipped \\ufffd\"}",
    );

    // A chunked body loses its framing headers.
    let answer = api.api_call(&call).await;
    answer.assert(
        StatusCode::OK,
        r#"{"status_code":200,"header":{},"body":"body"}"#,
    );
}

#[tokio::test]
async fn api_call_follows_redirects_as_go_does() {
    let upstream = Upstream::start(|n, _| match n {
        0 => http_response("302 Found", &[("Location", "/next")], b""),
        _ => http_response("200 OK", &[], b"done"),
    })
    .await;
    let api = Api::new();

    let answer = api
        .api_call(&json!({
            "method": "POST",
            "url": format!("{}/start", upstream.url),
            "header": { "Authorization": "Bearer abc", "Content-Type": "text/plain" },
            "data": "payload",
        }))
        .await;
    let response = answer.expect(StatusCode::OK);
    assert_eq!(response["body"], json!("done"));
    let requests = upstream.requests();
    assert_eq!(requests.len(), 2);
    // A 302 turns a POST into a GET without the body or its headers, and
    // keeps credentials on the same host.
    let second = &requests[1];
    assert_eq!(head(second)[0], "GET /next HTTP/1.1");
    assert_eq!(header(second, "authorization"), Some("Bearer abc"));
    assert_eq!(header(second, "content-type"), None);
    assert_eq!(header(second, "content-length"), None);
    assert_eq!(
        header(second, "referer"),
        Some(format!("{}/start", upstream.url).as_str())
    );
    assert_eq!(body(second), "");
}

#[tokio::test]
async fn api_call_stops_after_ten_redirects() {
    let upstream = Upstream::answering(http_response(
        "307 Temporary Redirect",
        &[("Location", "/again")],
        b"",
    ))
    .await;
    let api = Api::new();

    let answer = api
        .api_call(&json!({ "method": "GET", "url": upstream.url }))
        .await;
    answer.assert(StatusCode::BAD_GATEWAY, r#"{"error":"request failed"}"#);
    assert_eq!(upstream.requests().len(), 10);
}

#[tokio::test]
async fn api_call_caps_the_response_body() {
    let big = vec![b'a'; crate::api_call::MAX_RESPONSE_BODY + 1];
    let upstream = Upstream::answering(http_response("200 OK", &[], &big)).await;
    let api = Api::new();

    let answer = api
        .api_call(&json!({ "method": "GET", "url": upstream.url }))
        .await;
    answer.assert(
        StatusCode::BAD_GATEWAY,
        r#"{"error":"failed to read response"}"#,
    );
}

#[tokio::test]
async fn api_call_failures_never_show_the_token() {
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = closed.local_addr().unwrap();
    drop(closed);
    let api = Api::new();
    let mut auth = Auth {
        id: "secret-holder".into(),
        provider: "codex".into(),
        ..Auth::default()
    };
    auth.metadata
        .insert("access_token".into(), json!("very-secret-token"));
    let index = api.register(auth);

    let answer = api
        .api_call(&json!({
            "auth_index": index,
            "method": "GET",
            "url": format!("http://user:very-secret-password@{address}/?k=very-secret-query"),
            "header": { "Authorization": "Bearer $TOKEN$" },
        }))
        .await;
    answer.assert(StatusCode::BAD_GATEWAY, r#"{"error":"request failed"}"#);
}

#[tokio::test]
async fn api_call_names_the_host_to_a_forwarding_proxy() {
    let proxy = Upstream::answering(http_response("200 OK", &[], b"proxied")).await;
    let api = Api::new();
    let call = |host: &str| {
        json!({
            "method": "GET",
            "url": "http://upstream.invalid/path?q=1#part",
            "header": { "Host": host },
            "proxy_url": proxy.url,
        })
    };

    // Go's request line names the Host's host, its zone taken out, as the
    // Host header does.
    let cases = [
        ("override.test:8080", "override.test:8080"),
        ("[fe80::1%en0]:8080", "[fe80::1]:8080"),
    ];
    for (host, sent) in cases {
        api.api_call(&call(host)).await.expect(StatusCode::OK);
        let requests = proxy.requests();
        let request = requests.last().unwrap();
        assert_eq!(
            head(request)[0],
            format!("GET http://{sent}/path?q=1 HTTP/1.1")
        );
        assert_eq!(header(request, "host"), Some(sent));
    }
    // A Host Go finds invalid fails, as does one the url crate can't read.
    for host in ["a b", "["] {
        api.api_call(&call(host))
            .await
            .assert(StatusCode::BAD_GATEWAY, r#"{"error":"request failed"}"#);
    }
    assert_eq!(proxy.requests().len(), 2);
}

#[tokio::test]
async fn api_call_reads_bodies_after_a_handover_as_go_does() {
    let upstream = Upstream::start(|n, _| {
        let response: &[u8] = match n {
            0 => {
                b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\n\
                   Upgrade: websocket\r\n\r\nhello"
            }
            1 => b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\nhello",
            2 => b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok and more",
            3 => b"HTTP/1.1 200 Connection Established\r\n\r\nall of it",
            4 => b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nok",
            5 => b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n0\r\n\r\n",
            _ => b"HTTP/1.1 204 No Content\r\nContent-Length: x\r\nConnection: close\r\n\r\n",
        };
        response.to_vec()
    })
    .await;
    let api = Api::new();
    let get = json!({ "method": "GET", "url": upstream.url });
    let connect = json!({ "method": "CONNECT", "url": format!("{}/path", upstream.url) });

    // A switch of protocols has all the connection sends until it closes;
    // another 101 has no body.
    api.api_call(&get).await.assert(
        StatusCode::OK,
        r#"{"status_code":101,"header":{"Connection":["Upgrade"],"Upgrade":["websocket"]},"body":"hello"}"#,
    );
    api.api_call(&get).await.assert(
        StatusCode::OK,
        r#"{"status_code":101,"header":{"Upgrade":["websocket"]},"body":""}"#,
    );

    // A 2xx answer to CONNECT has its length, or all until the close.
    api.api_call(&connect).await.assert(
        StatusCode::OK,
        r#"{"status_code":200,"header":{"Content-Length":["2"]},"body":"ok"}"#,
    );
    api.api_call(&connect).await.assert(
        StatusCode::OK,
        r#"{"status_code":200,"header":{},"body":"all of it"}"#,
    );
    api.api_call(&connect).await.assert(
        StatusCode::BAD_GATEWAY,
        r#"{"error":"failed to read response"}"#,
    );
    // This port's deviation: a chunked one fails.
    api.api_call(&connect)
        .await
        .assert(StatusCode::BAD_GATEWAY, r#"{"error":"request failed"}"#);

    // Go refuses a Content-Length that isn't a number, body or not.
    api.api_call(&get)
        .await
        .assert(StatusCode::BAD_GATEWAY, r#"{"error":"request failed"}"#);

    let requests = upstream.requests();
    let authority = upstream.url.trim_start_matches("http://");
    assert_eq!(
        head(&requests[2])[0],
        format!("CONNECT {authority} HTTP/1.1")
    );
}
