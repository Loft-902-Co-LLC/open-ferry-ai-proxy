//! Ports these CLIProxyAPI tests (v8.0.10, MIT):
//! - internal/api/server_test.go: `TestModelsWithClientVersionReturnsCodexCatalog`,
//!   `TestCodexClientModelsEndpoint_FiltersMaxAndUltraForOlderClientVersion`
//!   and `TestModelsWithClientVersionApplyPatchRequiresExecutor`;
//! - internal/api/server_apply_patch_config_test.go and
//!   server_multi_agent_config_test.go;
//! - sdk/api/handlers/openai/codex_client_models_test.go;
//! - sdk/api/handlers/apply_patch_capability_test.go;
//! - sdk/cliproxy/auth/apply_patch_capability_test.go.
//!
//! Whether a provider takes `apply_patch` goes by its name here (see the
//! module docs), so upstream's test executors become provider names: one
//! that supports the tool becomes `codex` or `openai-compatible-custom`, one
//! that says it doesn't becomes `denied`, one without the capability
//! becomes `remote`, and a provider without an executor becomes `unknown`.
//! Nothing serves those last three, so none of them takes the tool.
//!
//! Changed:
//! - `TestModelsWithClientVersionApplyPatchRequiresExecutor` serves its
//!   models through `unknown` rather than through `codex` without an
//!   executor, as `codex` always takes the tool here.
//! - The config tests replace the config, as a reload does, and only their
//!   `local` case is ported: Home isn't.
//! - `TestCodexClientModelsApplyPatchRouting` doesn't check a provider
//!   before and after its executor is registered, an executor that loses
//!   the capability, or a handler without a credential manager: providers
//!   are known by name.
//! - `TestApplyPatchManagerAllCandidates` checks provider names, and also
//!   how they match; the nil manager case is dropped.
//! - `TestApplyPatchModelExactPublicRoute` drops its absent handler case.
//!
//! Added: `apply_patch_routing` and `apply_patch_needs_every_provider` also
//! check that `gemini` and `vertex` take the tool, as upstream's Gemini and
//! Vertex AI executors say they do, and `apply_patch_needs_every_provider`
//! that `gemini-interactions` and `meta` do, as upstream's Gemini
//! Interactions executor (a Gemini executor) and Meta executor say, while
//! `xai` doesn't.
//!
//! Dropped: `TestCodexClientModelsResponse_DevinDisplayName` and
//! `TestModelsWithClientVersion_DevinDisplayName`, as Devin isn't ported, and
//! `TestModelsWithClientVersionHomeApplyPatchRouting`, as Home isn't.

use std::sync::Arc;

use axum::body::Body;
use http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use open_ferry_core::config::{CodexClientConfig, Config};
use open_ferry_core::models::{ModelInfo, ThinkingSupport};
use open_ferry_core::registry::ModelRegistry;
use open_ferry_core::registry::codex_client::CodexClientCatalog;
use serde_json::{Map, Value, json};
use tower::ServiceExt;

use super::*;
use crate::config::ServerConfig;
use crate::testing::FakeDispatcher;

/// The image and video models the Codex list hides.
const HIDDEN_MODELS: [&str; 10] = [
    "grok-imagine-image-quality",
    "gpt-image-2",
    "gpt-image-2.5-flare",
    "gpt-image-2.5-sunburst",
    "gpt-image-2.5",
    "grok-imagine-image",
    "grok-imagine-image-2.0",
    "grok-imagine-video",
    "grok-imagine-video-1.5",
    "grok-imagine-video-1.5-preview",
];

/// The client versions the apply_patch tests ask as.
const VERSIONS: [&str; 4] = ["", "0.137.0", "0.153.4", "cpa"];

fn model(id: &str) -> ModelInfo {
    ModelInfo {
        id: id.to_owned(),
        ..ModelInfo::default()
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|&value| value.to_owned()).collect()
}

/// State serving `registry`'s models with `config`.
fn state(registry: &Arc<ModelRegistry>, config: ServerConfig) -> AppState {
    AppState::new(config, FakeDispatcher::new([]), Arc::clone(registry) as _)
}

/// A config with `client.codex` set as given.
fn codex_config(enable_apply_patch: bool, optimize_multi_agent_v2: bool) -> ServerConfig {
    ServerConfig {
        codex_client: CodexClientConfig {
            enable_apply_patch,
            optimize_multi_agent_v2,
        },
        ..ServerConfig::default()
    }
}

/// The body of a successful `GET uri`.
async fn get(state: &AppState, uri: &str, user_agent: Option<&str>) -> String {
    let mut request = Request::builder().uri(uri);
    if let Some(user_agent) = user_agent {
        request = request.header(header::USER_AGENT, user_agent);
    }
    let response = crate::router(state.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/json; charset=utf-8"
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// The entries of a Codex model list.
fn entries(body: &str) -> Vec<Map<String, Value>> {
    let list: Value = serde_json::from_str(body).unwrap();
    list["models"]
        .as_array()
        .expect("models is an array")
        .iter()
        .map(|entry| entry.as_object().expect("an object").clone())
        .collect()
}

/// The entry for `slug`.
fn entry<'a>(entries: &'a [Map<String, Value>], slug: &str) -> &'a Map<String, Value> {
    entries
        .iter()
        .find(|entry| entry["slug"] == slug)
        .unwrap_or_else(|| panic!("missing model {slug}"))
}

/// The reasoning efforts an entry lists.
fn efforts(entry: &Map<String, Value>) -> Vec<&str> {
    entry["supported_reasoning_levels"]
        .as_array()
        .expect("supported_reasoning_levels is an array")
        .iter()
        .map(|level| level["effort"].as_str().expect("an effort"))
        .collect()
}

/// `entry[key]`, which must be present.
fn field<'a>(entry: &'a Map<String, Value>, key: &str) -> &'a Value {
    entry
        .get(key)
        .unwrap_or_else(|| panic!("{key} must be present: {entry:?}"))
}

#[tokio::test]
async fn serves_the_codex_catalog() {
    let registry = Arc::new(ModelRegistry::new());
    let mut models = vec![
        ModelInfo {
            id: "gpt-5.5".into(),
            object: "model".into(),
            created: 1_776_902_400,
            owned_by: "openai".into(),
            model_type: "openai".into(),
            display_name: "GPT 5.5".into(),
            description: "Frontier model for complex coding, research, and real-world work.".into(),
            context_length: 272_000,
            max_completion_tokens: 64_000,
            thinking: Some(ThinkingSupport {
                levels: strings(&["low", "medium", "high", "xhigh"]),
                ..ThinkingSupport::default()
            }),
            ..ModelInfo::default()
        },
        ModelInfo {
            id: "custom-codex-model-test".into(),
            object: "model".into(),
            owned_by: "test".into(),
            model_type: "openai".into(),
            display_name: "Custom Codex Model".into(),
            description: "Custom model from registry".into(),
            context_length: 123_456,
            thinking: Some(ThinkingSupport {
                levels: strings(&[
                    "none",
                    "minimal",
                    "low",
                    "medium",
                    "unsupported",
                    "high",
                    "xhigh",
                ]),
                ..ThinkingSupport::default()
            }),
            ..ModelInfo::default()
        },
    ];
    models.extend(HIDDEN_MODELS.map(|id| {
        ModelInfo {
            object: "model".into(),
            owned_by: if id.starts_with("grok") {
                "xai"
            } else {
                "openai"
            }
            .into(),
            model_type: "openai".into(),
            ..model(id)
        }
    }));
    registry.register_client("test-client-version-catalog", "codex", &models);
    let state = state(&registry, codex_config(true, false));

    // A claude-cli client asking with a client version gets the Codex list.
    let body = get(&state, "/v1/models?client_version", Some("claude-cli/1.0")).await;
    let list: Value = serde_json::from_str(&body).unwrap();
    assert!(list.get("object").is_none() && list.get("data").is_none());
    let entries = entries(&body);

    let gpt55 = entry(&entries, "gpt-5.5");
    assert_eq!(gpt55["apply_patch_tool_type"], "freeform");
    assert!(gpt55.contains_key("minimal_client_version"));
    assert_eq!(gpt55["max_tokens"], 64000);
    assert_eq!(gpt55["service_tiers"].as_array().map(Vec::len), Some(1));

    let custom = entry(&entries, "custom-codex-model-test");
    assert_eq!(custom["display_name"], "Custom Codex Model");
    let max_template_priority = CodexClientCatalog::embedded()
        .unwrap()
        .templates()
        .filter_map(|template| template["priority"].as_i64())
        .max()
        .unwrap();
    assert_eq!(custom["priority"], max_template_priority + 100);
    assert_eq!(custom["description"], "Custom model from registry");
    assert_eq!(custom["context_window"], 123_456);
    assert_eq!(
        efforts(custom),
        ["none", "minimal", "low", "medium", "high", "xhigh"]
    );
    assert_ne!(custom["base_instructions"], gpt55["base_instructions"]);
    assert!(
        custom["base_instructions"]
            .as_str()
            .is_some_and(|text| !text.is_empty())
    );
    assert_eq!(
        custom["model_messages"]["instructions_template"],
        custom["base_instructions"]
    );
    assert!(custom["available_in_plans"].is_array());
    assert_ne!(custom.get("prefer_websockets"), Some(&json!(true)));
    assert_eq!(custom["service_tiers"], json!([]));
    assert_eq!(field(custom, "apply_patch_tool_type"), "freeform");
    assert_eq!(field(custom, "upgrade"), &Value::Null);
    assert_eq!(field(custom, "availability_nux"), &Value::Null);

    for id in HIDDEN_MODELS {
        assert_eq!(entry(&entries, id)["visibility"], "hide", "{id}");
    }
}

#[tokio::test]
async fn filters_max_and_ultra_for_older_clients() {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_client(
        "codex-client-version-filter-test",
        "openai",
        &[ModelInfo {
            object: "model".into(),
            owned_by: "openai".into(),
            model_type: "openai".into(),
            display_name: "GPT-5.6-Sol".into(),
            ..model("gpt-5.6-sol")
        }],
    );
    let state = state(&registry, ServerConfig::default());

    let old = get(&state, "/v1/models?client_version=0.137.0", None).await;
    let old = entries(&old);
    let old_efforts = efforts(entry(&old, "gpt-5.6-sol"));
    assert!(
        !old_efforts.contains(&"max") && !old_efforts.contains(&"ultra"),
        "{old_efforts:?}"
    );

    let new = get(&state, "/v1/models?client_version=0.149.1", None).await;
    let new = entries(&new);
    assert!(efforts(entry(&new, "gpt-5.6-sol")).contains(&"ultra"));
}

#[tokio::test]
async fn apply_patch_needs_providers_that_take_it() {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_client(
        "http-patch-missing-executor",
        "unknown",
        &[model("gpt-5.5"), model("http-patch-synthetic")],
    );
    let state = state(&registry, codex_config(true, false));
    for version in VERSIONS {
        let body = get(
            &state,
            &format!("/v1/models?client_version={version}"),
            None,
        )
        .await;
        let entries = entries(&body);
        assert_eq!(entries.len(), 2);
        for entry in &entries {
            assert_eq!(field(entry, "apply_patch_tool_type"), &Value::Null);
        }
    }
    // The ordinary list is unchanged.
    let body = get(&state, "/v1/models", None).await;
    assert!(body.contains(r#""object":"list""#), "{body}");
    assert!(!body.contains("apply_patch_tool_type"), "{body}");
}

#[tokio::test]
async fn apply_patch_follows_config_reloads() {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_client(
        "config-patch-models",
        "codex",
        &[
            model("gpt-5.5"),
            model("config-patch-synthetic"),
            model("gpt-image-2"),
        ],
    );
    registry.register_client(
        "config-patch-unknown",
        "unknown",
        &[model("config-patch-unknown")],
    );
    let state = state(&registry, ServerConfig::default());
    for (name, raw, want) in [
        ("omitted", None, false),
        (
            "disabled",
            Some("client: {codex: {enable-apply-patch: false}}"),
            false,
        ),
        (
            "enabled",
            Some("client: {codex: {enable-apply-patch: true}}"),
            true,
        ),
        (
            "disabled-again",
            Some("client: {codex: {enable-apply-patch: false}}"),
            false,
        ),
        (
            "enabled-again",
            Some("client: {codex: {enable-apply-patch: true}}"),
            true,
        ),
        ("removed", Some("{}"), false),
    ] {
        if let Some(raw) = raw {
            let parsed = Config::parse(raw).expect(name);
            state.set_config(ServerConfig::from(&parsed));
        }
        for version in VERSIONS {
            let body = get(
                &state,
                &format!("/v1/models?client_version={version}"),
                None,
            )
            .await;
            let mut seen = 0;
            for entry in entries(&body) {
                let want = match entry["slug"].as_str().unwrap() {
                    "gpt-5.5" | "config-patch-synthetic" if want => json!("freeform"),
                    "gpt-5.5"
                    | "config-patch-synthetic"
                    | "gpt-image-2"
                    | "config-patch-unknown" => Value::Null,
                    _ => continue,
                };
                seen += 1;
                assert_eq!(
                    entry.get("apply_patch_tool_type"),
                    Some(&want),
                    "{name} {version:?} {}",
                    entry["slug"]
                );
            }
            assert_eq!(seen, 4, "{name} {version:?}");
        }
    }
}

#[tokio::test]
async fn multi_agent_follows_config_reloads() {
    const MODEL_ID: &str = "config-multi-agent-synthetic";
    let registry = Arc::new(ModelRegistry::new());
    registry.register_client("config-multi-agent-models", "codex", &[model(MODEL_ID)]);
    let state = state(&registry, ServerConfig::default());
    for (name, raw, want) in [
        ("omitted", "{}", false),
        (
            "enabled",
            "client: {codex: {optimize-multi-agent-v2: true}}",
            true,
        ),
        (
            "disabled",
            "client: {codex: {optimize-multi-agent-v2: false}}",
            false,
        ),
        (
            "legacy enabled",
            "providers: {codex: {optimize-multi-agent-v2: true}}",
            true,
        ),
        (
            "new false wins",
            "client: {codex: {optimize-multi-agent-v2: false}}\n\
             oauth: {providers: {codex: {optimize-multi-agent-v2: true}}}",
            false,
        ),
        (
            "oauth alias enabled",
            "oauth: {providers: {codex: {optimize-multi-agent-v2: true}}}",
            true,
        ),
        ("removed", "{}", false),
    ] {
        let parsed = Config::parse(raw).expect(name);
        let mut config = ServerConfig::default();
        config.codex_client.optimize_multi_agent_v2 = ServerConfig::from(&parsed)
            .codex_client
            .optimize_multi_agent_v2;
        state.set_config(config);
        assert_eq!(
            state.settings().config.codex_client.optimize_multi_agent_v2,
            want,
            "{name}: the reload reaches the handlers"
        );
        let body = get(&state, "/v1/models?client_version=cpa", None).await;
        let entries = entries(&body);
        let want = if want { json!("v2") } else { Value::Null };
        assert_eq!(
            field(entry(&entries, MODEL_ID), "multi_agent_version"),
            &want,
            "{name}"
        );
    }
}

#[test]
fn multi_agent_v2_follows_config() {
    const MODEL_ID: &str = "codex-client-multi-agent-v2-test";
    let registry = Arc::new(ModelRegistry::new());
    registry.register_client(
        "codex-client-multi-agent-v2-test-client",
        "openai-compatibility",
        &[model(MODEL_ID)],
    );
    for (enabled, want) in [(false, Value::Null), (true, json!("v2"))] {
        let state = state(&registry, codex_config(false, enabled));
        let entries = entries(&response(&state, ""));
        assert_eq!(
            field(entry(&entries, MODEL_ID), "multi_agent_version"),
            &want,
            "enabled {enabled}"
        );
    }
}

#[test]
fn client_version_filtering() {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_client(
        "codex-version-filter-sdk-test",
        "openai-compatibility",
        &[ModelInfo {
            object: "model".into(),
            owned_by: "openai".into(),
            display_name: "GPT-5.6-Sol".into(),
            ..model("gpt-5.6-sol")
        }],
    );
    let state = state(&registry, ServerConfig::default());

    let old = entries(&response(&state, "0.137.0"));
    let old_efforts = efforts(entry(&old, "gpt-5.6-sol"));
    assert!(
        !old_efforts.contains(&"max") && !old_efforts.contains(&"ultra"),
        "{old_efforts:?}"
    );
    assert_eq!(old_efforts.len(), 4, "{old_efforts:?}");

    let new = entries(&response(&state, "0.149.1"));
    assert!(efforts(entry(&new, "gpt-5.6-sol")).contains(&"ultra"));
}

#[test]
fn model_maps_do_not_expose_metadata_model_id() {
    let registry = ModelRegistry::new();
    registry.register_client(
        "openai-models-no-metadata-id-test",
        "codex",
        &[ModelInfo {
            metadata_model_id: "gpt-6-astra".into(),
            display_name: "GPT 6.0 Astra".into(),
            ..model("codex-main")
        }],
    );
    let maps = registry.available_model_maps("openai");
    assert!(!maps.is_empty());
    for map in maps {
        assert!(!map.contains_key("metadata_model_id"), "{map:?}");
        assert!(!map.contains_key("MetadataModelID"), "{map:?}");
    }
}

#[test]
fn oauth_aliases_integration() {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_client(
        "codex-client-models-integration-test",
        "codex",
        &[
            ModelInfo {
                metadata_model_id: "gpt-6-astra".into(),
                display_name: "GPT 6.0 Astra".into(),
                ..model("codex-main")
            },
            ModelInfo {
                metadata_model_id: "gpt-5.6-luna".into(),
                display_name: "GPT 5.6 Luna".into(),
                ..model("codex-luna")
            },
        ],
    );
    let state = state(&registry, ServerConfig::default());
    let entries = entries(&response(&state, "0.153.4"));
    for id in ["codex-main", "codex-luna"] {
        let entry = entry(&entries, id);
        assert_eq!(entry["comp_hash"], "3000", "{id}");
        assert_eq!(entry["max_context_window"], 872_000, "{id}");
        assert_eq!(entry["supports_search_tool"], true, "{id}");
    }
}

#[test]
fn apply_patch_routing() {
    let registry = Arc::new(ModelRegistry::new());
    let alias = ModelInfo {
        metadata_model_id: "gpt-5.5".into(),
        supported_input_modalities: strings(&["text"]),
        ..model("catalog-patch-alias")
    };
    let unknown = ModelInfo {
        metadata_model_id: "gpt-5.5".into(),
        ..model("catalog-patch-unknown")
    };
    let registrations = [
        (
            "codex",
            vec![
                model("gpt-5.5"),
                model("gpt-reserve"),
                model("gpt-image-2"),
                model("catalog-patch-mixed"),
                model("catalog-patch-partial"),
            ],
        ),
        (
            "openai-compatible-custom",
            vec![
                model("catalog-patch-synthetic"),
                model("catalog-patch-mixed"),
                alias,
            ],
        ),
        (
            "remote",
            vec![
                model("catalog-patch-partial"),
                unknown,
                model("team/gpt-5.5"),
            ],
        ),
        ("denied", vec![model("catalog-patch-disabled")]),
        (
            "gemini",
            vec![model("catalog-patch-gemini"), model("catalog-patch-google")],
        ),
        (
            "vertex",
            vec![model("catalog-patch-vertex"), model("catalog-patch-google")],
        ),
    ];
    for (provider, models) in &registrations {
        registry.register_client(&format!("sdk-patch-{provider}"), provider, models);
    }
    let supported = [
        "gpt-5.5",
        "gpt-reserve",
        "catalog-patch-synthetic",
        "catalog-patch-alias",
        "catalog-patch-mixed",
        "catalog-patch-gemini",
        "catalog-patch-vertex",
        "catalog-patch-google",
    ];
    let unsupported = [
        "gpt-image-2",
        "catalog-patch-partial",
        "catalog-patch-unknown",
        "team/gpt-5.5",
        "catalog-patch-disabled",
    ];
    let state = state(&registry, ServerConfig::default());
    for enabled in [false, false, true, false] {
        state.set_config(codex_config(enabled, false));
        for version in VERSIONS {
            let entries = entries(&response(&state, version));
            assert_eq!(entries.len(), supported.len() + unsupported.len());
            for entry in &entries {
                let slug = entry["slug"].as_str().unwrap();
                let want = if enabled && supported.contains(&slug) {
                    json!("freeform")
                } else {
                    Value::Null
                };
                assert_eq!(
                    field(entry, "apply_patch_tool_type"),
                    &want,
                    "enabled {enabled} {version:?} {slug}"
                );
            }
        }
    }
    state.set_config(codex_config(true, false));
    for version in VERSIONS {
        let entries = entries(&response(&state, version));
        for id in supported {
            assert_eq!(
                field(entry(&entries, id), "apply_patch_tool_type"),
                "freeform",
                "{version:?} {id}"
            );
        }
        for id in unsupported {
            assert_eq!(
                field(entry(&entries, id), "apply_patch_tool_type"),
                &Value::Null,
                "{version:?} {id}"
            );
        }
    }
}

#[test]
fn apply_patch_model_exact_public_route() {
    let registry = ModelRegistry::new();
    registry.register_client(
        "patch-route-supported",
        "codex",
        &[
            model("public-patch-alias"),
            model("mixed-patch-alias"),
            model("gpt-image-2"),
            model("registered-patch(high)"),
        ],
    );
    registry.register_client(
        "patch-route-unsupported",
        "denied",
        &[
            model("mixed-patch-alias"),
            ModelInfo {
                metadata_model_id: "public-patch-alias".into(),
                ..model("unsupported-patch-alias")
            },
        ],
    );
    for (model, want) in [
        ("public-patch-alias", true),
        ("public-patch-alias(high)", true),
        ("registered-patch(high)", true),
        ("mixed-patch-alias", false),
        ("unsupported-patch-alias", false),
        ("unknown-patch", false),
        ("gpt-image-2", false),
        ("gpt-image-2(high)", false),
        ("", false),
    ] {
        assert_eq!(supports_apply_patch(&registry, model), want, "{model:?}");
    }
}

#[test]
fn apply_patch_needs_every_provider() {
    let custom = "openai-compatible-custom";
    for (providers, want) in [
        (&[][..], false),
        (&["unknown"][..], false),
        (&["remote"][..], false),
        (&[custom][..], true),
        (&[custom, custom][..], true),
        (&[custom, "denied"][..], false),
        (&[custom, "unknown"][..], false),
        (&[custom, "remote"][..], false),
        // Gemini and Vertex AI take the tool.
        (&["gemini"][..], true),
        (&["vertex"][..], true),
        (&[custom, "gemini", "vertex"][..], true),
        (&["gemini", "denied"][..], false),
        // So do Gemini Interactions, Meta and xAI.
        (&["gemini-interactions"][..], true),
        (&["meta"][..], true),
        (&[custom, "gemini-interactions", "meta"][..], true),
        (&["meta", "xai"][..], true),
        (&["xai", "denied"][..], false),
        // How names match.
        (&["codex", "claude", "openai-compatibility"][..], true),
        (&[" Codex ", "CLAUDE", "OpenAI-Compatible-Custom"][..], true),
        (&[" Gemini ", "VERTEX"][..], true),
        (&[" Gemini-Interactions ", "META"][..], true),
        (&[""][..], false),
        (&["openai-compatible-"][..], false),
        (&["openai"][..], false),
    ] {
        assert_eq!(
            supports_apply_patch_for_providers(&strings(providers)),
            want,
            "{providers:?}"
        );
    }
}

// Not upstream's: xAI's executor takes the tool, as upstream's
// XAIExecutor.SupportsApplyPatch says.
#[test]
fn apply_patch_xai() {
    assert!(supports_apply_patch_for_providers(&strings(&[
        " XAI ", "codex"
    ])));
}
