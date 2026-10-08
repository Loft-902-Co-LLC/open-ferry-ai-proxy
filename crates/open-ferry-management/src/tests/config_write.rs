//! How the routes that change the config use the writer (see
//! `crate::config_write`): under one lock, saving a changed copy of the
//! config, then having the service reload it.
//!
//! Upstream has no tests of its own for this; these are open-ferry's. The
//! routes' changes are tested in `config_settings`, `config_lists`,
//! `config_keys` and `config_file_write`, and the file the writer writes
//! in `config_v8_write`.

use futures_util::future::join_all;
use http::{Method, StatusCode};
use open_ferry_core::config::Config;
use open_ferry_core::config::save::{self, sha256_hex};

use super::{Api, AuthDir, Written, keyed_config};
use crate::{UndoCheck, undo_config};

const OK: &str = r#"{"status":"ok"}"#;

/// What a save answers when the file changed since the config the
/// handlers read was loaded or written.
const CHANGED: &str = r#"{"error":"config_changed","message":"the config file changed on disk since the server loaded it; it has been loaded again, so try again"}"#;

/// The config file's contents.
fn read(dir: &AuthDir) -> Vec<u8> {
    std::fs::read(dir.config_path()).unwrap()
}

// Not upstream's: a save is made only while the file holds what the config
// the handlers read was loaded from or last written as. A change made in
// the file since, as by `open-ferry config`, isn't written over: the save
// answers 409 and writes nothing, and the config the handlers read is
// loaded again, so the change made again keeps both. Upstream writes its
// config over the file, which brings back a client key revoked there.
#[tokio::test]
async fn a_save_never_writes_over_a_change_made_in_the_file() {
    let dir = AuthDir::new();
    let api = Api::over_config_file(&dir, "api-keys:\n  - revoked-client-key\n");
    let path = dir.config_path();
    save::write_file(
        &path,
        b"api-keys:\n  - new-client-key-1\n  - new-client-key-2\n",
    )
    .unwrap();
    let written = read(&dir);

    api.call(Method::PUT, "/v0/management/debug", r#"{"value":true}"#)
        .await
        .assert(StatusCode::CONFLICT, CHANGED);
    assert_eq!(read(&dir), written);
    assert_eq!(
        api.state.config().api_keys,
        ["new-client-key-1", "new-client-key-2"]
    );
    assert_eq!(api.state.config_sha256(), Some(sha256_hex(&written)));

    // Saves made one after another each expect what the last one wrote.
    for value in [true, false, true] {
        api.call(
            Method::PUT,
            "/v0/management/debug",
            &format!(r#"{{"value":{value}}}"#),
        )
        .await
        .assert(StatusCode::OK, OK);
    }
    let config = Config::load(&path).unwrap();
    assert_eq!(config.api_keys, ["new-client-key-1", "new-client-key-2"]);
    assert!(config.debug);
    assert_eq!(api.state.config_sha256(), Some(sha256_hex(&read(&dir))));
}

// Not upstream's: each write keeps the SHA-256 of what it wrote, the v8
// writes, `PUT config.yaml` and the undo included, so the save after it
// goes ahead without a reload. When the file changed since, and the
// service's reload leaves the config the handlers read behind it, as when
// the service had loaded what the file holds before the handlers' last
// write, the save loads the file itself; a file that doesn't load is said
// to, and nothing is written.
#[tokio::test]
async fn a_save_catches_up_with_the_file_by_itself() {
    let dir = AuthDir::new();
    let api = Api::over_config_file(&dir, "port: 8317\n").reloading_nothing();
    let debug = |value: bool| format!(r#"{{"value":{value}}}"#);
    let writes = [
        (Method::PUT, "/v0/management/debug", debug(true)),
        (Method::PUT, "/v0/management/debug", debug(false)),
        (
            Method::PUT,
            "/v8/management/config/server/port",
            "8318".to_owned(),
        ),
        (Method::PUT, "/v0/management/debug", debug(true)),
        (
            Method::PUT,
            "/v0/management/config.yaml",
            "port: 8319\n".to_owned(),
        ),
        (Method::PUT, "/v0/management/debug", debug(true)),
    ];
    for (method, route, body) in writes {
        let answer = api.call(method.clone(), route, &body).await;
        assert_eq!(
            answer.status,
            StatusCode::OK,
            "{method} {route}: {}",
            answer.body
        );
        assert_eq!(api.state.config_sha256(), Some(sha256_hex(&read(&dir))));
    }
    undo_config(&api.state, UndoCheck::default()).await.unwrap();
    assert_eq!(api.state.config_sha256(), Some(sha256_hex(&read(&dir))));
    api.call(Method::PUT, "/v0/management/debug", &debug(false))
        .await
        .assert(StatusCode::OK, OK);

    let path = dir.config_path();
    save::write_file(&path, b"port: 8320\napi-keys: [new-client-key]\n").unwrap();
    let written = read(&dir);
    let reloads = api.reload.count();
    api.call(Method::PUT, "/v0/management/debug", &debug(true))
        .await
        .assert(StatusCode::CONFLICT, CHANGED);
    assert_eq!(read(&dir), written);
    assert_eq!(api.reload.count(), reloads + 1);
    assert_eq!(api.state.config().api_keys, ["new-client-key"]);
    api.call(Method::PUT, "/v0/management/debug", &debug(true))
        .await
        .assert(StatusCode::OK, OK);
    let config = Config::load(&path).unwrap();
    assert_eq!((config.port, config.debug), (8320, true));
    assert_eq!(config.api_keys, ["new-client-key"]);

    std::fs::write(&path, "port: [\n").unwrap();
    api.call(Method::PUT, "/v0/management/debug", &debug(false))
        .await
        .assert(
            StatusCode::CONFLICT,
            r#"{"error":"config_changed","message":"the config file changed on disk since the server loaded it, and doesn't load; fix it, then try again"}"#,
        );
    assert_eq!(read(&dir), b"port: [\n");
}

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
