//! Ports the model list tests of CLIProxyAPI
//! internal/client/codex/optimize-multi-agent-v2/optimize_multi_agent_v2_test.go
//! (v8.0.10, MIT); the rest are in open-ferry-translate's
//! `codex_client/multi_agent_v2/tests.rs`.
//!
//! Upstream's model maps become [`ModelInfo`]s, and tests that register
//! models in upstream's global registry register them in a registry of their
//! own.
//!
//! Changed:
//! - `TestCodexSpawnAgentModelsFromSourcesIncludesModelMetadata` gives its
//!   templates as a map rather than catalog JSON, as a catalog is checked
//!   for the fields Codex needs when it is read.
//! - `TestCodexSpawnAgentModelsCacheInvalidation` checks that the list
//!   follows the registry, as there is no cache to invalidate.
//!
//! Dropped: `TestDecodeCodexHomeAvailableModels`, as Home isn't ported.
//!
//! Added: models with a catalog entry come first, an empty registry gives
//! an empty list, and the reasoning level and service tier rules.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use super::*;
use crate::registry::ModelRegistry;

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(fields) => fields,
        other => panic!("not an object: {other}"),
    }
}

fn model(id: &str, display_name: &str, description: &str) -> ModelInfo {
    ModelInfo {
        id: id.into(),
        display_name: display_name.into(),
        description: description.into(),
        ..ModelInfo::default()
    }
}

fn thinking(levels: &[&str]) -> Option<ThinkingSupport> {
    Some(ThinkingSupport {
        levels: levels.iter().map(|&level| level.to_owned()).collect(),
        ..ThinkingSupport::default()
    })
}

// TestCodexSpawnAgentModelsFromSourcesIncludesModelMetadata
#[test]
fn models_from_sources_include_model_metadata() {
    let catalog = json!([
        {"slug":"model-template","display_name":"Template","description":"Template model.","default_reasoning_level":"low","supported_reasoning_levels":[{"effort":"low"},{"effort":"medium"}],"service_tiers":[{"id":"priority"}],"priority":1},
        {"slug":"gpt-5.5","display_name":"Default","description":"Default model.","default_reasoning_level":"medium","supported_reasoning_levels":[{"effort":"low"},{"effort":"medium"},{"effort":"high"}],"service_tiers":[{"id":"priority"}],"priority":2}
    ]);
    let templates: HashMap<String, Map<String, Value>> = catalog
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            let entry = object(entry.clone());
            (map_string(&entry, "slug").to_owned(), entry)
        })
        .collect();
    let available = [
        model("custom-model", "Custom", "Registry description."),
        model("model-template", "", ""),
        model("custom-model", "", "duplicate"),
    ];
    let lookup = |id: &str| {
        (id == "custom-model").then(|| ModelInfo {
            description: "Dynamic model.".into(),
            thinking: thinking(&["none", "low", "medium", "high"]),
            ..ModelInfo::default()
        })
    };

    let models = models_from_templates(
        &available,
        |slug| templates.get(slug),
        &templates["gpt-5.5"],
        lookup,
    );
    assert_eq!(models.len(), 2);
    let template = &models[0];
    assert_eq!(template.id, "model-template");
    assert_eq!(template.description, "Template model.");
    assert_eq!(template.default_reasoning_effort, "low");
    assert_eq!(template.service_tiers, ["priority"]);
    let custom = &models[1];
    assert_eq!(custom.id, "custom-model");
    assert_eq!(custom.description, "Dynamic model.");
    assert_eq!(custom.reasoning_efforts, ["none", "low", "medium", "high"]);
    assert_eq!(custom.default_reasoning_effort, "medium");
    assert!(custom.service_tiers.is_empty());
    assert_eq!(custom.display_name, "Custom");
}

// TestCodexSpawnAgentModelsCacheInvalidation
#[test]
fn list_follows_the_registry() {
    let registry = ModelRegistry::new();
    let alpha = |levels: &[&str]| ModelInfo {
        thinking: thinking(levels),
        ..model(
            "test-spawn-model-alpha",
            "Test Spawn Model Alpha",
            "Initial description.",
        )
    };
    registry.register_client("client-1", "openai", &[alpha(&["low", "medium"])]);

    let first = spawn_agent_model_list(&registry);
    assert!(first.contains("test-spawn-model-alpha"), "{first}");
    assert!(first.contains("Reasoning efforts: low, medium"), "{first}");
    assert_eq!(spawn_agent_model_list(&registry), first);

    registry.register_client(
        "client-2",
        "openai",
        &[model(
            "test-spawn-model-beta",
            "Test Spawn Model Beta",
            "Second model.",
        )],
    );
    let second = spawn_agent_model_list(&registry);
    assert!(second.contains("test-spawn-model-beta"), "{second}");

    registry.register_client(
        "client-1",
        "openai",
        &[alpha(&["low", "medium", "high", "max"])],
    );
    let third = spawn_agent_model_list(&registry);
    assert!(
        third.contains("low, medium (default), high, max"),
        "{third}"
    );

    registry.unregister_client("client-2");
    let fourth = spawn_agent_model_list(&registry);
    assert!(!fourth.contains("test-spawn-model-beta"), "{fourth}");
}

#[test]
fn catalog_models_come_first_then_the_rest_by_display_name() {
    let registry = ModelRegistry::new();
    registry.register_client(
        "client",
        "openai",
        &[
            model("zeta-model", "alpha", "Zeta."),
            model("gpt-5.5", "", ""),
            model("beta-model", "Beta", ""),
        ],
    );
    let models = spawn_agent_models(&registry);
    let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(ids, ["gpt-5.5", "zeta-model", "beta-model"]);
    // The catalog's own entry keeps its service tiers and description.
    let template = CodexClientCatalog::embedded()
        .unwrap()
        .template("gpt-5.5")
        .unwrap();
    assert_eq!(models[0].description, map_string(template, "description"));
    assert_eq!(models[0].service_tiers, service_tier_ids(template));
    // A model without one takes its ID as its description when it has none.
    assert_eq!(models[2].description, "beta-model");
    assert!(models[2].service_tiers.is_empty());

    assert_eq!(spawn_agent_model_list(&ModelRegistry::new()), "");
}

#[test]
fn reasoning_levels_and_service_tiers() {
    let entry = object(json!({
        "default_reasoning_level": " HIGH ",
        "supported_reasoning_levels": [{"effort":"Low"}, {"effort":"minimal"}, "high", {"effort":" high "}],
        "service_tiers": [{"id":"flex"}, {"id":" flex "}, {"id":""}, 3, {"id":"priority"}],
        "priority": 7.9
    }));
    assert_eq!(
        reasoning_metadata(&entry),
        (vec!["low".to_owned(), "high".to_owned()], "high".to_owned())
    );
    assert_eq!(service_tier_ids(&entry), ["flex", "priority"]);
    assert_eq!(map_int(&entry, "priority"), 7);
    assert_eq!(map_int(&object(json!({"priority": "1"})), "priority"), 0);

    // A default that isn't listed falls back to the first level.
    let entry = object(json!({
        "default_reasoning_level": "xhigh",
        "supported_reasoning_levels": [{"effort":"medium"}, {"effort":"high"}]
    }));
    assert_eq!(reasoning_metadata(&entry).1, "medium");
    assert_eq!(
        reasoning_metadata(&object(json!({"default_reasoning_level": "low"}))),
        (Vec::new(), String::new())
    );

    // Registered levels default to medium, else the first but none.
    for (levels, default) in [
        (&["none", "low", "high"][..], "low"),
        (&["high", "medium", "low"][..], "medium"),
        (&["none", "minimal"][..], "none"),
    ] {
        let mut profile = SpawnAgentModel::default();
        apply_thinking(&mut profile, &thinking(levels).unwrap());
        assert_eq!(profile.default_reasoning_effort, default, "{levels:?}");
    }
    // Levels that aren't efforts leave the profile's own.
    let mut profile = SpawnAgentModel {
        reasoning_efforts: vec!["low".into()],
        default_reasoning_effort: "low".into(),
        ..SpawnAgentModel::default()
    };
    apply_thinking(&mut profile, &thinking(&["minimal", "auto"]).unwrap());
    assert_eq!(profile.reasoning_efforts, ["low"]);
}
