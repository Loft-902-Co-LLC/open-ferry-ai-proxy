// Ported from CLIProxyAPI internal/api/handlers/management/
// config_weight_test.go, config_priority_test.go,
// config_disable_cooling_test.go, config_codex_alpha_search_test.go,
// config_lists_delete_keys_test.go, config_meta_key_test.go,
// config_xai_key_test.go and config_claude_key_test.go
// (TestPatchClaudeKeyPriority) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The routes of `crate::config_keys`, which change the providers' API
//! keys.
//!
//! Upstream calls each handler with a config of its own and checks that
//! config, and some tests the file saved; here each test drives the router
//! over an API made by [`Api::writing`], and checks the config the
//! [`FakeWriter`](super::FakeWriter) is asked to save, which the handlers
//! then read.
//!
//! Deviations from upstream:
//! - `TestPatchPriorityForEveryProvider` and `TestPatchClaudeKeyPriority`
//!   also look for `priority: 7` (`20`) in the file saved; that part is
//!   ported in `config_v8_write`.
//! - The other tests in config_claude_key_test.go, and
//!   config_codex_disable_cloaking_test.go, are dropped: they test the
//!   client impersonation settings (`cloak`, `fingerprint-profile`,
//!   `disable-codex-cloaking`), which open-ferry doesn't read.

use http::{Method, StatusCode};
use open_ferry_core::config::{
    ClaudeKey, CodexKey, Config, GeminiKey, OpenAiCompatibility, OpenAiCompatibilityApiKey,
    VertexCompatKey,
};
use serde_json::Value;

use super::{Api, keyed_config};

const OK: &str = r#"{"status":"ok"}"#;

fn gemini(key: &str, base: &str) -> GeminiKey {
    GeminiKey {
        api_key: key.into(),
        base_url: base.into(),
        ..GeminiKey::default()
    }
}

fn claude(key: &str, base: &str) -> ClaudeKey {
    ClaudeKey {
        api_key: key.into(),
        base_url: base.into(),
        ..ClaudeKey::default()
    }
}

fn codex(key: &str, base: &str) -> CodexKey {
    CodexKey {
        api_key: key.into(),
        base_url: base.into(),
        ..CodexKey::default()
    }
}

fn vertex(key: &str, base: &str) -> VertexCompatKey {
    VertexCompatKey {
        api_key: key.into(),
        base_url: base.into(),
        ..VertexCompatKey::default()
    }
}

fn openai(name: &str, base: &str) -> OpenAiCompatibility {
    OpenAiCompatibility {
        name: name.into(),
        base_url: base.into(),
        ..OpenAiCompatibility::default()
    }
}

/// A config with the management key and, in list `name`, one entry with
/// key `key` and base URL `base`; in `openai-compatibility`, a provider
/// `compat` at `base`, without keys.
fn one_entry(name: &str, base: &str) -> Config {
    let mut config = keyed_config();
    match name {
        "gemini-api-key" => config.gemini_api_key = vec![gemini("key", base)],
        "interactions-api-key" => config.interactions_api_key = vec![gemini("key", base)],
        "claude-api-key" => config.claude_api_key = vec![claude("key", base)],
        "codex-api-key" => config.codex_api_key = vec![codex("key", base)],
        "xai-api-key" => config.xai_api_key = vec![codex("key", base)],
        "meta-api-key" => config.meta_api_key = vec![codex("key", base)],
        "vertex-api-key" => config.vertex_api_key = vec![vertex("key", base)],
        "openai-compatibility" => config.openai_compatibility = vec![openai("compat", base)],
        _ => panic!("no list {name}"),
    }
    config
}

/// The fields of a list's entry these tests check.
#[derive(Debug, PartialEq)]
struct Entry {
    priority: i64,
    weight: Option<i64>,
    prefix: String,
    disable_cooling: Option<bool>,
}

macro_rules! entry_from {
    ($($entry:ty),*) => {$(
        impl From<&$entry> for Entry {
            fn from(entry: &$entry) -> Self {
                Self {
                    priority: entry.priority,
                    weight: entry.weight,
                    prefix: entry.prefix.clone(),
                    disable_cooling: entry.disable_cooling,
                }
            }
        }
    )*};
}

entry_from!(GeminiKey, ClaudeKey, CodexKey, VertexCompatKey);

impl From<&OpenAiCompatibility> for Entry {
    fn from(provider: &OpenAiCompatibility) -> Self {
        Self {
            priority: provider.priority,
            weight: None,
            prefix: provider.prefix.clone(),
            disable_cooling: provider.disable_cooling,
        }
    }
}

/// The first entry of list `name`.
fn first(config: &Config, name: &str) -> Entry {
    match name {
        "gemini-api-key" => (&config.gemini_api_key[0]).into(),
        "interactions-api-key" => (&config.interactions_api_key[0]).into(),
        "claude-api-key" => (&config.claude_api_key[0]).into(),
        "codex-api-key" => (&config.codex_api_key[0]).into(),
        "xai-api-key" => (&config.xai_api_key[0]).into(),
        "meta-api-key" => (&config.meta_api_key[0]).into(),
        "vertex-api-key" => (&config.vertex_api_key[0]).into(),
        "openai-compatibility" => (&config.openai_compatibility[0]).into(),
        _ => panic!("no list {name}"),
    }
}

/// The `disable-cooling` of the first entry of list `name`.
fn disable_cooling_mut<'a>(config: &'a mut Config, name: &str) -> &'a mut Option<bool> {
    match name {
        "gemini-api-key" => &mut config.gemini_api_key[0].disable_cooling,
        "interactions-api-key" => &mut config.interactions_api_key[0].disable_cooling,
        "claude-api-key" => &mut config.claude_api_key[0].disable_cooling,
        "codex-api-key" => &mut config.codex_api_key[0].disable_cooling,
        "xai-api-key" => &mut config.xai_api_key[0].disable_cooling,
        "meta-api-key" => &mut config.meta_api_key[0].disable_cooling,
        "vertex-api-key" => &mut config.vertex_api_key[0].disable_cooling,
        "openai-compatibility" => &mut config.openai_compatibility[0].disable_cooling,
        _ => panic!("no list {name}"),
    }
}

/// `method /v0/management/<name><query>` with `body`.
async fn send(api: &Api, method: Method, name: &str, body: &str) -> super::Answer {
    api.call(method, &format!("/v0/management/{name}"), body)
        .await
}

/// `method /v0/management/<name>` with `body`, which must answer
/// `{"status":"ok"}`; then the config saved.
async fn change(api: &Api, method: Method, name: &str, body: &str) -> Config {
    let answer = send(api, method.clone(), name, body).await;
    assert_eq!(
        (answer.status, answer.body.as_str()),
        (StatusCode::OK, OK),
        "{method} {name} {body}"
    );
    api.saved()
}

/// `method /v0/management/<name>` with `body`, which must answer `status`
/// with `error`, and leave the config as it was, unsaved.
async fn refused(
    api: &Api,
    method: Method,
    name: &str,
    body: &str,
    status: StatusCode,
    error: &str,
) {
    let before = api.state.config();
    let saves = api.writer.written().len();
    let answer = send(api, method.clone(), name, body).await;
    let want = format!(r#"{{"error":"{error}"}}"#);
    assert_eq!(
        (answer.status, answer.body.as_str()),
        (status, want.as_str()),
        "{method} {name} {body}"
    );
    assert!(*api.state.config() == *before, "{method} {name} {body}");
    assert_eq!(api.writer.written().len(), saves, "{method} {name} {body}");
}

/// Ported from upstream's config_weight_test.go
/// (TestPatchAPIKeyWeightForEveryFamily).
#[tokio::test]
async fn patch_api_key_weight_for_every_family() {
    for (name, base) in [
        ("gemini-api-key", ""),
        ("interactions-api-key", ""),
        ("claude-api-key", ""),
        ("vertex-api-key", "https://example.com"),
        ("codex-api-key", "https://example.com"),
        ("xai-api-key", "https://example.com"),
        ("meta-api-key", "https://example.com"),
    ] {
        let api = Api::writing(one_entry(name, base));
        let config = change(
            &api,
            Method::PATCH,
            name,
            r#"{"index":0,"value":{"weight":7}}"#,
        )
        .await;
        assert_eq!(first(&config, name).weight, Some(7), "{name}");
    }
}

/// Ported from upstream's config_weight_test.go
/// (TestPatchAPIKeyWeightResetAndStrictValidation), with the messages.
#[tokio::test]
async fn patch_api_key_weight_reset_and_strict_validation() {
    let mut config = keyed_config();
    config.gemini_api_key = vec![GeminiKey {
        weight: Some(5),
        ..gemini("key", "")
    }];
    let api = Api::writing(config);
    for (invalid, error) in [
        ("1.5", "weight must be an integer"),
        ("1000001", "weight must not exceed 1000000"),
        ("9223372036854775808", "weight must be an integer"),
        (r#""7""#, "weight must be an integer"),
    ] {
        let body = format!(r#"{{"index":0,"value":{{"weight":{invalid}}}}}"#);
        refused(
            &api,
            Method::PATCH,
            "gemini-api-key",
            &body,
            StatusCode::BAD_REQUEST,
            error,
        )
        .await;
        assert_eq!(api.state.config().gemini_api_key[0].weight, Some(5));
    }

    let body = r#"{"index":0,"value":{"weight":null}}"#;
    let config = change(&api, Method::PATCH, "gemini-api-key", body).await;
    assert_eq!(config.gemini_api_key[0].weight, None);
}

/// Ported from upstream's config_weight_test.go
/// (TestPutAPIKeyWeightRejectsAboveMaximum), with the message.
#[tokio::test]
async fn put_api_key_weight_rejects_above_maximum() {
    let api = Api::writing(keyed_config());
    refused(
        &api,
        Method::PUT,
        "gemini-api-key",
        r#"[{"api-key":"key","weight":1000001}]"#,
        StatusCode::BAD_REQUEST,
        "gemini-api-key[0].weight: weight must not exceed 1000000",
    )
    .await;
    assert!(api.state.config().gemini_api_key.is_empty());
}

// Not upstream's: every list's PUT checks its weights, naming the entry.
#[tokio::test]
async fn put_weights_name_the_entry() {
    let api = Api::writing(keyed_config());
    for (name, body, error) in [
        (
            "claude-api-key",
            r#"[{"api-key":"a"},{"api-key":"b","weight":2000000}]"#,
            "claude-api-key[1].weight: weight must not exceed 1000000",
        ),
        (
            "codex-api-key",
            r#"{"items":[{"api-key":"a","base-url":"https://c","weight":1000001}]}"#,
            "codex-api-key[0].weight: weight must not exceed 1000000",
        ),
        (
            "vertex-api-key",
            r#"[{"api-key":"a","weight":1000001}]"#,
            "vertex-api-key[0].weight: weight must not exceed 1000000",
        ),
        (
            "openai-compatibility",
            r#"[{"name":"o","base-url":"https://o","api-key-entries":[{"api-key":"k"},{"api-key":"l","weight":1000001}]}]"#,
            "openai-compatibility[0].api-key-entries[1].weight: weight must not exceed 1000000",
        ),
    ] {
        refused(
            &api,
            Method::PUT,
            name,
            body,
            StatusCode::BAD_REQUEST,
            error,
        )
        .await;
    }
    let mut config = one_entry("openai-compatibility", "https://o");
    config.openai_compatibility[0].name = "o".into();
    let api = Api::writing(config);
    refused(
        &api,
        Method::PATCH,
        "openai-compatibility",
        r#"{"name":"o","value":{"api-key-entries":[{"api-key":"k","weight":1000001}]}}"#,
        StatusCode::BAD_REQUEST,
        "api-key-entries[0].weight: weight must not exceed 1000000",
    )
    .await;
}

/// Ported from upstream's config_priority_test.go
/// (TestPatchPriorityForEveryProvider), but for the file.
#[tokio::test]
async fn patch_priority_for_every_provider() {
    for (name, base) in [
        ("claude-api-key", ""),
        ("xai-api-key", "https://example.com"),
        ("meta-api-key", "https://example.com"),
        ("codex-api-key", "https://example.com"),
        ("gemini-api-key", ""),
        ("interactions-api-key", ""),
        ("vertex-api-key", "https://example.com"),
        ("openai-compatibility", "https://compat.example.com"),
    ] {
        let api = Api::writing(one_entry(name, base));
        let patch = |value: &str| format!(r#"{{"index":0,"value":{value}}}"#);

        let config = change(&api, Method::PATCH, name, &patch(r#"{"priority":7}"#)).await;
        assert_eq!(first(&config, name).priority, 7, "{name}");
        // Omitting the priority keeps it.
        let config = change(&api, Method::PATCH, name, &patch(r#"{"prefix":"team-a"}"#)).await;
        let entry = first(&config, name);
        assert_eq!(
            (entry.priority, entry.prefix.as_str()),
            (7, "team-a"),
            "{name}"
        );
        let config = change(&api, Method::PATCH, name, &patch(r#"{"priority":0}"#)).await;
        assert_eq!(first(&config, name).priority, 0, "{name}");
    }
}

/// Ported from upstream's config_disable_cooling_test.go
/// (TestPatchDisableCoolingOverrideForEveryFamily), and a value that isn't
/// a boolean.
#[tokio::test]
async fn patch_disable_cooling_override_for_every_family() {
    for (name, base) in [
        ("gemini-api-key", ""),
        ("interactions-api-key", ""),
        ("claude-api-key", ""),
        ("openai-compatibility", "https://compat.example.com"),
        ("vertex-api-key", "https://vertex.example.com"),
        ("codex-api-key", "https://codex.example.com"),
        ("xai-api-key", "https://api.x.ai/v1"),
        ("meta-api-key", "https://api.meta.ai/v1"),
    ] {
        let mut config = one_entry(name, base);
        *disable_cooling_mut(&mut config, name) = Some(true);
        if name == "openai-compatibility" {
            config.openai_compatibility[0].api_key_entries = vec![OpenAiCompatibilityApiKey {
                api_key: "key".into(),
                ..OpenAiCompatibilityApiKey::default()
            }];
        }
        let api = Api::writing(config);
        let patch = |value: &str| format!(r#"{{"index":0,"value":{{"disable-cooling":{value}}}}}"#);

        let config = change(&api, Method::PATCH, name, &patch("false")).await;
        assert_eq!(first(&config, name).disable_cooling, Some(false), "{name}");
        let config = change(&api, Method::PATCH, name, &patch("null")).await;
        assert_eq!(first(&config, name).disable_cooling, None, "{name}");
        refused(
            &api,
            Method::PATCH,
            name,
            &patch(r#""yes""#),
            StatusCode::BAD_REQUEST,
            "disable-cooling must be a boolean or null",
        )
        .await;
    }
}

/// Ported from upstream's config_codex_alpha_search_test.go
/// (TestPatchCodexKeySupportConfigurationUpdate).
#[tokio::test]
async fn patch_codex_key_support_configuration_update() {
    let api = Api::writing(one_entry("codex-api-key", "https://codex.example.com"));
    for (method, body, enabled) in [
        (
            Method::PATCH,
            r#"{"index":0,"value":{"models":[{"name":"custom-one","support-configuration-update":true},{"name":"custom-two"}]}}"#,
            "custom-one",
        ),
        (
            Method::PUT,
            r#"[{"api-key":"key","base-url":"https://codex.example.com","models":[{"name":"custom-two","support-configuration-update":true},{"name":"custom-one"}]}]"#,
            "custom-two",
        ),
    ] {
        let config = change(&api, method.clone(), "codex-api-key", body).await;
        assert_eq!(config.codex_api_key.len(), 1, "{method}");
        let models = &config.codex_api_key[0].models;
        assert_eq!(models.len(), 2, "{method}");
        assert_eq!(models[0].name, enabled, "{method}");
        assert!(models[0].support_configuration_update, "{method}");
        assert!(!models[1].support_configuration_update, "{method}");

        let listed = api
            .get("/v0/management/codex-api-key")
            .await
            .expect(StatusCode::OK);
        let models = &listed["codex-api-key"][0]["models"];
        assert_eq!(models[0]["name"], enabled, "{listed}");
        assert_eq!(models[0]["support-configuration-update"], true, "{listed}");
        assert_ne!(models[1]["support-configuration-update"], true, "{listed}");
        assert!(models[2].is_null(), "{listed}");
    }
}

/// Ported from upstream's config_codex_alpha_search_test.go
/// (TestPatchCodexKeyUpdatesAlphaSearch), and xAI's and Meta's keys, which
/// are saved without it.
#[tokio::test]
async fn patch_codex_key_updates_alpha_search() {
    let body = r#"{"index":0,"value":{"alpha-search":true}}"#;
    let api = Api::writing(one_entry("codex-api-key", "https://codex.example.com"));
    let config = change(&api, Method::PATCH, "codex-api-key", body).await;
    assert!(config.codex_api_key[0].alpha_search);

    for name in ["xai-api-key", "meta-api-key"] {
        let api = Api::writing(one_entry(name, "https://example.com"));
        let config = change(&api, Method::PATCH, name, body).await;
        let keys = match name {
            "xai-api-key" => &config.xai_api_key,
            _ => &config.meta_api_key,
        };
        assert!(!keys[0].alpha_search, "{name}");
    }
}

/// Ported from upstream's config_meta_key_test.go
/// (TestPatchMetaKeyUpdatesExecutionFields).
#[tokio::test]
async fn patch_meta_key_updates_execution_fields() {
    let mut config = keyed_config();
    config.meta_api_key = vec![CodexKey {
        priority: 1,
        disable_cooling: Some(false),
        ..codex("meta-key", "https://api.meta.ai/v1")
    }];
    let api = Api::writing(config);
    let body = r#"{"index":0,"value":{"priority":7,"disable-cooling":true,"request-retry":0}}"#;
    let config = change(&api, Method::PATCH, "meta-api-key", body).await;
    let entry = &config.meta_api_key[0];
    assert_eq!(entry.priority, 7);
    assert_eq!(entry.disable_cooling, Some(true));
    assert_eq!(entry.request_retry, Some(0));
}

/// Ported from upstream's config_xai_key_test.go
/// (TestPatchXAIKeyUpdatesExecutionFields).
#[tokio::test]
async fn patch_xai_key_updates_execution_fields() {
    let mut config = keyed_config();
    config.xai_api_key = vec![CodexKey {
        priority: 1,
        websockets: true,
        disable_cooling: Some(false),
        ..codex("xai-key", "https://api.x.ai/v1")
    }];
    let api = Api::writing(config);
    let body = r#"{
        "index": 0,
        "value": {
            "priority": 7,
            "websockets": false,
            "disable-cooling": true,
            "request-retry": 0
        }
    }"#;
    let config = change(&api, Method::PATCH, "xai-api-key", body).await;
    let entry = &config.xai_api_key[0];
    assert_eq!(entry.priority, 7);
    assert!(!entry.websockets);
    assert_eq!(entry.disable_cooling, Some(true));
    assert_eq!(entry.request_retry, Some(0));
}

/// Ported from upstream's config_claude_key_test.go
/// (TestPatchClaudeKeyPriority), but for the file.
#[tokio::test]
async fn patch_claude_key_priority() {
    let mut config = keyed_config();
    config.claude_api_key = vec![
        claude("key-0", ""),
        ClaudeKey {
            priority: 5,
            ..claude("key-1", "")
        },
    ];
    let api = Api::writing(config);
    let name = "claude-api-key";

    let config = change(
        &api,
        Method::PATCH,
        name,
        r#"{"index":1,"value":{"priority":20}}"#,
    )
    .await;
    assert_eq!(config.claude_api_key[1].priority, 20);
    let listed = api
        .get("/v0/management/claude-api-key")
        .await
        .expect(StatusCode::OK);
    let keys = listed["claude-api-key"].as_array().unwrap();
    assert_eq!(keys.len(), 2, "{listed}");
    assert_eq!(keys[1]["priority"], 20, "{listed}");

    let body = r#"{"index":1,"value":{"prefix":"team-test"}}"#;
    let config = change(&api, Method::PATCH, name, body).await;
    assert_eq!(config.claude_api_key[1].priority, 20);
    assert_eq!(config.claude_api_key[1].prefix, "team-test");

    let config = change(
        &api,
        Method::PATCH,
        name,
        r#"{"index":1,"value":{"priority":0}}"#,
    )
    .await;
    assert_eq!(config.claude_api_key[1].priority, 0);

    refused(
        &api,
        Method::PATCH,
        name,
        r#"{"index":1,"value":{"priority":"invalid"}}"#,
        StatusCode::BAD_REQUEST,
        "invalid body",
    )
    .await;
}

/// A config with the management key, and the Gemini keys `keys`.
fn with_gemini(keys: Vec<GeminiKey>) -> Config {
    let mut config = keyed_config();
    config.gemini_api_key = keys;
    config
}

/// Ported from upstream's config_lists_delete_keys_test.go
/// (TestDeleteGeminiKey_RequiresBaseURLWhenAPIKeyDuplicated), with the
/// message.
#[tokio::test]
async fn delete_gemini_key_requires_base_url_when_api_key_duplicated() {
    let api = Api::writing(with_gemini(vec![
        gemini("shared-key", "https://a.example.com"),
        gemini("shared-key", "https://b.example.com"),
    ]));
    refused(
        &api,
        Method::DELETE,
        "gemini-api-key?api-key=shared-key",
        "",
        StatusCode::BAD_REQUEST,
        "multiple items match api-key; base-url is required",
    )
    .await;
    assert_eq!(api.state.config().gemini_api_key.len(), 2);
}

/// Ported from upstream's config_lists_delete_keys_test.go
/// (TestDeleteGeminiKey_DeletesOnlyMatchingBaseURL).
#[tokio::test]
async fn delete_gemini_key_deletes_only_matching_base_url() {
    let api = Api::writing(with_gemini(vec![
        gemini("shared-key", "https://a.example.com"),
        gemini("shared-key", "https://b.example.com"),
    ]));
    let path = "gemini-api-key?api-key=shared-key&base-url=https://a.example.com";
    let config = change(&api, Method::DELETE, path, "").await;
    assert_eq!(config.gemini_api_key.len(), 1);
    assert_eq!(config.gemini_api_key[0].base_url, "https://b.example.com");
}

/// Ported from upstream's config_lists_delete_keys_test.go
/// (TestDeleteGeminiStyleKeyRejectsAmbiguousRoutingIdentity), with the
/// message.
#[tokio::test]
async fn delete_gemini_style_key_rejects_ambiguous_routing_identity() {
    for name in ["gemini-api-key", "interactions-api-key"] {
        let entries = vec![
            GeminiKey {
                prefix: "team-a".into(),
                ..gemini("shared-key", "https://shared.example.com")
            },
            GeminiKey {
                prefix: "team-b".into(),
                ..gemini("shared-key", "https://shared.example.com")
            },
        ];
        let mut config = keyed_config();
        match name {
            "gemini-api-key" => config.gemini_api_key = entries,
            _ => config.interactions_api_key = entries,
        }
        let api = Api::writing(config);
        let path = format!("{name}?api-key=shared-key&base-url=https://shared.example.com");
        refused(
            &api,
            Method::DELETE,
            &path,
            "",
            StatusCode::BAD_REQUEST,
            "multiple items match; index is required",
        )
        .await;
    }
}

/// Ported from upstream's config_lists_delete_keys_test.go
/// (TestPatchGeminiStyleKeyRoutingIdentity).
#[tokio::test]
async fn patch_gemini_style_key_routing_identity() {
    for name in ["gemini-api-key", "interactions-api-key"] {
        for (first_base, unique) in [
            ("https://first.example.com", true),
            ("https://shared.example.com", false),
        ] {
            let entries = vec![
                GeminiKey {
                    prefix: "team-a".into(),
                    ..gemini("shared-key", first_base)
                },
                GeminiKey {
                    prefix: "team-b".into(),
                    ..gemini("shared-key", "https://shared.example.com")
                },
            ];
            let mut config = keyed_config();
            match name {
                "gemini-api-key" => config.gemini_api_key = entries,
                _ => config.interactions_api_key = entries,
            }
            let api = Api::writing(config);
            let path = format!("{name}?base-url=https://shared.example.com");
            let body = r#"{"match":"shared-key","value":{"prefix":"updated"}}"#;
            if unique {
                let config = change(&api, Method::PATCH, &path, body).await;
                let keys = match name {
                    "gemini-api-key" => &config.gemini_api_key,
                    _ => &config.interactions_api_key,
                };
                let prefixes: Vec<&str> = keys.iter().map(|key| key.prefix.as_str()).collect();
                assert_eq!(prefixes, ["team-a", "updated"], "{name}");
            } else {
                refused(
                    &api,
                    Method::PATCH,
                    &path,
                    body,
                    StatusCode::BAD_REQUEST,
                    "multiple items match; index is required",
                )
                .await;
            }
        }
    }
}

/// Ported from upstream's config_lists_delete_keys_test.go
/// (TestDeleteClaudeKey_DeletesEmptyBaseURLWhenExplicitlyProvided).
#[tokio::test]
async fn delete_claude_key_deletes_empty_base_url_when_explicitly_provided() {
    let mut config = keyed_config();
    config.claude_api_key = vec![
        claude("shared-key", ""),
        claude("shared-key", "https://claude.example.com"),
    ];
    let api = Api::writing(config);
    let path = "claude-api-key?api-key=shared-key&base-url=";
    let config = change(&api, Method::DELETE, path, "").await;
    assert_eq!(config.claude_api_key.len(), 1);
    assert_eq!(
        config.claude_api_key[0].base_url,
        "https://claude.example.com"
    );
}

/// Ported from upstream's config_lists_delete_keys_test.go
/// (TestDeleteVertexCompatKey_DeletesOnlyMatchingBaseURL).
#[tokio::test]
async fn delete_vertex_compat_key_deletes_only_matching_base_url() {
    let mut config = keyed_config();
    config.vertex_api_key = vec![
        vertex("shared-key", "https://a.example.com"),
        vertex("shared-key", "https://b.example.com"),
    ];
    let api = Api::writing(config);
    let path = "vertex-api-key?api-key=shared-key&base-url=https://b.example.com";
    let config = change(&api, Method::DELETE, path, "").await;
    assert_eq!(config.vertex_api_key.len(), 1);
    assert_eq!(config.vertex_api_key[0].base_url, "https://a.example.com");
}

/// Ported from upstream's config_lists_delete_keys_test.go
/// (TestDeleteXAIKey_RequiresBaseURLWhenAPIKeyDuplicated,
/// TestDeleteMetaKey_RequiresBaseURLWhenAPIKeyDuplicated and
/// TestDeleteCodexKey_RequiresBaseURLWhenAPIKeyDuplicated), with the
/// message.
#[tokio::test]
async fn delete_codex_style_key_requires_base_url_when_api_key_duplicated() {
    for name in ["xai-api-key", "meta-api-key", "codex-api-key"] {
        let keys = vec![
            codex("shared-key", "https://a.example.com"),
            codex("shared-key", "https://b.example.com"),
        ];
        let mut config = keyed_config();
        match name {
            "xai-api-key" => config.xai_api_key = keys,
            "meta-api-key" => config.meta_api_key = keys,
            _ => config.codex_api_key = keys,
        }
        let api = Api::writing(config);
        refused(
            &api,
            Method::DELETE,
            &format!("{name}?api-key=shared-key"),
            "",
            StatusCode::BAD_REQUEST,
            "multiple items match api-key; base-url is required",
        )
        .await;
    }
}

// Not upstream's: what DELETE does with one key, an index, a name, or
// nothing.
#[tokio::test]
async fn delete_by_key_index_or_name() {
    let mut config = keyed_config();
    config.claude_api_key = vec![claude("a", ""), claude(" b ", ""), claude("c", "")];
    config.gemini_api_key = vec![gemini("g", "")];
    config.openai_compatibility = vec![
        openai("o", "https://1"),
        openai("p", "https://2"),
        openai("o", "https://3"),
    ];
    let api = Api::writing(config);

    let config = change(&api, Method::DELETE, "claude-api-key?api-key=%20b", "").await;
    let keys: Vec<&str> = config
        .claude_api_key
        .iter()
        .map(|key| key.api_key.as_str())
        .collect();
    assert_eq!(keys, ["a", "c"]);
    // A key no entry has removes nothing, but for Gemini keys.
    change(&api, Method::DELETE, "claude-api-key?api-key=none", "").await;
    assert_eq!(api.state.config().claude_api_key.len(), 2);
    refused(
        &api,
        Method::DELETE,
        "gemini-api-key?api-key=none",
        "",
        StatusCode::NOT_FOUND,
        "item not found",
    )
    .await;
    refused(
        &api,
        Method::DELETE,
        "gemini-api-key?api-key=g&base-url=https://none",
        "",
        StatusCode::NOT_FOUND,
        "item not found",
    )
    .await;
    let config = change(&api, Method::DELETE, "claude-api-key?index=1", "").await;
    assert_eq!(config.claude_api_key.len(), 1);
    for query in ["", "?index=5", "?index=-1", "?index=x"] {
        refused(
            &api,
            Method::DELETE,
            &format!("claude-api-key{query}"),
            "",
            StatusCode::BAD_REQUEST,
            "missing api-key or index",
        )
        .await;
    }

    let config = change(&api, Method::DELETE, "openai-compatibility?name=o", "").await;
    let names: Vec<&str> = config
        .openai_compatibility
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    assert_eq!(names, ["p"]);
    refused(
        &api,
        Method::DELETE,
        "openai-compatibility?index=3",
        "",
        StatusCode::BAD_REQUEST,
        "missing name or index",
    )
    .await;
    let config = change(&api, Method::DELETE, "openai-compatibility?index=0", "").await;
    assert!(config.openai_compatibility.is_empty());
}

// Not upstream's: PUT replaces a list, normalized as upstream normalizes
// each list.
#[tokio::test]
async fn put_normalizes_each_list() {
    let api = Api::writing(keyed_config());
    let body = r#"[{"api-key":" a ","base-url":" https://c ","prefix":" /p/ "},{"api-key":"b"}]"#;
    let config = change(&api, Method::PUT, "codex-api-key", body).await;
    assert_eq!(config.codex_api_key.len(), 1);
    let key = &config.codex_api_key[0];
    assert_eq!(
        (
            key.api_key.as_str(),
            key.base_url.as_str(),
            key.prefix.as_str()
        ),
        ("a", "https://c", "p")
    );

    let body =
        r#"{"items":[{"api-key":"x","base-url":"https://x","alpha-search":true},{"api-key":"y"}]}"#;
    let config = change(&api, Method::PUT, "xai-api-key", body).await;
    assert_eq!(config.xai_api_key.len(), 1);
    assert!(!config.xai_api_key[0].alpha_search);

    let config = change(&api, Method::PUT, "meta-api-key", r#"[{"api-key":"m"}]"#).await;
    assert_eq!(config.meta_api_key[0].base_url, "https://api.meta.ai/v1");

    let body = r#"[{"api-key":" v ","models":[{"name":"n"},{"name":" n ","alias":" a "}]}]"#;
    let config = change(&api, Method::PUT, "vertex-api-key", body).await;
    let key = &config.vertex_api_key[0];
    assert_eq!(key.api_key, "v");
    assert_eq!(key.models.len(), 1);
    assert_eq!(
        (key.models[0].name.as_str(), key.models[0].alias.as_str()),
        ("n", "a")
    );
    refused(
        &api,
        Method::PUT,
        "vertex-api-key",
        r#"[{"api-key":"v"},{"api-key":" "}]"#,
        StatusCode::BAD_REQUEST,
        "vertex-api-key[1].api-key is required",
    )
    .await;

    let body = r#"[{"name":" o ","base-url":" https://o ","api-key-entries":[{"api-key":" k ","weight":3}]},{"name":"none"}]"#;
    let config = change(&api, Method::PUT, "openai-compatibility", body).await;
    assert_eq!(config.openai_compatibility.len(), 1);
    let provider = &config.openai_compatibility[0];
    assert_eq!(
        (provider.name.as_str(), provider.base_url.as_str()),
        ("o", "https://o")
    );
    assert_eq!(provider.api_key_entries[0].api_key, "k");
    assert_eq!(provider.api_key_entries[0].weight, Some(3));

    let body =
        r#"[{"api-key":" c ","models":[{"name":" m "},{"name":" "}],"excluded-models":["A"]}]"#;
    let config = change(&api, Method::PUT, "claude-api-key", body).await;
    let key = &config.claude_api_key[0];
    assert_eq!(key.api_key, "c");
    assert_eq!(key.models.len(), 1);
    assert_eq!(key.models[0].name, "m");
    assert_eq!(key.excluded_models, ["a"]);

    let config = change(
        &api,
        Method::PUT,
        "gemini-api-key",
        r#"[{"api-key":"g"},{"api-key":"g"}]"#,
    )
    .await;
    assert_eq!(config.gemini_api_key.len(), 1);
    let config = change(&api, Method::PUT, "interactions-api-key", "[]").await;
    assert!(config.interactions_api_key.is_empty());

    assert!(api.writer.saved().iter().all(|(_, migrate)| !migrate));
}

// Not upstream's: a PATCH that leaves an entry without what identifies it
// removes the entry, but for Meta's, which gets the default base URL.
#[tokio::test]
async fn patch_removes_an_entry_left_without_its_identity() {
    let mut config = keyed_config();
    config.gemini_api_key = vec![gemini("g", ""), gemini("h", "")];
    config.codex_api_key = vec![codex("c", "https://c")];
    config.xai_api_key = vec![codex("x", "https://x")];
    config.meta_api_key = vec![codex("m", "https://m")];
    config.openai_compatibility = vec![openai("o", "https://o")];
    config.vertex_api_key = vec![vertex("v", "")];
    let api = Api::writing(config);

    let body = r#"{"match":"g","value":{"api-key":" "}}"#;
    let config = change(&api, Method::PATCH, "gemini-api-key", body).await;
    assert_eq!(config.gemini_api_key.len(), 1);
    assert_eq!(config.gemini_api_key[0].api_key, "h");
    let body = r#"{"index":0,"value":{"base-url":" "}}"#;
    let config = change(&api, Method::PATCH, "codex-api-key", body).await;
    assert!(config.codex_api_key.is_empty());
    let config = change(&api, Method::PATCH, "xai-api-key", body).await;
    assert!(config.xai_api_key.is_empty());
    let config = change(&api, Method::PATCH, "meta-api-key", body).await;
    assert_eq!(config.meta_api_key[0].base_url, "https://api.meta.ai/v1");
    let body = r#"{"name":"o","value":{"base-url":""}}"#;
    let config = change(&api, Method::PATCH, "openai-compatibility", body).await;
    assert!(config.openai_compatibility.is_empty());
    let body = r#"{"match":"v","value":{"api-key":""}}"#;
    let config = change(&api, Method::PATCH, "vertex-api-key", body).await;
    assert!(config.vertex_api_key.is_empty());
}

// Not upstream's: a PATCH finds its entry by index, else by key (name),
// and changes only the fields it gives.
#[tokio::test]
async fn patch_finds_the_entry_and_changes_what_it_gives() {
    let mut config = keyed_config();
    config.claude_api_key = vec![claude("a", ""), claude("b", "https://b")];
    config.openai_compatibility = vec![openai("o", "https://o"), openai("p", "https://p")];
    let api = Api::writing(config);

    let body = r#"{"index":7,"match":" b ","value":{"headers":{" X ":" 1 ","Y":" "},"excluded-models":[" M "],"rebuild-mid-system-message":true,"request-retry":2}}"#;
    let config = change(&api, Method::PATCH, "claude-api-key", body).await;
    let key = &config.claude_api_key[1];
    assert_eq!(key.headers.len(), 1);
    assert_eq!(key.headers["X"], "1");
    assert_eq!(key.excluded_models, ["m"]);
    assert!(key.rebuild_mid_system_message);
    assert_eq!(key.request_retry, Some(2));
    assert_eq!(key.base_url, "https://b");
    assert!(config.claude_api_key[0] == claude("a", ""));

    let body = r#"{"name":"p","value":{"name":" q ","disabled":true,"support-prompt-cache-key":true,"models":[{"name":"m","alias":"n"}]}}"#;
    let config = change(&api, Method::PATCH, "openai-compatibility", body).await;
    let provider = &config.openai_compatibility[1];
    assert_eq!(provider.name, "q");
    assert!(provider.disabled && provider.support_prompt_cache_key);
    assert_eq!(provider.models[0].alias, "n");

    for (name, body) in [
        ("claude-api-key", r#"{"match":"none","value":{}}"#),
        ("claude-api-key", r#"{"index":-1,"value":{}}"#),
        ("openai-compatibility", r#"{"name":"none","value":{}}"#),
        ("openai-compatibility", r#"{"match":"o","value":{}}"#),
        ("vertex-api-key", r#"{"match":" ","value":{}}"#),
        ("gemini-api-key", r#"{"value":{}}"#),
    ] {
        refused(
            &api,
            Method::PATCH,
            name,
            body,
            StatusCode::NOT_FOUND,
            "item not found",
        )
        .await;
    }
    let mut config = keyed_config();
    config.gemini_api_key = vec![gemini("g", "https://1"), gemini("g", "https://2")];
    let api = Api::writing(config);
    refused(
        &api,
        Method::PATCH,
        "gemini-api-key",
        r#"{"match":"g","value":{"priority":1}}"#,
        StatusCode::BAD_REQUEST,
        "multiple items match; index is required",
    )
    .await;
}

// Not upstream's: bodies that don't read as the request.
#[tokio::test]
async fn invalid_bodies() {
    let api = Api::writing(one_entry("codex-api-key", "https://c"));
    for (method, body) in [
        (Method::PUT, ""),
        (Method::PUT, "{"),
        (Method::PUT, r#"{"items":[]}"#),
        (Method::PUT, r#"[{"api-key":1}]"#),
        (Method::PATCH, ""),
        (Method::PATCH, r#"{"index":0}"#),
        (Method::PATCH, r#"{"index":0,"value":null}"#),
        (Method::PATCH, r#"{"index":"0","value":{}}"#),
        (Method::PATCH, r#"{"index":0,"value":[]}"#),
        (Method::PATCH, r#"{"index":0,"value":{"models":{}}}"#),
        (
            Method::PATCH,
            r#"{"index":0,"value":{"alpha-search":"yes"}}"#,
        ),
    ] {
        refused(
            &api,
            method,
            "codex-api-key",
            body,
            StatusCode::BAD_REQUEST,
            "invalid body",
        )
        .await;
    }
    // The impersonation settings are skipped, whatever their value.
    let body = r#"{"index":0,"value":{"disable-codex-cloaking":"x","priority":3}}"#;
    let config = change(&api, Method::PATCH, "codex-api-key", body).await;
    assert_eq!(config.codex_api_key[0].priority, 3);
    let api = Api::writing(one_entry("claude-api-key", ""));
    let body = r#"{"index":0,"value":{"cloak":{"mode":"bogus"},"fingerprint-profile":"bogus","priority":4}}"#;
    let config = change(&api, Method::PATCH, "claude-api-key", body).await;
    assert_eq!(config.claude_api_key[0].priority, 4);
    let body = r#"[{"api-key":"c","cloak":{"mode":"bogus"},"fingerprint-profile":"bogus"}]"#;
    let config = change(&api, Method::PUT, "claude-api-key", body).await;
    assert_eq!(config.claude_api_key[0].api_key, "c");
}

// Not upstream's: the listing after a change shows the change.
#[tokio::test]
async fn listing_shows_the_change() {
    let api = Api::writing(keyed_config());
    change(
        &api,
        Method::PUT,
        "vertex-api-key",
        r#"[{"api-key":"v","priority":3}]"#,
    )
    .await;
    let listed = api
        .get("/v0/management/vertex-api-key")
        .await
        .expect(StatusCode::OK);
    let keys = match &listed["vertex-api-key"] {
        Value::Array(keys) => keys.clone(),
        other => panic!("not a list: {other}"),
    };
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0]["priority"], 3);
    assert_eq!(api.reload.count(), 1);
}
