//! How the routes that change the config use the writer (see
//! `crate::config_write`): under one lock, saving a changed copy of the
//! config, then having the service reload it.
//!
//! Upstream has no tests of its own for this; these are open-ferry's. The
//! routes' changes are tested in `config_settings`, `config_lists`,
//! `config_keys` and `config_file_write`, and the file the writer writes
//! once it is wired in.

use futures_util::future::join_all;
use http::{Method, StatusCode};

use super::{Api, Written, keyed_config};

const OK: &str = r#"{"status":"ok"}"#;

// Not upstream's: a change is saved under the lock, becomes the config the
// handlers read, and has the service reload.
#[tokio::test]
async fn a_change_is_saved_then_reloaded() {
    let api = Api::writing(keyed_config());
    api.call(Method::PUT, "/v0/management/debug", r#"{"value":true}"#)
        .await
        .assert(StatusCode::OK, OK);

    let mut want = keyed_config();
    want.debug = true;
    assert_eq!(
        api.writer.written(),
        [Written::Saved {
            config: Box::new(want.clone()),
            migrate_v8: false,
        }]
    );
    assert!(api.saved() == want);
    assert_eq!(api.writer.lock_held(), [true]);
    assert_eq!(api.reload.count(), 1);
    let answer = api.get("/v0/management/debug").await;
    answer.assert(StatusCode::OK, r#"{"debug":true}"#);
}

// Not upstream's: a save the writer refuses answers 500 and leaves the
// config as it was, where upstream keeps the change.
#[tokio::test]
async fn a_refused_save_changes_nothing() {
    let api = Api::writing(keyed_config());
    api.writer.fail("disk full");
    api.call(Method::PUT, "/v0/management/debug", r#"{"value":true}"#)
        .await
        .assert(
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":"failed to save config: disk full"}"#,
        );
    api.call(Method::PUT, "/v0/management/api-keys", r#"["k"]"#)
        .await
        .assert(
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":"failed to save config: disk full"}"#,
        );

    assert!(*api.state.config() == keyed_config());
    assert_eq!(api.writer.saved().len(), 2);
    assert_eq!(api.reload.count(), 0);
}

// Not upstream's: a change the handler refuses saves nothing.
#[tokio::test]
async fn a_refused_change_saves_nothing() {
    let api = Api::writing(keyed_config());
    for (method, path, body, status, answer) in [
        (
            Method::PATCH,
            "/v0/management/gemini-api-key",
            r#"{"match":"missing","value":{"priority":1}}"#,
            StatusCode::NOT_FOUND,
            r#"{"error":"item not found"}"#,
        ),
        (
            Method::DELETE,
            "/v0/management/oauth-model-alias?channel=codex",
            "",
            StatusCode::NOT_FOUND,
            r#"{"error":"channel not found"}"#,
        ),
        (
            Method::PUT,
            "/v0/management/routing/strategy",
            r#"{"value":"random"}"#,
            StatusCode::BAD_REQUEST,
            r#"{"error":"invalid strategy"}"#,
        ),
        (
            Method::PATCH,
            "/v0/management/api-keys",
            "{}",
            StatusCode::BAD_REQUEST,
            r#"{"error":"missing fields"}"#,
        ),
    ] {
        api.call(method, path, body).await.assert(status, answer);
    }
    assert!(api.writer.written().is_empty());
    assert_eq!(api.reload.count(), 0);
    assert!(*api.state.config() == keyed_config());
}

// Not upstream's: changes made at once never lose one another, as each
// copies the config the last one saved.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changes_are_serialized() {
    let api = Api::writing(keyed_config());
    let bodies: Vec<String> = (0..24)
        .map(|i| format!(r#"{{"old":"none","new":"key-{i}"}}"#))
        .collect();
    let answers = join_all(
        bodies
            .iter()
            .map(|body| api.call(Method::PATCH, "/v0/management/api-keys", body)),
    )
    .await;
    for answer in answers {
        answer.assert(StatusCode::OK, OK);
    }

    let mut keys = api.saved().api_keys;
    keys.sort();
    let mut want: Vec<String> = (0..24).map(|i| format!("key-{i}")).collect();
    want.sort();
    assert_eq!(keys, want);
    assert_eq!(api.writer.saved().len(), 24);
    assert!(api.writer.lock_held().iter().all(|&held| held));
    assert_eq!(api.reload.count(), 24);
}

// Not upstream's: without a writer a change is refused before its body is
// read, so a body over the limit still answers 503.
#[tokio::test]
async fn without_a_writer_the_body_is_not_read() {
    let api = Api::new();
    let body = format!(r#"{{"value":"{}"}}"#, "x".repeat(17 << 20));
    api.call(Method::PUT, "/v0/management/proxy-url", &body)
        .await
        .assert(
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"error":"config writer unavailable"}"#,
        );
    let answer = Api::writing(keyed_config())
        .call(Method::PUT, "/v0/management/proxy-url", &body)
        .await;
    assert_eq!(
        answer.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "{}",
        answer.body
    );
}
