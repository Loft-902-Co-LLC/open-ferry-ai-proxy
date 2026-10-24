//! Deliberate writer refusals are conflicts, with no partial publication.
use std::sync::Arc;

use http::{Method, StatusCode};
use open_ferry_core::config::Config;
use serde_json::json;

use super::{Api, AuthDir};
use crate::{ConfigWriter, V8Edit, V8EditError, V8Precondition, WriteError};

const REASON: &str =
    "payload.default[0]: a mapping with a key that isn't a string can't be written";

pub(super) struct RefusingWriter;

impl ConfigWriter for RefusingWriter {
    fn save_preserving_comments(
        &self,
        _: &Config,
        _: bool,
        _: Option<&str>,
    ) -> Result<Option<String>, WriteError> {
        Err(WriteError::unwritable(REASON))
    }

    fn write_file(&self, _: &[u8]) -> Result<Option<String>, WriteError> {
        Err(WriteError::unwritable(REASON))
    }

    fn edit_v8(
        &self,
        _: &V8Edit,
        _: Option<&V8Precondition>,
    ) -> Result<(Config, Option<String>), V8EditError> {
        Err(V8EditError::Unwritable(REASON.into()))
    }
}

#[tokio::test]
async fn refusal_routes_answer_conflict_and_preserve_file_and_snapshot() {
    let dir = AuthDir::new();
    let mut raw = "debug: false\napi-keys: [placeholder]\noauth-excluded-models: {codex: [example]}\noauth-model-alias: {codex: [{name: example, alias: other}]}\noauth-request-scoped-errors: {codex: [{status: 429, match: [example], action: stop}]}\n".to_owned();
    for family in [
        "gemini-api-key",
        "interactions-api-key",
        "claude-api-key",
        "codex-api-key",
        "xai-api-key",
        "meta-api-key",
        "vertex-api-key",
    ] {
        raw.push_str(&format!(
            "{family}:\n- api-key: placeholder\n  base-url: https://example.invalid\n"
        ));
    }
    raw.push_str("openai-compatibility:\n- name: example\n  base-url: https://example.invalid\n  api-key-entries: [{api-key: placeholder}]\n");
    let mut api = Api::over_config_file(&dir, &raw);
    let before = std::fs::read(dir.config_path()).unwrap();
    let config = api.state.config();
    let hash = api.state.config_sha256();
    api.state = api
        .state
        .clone()
        .with_config_writer(Arc::new(RefusingWriter));
    api.router = crate::router(api.state.clone());
    let mut cases = vec![
        (Method::PUT, "/v0/management/config.yaml", "debug: true\n"),
        (Method::PUT, "/v8/management/config.yaml", "debug: true\n"),
        (Method::PUT, "/v8/management/config", "{}"),
        (Method::PATCH, "/v8/management/config", "{}"),
        (Method::PUT, "/v8/management/config/", "{}"),
        (Method::PATCH, "/v8/management/config/", "{}"),
        (Method::DELETE, "/v8/management/config/", ""),
        (
            Method::PUT,
            "/v8/management/config/observability/logs/debug",
            "true",
        ),
        (
            Method::PATCH,
            "/v8/management/config/observability/logs/debug",
            "true",
        ),
        (
            Method::DELETE,
            "/v8/management/config/observability/logs/debug",
            "",
        ),
        (Method::PUT, "/v0/management/debug", r#"{"value":true}"#),
        (Method::PATCH, "/v0/management/debug", r#"{"value":true}"#),
        (Method::DELETE, "/v0/management/proxy-url", ""),
        (Method::PUT, "/v0/management/api-keys", r#"["dummy-key"]"#),
        (
            Method::PATCH,
            "/v0/management/api-keys",
            r#"{"old":"placeholder","new":"replacement"}"#,
        ),
        (Method::PUT, "/v0/management/oauth-excluded-models", "{}"),
        (Method::PUT, "/v0/management/oauth-model-alias", "{}"),
        (
            Method::PUT,
            "/v0/management/oauth-request-scoped-errors",
            "{}",
        ),
        (Method::DELETE, "/v0/management/api-keys?index=0", ""),
        (
            Method::PATCH,
            "/v0/management/oauth-excluded-models",
            r#"{"provider":"codex","models":["example"]}"#,
        ),
        (
            Method::DELETE,
            "/v0/management/oauth-excluded-models?provider=codex",
            "",
        ),
        (
            Method::PATCH,
            "/v0/management/oauth-model-alias",
            r#"{"channel":"codex","aliases":[{"name":"example","alias":"other"}]}"#,
        ),
        (
            Method::DELETE,
            "/v0/management/oauth-model-alias?channel=codex",
            "",
        ),
        (
            Method::PATCH,
            "/v0/management/oauth-request-scoped-errors",
            r#"{"channel":"codex","rules":[{"status":429,"match":["example"],"action":"stop"}]}"#,
        ),
        (
            Method::DELETE,
            "/v0/management/oauth-request-scoped-errors?channel=codex",
            "",
        ),
    ];
    for (path, delete) in [
        (
            "/v0/management/gemini-api-key",
            "/v0/management/gemini-api-key?index=0",
        ),
        (
            "/v0/management/interactions-api-key",
            "/v0/management/interactions-api-key?index=0",
        ),
        (
            "/v0/management/claude-api-key",
            "/v0/management/claude-api-key?index=0",
        ),
        (
            "/v0/management/codex-api-key",
            "/v0/management/codex-api-key?index=0",
        ),
        (
            "/v0/management/xai-api-key",
            "/v0/management/xai-api-key?index=0",
        ),
        (
            "/v0/management/meta-api-key",
            "/v0/management/meta-api-key?index=0",
        ),
        (
            "/v0/management/openai-compatibility",
            "/v0/management/openai-compatibility?index=0",
        ),
        (
            "/v0/management/vertex-api-key",
            "/v0/management/vertex-api-key?index=0",
        ),
    ] {
        cases.push((Method::PUT, path, "[]"));
        cases.push((Method::PATCH, path, r#"{"index":0,"value":{"priority":1}}"#));
        cases.push((Method::DELETE, delete, ""));
    }
    for (path, body) in [
        (
            "/v0/management/usage-statistics-enabled",
            r#"{"value":true}"#,
        ),
        ("/v0/management/logging-to-file", r#"{"value":true}"#),
        ("/v0/management/logs-max-total-size-mb", r#"{"value":3}"#),
        ("/v0/management/error-logs-max-files", r#"{"value":3}"#),
        ("/v0/management/proxy-url", r#"{"value":""}"#),
        (
            "/v0/management/quota-exceeded/switch-project",
            r#"{"value":true}"#,
        ),
        (
            "/v0/management/quota-exceeded/switch-preview-model",
            r#"{"value":true}"#,
        ),
        ("/v0/management/request-log", r#"{"value":true}"#),
        ("/v0/management/ws-auth", r#"{"value":true}"#),
        ("/v0/management/request-retry", r#"{"value":3}"#),
        ("/v0/management/max-retry-credentials", r#"{"value":3}"#),
        ("/v0/management/max-retry-interval", r#"{"value":3}"#),
        ("/v0/management/force-model-prefix", r#"{"value":true}"#),
        (
            "/v0/management/routing/strategy",
            r#"{"value":"fill-first"}"#,
        ),
    ] {
        for method in [Method::PUT, Method::PATCH] {
            cases.push((method, path, body));
        }
    }
    for (method, path, body) in cases {
        let answer = api.call(method, path, body).await;
        assert_eq!(answer.status, StatusCode::CONFLICT, "{path}");
        let body: serde_json::Value = serde_json::from_str(&answer.body).unwrap();
        assert_eq!(body, json!({"error":"config_unwritable", "message":REASON}));
        assert_eq!(std::fs::read(dir.config_path()).unwrap(), before);
        assert!(*api.state.config() == *config);
        assert_eq!(api.state.config_sha256(), hash);
        assert!(!open_ferry_core::config::save::backup_path(&dir.config_path()).exists());
    }
}

#[tokio::test]
async fn real_generator_refusals_keep_both_reasons_and_leave_bytes_unchanged() {
    for (value, reason) in [
        ("{1: placeholder}", REASON.to_owned()),
        ("2001-12-15T02:59:43+24:00", "payload.default[0]: a timestamp with a zone 24 hours or more from UTC can't be written".to_owned()),
    ] {
        let raw = format!("debug: false\npayload:\n  default:\n    - params:\n        field: {value}\n");
        let dir = AuthDir::new();
        let api = Api::over_config_file(&dir, &raw);
        let hash = api.state.config_sha256();
        for method in [Method::PUT, Method::PATCH] {
            let answer = api.call(method, "/v0/management/debug", r#"{"value":true}"#).await;
            assert_eq!(answer.status, StatusCode::CONFLICT);
            let body: serde_json::Value = serde_json::from_str(&answer.body).unwrap();
            assert_eq!(body, json!({"error":"config_unwritable", "message":reason}));
            assert_eq!(std::fs::read(dir.config_path()).unwrap(), raw.as_bytes());
            assert!(!api.state.config().debug);
            assert_eq!(api.state.config_sha256(), hash);
        }
    }
}

#[tokio::test]
async fn unreadable_and_invalid_stored_files_keep_server_errors() {
    let dir = AuthDir::new();
    let api = Api::over_config_file(&dir, "debug: false\n");
    std::fs::write(dir.config_path(), "port: [\n").unwrap();
    let answer = api
        .call(
            Method::PATCH,
            "/v8/management/config/observability/logs/debug",
            "true",
        )
        .await;
    assert_eq!(answer.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&answer.body).unwrap()["error"],
        "invalid_config"
    );
    assert_eq!(std::fs::read(dir.config_path()).unwrap(), b"port: [\n");
    std::fs::remove_file(dir.config_path()).unwrap();
    let answer = api
        .call(
            Method::PATCH,
            "/v8/management/config/observability/logs/debug",
            "true",
        )
        .await;
    assert_eq!(answer.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&answer.body).unwrap()["error"],
        "read_failed"
    );
}

#[test]
fn io_messages_containing_refused_are_still_io_failures() {
    let dir = super::scratch_dir();
    let path = dir.path().join("refused");
    std::fs::create_dir(&path).unwrap();
    let writer = crate::FileConfigWriter::new(&path);
    let error = writer.write_file(b"debug: false\n").unwrap_err();
    assert!(error.to_string().contains("refused"));
    assert!(!error.is_unwritable());
    assert!(!WriteError::new(REASON).is_unwritable());
    let error = writer
        .save_preserving_comments(&Config::default(), false, None)
        .unwrap_err();
    assert!(!error.is_unwritable());
}

#[tokio::test]
async fn reads_and_undo_do_not_generate_or_refuse_the_stored_payload() {
    let raw = "debug: false\npayload:\n  default:\n  - params:\n      field: {1: placeholder}\n";
    let dir = AuthDir::new();
    let api = Api::over_config_file(&dir, raw);
    api.get("/v8/management/config/observability/logs/debug")
        .await
        .expect(StatusCode::OK);
    let answer = api.get("/v8/management/config.yaml").await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.body, raw);
    assert_eq!(std::fs::read(dir.config_path()).unwrap(), raw.as_bytes());
    open_ferry_core::config::save::write_file(&dir.config_path(), b"debug: true\n").unwrap();
    crate::undo_config(&api.state, crate::UndoCheck::default())
        .await
        .unwrap();
    assert_eq!(std::fs::read(dir.config_path()).unwrap(), raw.as_bytes());
}
