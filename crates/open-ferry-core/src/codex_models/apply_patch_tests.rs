//! Ports CLIProxyAPI internal/client/codex/models/apply_patch_test.go
//! (v8.0.10, MIT).
//!
//! Changed:
//! - `TestCodexCatalogApplyPatchCapability` builds its baseline without a
//!   web search capability, as `cpa_capabilities` isn't ported; the `cpa`
//!   client version is then like any other.
//! - `TestCodexCatalogApplyPatchLegacyEntryPointsUnknown` builds each list
//!   without a capability, which is what upstream's legacy entry points do;
//!   they aren't ported themselves.
//! - `TestApplyPatchFieldModalities`: Go's `[]string` and `[]any` cases are
//!   the same JSON array here, and a missing list is `null`.

use std::cell::Cell;

use serde_json::{Map, Value, json};

use super::*;
use crate::registry::ModelRegistry;

fn entries(response: &Value) -> Vec<Map<String, Value>> {
    response["models"]
        .as_array()
        .expect("models is an array")
        .iter()
        .map(|entry| entry.as_object().expect("an object").clone())
        .collect()
}

#[test]
fn catalog_apply_patch_capability() {
    let registry = ModelRegistry::new();
    registry.register_client(
        "catalog-patch-metadata-test",
        "custom-compat",
        &[
            ModelInfo {
                id: "catalog-patch-alias".to_owned(),
                metadata_model_id: "gpt-5.5".to_owned(),
                ..ModelInfo::default()
            },
            ModelInfo {
                id: "catalog-patch-image".to_owned(),
                model_type: OPENAI_IMAGE_MODEL_TYPE.to_owned(),
                ..ModelInfo::default()
            },
            ModelInfo {
                id: "catalog-patch-image-input".to_owned(),
                supported_input_modalities: vec!["image".to_owned()],
                ..ModelInfo::default()
            },
        ],
    );
    let custom = |_: &str| vec!["custom-compat".to_owned()];
    let mixed = |_: &str| vec!["codex".to_owned(), "custom-compat".to_owned()];
    let lookups: [(&str, Option<ProvidersForModel<'_>>); 3] = [
        ("nil", None),
        ("custom", Some(&custom)),
        ("mixed", Some(&mixed)),
    ];

    for version in ["", "0.137.0", "0.153.4", "cpa"] {
        for (id, can_text) in [
            ("gpt-5.5", true),
            ("custom-model", true),
            ("catalog-patch-alias", true),
            ("team/gpt-5.5", true),
            ("gpt-reserve", true),
            ("gpt-image-2", false),
            ("team/gpt-image-2", false),
            ("team/nested/gpt-image-2", false),
            ("GPT-IMAGE-2", false),
            ("grok-imagine-video", false),
            ("catalog-patch-image", false),
            ("catalog-patch-image-input", false),
        ] {
            let unsupported = |_: &str| false;
            let supported = |queried: &str| {
                assert_eq!(queried, id, "the capability takes the exact public ID");
                true
            };
            let capabilities: [(&str, Option<ApplyPatchCapability<'_>>, bool); 3] = [
                ("nil", None, false),
                ("unsupported", Some(&unsupported), false),
                ("supported", Some(&supported), can_text),
            ];
            for (lookup_name, providers) in lookups {
                for (capability_name, capability, want) in capabilities {
                    let name = format!("{version}/{id}/{lookup_name}/{capability_name}");
                    let available = [ModelInfo {
                        id: format!(" {id} "),
                        ..ModelInfo::default()
                    }];
                    let baseline =
                        build_response(&registry, &available, providers, None, true, version);
                    let response =
                        build_response(&registry, &available, providers, capability, true, version);
                    let mut response_entries = entries(&response);
                    assert_eq!(response_entries.len(), 1, "{name}");
                    let want = if want { json!("freeform") } else { Value::Null };
                    assert_eq!(
                        response_entries[0].get("apply_patch_tool_type"),
                        Some(&want),
                        "{name}"
                    );
                    // Opting in must not change anything else.
                    let mut baseline_entries = entries(&baseline);
                    response_entries[0].shift_remove("apply_patch_tool_type");
                    baseline_entries[0].shift_remove("apply_patch_tool_type");
                    assert_eq!(response_entries, baseline_entries, "{name}");
                }
            }
        }
    }
}

#[test]
fn catalog_apply_patch_without_capability_is_unknown() {
    let available = ["gpt-5.5", "custom-model", "gpt-reserve"].map(|id| ModelInfo {
        id: id.to_owned(),
        ..ModelInfo::default()
    });
    let registry = ModelRegistry::new();
    for version in ["", "0.153.4", "cpa"] {
        let response = build_response(&registry, &available, None, None, false, version);
        for entry in entries(&response) {
            assert_eq!(
                entry.get("apply_patch_tool_type"),
                Some(&Value::Null),
                "{version:?} {}",
                entry["slug"]
            );
        }
    }
}

#[test]
fn apply_patch_field_modalities() {
    let cases = [
        (
            "hidden-text-and-image",
            "hide",
            json!(["text", "image"]),
            true,
        ),
        ("hidden-text", "hide", json!(["text"]), true),
        ("hidden-image", "hide", json!(["image"]), false),
        ("hidden-unknown", "hide", Value::Null, false),
        ("hidden-empty", "hide", json!([]), false),
        ("public-image", "list", json!(["image"]), false),
        ("public-unconstrained", "list", json!([]), true),
        ("public-text", "list", json!(["text"]), true),
    ];
    let registry = ModelRegistry::new();
    for (name, visibility, modalities, want) in cases {
        let mut entry = Map::new();
        entry.insert("visibility".into(), visibility.into());
        entry.insert("input_modalities".into(), modalities);
        entry.insert("apply_patch_tool_type".into(), "function".into());
        let called = Cell::new(false);
        let capability = |id: &str| {
            called.set(true);
            assert_eq!(id, "public-alias", "{name}");
            true
        };
        let builder = Builder {
            catalog: &registry,
            providers_for_model: None,
            apply_patch: Some(&capability),
            optimize_multi_agent_v2: false,
            client_version: "",
        };
        builder.apply_apply_patch_capability(&mut entry, " public-alias ");
        let want_value = if want { json!("freeform") } else { Value::Null };
        assert_eq!(
            entry.get("apply_patch_tool_type"),
            Some(&want_value),
            "{name}"
        );
        assert_eq!(called.get(), want, "{name}: callback called");
    }
}
