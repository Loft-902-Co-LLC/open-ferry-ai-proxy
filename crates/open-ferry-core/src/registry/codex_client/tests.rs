//! Ports CLIProxyAPI internal/registry/codex_client_models_test.go
//! (v8.0.20, MIT).
//!
//! Changed: `TestEmbeddedCodexClientModelsCatalogIsValid` checks the built-in
//! catalog and its default template; the snapshot and revision checks are
//! dropped, as the built-in catalog has neither.
//! `TestLoadCodexClientModelsRejectsInvalidWithoutReplacing` becomes a check
//! that an invalid catalog doesn't load, since there is no loaded catalog to
//! replace. `TestValidateCodexClientModelsJSON` also checks each error's
//! text, except for Go's decode error.
//!
//! Dropped: `TestFetchCodexClientModelsFallsBackToNextURL` and
//! `TestRefreshCodexClientModelsKeepsLastValidSnapshot`, as the catalog isn't
//! fetched or refreshed.
//!
//! Added: the Go decoding rules and the number and level checks.

use serde_json::{Value, json};

use super::*;

fn test_model(slug: &str, priority: i64) -> Value {
    json!({
        "slug": slug,
        "display_name": format!("Test {slug}"),
        "description": "Test model",
        "base_instructions": "Test instructions",
        "minimal_client_version": "0.144.0",
        "visibility": "list",
        "context_window": 372000,
        "max_context_window": 372000,
        "priority": priority,
        "default_reasoning_level": "medium",
        "supported_reasoning_levels": [{"effort": "medium", "description": "Balanced"}],
    })
}

fn test_catalog(models: &[Value]) -> Vec<u8> {
    json!({ "models": models }).to_string().into_bytes()
}

#[test]
fn embedded_catalog_is_valid() {
    validate_codex_client_models_json(EMBEDDED_CATALOG).unwrap();
    let catalog = CodexClientCatalog::embedded().expect("the built-in catalog loads");
    assert_eq!(catalog.default_template()["slug"], DEFAULT_TEMPLATE);
    assert_eq!(catalog.templates().count(), 10);
    assert!(catalog.template("gpt-6-astra").is_some());
}

#[test]
fn validate_codex_client_models_json_rejects_incomplete_catalogs() {
    let valid_default = test_model("gpt-5.5", 1);
    let valid_other = test_model("gpt-5.6-sol", 2);
    let mut empty_slug = test_model("gpt-5.5", 1);
    empty_slug["slug"] = json!("");
    let mut missing_field = test_model("gpt-5.5", 1);
    missing_field
        .as_object_mut()
        .unwrap()
        .shift_remove("base_instructions");
    let mut wrong_field_type = test_model("gpt-5.5", 1);
    wrong_field_type["context_window"] = json!("372000");
    let mut unsupported_default = test_model("gpt-5.5", 1);
    unsupported_default["default_reasoning_level"] = json!("high");

    let cases: [(&str, Vec<u8>, &str); 8] = [
        (
            "malformed",
            br#"{"models":"#.to_vec(),
            "decode Codex client model catalog: ",
        ),
        (
            "empty",
            br#"{"models":[]}"#.to_vec(),
            "Codex client model catalog has no models",
        ),
        (
            "empty slug",
            test_catalog(&[empty_slug]),
            r#"Codex client model catalog models[0]: field "slug" must be a non-empty string"#,
        ),
        (
            "duplicate slug",
            test_catalog(&[valid_default.clone(), valid_default.clone()]),
            r#"Codex client model catalog contains duplicate slug "gpt-5.5""#,
        ),
        (
            "missing default",
            test_catalog(std::slice::from_ref(&valid_other)),
            r#"Codex client model catalog is missing default template "gpt-5.5""#,
        ),
        (
            "missing required field",
            test_catalog(&[missing_field]),
            r#"Codex client model catalog model "gpt-5.5": field "base_instructions" must be a non-empty string"#,
        ),
        (
            "wrong required field type",
            test_catalog(&[wrong_field_type]),
            r#"Codex client model catalog model "gpt-5.5": field "context_window" must be an integer"#,
        ),
        (
            "default reasoning level not supported",
            test_catalog(&[unsupported_default]),
            r#"Codex client model catalog model "gpt-5.5": default_reasoning_level "high" is not listed in supported_reasoning_levels"#,
        ),
    ];
    for (name, raw, want) in cases {
        let err = validate_codex_client_models_json(&raw).expect_err(name);
        assert!(err.to_string().starts_with(want), "{name}: {err}");
    }

    let valid = test_catalog(&[valid_default, valid_other]);
    validate_codex_client_models_json(&valid).unwrap();
}

#[test]
fn invalid_catalog_does_not_load() {
    let valid = test_catalog(&[test_model("gpt-5.5", 1)]);
    let catalog = CodexClientCatalog::from_json(&valid).unwrap();
    assert_eq!(catalog.default_template()["display_name"], "Test gpt-5.5");
    assert!(CodexClientCatalog::from_json(br#"{"models":[]}"#).is_err());
}

#[test]
fn validation_follows_go_decoding() {
    // The key matches in any case, the last one winning; null is empty.
    let model = test_model("gpt-5.5", 1);
    let raw = json!({"models": [], "MODELS": [model]}).to_string();
    validate_codex_client_models_json(raw.as_bytes()).unwrap();
    for raw in ["null", r#"{"models":null}"#, "{}"] {
        let err = validate_codex_client_models_json(raw.as_bytes()).unwrap_err();
        assert_eq!(err.to_string(), "Codex client model catalog has no models");
    }
    for raw in [
        "[]",
        r#"{"models":{}}"#,
        r#"{"models":[1]}"#,
        r#"{"models":[{"x":1e400}]}"#,
    ] {
        let err = validate_codex_client_models_json(raw.as_bytes()).unwrap_err();
        assert!(err.to_string().starts_with("decode "), "{raw}: {err}");
    }
    let err = validate_codex_client_models_json(br#"{"models":[null]}"#).unwrap_err();
    assert_eq!(
        err.to_string(),
        r#"Codex client model catalog models[0]: field "slug" must be a non-empty string"#
    );
}

#[test]
fn validation_checks_numbers_and_levels() {
    let cases: [(&str, Value, &str); 7] = [
        (
            "context_window",
            json!(0),
            r#"field "context_window" must be positive"#,
        ),
        (
            "context_window",
            json!(1.5),
            r#"field "context_window" must be an integer"#,
        ),
        (
            "priority",
            json!(-1),
            r#"field "priority" must not be negative"#,
        ),
        (
            "context_window",
            json!(372001),
            "context_window 372001 exceeds max_context_window 372000",
        ),
        (
            "supported_reasoning_levels",
            json!([]),
            r#"field "supported_reasoning_levels" must be a non-empty array"#,
        ),
        (
            "supported_reasoning_levels",
            json!(["medium"]),
            r#"field "supported_reasoning_levels" entry 0 must be an object"#,
        ),
        (
            "supported_reasoning_levels",
            json!([{"effort": "medium"}, {"effort": " medium "}]),
            r#"field "supported_reasoning_levels" contains duplicate effort "medium""#,
        ),
    ];
    for (field, value, want) in cases {
        let mut model = test_model("gpt-5.5", 1);
        model[field] = value;
        let err = validate_codex_client_models_json(&test_catalog(&[model])).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(r#"Codex client model catalog model "gpt-5.5": {want}"#)
        );
    }
    // A whole number written with a fraction is an integer, as in Go.
    let mut model = test_model("gpt-5.5", 1);
    model["priority"] = serde_json::from_str("4.0").unwrap();
    validate_codex_client_models_json(&test_catalog(&[model])).unwrap();
}
