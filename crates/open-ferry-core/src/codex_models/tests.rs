//! Ports CLIProxyAPI internal/client/codex/models/models_test.go and
//! web_search_capability_test.go (v8.0.15, MIT).
//!
//! Upstream's model maps become [`ModelInfo`]s, and tests that register
//! models in upstream's global registry register them in a registry of their
//! own.
//!
//! Changed:
//! - `TestCodexClientModelsResponseUsesProvidedCapabilitiesForNewHomeModel`
//!   and `TestCodexClientModelsResponseDoesNotInheritUnsupportedReasoningLevels`
//!   give the model's reasoning levels by registering it: a model map's
//!   `thinking` comes only from Home, which isn't ported.
//! - `TestSanitizeCodexClientReasoningMetadataPreservesEmptyArray` drops its
//!   "nil levels" case. A Go nil slice has no JSON counterpart here; a JSON
//!   `null` is left alone, as upstream leaves it.
//! - `TestCodexClientModelsResponse_CPAWebSearchCapabilitiesOnlyForCPAClient`
//!   and `TestCPAWebSearchCapabilityNeverTrustsTemplateClaims` check that
//!   `cpa_capabilities` is removed for every client version, `cpa`
//!   included, as it isn't ported.
//!
//! Dropped:
//! - `TestLoadCodexClientModelTemplatesRefreshesOnRevision`: the catalog is
//!   built in and has no revisions.
//! - `TestCodexClientModelsResponse_CPAWebSearchCapabilities` and
//!   `TestCPAWebSearchCapabilityOverridesTemplateWithExplicitFalse`:
//!   `cpa_capabilities` isn't ported.
//! - `TestCodexClientModelsResponse_DevinDisplayName`: Devin isn't ported.
//!
//! Added: [`marshal_compact`]'s escapes and key order, the order of models
//! without a template, hidden image and video models, and version parsing.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use super::*;
use crate::registry::ModelRegistry;

type Entry = Map<String, Value>;

fn model(id: &str) -> ModelInfo {
    ModelInfo {
        id: id.to_owned(),
        ..ModelInfo::default()
    }
}

fn named(id: &str, display_name: &str) -> ModelInfo {
    ModelInfo {
        display_name: display_name.to_owned(),
        ..model(id)
    }
}

fn alias(id: &str, metadata_model_id: &str, display_name: &str) -> ModelInfo {
    ModelInfo {
        metadata_model_id: metadata_model_id.to_owned(),
        ..named(id, display_name)
    }
}

fn thinking(levels: &[&str]) -> Option<ThinkingSupport> {
    Some(ThinkingSupport {
        levels: levels.iter().map(|level| (*level).to_owned()).collect(),
        ..ThinkingSupport::default()
    })
}

fn models(response: Value) -> Vec<Entry> {
    let Value::Array(models) = &response["models"] else {
        panic!("models = {}, want an array", response["models"]);
    };
    models
        .iter()
        .map(|entry| entry.as_object().expect("an object").clone())
        .collect()
}

fn build(
    catalog: &dyn ModelCatalog,
    available: &[ModelInfo],
    providers: Option<ProvidersForModel<'_>>,
    optimize: bool,
    version: &str,
) -> Vec<Entry> {
    models(build_response(
        catalog, available, providers, None, optimize, version,
    ))
}

fn build_plain(available: &[ModelInfo], optimize: bool, version: &str) -> Vec<Entry> {
    build(&ModelRegistry::new(), available, None, optimize, version)
}

fn by_slug(entries: Vec<Entry>) -> HashMap<String, Entry> {
    entries
        .into_iter()
        .map(|entry| (string_value(&entry, "slug").to_owned(), entry))
        .collect()
}

fn efforts(entry: &Entry) -> Vec<String> {
    let Some(Value::Array(levels)) = entry.get("supported_reasoning_levels") else {
        panic!(
            "supported_reasoning_levels = {:?}, want an array",
            entry.get("supported_reasoning_levels")
        );
    };
    levels
        .iter()
        .map(|level| level["effort"].as_str().unwrap_or("").to_owned())
        .collect()
}

fn int_value(entry: &Entry, key: &str) -> i64 {
    entry.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn assert_null(entry: &Entry, key: &str) {
    assert_eq!(
        entry.get(key),
        Some(&Value::Null),
        "{key} must be present and null so Codex can read the list"
    );
}

#[test]
fn input_modalities_from_registry() {
    let registry = ModelRegistry::new();
    let compat = |id: &str, modalities: &[&str]| ModelInfo {
        object: "model".to_owned(),
        owned_by: "mimo".to_owned(),
        model_type: "openai-compatibility".to_owned(),
        supported_input_modalities: modalities.iter().map(|m| (*m).to_owned()).collect(),
        ..named(id, id)
    };
    registry.register_client(
        "codex-input-modalities-test",
        "openai-compatibility",
        &[
            compat("mimo-v2.5-pro-codex-test", &["text", "image"]),
            compat("mimo-text-only-codex-test", &["text"]),
            compat(
                "mimo-mixed-modalities-codex-test",
                &["text", "image", "audio", "video", "TEXT", "IMAGE"],
            ),
            ModelInfo {
                object: "model".to_owned(),
                owned_by: "mimo".to_owned(),
                model_type: OPENAI_IMAGE_MODEL_TYPE.to_owned(),
                ..model("compat-image-only-codex-test")
            },
        ],
    );

    let entries = by_slug(build(
        &registry,
        &registry.available_models(),
        None,
        false,
        "",
    ));
    let vision = &entries["mimo-v2.5-pro-codex-test"];
    assert_eq!(vision["input_modalities"], json!(["text", "image"]));
    assert_eq!(vision["supports_image_detail_original"], true);

    let text_only = &entries["mimo-text-only-codex-test"];
    assert_eq!(text_only["input_modalities"], json!(["text"]));
    assert!(!text_only.contains_key("supports_image_detail_original"));

    let mixed = &entries["mimo-mixed-modalities-codex-test"];
    assert_eq!(mixed["input_modalities"], json!(["text", "image"]));
    assert_eq!(mixed["supports_image_detail_original"], true);

    let image = &entries["compat-image-only-codex-test"];
    assert_eq!(image["visibility"], "hide");
    assert!(!image.contains_key("input_modalities"));
}

#[test]
fn applies_display_name_to_template_model() {
    let entries = build_plain(&[named("gpt-5.5", "Configured Codex Name")], false, "");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["display_name"], "Configured Codex Name");
}

#[test]
fn rewrites_template_multi_agent_version_when_enabled() {
    let entries = build_plain(&[model("gpt-5.6-luna"), model("gpt-5.5")], true, "");
    assert_eq!(entries.len(), 2);
    for entry in entries {
        assert_eq!(entry["multi_agent_version"], "v2", "{}", entry["slug"]);
    }
}

#[test]
fn disables_search_tool_for_synthesized_models() {
    let entries = by_slug(build_plain(
        &[model("custom-openai-compatible-model"), model("gpt-5.5")],
        false,
        "",
    ));
    assert_eq!(
        entries["custom-openai-compatible-model"]["supports_search_tool"],
        false
    );
    assert_eq!(entries["gpt-5.5"]["supports_search_tool"], true);
}

#[test]
fn requires_template_and_codex_providers_for_search_tool() {
    let providers: HashMap<&str, Vec<String>> = [
        ("new-codex-model", vec!["codex"]),
        ("gpt-5.5", vec!["openai-compatible-deepseek"]),
        ("gpt-5.4", vec!["codex", "xai"]),
        ("gpt-5.6-sol", vec!["codex"]),
    ]
    .into_iter()
    .map(|(id, names)| (id, names.into_iter().map(str::to_owned).collect()))
    .collect();
    let lookup = |id: &str| providers.get(id).cloned().unwrap_or_default();
    let entries = by_slug(build(
        &ModelRegistry::new(),
        &[
            model("new-codex-model"),
            model("gpt-5.5"),
            model("gpt-5.4"),
            model("gpt-5.6-sol"),
        ],
        Some(&lookup),
        false,
        "",
    ));
    assert_eq!(entries["gpt-5.6-sol"]["supports_search_tool"], true);
    for slug in ["new-codex-model", "gpt-5.5", "gpt-5.4"] {
        assert_eq!(entries[slug]["supports_search_tool"], false, "{slug}");
    }
}

#[test]
fn preserves_ultra_reasoning_effort() {
    let entries = by_slug(build_plain(&[model("gpt-5.6-sol")], false, ""));
    assert!(efforts(&entries["gpt-5.6-sol"]).contains(&"ultra".to_owned()));
}

#[test]
fn filters_max_and_ultra_for_older_clients() {
    let entries = by_slug(build_plain(&[model("gpt-5.6-sol")], false, "0.137.0"));
    let efforts = efforts(&entries["gpt-5.6-sol"]);
    assert!(!efforts.is_empty());
    for effort in efforts {
        assert!(effort != "max" && effort != "ultra", "{effort}");
    }
}

#[test]
fn supports_extended_reasoning_levels_by_version() {
    let cases = [
        ("", true),
        ("pi", true),
        ("latest", true),
        ("unknown", true),
        ("0.137.0", false),
        ("0.143.9", false),
        ("v0.137.0", false),
        ("0.137.0-beta.1", false),
        ("0.144.0", true),
        ("0.144.1", true),
        ("0.149.1", true),
        ("1.0.0", true),
        ("invalid", true),
        // Added: a part that isn't a number makes the version unreadable,
        // missing parts count as 0, and empty parts are skipped.
        ("0.137.x", true),
        ("0.144", true),
        ("0.143", false),
        ("V0.143.9+build.1", false),
        (" 0.143.9 ", false),
        ("0..144", true),
        ("0..143.9", false),
    ];
    for (version, want) in cases {
        assert_eq!(
            supports_extended_reasoning_levels(version),
            want,
            "{version:?}"
        );
    }
}

#[test]
fn model_metadata_preserves_multi_agent_version_when_disabled() {
    let registry = ModelRegistry::new();
    let mut entry = Map::new();
    entry.insert("multi_agent_version".into(), "v1".into());
    let custom = model("custom-model");
    let builder = |optimize_multi_agent_v2| Builder {
        catalog: &registry,
        statics: StaticCatalog::embedded_shared(),
        providers_for_model: None,
        apply_patch: None,
        optimize_multi_agent_v2,
        client_version: "",
    };

    builder(false).apply_model_metadata(&mut entry, "custom-model", &custom);
    assert_eq!(entry["multi_agent_version"], "v1");
    builder(true).apply_model_metadata(&mut entry, "custom-model", &custom);
    assert_eq!(entry["multi_agent_version"], "v2");
}

#[test]
fn applies_max_context_length_override() {
    let with_override = |id: &str| ModelInfo {
        max_context_length: 1_048_576,
        ..model(id)
    };
    let entries = by_slug(build_plain(
        &[
            with_override("deepseek-v4-flash"),
            model("deepseek-v4-pro"),
            with_override("gpt-5.5"),
        ],
        false,
        "",
    ));
    for (slug, want) in [
        ("deepseek-v4-flash", 1_048_576),
        ("deepseek-v4-pro", 272_000),
        ("gpt-5.5", 1_048_576),
    ] {
        assert_eq!(int_value(&entries[slug], "context_window"), want, "{slug}");
        assert_eq!(
            int_value(&entries[slug], "max_context_window"),
            want,
            "{slug}"
        );
    }
}

#[test]
fn maps_max_completion_tokens_to_max_tokens() {
    let limited = |id: &str, limit| ModelInfo {
        max_completion_tokens: limit,
        ..model(id)
    };
    let entries = by_slug(build_plain(
        &[
            limited("gpt-5.5", 64_000),
            limited("custom-output-limit-model", 32_000),
        ],
        false,
        "",
    ));
    assert_eq!(int_value(&entries["gpt-5.5"], "max_tokens"), 64_000);
    assert_eq!(
        int_value(&entries["custom-output-limit-model"], "max_tokens"),
        32_000
    );
}

#[test]
fn uses_registered_capabilities_for_new_model() {
    let new_model = ModelInfo {
        context_length: 1_048_576,
        thinking: thinking(&["low", "medium", "high"]),
        ..model("gemini-new-home-model-test")
    };
    let registry = ModelRegistry::new();
    registry.register_client("new-model-test", "gemini", std::slice::from_ref(&new_model));
    let entries = build(&registry, &[new_model], None, false, "");
    assert_eq!(entries.len(), 1);
    assert_eq!(int_value(&entries[0], "context_window"), 1_048_576);
    assert_eq!(int_value(&entries[0], "max_context_window"), 1_048_576);
    assert_eq!(efforts(&entries[0]), ["low", "medium", "high"]);
}

#[test]
fn does_not_inherit_unsupported_reasoning_levels() {
    let cases: [(&str, &str, ThinkingSupport, &[&str], &str); 5] = [
        (
            "modern client",
            "0.144.0",
            thinking(&["max", "ultra"]).unwrap(),
            &["max", "ultra"],
            "max",
        ),
        (
            "legacy client with no compatible level",
            "0.143.9",
            thinking(&["max", "ultra"]).unwrap(),
            &[],
            "",
        ),
        (
            "legacy client with one compatible level",
            "0.143.9",
            thinking(&["high", "max"]).unwrap(),
            &["high"],
            "high",
        ),
        (
            "budget-only model",
            "0.153.3",
            ThinkingSupport {
                min: 1024,
                max: 64000,
                zero_allowed: true,
                dynamic_allowed: true,
                levels: Vec::new(),
            },
            &[],
            "",
        ),
        ("empty levels", "0.153.3", thinking(&[]).unwrap(), &[], ""),
    ];
    for (name, version, support, want_efforts, want_default) in cases {
        let reasoning_model = [ModelInfo {
            thinking: Some(support),
            ..model("home-extended-reasoning-model-test")
        }];
        let registry = ModelRegistry::new();
        registry.register_client("reasoning-test", "gemini", &reasoning_model);
        let entries = build(&registry, &reasoning_model, None, false, version);
        assert_eq!(entries.len(), 1, "{name}");
        let entry = &entries[0];
        if want_efforts.is_empty() {
            assert_eq!(entry["supported_reasoning_levels"], json!([]), "{name}");
            assert!(!entry.contains_key("default_reasoning_level"), "{name}");
            continue;
        }
        assert_eq!(efforts(entry), want_efforts, "{name}");
        assert_eq!(entry["default_reasoning_level"], want_default, "{name}");
    }
}

#[test]
fn sanitize_reasoning_metadata_preserves_empty_array() {
    let cases = [
        ("empty levels", "0.153.3", json!([])),
        (
            "legacy client with no compatible level",
            "0.143.9",
            json!([{"effort": "max"}, {"effort": "ultra"}]),
        ),
        (
            "invalid levels",
            "0.153.3",
            json!([null, {"effort": "unknown"}]),
        ),
    ];
    for (name, version, levels) in cases {
        let mut entry = Map::new();
        entry.insert("supported_reasoning_levels".into(), levels);
        entry.insert("default_reasoning_level".into(), "max".into());
        sanitize_reasoning_metadata(&mut entry, version);
        assert_eq!(
            marshal_compact(&Value::Object(entry)),
            r#"{"supported_reasoning_levels":[]}"#,
            "{name}"
        );
    }
}

#[test]
fn prefixed_route_inherits_canonical_template() {
    let entries = build_plain(&[model("1/gpt-6-astra")], false, "");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["slug"], "1/gpt-6-astra");
    assert_eq!(entries[0]["comp_hash"], "3000");
    assert_eq!(int_value(&entries[0], "max_context_window"), 872_000);
}

#[test]
fn oauth_aliases_inherit_canonical_metadata() {
    let registry = ModelRegistry::new();
    registry.register_client(
        "codex-oauth-alias-metadata-test-client",
        "codex",
        &[
            alias("codex-main", "gpt-6-astra", "GPT 6.0 Astra"),
            alias("codex-compact", "gpt-6-astra", "GPT 6.0 Astra"),
            alias("codex-luna", "gpt-5.6-luna", "GPT 5.6 Luna"),
            alias("1/codex-main", "gpt-6-astra", "Prefixed Astra Alias"),
        ],
    );
    let available = [
        named("codex-main", "GPT 6.0 Astra"),
        named("codex-compact", "GPT 6.0 Astra"),
        named("codex-luna", "GPT 5.6 Luna"),
        named("1/codex-main", "Prefixed Astra Alias"),
    ];
    let providers = |id: &str| registry.model_providers(id);
    let entries = by_slug(build(
        &registry,
        &available,
        Some(&providers),
        false,
        "0.153.4",
    ));
    for slug in ["codex-main", "codex-compact", "1/codex-main", "codex-luna"] {
        let entry = &entries[slug];
        assert_eq!(entry["comp_hash"], "3000", "{slug}");
        assert_eq!(int_value(entry, "max_context_window"), 872_000, "{slug}");
        assert_eq!(entry["supports_search_tool"], true, "{slug}");
    }
}

#[test]
fn explicit_route_overrides_honored() {
    let registry = ModelRegistry::new();
    registry.register_client(
        "codex-route-override-test-client",
        "codex",
        &[
            ModelInfo {
                max_context_length: 123_456,
                explicit_thinking: true,
                thinking: thinking(&["low", "high"]),
                ..alias("custom-sol", "gpt-5.6-sol", "Overridden Display Name")
            },
            ModelInfo {
                explicit_thinking: true,
                // The registry's levels for this model, but set in the config.
                thinking: thinking(&["low", "medium", "high", "xhigh", "max"]),
                ..alias(
                    "custom-astra-explicit-no-ultra",
                    "gpt-6-astra",
                    "Astra Without Ultra",
                )
            },
            ModelInfo {
                explicit_thinking: true,
                thinking: thinking(&["low", "high"]),
                ..named("gpt-6-astra", "Direct Canonical With Explicit Thinking")
            },
        ],
    );
    let available = [
        ModelInfo {
            max_context_length: 123_456,
            ..named("custom-sol", "Overridden Display Name")
        },
        named("custom-astra-explicit-no-ultra", "Astra Without Ultra"),
        named("gpt-6-astra", "Direct Canonical With Explicit Thinking"),
    ];
    let providers = |id: &str| registry.model_providers(id);
    let entries = build(&registry, &available, Some(&providers), false, "0.153.4");
    assert_eq!(entries.len(), 3);
    let entries = by_slug(entries);

    let sol = &entries["custom-sol"];
    assert_eq!(sol["display_name"], "Overridden Display Name");
    assert_eq!(int_value(sol, "context_window"), 123_456);
    assert_eq!(int_value(sol, "max_context_window"), 123_456);
    assert_eq!(sol["comp_hash"], "3000", "inherited from gpt-5.6-sol");
    assert_eq!(efforts(sol), ["low", "high"]);
    assert_eq!(sol["default_reasoning_level"], "low");

    let no_ultra = &entries["custom-astra-explicit-no-ultra"];
    assert_eq!(efforts(no_ultra), ["low", "medium", "high", "xhigh", "max"]);

    let direct = &entries["gpt-6-astra"];
    assert_eq!(efforts(direct), ["low", "high"]);
    assert_eq!(direct["default_reasoning_level"], "low");
}

#[test]
fn unrelated_custom_provider_retains_fallback() {
    let registry = ModelRegistry::new();
    registry.register_client(
        "codex-unrelated-custom-provider-test-client",
        "openai-compatibility",
        &[named("my-unrelated-custom-model", "Unrelated Model")],
    );
    let providers = |id: &str| registry.model_providers(id);
    let entries = build(
        &registry,
        &[named("my-unrelated-custom-model", "Unrelated Model")],
        Some(&providers),
        false,
        "0.153.4",
    );
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry["slug"], "my-unrelated-custom-model");
    assert_eq!(entry["comp_hash"], "2911", "from the gpt-5.5 template");
    assert_eq!(entry["supports_search_tool"], false);
    assert_eq!(entry["input_modalities"], json!(["text", "image"]));
    assert_eq!(entry["supports_image_detail_original"], true);
}

/// Checks what a model must not advertise when a provider other than Codex
/// serves it.
fn assert_protocol_capabilities_restricted(entry: &Entry) {
    assert_eq!(entry["comp_hash"], "3000");
    assert_eq!(int_value(entry, "max_context_window"), 872_000);
    assert_eq!(entry["supports_search_tool"], false);
    assert_eq!(entry["prefer_websockets"], false);
    assert_eq!(
        entry["apply_patch_tool_type"], "freeform",
        "the capability decides apply_patch"
    );
    assert_eq!(entry["service_tiers"], json!([]));
    assert_null(entry, "upgrade");
    assert_null(entry, "availability_nux");
    assert_eq!(efforts(entry), ["low", "medium", "high"]);
}

#[test]
fn unrelated_provider_cannot_widen_search_tool() {
    let registry = ModelRegistry::new();
    registry.register_client(
        "codex-unrelated-provider-widen-test-client",
        "openai-compatibility",
        &[ModelInfo {
            // OpenAI-compatible models get these levels without
            // `explicit_thinking`.
            thinking: thinking(&["low", "medium", "high"]),
            ..alias(
                "custom-astra-on-openai-compat",
                "gpt-6-astra",
                "Astra on OpenAI Compat",
            )
        }],
    );
    let providers = |id: &str| registry.model_providers(id);
    let supported = |_: &str| true;
    let entries = models(build_response(
        &registry,
        &[named(
            "custom-astra-on-openai-compat",
            "Astra on OpenAI Compat",
        )],
        Some(&providers),
        Some(&supported),
        false,
        "0.153.4",
    ));
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["slug"], "custom-astra-on-openai-compat");
    assert_protocol_capabilities_restricted(&entries[0]);
}

#[test]
fn mixed_providers_restrict_protocol_capabilities() {
    for codex_first in [true, false] {
        let registry = ModelRegistry::new();
        let alias_name = "mixed-astra-alias";
        let register_codex = || {
            registry.register_client(
                "client-codex",
                "codex",
                &[alias(alias_name, "gpt-6-astra", "Mixed Astra Alias")],
            );
        };
        let register_compat = || {
            registry.register_client(
                "client-compat",
                "openai-compatibility",
                &[ModelInfo {
                    thinking: thinking(&["low", "medium", "high"]),
                    ..alias(alias_name, "gpt-6-astra", "Mixed Astra Alias")
                }],
            );
        };
        if codex_first {
            register_codex();
            register_compat();
        } else {
            register_compat();
            register_codex();
        }
        let providers = |id: &str| registry.model_providers(id);
        let supported = |_: &str| true;
        let entries = models(build_response(
            &registry,
            &[model(alias_name)],
            Some(&providers),
            Some(&supported),
            false,
            "0.153.4",
        ));
        assert_eq!(entries.len(), 1, "codex first: {codex_first}");
        assert_protocol_capabilities_restricted(&entries[0]);
    }
}

#[test]
fn mixed_providers_intersect_codex_explicit_restrictions() {
    for codex_first in [true, false] {
        let registry = ModelRegistry::new();
        let alias_name = "intersect-explicit-alias";
        let register_codex = || {
            registry.register_client(
                "codex-explicit-client",
                "codex",
                &[ModelInfo {
                    explicit_thinking: true,
                    // Codex is limited to low and high in the config.
                    thinking: thinking(&["low", "high"]),
                    ..alias(alias_name, "gpt-6-astra", "Astra Mixed Explicit")
                }],
            );
        };
        let register_compat = || {
            registry.register_client(
                "compat-client",
                "openai-compatibility",
                &[ModelInfo {
                    thinking: thinking(&["low", "medium", "high"]),
                    ..alias(alias_name, "gpt-6-astra", "Astra Mixed Explicit")
                }],
            );
        };
        if codex_first {
            register_codex();
            register_compat();
        } else {
            register_compat();
            register_codex();
        }
        let providers = |id: &str| registry.model_providers(id);
        let entries = build(
            &registry,
            &[model(alias_name)],
            Some(&providers),
            false,
            "0.153.4",
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(
            efforts(&entries[0]),
            ["low", "high"],
            "codex first: {codex_first}"
        );
    }
}

#[test]
fn disjoint_modalities_intersection_does_not_restore_template() {
    let registry = ModelRegistry::new();
    let alias_name = "disjoint-modalities-alias";
    for (client, provider, modality) in [
        ("modalities-client-a", "openai-compatibility", "text"),
        ("modalities-client-b", "custom-provider-b", "image"),
        ("modalities-client-c", "custom-provider-c", "audio"),
    ] {
        registry.register_client(
            client,
            provider,
            &[ModelInfo {
                supported_input_modalities: vec![modality.to_owned()],
                ..alias(alias_name, "gpt-6-astra", "")
            }],
        );
    }
    let providers = |id: &str| registry.model_providers(id);
    let entries = build(
        &registry,
        &[model(alias_name)],
        Some(&providers),
        false,
        "0.153.4",
    );
    assert_eq!(entries.len(), 1);
    // The empty intersection stays empty rather than restoring the
    // template's text and image.
    assert_eq!(entries[0]["input_modalities"], json!([]));
    assert!(!entries[0].contains_key("supports_image_detail_original"));
}

#[test]
fn audio_only_modalities_does_not_restore_template() {
    let registry = ModelRegistry::new();
    let alias_name = "audio-only-astra-alias";
    registry.register_client(
        "audio-only-client",
        "openai-compatibility",
        &[ModelInfo {
            supported_input_modalities: vec!["audio".to_owned()],
            ..alias(alias_name, "gpt-6-astra", "")
        }],
    );
    let providers = |id: &str| registry.model_providers(id);
    let entries = build(
        &registry,
        &[model(alias_name)],
        Some(&providers),
        false,
        "0.153.4",
    );
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["input_modalities"], json!([]));
    assert!(!entries[0].contains_key("supports_image_detail_original"));
}

#[test]
fn mixed_provider_without_thinking_restricts_reasoning() {
    for codex_first in [true, false] {
        let registry = ModelRegistry::new();
        let alias_name = "mixed-no-think-alias";
        let register = |client: &str, provider: &str| {
            // Neither registration has reasoning levels.
            registry.register_client(
                client,
                provider,
                &[alias(alias_name, "gpt-6-astra", "Mixed Astra Alias")],
            );
        };
        if codex_first {
            register("codex-mixed-no-think", "codex");
            register("compat-mixed-no-think", "openai-compatibility");
        } else {
            register("compat-mixed-no-think", "openai-compatibility");
            register("codex-mixed-no-think", "codex");
        }
        let providers = |id: &str| registry.model_providers(id);
        let entries = build(
            &registry,
            &[model(alias_name)],
            Some(&providers),
            false,
            "0.153.4",
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0]["supported_reasoning_levels"],
            json!([]),
            "codex first: {codex_first}"
        );
        assert!(!entries[0].contains_key("default_reasoning_level"));
    }
}

#[test]
fn oauth_aliases_inherit_complete_reasoning_levels_with_ultra() {
    let astra = StaticCatalog::embedded()
        .lookup("gpt-6-astra")
        .expect("gpt-6-astra is in the static catalog");
    let codex_main = ModelInfo {
        id: "codex-main".to_owned(),
        metadata_model_id: "gpt-6-astra".to_owned(),
        ..astra.clone()
    };
    let prefixed_main = ModelInfo {
        id: "1/codex-main".to_owned(),
        ..codex_main.clone()
    };
    let registry = ModelRegistry::new();
    registry.register_client(
        "codex-reasoning-levels-test-client",
        "codex",
        &[astra, codex_main, prefixed_main],
    );
    let available = [
        model("gpt-6-astra"),
        model("codex-main"),
        model("1/codex-main"),
    ];
    let providers = |id: &str| registry.model_providers(id);
    let slugs = ["gpt-6-astra", "codex-main", "1/codex-main"];

    let modern = by_slug(build(
        &registry,
        &available,
        Some(&providers),
        false,
        "0.153.4",
    ));
    for slug in slugs {
        let entry = &modern[slug];
        assert!(efforts(entry).contains(&"ultra".to_owned()), "{slug}");
        let levels = entry["supported_reasoning_levels"].as_array().unwrap();
        assert!(
            levels
                .iter()
                .any(|level| !level["description"].as_str().unwrap_or("").is_empty()),
            "{slug} keeps the template's descriptions"
        );
    }

    let legacy = by_slug(build(
        &registry,
        &available,
        Some(&providers),
        false,
        "0.137.0",
    ));
    for slug in slugs {
        for effort in efforts(&legacy[slug]) {
            assert!(effort != "max" && effort != "ultra", "{slug}: {effort}");
        }
    }
}

#[test]
fn cpa_capabilities_never_appear() {
    for version in ["", "0.153.4", "CPA", "cpa-preview", "cpa", "codex"] {
        let entries = build_plain(&[model("gpt-5.5"), model("unknown")], false, version);
        assert_eq!(entries.len(), 2);
        for entry in entries {
            assert!(!entry.contains_key("cpa_capabilities"), "{version:?}");
        }
    }

    // A template's own claim is removed too.
    let mut catalog: Value =
        serde_json::from_slice(crate::registry::codex_client::EMBEDDED_CATALOG).unwrap();
    for template in catalog["models"].as_array_mut().unwrap() {
        template["cpa_capabilities"] = json!({"web_search": true});
    }
    let templates = CodexClientCatalog::from_json(catalog.to_string().as_bytes()).unwrap();
    let registry = ModelRegistry::new();
    let builder = Builder {
        catalog: &registry,
        statics: StaticCatalog::embedded_shared(),
        providers_for_model: None,
        apply_patch: None,
        optimize_multi_agent_v2: false,
        client_version: "cpa",
    };
    let Value::Array(entries) = builder.build(&templates, &[model("gpt-5.5"), model("unknown")])
    else {
        panic!("models must be an array");
    };
    assert_eq!(entries.len(), 2);
    for entry in entries {
        assert!(entry.get("cpa_capabilities").is_none(), "{}", entry["slug"]);
    }
}

#[test]
fn marshal_compact_json_is_single_line() {
    let body = marshal_compact(&json!({
        "models": [{"slug": "demo", "description": "line1\nline2 <tag>"}],
    }));
    assert!(!body.contains('\n'), "{body}");
    assert!(body.contains("<tag>"), "{body}");
    assert!(body.contains(r"line1\nline2"), "{body}");
}

#[test]
fn marshal_compact_escapes_as_go_does() {
    let value = json!({
        "z": 1,
        "a": {"y": [true, null, 2.5], "b": "x"},
        "s": "<>&\u{7f}\"\\/\u{8}\u{c}\n\r\t\u{1}\u{1f}\u{2028}\u{2029}é",
    });
    let backslash = '\\';
    let want = format!(
        r#"{{"a":{{"b":"x","y":[true,null,2.5]}},"s":"<>&{del}{b}"{b}{b}/{b}b{b}f{b}n{b}r{b}t{b}u0001{b}u001f{b}u2028{b}u2029é","z":1}}"#,
        del = '\u{7f}',
        b = backslash,
    );
    assert_eq!(marshal_compact(&value), want);
}

#[test]
fn non_template_catalog_stays_within_one_mib() {
    let mut ids: Vec<String> = [
        "gpt-6-astra",
        "gpt-6-sol",
        "gpt-6-luna",
        "gpt-reserve",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
        "gpt-5.5",
        "codex-auto-review",
    ]
    .map(str::to_owned)
    .to_vec();
    ids.extend((0..120).map(|index| format!("custom-model-{index}")));
    let available: Vec<ModelInfo> = ids.iter().map(|id| model(id)).collect();

    let response = build_response(&ModelRegistry::new(), &available, None, None, false, "");
    let body = marshal_compact(&response);
    assert!(!body.contains('\n'));
    assert!(body.len() < 1 << 20, "{} bytes", body.len());

    let entries = by_slug(models(response));
    assert!(string_value(&entries["gpt-5.5"], "base_instructions").len() >= 1000);
    assert_eq!(
        entries["custom-model-0"]["base_instructions"],
        FALLBACK_INSTRUCTIONS
    );
}

#[test]
fn non_template_models_follow_the_catalog_by_display_name() {
    let entries = build_plain(
        &[
            named("zeta-model", "beta"),
            model("gpt-5.5"),
            named("alpha-model", "Beta"),
            model("Alpha"),
            model("gpt-6-astra"),
        ],
        false,
        "",
    );
    let order: Vec<(&str, i64)> = entries
        .iter()
        .map(|entry| (string_value(entry, "slug"), int_value(entry, "priority")))
        .collect();
    // The catalog's own priorities come first; the rest follow the highest
    // of them (43), 100 apart, by display name in lower case, then slug.
    assert_eq!(
        order,
        [
            ("gpt-6-astra", 2),
            ("gpt-5.5", 13),
            ("Alpha", 143),
            ("alpha-model", 243),
            ("zeta-model", 343),
        ]
    );
}

#[test]
fn image_and_video_models_are_hidden() {
    let entries = by_slug(build_plain(
        &[
            model("grok-imagine-video"),
            model("team/gpt-image-2"),
            model("gpt-image-2-mini"),
        ],
        false,
        "",
    ));
    assert_eq!(entries["grok-imagine-video"]["visibility"], "hide");
    assert_eq!(entries["team/gpt-image-2"]["visibility"], "hide");
    assert_eq!(entries["gpt-image-2-mini"]["visibility"], "list");
}

// Ported from TestCodexClientHidesSpeechModels; not upstream's: the
// entries' visibility.
#[test]
fn speech_models_are_hidden() {
    for id in ["grok-tts", "grok-voice-tts-1.0", "xai/grok-tts"] {
        assert!(is_image_or_video_model(id), "{id}");
    }
    assert!(!is_image_or_video_model("grok-4"));
    let entries = by_slug(build_plain(
        &[
            model("grok-tts"),
            model("xai/grok-voice-tts-1.0"),
            model("grok-4"),
        ],
        false,
        "",
    ));
    assert_eq!(entries["grok-tts"]["visibility"], "hide");
    assert_eq!(entries["xai/grok-voice-tts-1.0"]["visibility"], "hide");
    assert_eq!(entries["grok-4"]["visibility"], "list");
}

#[test]
fn synthesized_entries_get_compact_instructions_and_null_options() {
    let entries = build_plain(&[named("custom-model", "  Custom  ")], false, "");
    let entry = &entries[0];
    assert_eq!(entry["display_name"], "Custom");
    assert_eq!(entry["description"], "custom-model");
    assert_eq!(entry["prefer_websockets"], false);
    assert_eq!(entry["service_tiers"], json!([]));
    for key in ["apply_patch_tool_type", "upgrade", "availability_nux"] {
        assert_null(entry, key);
    }
    assert_eq!(
        entry["model_messages"],
        json!({
            "instructions_template": FALLBACK_INSTRUCTIONS,
            "instructions_variables": null,
            "approvals": null,
            "collaboration_modes": null,
            "auto_review": null,
            "permissions": null,
            "multi_agent": null,
        })
    );
}
