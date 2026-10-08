//! Ports CLIProxyAPI internal/client/codex/models/apply_patch_test.go
//! (v8.0.20, MIT).
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
                    // Without a capability, a model whose template declares
                    // the tool keeps it, unless other providers serve it.
                    let template = capability.is_none()
                        && providers.is_none()
                        && matches!(
                            id,
                            "gpt-5.5" | "catalog-patch-alias" | "team/gpt-5.5" | "gpt-reserve"
                        );
                    let want = if want || template {
                        json!("freeform")
                    } else {
                        Value::Null
                    };
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
            let want = match entry["slug"].as_str() {
                Some("gpt-5.5" | "gpt-reserve") => json!("freeform"),
                _ => Value::Null,
            };
            assert_eq!(
                entry.get("apply_patch_tool_type"),
                Some(&want),
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
            statics: StaticCatalog::embedded_shared(),
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

// TestCodexCatalogApplyPatch_TemplateModelsRetainFreeformByDefault_Issue6286
#[test]
fn template_models_retain_freeform_by_default_issue_6286() {
    let registry = ModelRegistry::new();
    let only_entry = |id: &str| {
        let available = [ModelInfo {
            id: id.to_owned(),
            ..ModelInfo::default()
        }];
        let response = build_response(&registry, &available, None, None, false, "0.153.4");
        let mut entries = entries(&response);
        assert_eq!(entries.len(), 1, "{id}");
        entries.remove(0)
    };
    for id in [
        "gpt-6.1-sol",
        "gpt-6-astra",
        "gpt-6-sol",
        "gpt-6-luna",
        "gpt-reserve",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
        "gpt-5.5",
    ] {
        assert_eq!(
            only_entry(id).get("apply_patch_tool_type"),
            Some(&json!("freeform")),
            "{id}"
        );
    }
    // A model without a template of its own still gets null.
    assert_eq!(
        only_entry("non-template-custom-model").get("apply_patch_tool_type"),
        Some(&Value::Null)
    );
}
