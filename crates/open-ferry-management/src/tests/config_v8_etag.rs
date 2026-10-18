// Ported from CLIProxyAPI internal/api/handlers/management/
// config_v8_etag_test.go (TestConfigV8ETagIfMatch) (v8.0.32, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The version token of the v8 config routes (see
//! [`crate::config_etag`]): the `ETag` of a read, and `If-Match` on a write,
//! over a config file in a temporary directory.
//!
//! Deviations from upstream's test: it calls the handler on gin's test
//! router; here the requests go through the management router, with the
//! management key. Its `TestConfigV8ETagExposedForCORS` runs the real
//! server, which this crate isn't; the exposed header is checked in
//! `open-ferry-server`.

use http::{HeaderValue, Method, StatusCode, header};
use open_ferry_core::config::Config;
use open_ferry_core::config::save::sha256_hex;

use super::{Answer, Api, AuthDir, keyed};

const RAW: &str = "config-version: 8\nserver: {port: 8317}\naccess: {api-keys: [revoked-key]}\n";

/// `method path` with `body` and one `If-Match` line per entry of `lines`.
async fn send(api: &Api, method: Method, path: &str, body: &str, lines: &[&str]) -> Answer {
    let mut request = keyed(method, path, body);
    for line in lines {
        request
            .headers_mut()
            .append(header::IF_MATCH, HeaderValue::from_str(line).unwrap());
    }
    api.send(request).await
}

/// `method path` with `body` and the single `If-Match` line `if_match`
/// (none when empty).
async fn call(api: &Api, method: Method, path: &str, body: &str, if_match: &str) -> Answer {
    if if_match.is_empty() {
        send(api, method, path, body, &[]).await
    } else {
        send(api, method, path, body, &[if_match]).await
    }
}

fn file_bytes(dir: &AuthDir) -> String {
    std::fs::read_to_string(dir.config_path()).unwrap()
}

/// The tag of the file as it is now.
fn file_etag(dir: &AuthDir) -> String {
    format!("\"{}\"", sha256_hex(file_bytes(dir).as_bytes()))
}

fn status(answer: &Answer, status: StatusCode) {
    assert_eq!(answer.status, status, "{}", answer.body);
}

/// Upstream's `TestConfigV8ETagIfMatch`: a stale whole-document write can't
/// resurrect a change another writer saved.
#[tokio::test]
async fn etag_and_if_match_fence_v8_writes() {
    let dir = AuthDir::new();
    let api = Api::over_config_file(&dir, RAW);
    let config = "/v8/management/config";
    let yaml = "/v8/management/config.yaml";
    let keys = "/v8/management/config/access/api-keys";
    let port = "/v8/management/config/server/port";

    // PATCH at the root with a stale ETag fails with 412.
    let answer = call(
        &api,
        Method::PATCH,
        config,
        r#"{"server":{"port":8319}}"#,
        "\"stale\"",
    )
    .await;
    status(&answer, StatusCode::PRECONDITION_FAILED);
    assert_eq!(answer.body, r#"{"error":"precondition_failed"}"#);
    assert_eq!(file_bytes(&dir), RAW, "a stale PATCH changed the file");

    // PATCH with the current ETag goes through and returns the new one.
    let current = file_etag(&dir);
    let answer = call(
        &api,
        Method::PATCH,
        config,
        r#"{"server":{"port":8319}}"#,
        &current,
    )
    .await;
    status(&answer, StatusCode::OK);
    assert_eq!(answer.header("etag"), Some(file_etag(&dir).as_str()));
    assert_ne!(file_etag(&dir), current);
    assert_eq!(Config::load(dir.config_path()).unwrap().port, 8319);

    // A read advertises the on-disk version as a strong ETag.
    let get = api.get(yaml).await;
    status(&get, StatusCode::OK);
    let etag = get.header("etag").unwrap().to_owned();
    assert_eq!(etag, file_etag(&dir));
    let sub = api.get("/v8/management/config/server").await;
    status(&sub, StatusCode::OK);
    assert_eq!(sub.header("etag"), Some(etag.as_str()));
    let whole = api.get(config).await;
    status(&whole, StatusCode::OK);
    assert_eq!(whole.header("etag"), Some(etag.as_str()));

    // Another writer revokes the API key between that read and our write.
    let revoked = call(&api, Method::PUT, keys, r#"["other-key"]"#, "").await;
    status(&revoked, StatusCode::OK);
    assert_eq!(revoked.header("etag"), Some(file_etag(&dir).as_str()));

    // A whole-document PUT based on the stale read doesn't resurrect it.
    let before = file_bytes(&dir);
    let stale = call(&api, Method::PUT, yaml, RAW, &etag).await;
    status(&stale, StatusCode::PRECONDITION_FAILED);
    assert_eq!(stale.header("etag"), Some(file_etag(&dir).as_str()));
    assert_eq!(file_bytes(&dir), before, "a stale PUT changed the file");
    assert_eq!(
        Config::load(dir.config_path()).unwrap().api_keys,
        ["other-key"]
    );

    // DELETE with a stale ETag is refused the same way.
    let answer = call(&api, Method::DELETE, keys, "", &etag).await;
    status(&answer, StatusCode::PRECONDITION_FAILED);
    assert_eq!(file_bytes(&dir), before, "a stale DELETE changed the file");

    // Weak and malformed validators never match under strong comparison, a
    // comma inside quotes is tag content, and only the whole field is the
    // wildcard.
    let weak = format!("W/{}", file_etag(&dir));
    for if_match in [
        weak.as_str(),
        "not-an-etag",
        "\"unterminated",
        "\"a\"b\"",
        "\"stale,*,tag\"",
        "W/\"stale,*,tag\"",
        "\"stale\", *",
    ] {
        let answer = call(&api, Method::PUT, yaml, &before, if_match).await;
        status(&answer, StatusCode::PRECONDITION_FAILED);
    }

    // Tags split across repeated header lines are one list.
    let current = file_etag(&dir);
    let answer = send(&api, Method::PUT, port, "8318", &["\"stale\"", &current]).await;
    status(&answer, StatusCode::OK);

    // A quoted current hash with a trailing comma inside the quotes is a
    // different tag.
    let hash = sha256_hex(file_bytes(&dir).as_bytes());
    let answer = call(&api, Method::PUT, port, "8317", &format!("\"{hash},\"")).await;
    status(&answer, StatusCode::PRECONDITION_FAILED);

    // A header that is there but blank carries no tag and fails.
    for blank in ["", "  \t  "] {
        let answer = send(&api, Method::PUT, port, "8317", &[blank]).await;
        status(&answer, StatusCode::PRECONDITION_FAILED);
    }

    // A list holding the current tag, with whitespace and weak members.
    let current = file_etag(&dir);
    let list = format!("\"stale\", {current} , W/{current}");
    let answer = call(&api, Method::DELETE, keys, "", &list).await;
    status(&answer, StatusCode::OK);
    assert!(Config::load(dir.config_path()).unwrap().api_keys.is_empty());

    // The wildcard matches any existing version.
    let answer = call(&api, Method::PUT, port, "8320", "*").await;
    status(&answer, StatusCode::OK);

    // A matching ETag chains: the write's ETag is the next write's.
    let answer = call(&api, Method::PUT, port, "8321", &file_etag(&dir)).await;
    status(&answer, StatusCode::OK);
    let next = answer.header("etag").unwrap().to_owned();
    assert_eq!(next, file_etag(&dir));
    let answer = call(&api, Method::PUT, port, "8322", &next).await;
    status(&answer, StatusCode::OK);

    // Without If-Match a write is unconditional.
    let answer = call(&api, Method::PUT, port, "8323", "").await;
    status(&answer, StatusCode::OK);

    // A read of a missing key is a 404 without an ETag.
    let missing = api.get("/v8/management/config/no-such-key").await;
    status(&missing, StatusCode::NOT_FOUND);
    assert_eq!(missing.header("etag"), None);
}

// Not upstream's: a write refused before it is made (a path that isn't a
// key, a DELETE of nothing) is still checked against If-Match first, as
// upstream checks it before reading the path or the body; and a 412 leaves
// the reload and the handlers' config alone.
#[tokio::test]
async fn if_match_is_checked_before_the_write_is_judged() {
    let dir = AuthDir::new();
    let api = Api::over_config_file(&dir, RAW);
    let delete_whole = "/v8/management/config/";
    let missing = "/v8/management/config/no-such-key";

    // Matching: the write's own answer.
    let current = file_etag(&dir);
    let answer = call(&api, Method::DELETE, missing, "", &current).await;
    status(&answer, StatusCode::NOT_FOUND);
    let answer = call(&api, Method::DELETE, delete_whole, "", &current).await;
    status(&answer, StatusCode::BAD_REQUEST);

    // Stale: 412, with the current tag.
    for path in [missing, delete_whole] {
        let answer = call(&api, Method::DELETE, path, "", "\"stale\"").await;
        status(&answer, StatusCode::PRECONDITION_FAILED);
        assert_eq!(answer.header("etag"), Some(current.as_str()));
    }
    let answer = call(
        &api,
        Method::PUT,
        "/v8/management/config/server",
        "not json",
        "\"stale\"",
    )
    .await;
    status(&answer, StatusCode::PRECONDITION_FAILED);
    let answer = call(
        &api,
        Method::PUT,
        "/v8/management/config/server",
        "not json",
        &current,
    )
    .await;
    status(&answer, StatusCode::BAD_REQUEST);
    assert_eq!(file_bytes(&dir), RAW);
    assert!(*api.state.config() == Config::load(dir.config_path()).unwrap());
}

// Not upstream's: a file that can't be read answers read_failed to a write
// with If-Match, as upstream does before it compares.
#[tokio::test]
async fn if_match_on_a_missing_file_is_read_failed() {
    let dir = AuthDir::new();
    let api = Api::over_config_file(&dir, RAW);
    std::fs::remove_file(dir.config_path()).unwrap();
    let answer = call(
        &api,
        Method::PUT,
        "/v8/management/config/server/port",
        "8318",
        "*",
    )
    .await;
    status(&answer, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(answer.body, r#"{"error":"read_failed"}"#);
}
