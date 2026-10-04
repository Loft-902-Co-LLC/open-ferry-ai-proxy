//! Tests of the routes of `crate::latest_version`. Upstream's one test,
//! config_basic_version_test.go (TestSetLatestReleaseRequestHeaders), is
//! ported in `crate::latest_version`; these ask a local server standing in
//! for GitHub, never GitHub.

use http::{Method, StatusCode};
use open_ferry_core::config::Config;

use super::{Answer, Api, LOCAL, Upstream, http_response, keyed_config, request_from};

/// The API with `config`, asking `url` for the latest release.
fn asking(config: Config, url: &str) -> Api {
    let url = url.to_owned();
    Api::build(config, None, None, move |state, _| {
        state.with_latest_release_url(url)
    })
}

/// The latest version, as the API answers when GitHub answers `response`;
/// and the request GitHub got.
async fn latest(response: Vec<u8>) -> (Answer, String) {
    let upstream = Upstream::answering(response).await;
    let url = format!("{}/repos/o/r/releases/latest", upstream.url);
    let answer = asking(keyed_config(), &url)
        .get("/v0/management/latest-version")
        .await;
    let requests = upstream.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    (answer, requests.concat())
}

/// A 200 with `body` as JSON.
fn release(body: &str) -> Vec<u8> {
    http_response(
        "200 OK",
        &[("Content-Type", "application/json")],
        body.as_bytes(),
    )
}

/// Not upstream's: the version is the release's tag, trimmed, else its
/// name; the request takes JSON as open-ferry, without a token.
#[tokio::test]
async fn the_latest_version_is_the_release_tag() {
    let (answer, request) = latest(release(r#"{"tag_name":" v1.2.3 ","name":"n"}"#)).await;
    answer.assert(StatusCode::OK, r#"{"latest-version":"v1.2.3"}"#);
    let head = request.to_ascii_lowercase();
    assert!(
        head.starts_with("get /repos/o/r/releases/latest http/1.1\r\n"),
        "{request}"
    );
    assert!(
        head.contains("\r\naccept: application/vnd.github+json\r\n"),
        "{request}"
    );
    assert!(head.contains("\r\nuser-agent: open-ferry/"), "{request}");
    assert!(!head.contains("authorization"), "{request}");

    let (answer, _) = latest(release(r#"{"tag_name":"  ","name":" Release 9 "}"#)).await;
    answer.assert(StatusCode::OK, r#"{"latest-version":"Release 9"}"#);
    let (answer, _) = latest(release(r#"{"name":"only-name","extra":[1]}"#)).await;
    answer.assert(StatusCode::OK, r#"{"latest-version":"only-name"}"#);
}

/// Not upstream's: a release without a version, an answer that isn't a
/// release, and one that isn't a 200 are answered with a 502 saying so.
#[tokio::test]
async fn bad_releases_are_bad_gateways() {
    for (response, body) in [
        (
            release(r#"{"tag_name":"","name":" "}"#),
            r#"{"error":"invalid_response","message":"missing release version"}"#,
        ),
        (release(""), r#"{"error":"decode_failed","message":"EOF"}"#),
        (
            release(r#"{"tag_name":5}"#),
            r#"{"error":"decode_failed","message":"the release isn't a JSON object of strings"}"#,
        ),
        (
            release("not json"),
            r#"{"error":"decode_failed","message":"the release isn't a JSON object of strings"}"#,
        ),
        (
            http_response("404 Not Found", &[], b" \n{\"message\":\"Not Found\"}\n"),
            r#"{"error":"unexpected_status","message":"status 404: {\"message\":\"Not Found\"}"}"#,
        ),
        (
            http_response("302 Found", &[("Location", "/elsewhere")], b""),
            r#"{"error":"unexpected_status","message":"status 302: "}"#,
        ),
    ] {
        let (answer, _) = latest(response).await;
        answer.assert(StatusCode::BAD_GATEWAY, body);
    }

    // At most 1024 bytes of an unexpected answer are quoted.
    let long = format!("{}{}", "a".repeat(1023), "bc");
    let (answer, _) = latest(http_response(
        "500 Internal Server Error",
        &[],
        long.as_bytes(),
    ))
    .await;
    let body = answer.expect(StatusCode::BAD_GATEWAY);
    assert_eq!(body["error"], "unexpected_status");
    let want = format!("status 500: {}b", "a".repeat(1023));
    assert_eq!(body["message"], want.as_str());
}

/// Not upstream's: a URL that doesn't parse is a 500, and a server that
/// can't be reached a 502.
#[tokio::test]
async fn unreachable_releases_fail() {
    let answer = asking(keyed_config(), "not a url")
        .get("/v0/management/latest-version")
        .await;
    let body = answer.expect(StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"], "request_create_failed");
    assert!(!body["message"].as_str().unwrap().is_empty());

    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let url = format!("http://127.0.0.1:{port}/latest");
    let answer = asking(keyed_config(), &url)
        .get("/v8/management/server/latest-version")
        .await;
    let body = answer.expect(StatusCode::BAD_GATEWAY);
    assert_eq!(body["error"], "request_failed");
    assert!(!body["message"].as_str().unwrap().is_empty());
}

/// Not upstream's: the request goes through the config's proxy.
#[tokio::test]
async fn releases_are_asked_through_the_proxy() {
    let proxy = Upstream::answering(release(r#"{"tag_name":"v9"}"#)).await;
    let mut config = keyed_config();
    config.proxy_url = proxy.url.clone();
    let answer = asking(config, "http://example.invalid/releases/latest")
        .get("/v0/management/latest-version")
        .await;
    answer.assert(StatusCode::OK, r#"{"latest-version":"v9"}"#);
    let requests = proxy.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert!(
        requests[0].starts_with("GET http://example.invalid/releases/latest HTTP/1.1\r\n"),
        "{}",
        requests[0]
    );
}

/// Not upstream's: the v8 route answers as the v0 one, both take the key,
/// and writes aren't routes.
#[tokio::test]
async fn both_routes_answer_alike() {
    let upstream = Upstream::answering(release(r#"{"tag_name":"v1"}"#)).await;
    let api = asking(keyed_config(), &format!("{}/latest", upstream.url));
    for path in [
        "/v0/management/latest-version",
        "/v8/management/server/latest-version",
    ] {
        api.get(path)
            .await
            .assert(StatusCode::OK, r#"{"latest-version":"v1"}"#);
        let request = request_from(LOCAL, Method::GET, path, "");
        assert_eq!(api.send(request).await.status, StatusCode::UNAUTHORIZED);
        let answer = api.send(super::keyed(Method::PUT, path, "{}")).await;
        assert_eq!(
            (answer.status, answer.body.as_str()),
            (StatusCode::NOT_FOUND, "")
        );
    }
    assert_eq!(upstream.requests().len(), 2);
}
