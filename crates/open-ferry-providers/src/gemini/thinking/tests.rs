// Ported from CLIProxyAPI test/thinking_conversion_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The Gemini cases of the thinking conversion matrix, and the applier's
//! own rules.
//!
//! Changed:
//! - Upstream translates each case's request to Gemini first. The suffix
//!   cases here start from the Gemini body a translation gives (one with no
//!   thinking setting), with the client's request as the payload, so they
//!   don't depend on the translators; the body cases are those already in
//!   Gemini's format.
//! - The test models are registered in a model registry of their own,
//!   rather than the global one.
//!
//! Dropped:
//! - The cases for targets other than Gemini, and the body cases that need
//!   a translator from another format.

use std::sync::Arc;

use open_ferry_core::models::{ModelCatalog, ModelInfo, ThinkingSupport};
use open_ferry_core::registry::ModelRegistry;
use serde_json::{Value, json};

use super::*;

fn support(min: i64, max: i64, levels: &[&str], zero: bool, dynamic: bool) -> ThinkingSupport {
    ThinkingSupport {
        min,
        max,
        levels: levels.iter().map(|level| (*level).to_owned()).collect(),
        zero_allowed: zero,
        dynamic_allowed: dynamic,
    }
}

fn model(id: &str, model_type: &str, thinking: Option<ThinkingSupport>) -> ModelInfo {
    ModelInfo {
        id: id.to_owned(),
        model_type: model_type.to_owned(),
        thinking,
        ..ModelInfo::default()
    }
}

/// `getTestModels`, the ones the Gemini cases use.
fn catalog() -> Arc<ModelRegistry> {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_client(
        "thinking-test",
        "test",
        &[
            model(
                "level-subset-model",
                "gemini",
                Some(support(0, 0, &["low", "high"], false, false)),
            ),
            model(
                "gemini-budget-model",
                "gemini",
                Some(support(128, 20000, &[], false, true)),
            ),
            model(
                "gemini-mixed-model",
                "gemini",
                Some(support(128, 32768, &["low", "high"], false, true)),
            ),
            model(
                "gemini-toggle-mixed-model",
                "gemini",
                Some(support(128, 32768, &["low", "high"], true, true)),
            ),
            model("no-thinking-model", "openai", None),
            ModelInfo {
                user_defined: true,
                ..model("user-defined-model", "openai", None)
            },
        ],
    );
    registry
}

/// Applies `model`'s setting to `body` for a request from `from`, whose
/// client request was `payload`.
fn apply(from: &str, model: &str, payload: &Value, mut body: Value) -> Result<Value, ExecError> {
    let catalog = catalog();
    let payload = Body::Json(payload.clone());
    apply_request(
        &mut body,
        model,
        from,
        &payload,
        &Body::Empty,
        Some(catalog.as_ref() as &dyn ModelCatalog),
        "gemini",
    )?;
    Ok(body)
}

/// The Gemini body a translation of a request without a thinking setting
/// gives.
fn gemini_body(base: &str) -> Value {
    json!({"model": base, "contents": [{"role": "user", "parts": [{"text": "hi"}]}]})
}

fn client_request(from: &str, model: &str) -> Value {
    match from {
        "claude" => {
            json!({"model": model, "max_tokens": 1024, "messages": [{"role": "user", "content": "hi"}]})
        }
        "openai-response" => json!({"model": model, "input": "hi"}),
        _ => json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}),
    }
}

const BUDGET: &str = "generationConfig.thinkingConfig.thinkingBudget";
const LEVEL: &str = "generationConfig.thinkingConfig.thinkingLevel";
const INCLUDE: &str = "generationConfig.thinkingConfig.includeThoughts";

/// A matrix case: its upstream number, the client's format, the model, and
/// the field and value expected, if any.
type Case<'a> = (&'a str, &'a str, &'a str, Option<(&'a str, Value)>);

// TestThinkingE2EMatrix_Suffix: the cases with a Gemini target.
#[test]
fn suffix_matrix() {
    let cases: &[Case<'_>] = &[
        (
            "17",
            "claude",
            "level-subset-model(1)",
            Some((LEVEL, json!("low"))),
        ),
        ("18", "openai", "gemini-budget-model", None),
        (
            "19",
            "openai",
            "gemini-budget-model(medium)",
            Some((BUDGET, json!(8192))),
        ),
        (
            "20",
            "openai",
            "gemini-budget-model(xhigh)",
            Some((BUDGET, json!(20000))),
        ),
        (
            "21",
            "openai",
            "gemini-budget-model(none)",
            Some((BUDGET, json!(128))),
        ),
        (
            "22",
            "openai",
            "gemini-budget-model(auto)",
            Some((BUDGET, json!(-1))),
        ),
        ("23", "claude", "gemini-budget-model", None),
        (
            "24",
            "claude",
            "gemini-budget-model(8192)",
            Some((BUDGET, json!(8192))),
        ),
        (
            "25",
            "claude",
            "gemini-budget-model(64000)",
            Some((BUDGET, json!(20000))),
        ),
        (
            "26",
            "claude",
            "gemini-budget-model(0)",
            Some((BUDGET, json!(128))),
        ),
        (
            "27",
            "claude",
            "gemini-budget-model(-1)",
            Some((BUDGET, json!(-1))),
        ),
        ("28", "openai", "gemini-mixed-model", None),
        (
            "29",
            "openai",
            "gemini-mixed-model(high)",
            Some((LEVEL, json!("high"))),
        ),
        (
            "30",
            "openai",
            "gemini-mixed-model(xhigh)",
            Some((LEVEL, json!("high"))),
        ),
        (
            "31",
            "openai",
            "gemini-mixed-model(none)",
            Some((LEVEL, json!("low"))),
        ),
        (
            "32",
            "openai",
            "gemini-mixed-model(auto)",
            Some((BUDGET, json!(-1))),
        ),
        ("33", "claude", "gemini-mixed-model", None),
        (
            "34",
            "claude",
            "gemini-mixed-model(8192)",
            Some((BUDGET, json!(8192))),
        ),
        (
            "35",
            "claude",
            "gemini-mixed-model(64000)",
            Some((BUDGET, json!(32768))),
        ),
        (
            "36",
            "claude",
            "gemini-mixed-model(0)",
            Some((LEVEL, json!("low"))),
        ),
        (
            "37",
            "claude",
            "gemini-mixed-model(-1)",
            Some((BUDGET, json!(-1))),
        ),
        (
            "76",
            "openai",
            "user-defined-model(8192)",
            Some((BUDGET, json!(8192))),
        ),
        (
            "78",
            "openai-response",
            "user-defined-model(8192)",
            Some((BUDGET, json!(8192))),
        ),
        (
            "84",
            "gemini",
            "gemini-budget-model(8192)",
            Some((BUDGET, json!(8192))),
        ),
        (
            "85",
            "gemini",
            "gemini-budget-model(64000)",
            Some((BUDGET, json!(20000))),
        ),
    ];
    for (name, from, model, want) in cases {
        let base = crate::thinking::parse_suffix(model).0;
        let body = apply(from, model, &client_request(from, model), gemini_body(base))
            .unwrap_or_else(|error| panic!("case {name}: {}", error.message));
        match want {
            Some((field, value)) => {
                assert_eq!(json::get(&body, field), Some(value), "case {name}: {body}");
            }
            None => assert!(
                !json::exists(&body, "generationConfig.thinkingConfig"),
                "case {name}: {body}"
            ),
        }
        assert!(!json::exists(&body, INCLUDE), "case {name}: {body}");
    }
}

// TestThinkingE2EMatrix_Body: the cases already in Gemini's format.
#[test]
fn body_matrix() {
    let request = |budget: i64| {
        json!({
            "model": "gemini-budget-model",
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "generationConfig": {"thinkingConfig": {"thinkingBudget": budget}}
        })
    };
    let body = apply(
        "gemini",
        "gemini-budget-model",
        &request(8192),
        request(8192),
    )
    .unwrap();
    assert_eq!(json::get(&body, BUDGET), Some(&json!(8192)));
    assert!(!json::exists(&body, INCLUDE));

    // A request's own budget must fit the model.
    let error = apply(
        "gemini",
        "gemini-budget-model",
        &request(64000),
        request(64000),
    )
    .unwrap_err();
    assert_eq!(error.status, 400);
}

#[test]
fn keeps_a_boolean_include_thoughts() {
    let with =
        |thinking: Value| json!({"contents": [], "generationConfig": {"thinkingConfig": thinking}});
    // Snake case becomes camel case; a level replaces a budget.
    let body = apply(
        "gemini",
        "gemini-mixed-model(high)",
        &json!({}),
        with(json!({"thinking_budget": 512, "include_thoughts": true})),
    )
    .unwrap();
    assert_eq!(
        body["generationConfig"]["thinkingConfig"],
        json!({"thinkingLevel": "high", "includeThoughts": true})
    );
    // Only a boolean is kept.
    let body = apply(
        "gemini",
        "gemini-budget-model(1024)",
        &json!({}),
        with(json!({"thinkingLevel": "low", "includeThoughts": "yes", "include_thoughts": false})),
    )
    .unwrap();
    assert_eq!(
        body["generationConfig"]["thinkingConfig"],
        json!({"thinkingBudget": 1024, "includeThoughts": false})
    );
}

#[test]
fn turning_thinking_off() {
    // A model that may turn thinking off loses the whole setting, even a
    // request to show thoughts.
    let body = apply(
        "gemini",
        "gemini-toggle-mixed-model(none)",
        &json!({}),
        json!({"generationConfig": {"thinkingConfig": {"thinkingLevel": "high", "includeThoughts": true}, "temperature": 1}}),
    )
    .unwrap();
    assert_eq!(body["generationConfig"], json!({"temperature": 1}));

    // A budget-only model that can't is clamped to its least.
    let body = apply(
        "gemini",
        "gemini-budget-model(none)",
        &json!({}),
        gemini_body("gemini-budget-model"),
    )
    .unwrap();
    assert_eq!(json::get(&body, BUDGET), Some(&json!(128)));
}

#[test]
fn a_model_without_thinking_loses_the_setting() {
    let body = apply(
        "gemini",
        "no-thinking-model",
        &json!({}),
        json!({"generationConfig": {"thinkingConfig": {"thinkingBudget": 1024}, "topK": 3}}),
    )
    .unwrap();
    assert_eq!(body["generationConfig"], json!({"topK": 3}));
}

// The user-defined path (`applyCompatible`).
#[test]
fn applies_compatible_settings() {
    let cases = [
        (Config::auto(), json!({"thinkingBudget": -1})),
        (Config::level("high"), json!({"thinkingLevel": "high"})),
        (Config::budget(300), json!({"thinkingBudget": 300})),
        (
            Config {
                mode: Mode::None,
                budget: 0,
                level: "low".into(),
            },
            json!({"thinkingLevel": "low"}),
        ),
        (Config::none(), json!({"thinkingBudget": 0})),
    ];
    for (config, want) in cases {
        let mut body = json!({"generationConfig": {"thinkingConfig": {"thinking_level": "x"}}});
        Gemini::apply_compatible(&mut body, &config);
        assert_eq!(
            body["generationConfig"]["thinkingConfig"], want,
            "{config:?}"
        );
    }
}

// LookupModelInfo: the provider's registration first, then the catalog.
#[test]
fn looks_models_up_by_provider() {
    let registry = ModelRegistry::new();
    registry.register_client(
        "vertex-client",
        "vertex",
        &[ModelInfo {
            output_token_limit: 7,
            ..model("gemini-2.5-pro", "vertex", None)
        }],
    );
    let found = lookup(Some(&registry), " gemini-2.5-pro ", " Vertex ").unwrap();
    assert_eq!(
        (found.model_type.as_str(), found.output_token_limit),
        ("vertex", 7)
    );
    let fallback = lookup(None, "gemini-2.5-pro", "gemini").unwrap();
    assert!(fallback.thinking.is_some());
    assert!(lookup(Some(&registry), "  ", "gemini").is_none());
    assert!(lookup(None, "no-such-model", "gemini").is_none());
}
