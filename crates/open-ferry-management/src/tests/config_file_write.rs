//! The routes of `crate::config_file_write`: `PUT config.yaml`, and the v8
//! writes of the whole config or a path in it.
//!
//! Upstream's tests of these routes (in config_v8_test.go,
//! config_v8_compatibility_test.go and config_v8_upstream_test.go) check
//! the file written, and are ported in `config_v8_write`. These are
//! open-ferry's: they check what the route asks the
//! [`FakeWriter`](super::FakeWriter) to write, what it answers, and the
//! config the handlers read afterwards.

use http::{Method, StatusCode};
use open_ferry_core::config::Config;

use super::{Api, Written, keyed_config};
use crate::{V8Edit, V8EditError, V8Method};

const V8_OK: &str = r#"{"config-version":8,"status":"ok"}"#;

// Not upstream's: a body that loads replaces the file, becomes the config
// the handlers read, and has the service reload.
#[tokio::test]
async fn put_config_yaml_writes_the_body() {
    let api = Api::writing(keyed_config());
    let body = "# kept as sent\nport: 8318\ndebug: true\nremote-management:\n  secret-key: other\n";
    api.call(Method::PUT, "/v0/management/config.yaml", body)
        .await
        .assert(StatusCode::OK, r#"{"changed":["config"],"ok":true}"#);

    assert_eq!(
        api.writer.written(),
        [Written::File(body.as_bytes().to_vec())]
    );
    assert_eq!(api.writer.lock_held(), [true]);
    let config = api.state.config();
    assert!(*config == Config::load_bytes(body.as_bytes()).unwrap());
    assert_eq!((config.port, config.debug), (8318, true));
    assert_eq!(config.remote_management.secret_key, "other");
    assert_eq!(api.reload.count(), 1);
}

// Not upstream's: a body that doesn't load answers 400 or 422, as
// upstream's decoder or its loader's checks refuse it, and writes nothing.
#[tokio::test]
async fn put_config_yaml_refuses_a_body_that_does_not_load() {
    let api = Api::writing(keyed_config());
    for (body, status, error, message) in [
        (
            "port: abc\n",
            StatusCode::BAD_REQUEST,
            "invalid_yaml",
            Some("yaml: unmarshal errors:\\n  line 1: cannot unmarshal !!str into int"),
        ),
        ("port: [1\n", StatusCode::BAD_REQUEST, "invalid_yaml", None),
        ("- a\n- b\n", StatusCode::BAD_REQUEST, "invalid_yaml", None),
        (
            "config-version: 7\n",
            StatusCode::BAD_REQUEST,
            "invalid_yaml",
            Some("unsupported config-version (expected 8)"),
        ),
        (
            "api-keys:\n  codex:\n    - base-url: http://x\n      keys:\n        - api-key: k1\n          weight: '3'\n",
            StatusCode::BAD_REQUEST,
            "invalid_yaml",
            Some("api-keys.codex.keys[0].weight: weight must be an integer"),
        ),
        (
            "codex-api-key:\n  - api-key: k\n    base-url: https://c\n    weight: 2000000\n",
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_config",
            Some("codex-api-key[0].weight: weight must not exceed 1000000"),
        ),
        (
            "trusted-proxies: ['nope']\n",
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_config",
            Some(r#"invalid trusted-proxies entry \"nope\": invalid CIDR address: nope"#),
        ),
    ] {
        let answer = api
            .call(Method::PUT, "/v0/management/config.yaml", body)
            .await;
        let json = answer.expect(status);
        assert_eq!(json["error"], error, "{body}: {json}");
        let text = json["message"].as_str().unwrap_or_default();
        assert!(!text.is_empty(), "{body}: {json}");
        assert!(
            !text.starts_with("failed to parse config file"),
            "{body}: {json}"
        );
        if let Some(message) = message {
            let want = format!(r#"{{"error":"{error}","message":"{message}"}}"#);
            assert_eq!(answer.body, want, "{body}");
        }
    }
    assert!(api.writer.written().is_empty());
    assert_eq!(api.reload.count(), 0);
    assert!(*api.state.config() == keyed_config());
}

// Not upstream's: a file the writer can't write answers 500, without the
// writer's message, and changes nothing.
#[tokio::test]
async fn put_config_yaml_write_failure() {
    let api = Api::writing(keyed_config());
    api.writer.fail(r"C:\secret\path: access denied");
    api.call(Method::PUT, "/v0/management/config.yaml", "debug: true\n")
        .await
        .assert(
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":"write_failed","message":"failed to write config"}"#,
        );
    assert_eq!(
        api.writer.written(),
        [Written::File(b"debug: true\n".to_vec())]
    );
    assert!(*api.state.config() == keyed_config());
    assert_eq!(api.reload.count(), 0);
}

/// A v8 edit.
fn edit(method: V8Method, path: &[&str], body: &str, yaml: bool) -> V8Edit {
    V8Edit {
        method,
        path: path.iter().map(|part| (*part).to_owned()).collect(),
        body: body.as_bytes().to_vec(),
        yaml,
    }
}

// Not upstream's: each v8 write hands the writer its method, path and
// body, under the lock; the config the writer returns is the one the
// handlers read, and the service reloads.
#[tokio::test]
async fn v8_writes_are_handed_to_the_writer() {
    let mut written = keyed_config();
    written.debug = true;
    written.port = 9000;
    let cases = [
        (
            Method::PUT,
            "/v8/management/config",
            r#"{"debug":true}"#,
            edit(V8Method::Put, &[], r#"{"debug":true}"#, false),
        ),
        (
            Method::PATCH,
            "/v8/management/config",
            r#"{"port":9000}"#,
            edit(V8Method::Patch, &[], r#"{"port":9000}"#, false),
        ),
        (
            Method::PUT,
            "/v8/management/config/",
            "{}",
            edit(V8Method::Put, &[], "{}", false),
        ),
        (
            Method::PATCH,
            "/v8/management/config/",
            "{}",
            edit(V8Method::Patch, &[], "{}", false),
        ),
        (
            Method::DELETE,
            "/v8/management/config/",
            "",
            edit(V8Method::Delete, &[], "", false),
        ),
        (
            Method::PUT,
            "/v8/management/config/api-keys/xai",
            "[]",
            edit(V8Method::Put, &["api-keys", "xai"], "[]", false),
        ),
        (
            Method::PATCH,
            "/v8/management/config/routing/strategy/",
            r#""fill-first""#,
            edit(
                V8Method::Patch,
                &["routing", "strategy"],
                r#""fill-first""#,
                false,
            ),
        ),
        (
            Method::DELETE,
            "/v8/management/config/debug",
            "",
            edit(V8Method::Delete, &["debug"], "", false),
        ),
        (
            Method::PUT,
            "/v8/management/config/a%20b/c",
            "1",
            edit(V8Method::Put, &["a b", "c"], "1", false),
        ),
        (
            Method::PUT,
            "/v8/management/config/a//b",
            "1",
            edit(V8Method::Put, &["a", "", "b"], "1", false),
        ),
        (
            Method::PUT,
            "/v8/management/config.yaml",
            "debug: true\n",
            edit(V8Method::Put, &[], "debug: true\n", true),
        ),
    ];
    for (method, path, body, want) in cases {
        let api = Api::writing(keyed_config());
        api.writer.set_v8_result(Ok(written.clone()));
        api.call(method.clone(), path, body)
            .await
            .assert(StatusCode::OK, V8_OK);
        assert_eq!(api.writer.written(), [Written::V8(want)], "{method} {path}");
        assert_eq!(api.writer.lock_held(), [true], "{method} {path}");
        assert!(*api.state.config() == written, "{method} {path}");
        assert_eq!(api.reload.count(), 1, "{method} {path}");
    }
}

// Not upstream's: what each refusal of the writer answers; the config is
// left as it was, and the service isn't reloaded.
#[tokio::test]
async fn v8_refusals() {
    for (error, status, body) in [
        (
            V8EditError::ReadFailed,
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":"read_failed"}"#,
        ),
        (
            V8EditError::StoredInvalid("bad file".into()),
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":"invalid_config","message":"bad file"}"#,
        ),
        (
            V8EditError::CannotDeleteConfig,
            StatusCode::BAD_REQUEST,
            r#"{"error":"cannot_delete_config"}"#,
        ),
        (
            V8EditError::NotFound,
            StatusCode::NOT_FOUND,
            r#"{"error":"not_found"}"#,
        ),
        (
            V8EditError::InvalidBody,
            StatusCode::BAD_REQUEST,
            r#"{"error":"invalid_body"}"#,
        ),
        (
            V8EditError::InvalidJson,
            StatusCode::BAD_REQUEST,
            r#"{"error":"invalid_json"}"#,
        ),
        (
            V8EditError::ConfigMustBeObject,
            StatusCode::BAD_REQUEST,
            r#"{"error":"config_must_be_object"}"#,
        ),
        (
            V8EditError::InvalidPath,
            StatusCode::BAD_REQUEST,
            r#"{"error":"invalid_path"}"#,
        ),
        (
            V8EditError::InvalidConfig("api-keys.codex must be a list".into()),
            StatusCode::BAD_REQUEST,
            r#"{"error":"invalid_config","message":"api-keys.codex must be a list"}"#,
        ),
        (
            V8EditError::ReadOnlyField("home".into()),
            StatusCode::BAD_REQUEST,
            r#"{"error":"read_only_field","field":"home"}"#,
        ),
        (
            V8EditError::Unprocessable("weight must not exceed 1000000".into()),
            StatusCode::UNPROCESSABLE_ENTITY,
            r#"{"error":"invalid_config","message":"weight must not exceed 1000000"}"#,
        ),
        (
            V8EditError::WriteFailed("disk full".into()),
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":"write_failed","message":"disk full"}"#,
        ),
    ] {
        let api = Api::writing(keyed_config());
        api.writer.set_v8_result(Err(error.clone()));
        api.call(Method::PATCH, "/v8/management/config/debug", "true")
            .await
            .assert(status, body);
        assert_eq!(api.writer.written().len(), 1, "{error}");
        assert!(*api.state.config() == keyed_config(), "{error}");
        assert_eq!(api.reload.count(), 0, "{error}");
    }

    // A write the writer can't make is reported as it reports it.
    let api = Api::writing(keyed_config());
    api.writer.fail("disk full");
    api.call(Method::DELETE, "/v8/management/config/debug", "")
        .await
        .assert(
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":"write_failed","message":"disk full"}"#,
        );
    assert_eq!(api.reload.count(), 0);
}

// Not upstream's: a path that isn't UTF-8 is refused before the writer is
// asked.
#[tokio::test]
async fn v8_path_that_is_not_utf8() {
    let api = Api::writing(keyed_config());
    for method in [Method::PUT, Method::PATCH] {
        api.call(method, "/v8/management/config/%FF", "1")
            .await
            .assert(StatusCode::BAD_REQUEST, r#"{"error":"invalid_path"}"#);
    }
    api.call(Method::DELETE, "/v8/management/config/%FF", "")
        .await
        .assert(StatusCode::NOT_FOUND, r#"{"error":"not_found"}"#);
    assert!(api.writer.written().is_empty());
    assert_eq!(api.reload.count(), 0);

    let api = Api::new();
    api.call(Method::DELETE, "/v8/management/config/%FF", "")
        .await
        .assert(
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"error":"config writer unavailable"}"#,
        );
}
