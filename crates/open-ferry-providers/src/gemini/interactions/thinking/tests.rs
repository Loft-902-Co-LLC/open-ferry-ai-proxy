// Ported from CLIProxyAPI test/thinking_conversion_test.go and
// test/summary_intent_translation_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The Interactions thinking conversion matrix
//! (`TestThinkingE2EInteractionsMatrix`), what an Interactions request's
//! invalid summary setting becomes, and the applier's own rules.
//!
//! Changed:
//! - The test models are registered in a model registry of their own,
//!   rather than the global one.
//! - The OpenAI, Codex and Gemini cases apply the setting through those
//!   targets' executor entries, with no client request, since their targets
//!   are private to their modules: as upstream's `ApplyThinking`, the body
//!   alone then says whether summaries are shown.
//!
//! Dropped:
//! - `OUT1` and `OUT8` (Claude): the Claude target looks models up in the
//!   built-in catalog only, which has no `claude-budget-model`.
//! - `OUT5` and `OUT9` (Antigravity), `OUT6` and `OUT10` (Kimi) and `OUT7`
//!   (xAI), and the Antigravity case of
//!   `TestInvalidInteractionsSummaryDoesNotWriteTargetControl`: those
//!   targets aren't ported.

use std::sync::Arc;

use open_ferry_core::models::{ModelCatalog, ModelInfo, ThinkingSupport};
use open_ferry_core::registry::ModelRegistry;
use open_ferry_translate::registry::{Format, Registry};
use serde_json::{Value, json};

use super::*;
use crate::codex::thinking::lookup as find_model;
use crate::thinking::{apply_thinking, parse_suffix};

fn support(min: i64, max: i64, levels: &[&str], zero: bool, dynamic: bool) -> ThinkingSupport {
    ThinkingSupport {
        min,
        max,
        levels: levels.iter().map(|level| (*level).to_owned()).collect(),
        zero_allowed: zero,
        dynamic_allowed: dynamic,
    }
}

fn model(id: &str, model_type: &str, thinking: ThinkingSupport) -> ModelInfo {
    ModelInfo {
        id: id.to_owned(),
        model_type: model_type.to_owned(),
        thinking: Some(thinking),
        ..ModelInfo::default()
    }
}

/// `getTestModels`, the ones the Interactions matrix uses.
fn catalog() -> Arc<ModelRegistry> {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_client(
        "thinking-test",
        "test",
        &[
            model(
                "level-model",
                "openai",
                support(0, 0, &["minimal", "low", "medium", "high"], false, false),
            ),
            model(
                "level-subset-model",
                "gemini",
                support(0, 0, &["low", "high"], false, false),
            ),
            model(
                "gemini-budget-model",
                "gemini",
                support(128, 20000, &[], false, true),
            ),
            model(
                "gemini-toggle-mixed-model",
                "gemini",
                support(128, 32768, &["low", "high"], true, true),
            ),
        ],
    );
    registry
}

/// A matrix case: its upstream name, the client's format, the model, the
/// client's request, the fields expected with their values, and the fields
/// expected absent.
type Case = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
    &'static [&'static str],
);

/// `runThinkingTests` for one target, named `to`: translates each case's
/// request to `to` and applies the setting with `apply`, then checks the
/// fields. On a Gemini body, `includeThoughts` must be absent too, as no
/// case asks for summaries.
fn run_matrix(
    to: &str,
    cases: &[Case],
    apply: impl Fn(&mut Value, &str, &str, &dyn ModelCatalog) -> Result<(), ExecError>,
) {
    let catalog = catalog();
    for &(name, from, model, input, want, absent) in cases {
        let name = format!("Case{name}_{from}->{to}_{model}");
        let input: Value = serde_json::from_str(input).unwrap();
        let mut body = Registry::global().translate_request(
            &Format::new(from.to_owned()),
            &Format::new(to.to_owned()),
            parse_suffix(model).0,
            input,
            true,
        );
        apply(&mut body, model, from, catalog.as_ref())
            .unwrap_or_else(|error| panic!("{name}: {}: {body}", error.message));
        for field in absent {
            assert!(!json::exists(&body, field), "{name}: {field}: {body}");
        }
        for (field, value) in want {
            let found = json::get(&body, field);
            assert!(found.is_some(), "{name}: {field}: {body}");
            assert_eq!(json::str_of(found), *value, "{name}: {field}: {body}");
        }
        if to == "gemini" {
            assert!(
                !json::exists(&body, "generationConfig.thinkingConfig.includeThoughts"),
                "{name}: {body}"
            );
        }
    }
}

const LEVEL: &str = "generation_config.thinking_level";

/// Ports TestThinkingE2EInteractionsMatrix: the cases with an Interactions target.
#[test]
fn interactions_matrix_as_provider() {
    run_matrix(
        "interactions",
        &[
            (
                "IN1",
                "claude",
                "level-model",
                r#"{"model":"level-model","max_tokens":1024,"messages":[{"role":"user","content":"hi"}],"thinking":{"type":"enabled","budget_tokens":10000}}"#,
                &[(LEVEL, "high")],
                &[],
            ),
            (
                "IN2",
                "openai",
                "level-model",
                r#"{"model":"level-model","messages":[{"role":"user","content":"hi"}],"reasoning_effort":"minimal"}"#,
                &[(LEVEL, "minimal")],
                &[],
            ),
            (
                "IN3",
                "openai-response",
                "level-model",
                r#"{"model":"level-model","input":"hi","reasoning":{"effort":"low"}}"#,
                &[(LEVEL, "low")],
                &[],
            ),
            (
                "IN4",
                "gemini",
                "level-model",
                r#"{"model":"level-model","contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"thinkingConfig":{"includeThoughts":true,"thinkingBudget":20000}}}"#,
                &[(LEVEL, "high")],
                &[],
            ),
            // A level the model doesn't have becomes its highest.
            (
                "IN5",
                "openai",
                "level-subset-model",
                r#"{"model":"level-subset-model","messages":[{"role":"user","content":"hi"}],"reasoning_effort":"xhigh"}"#,
                &[(LEVEL, "high")],
                &[],
            ),
            // The model can't think not at all, so it thinks least.
            (
                "IN6",
                "claude",
                "level-model",
                r#"{"model":"level-model","max_tokens":1024,"messages":[{"role":"user","content":"hi"}],"thinking":{"type":"disabled"}}"#,
                &[(LEVEL, "minimal")],
                &[],
            ),
            (
                "IN7",
                "openai",
                "level-model(none)",
                r#"{"model":"level-model(none)","messages":[{"role":"user","content":"hi"}]}"#,
                &[(LEVEL, "minimal")],
                &[],
            ),
            (
                "IN8",
                "interactions",
                "level-model",
                r#"{"model":"level-model","generation_config":{"thinking_level":"none"},"input":"hi"}"#,
                &[(LEVEL, "minimal")],
                &[],
            ),
            (
                "IN9",
                "interactions",
                "level-model",
                r#"{"model":"level-model","generation_config":{"thinking_level":"low","thinking_summaries":"auto"},"input":"hi"}"#,
                &[
                    (LEVEL, "low"),
                    ("generation_config.thinking_summaries", "auto"),
                ],
                &[],
            ),
            // A budget becomes the level it stands for.
            (
                "IN10",
                "interactions",
                "level-model",
                r#"{"model":"level-model","generation_config":{"thinking_budget":400},"input":"hi"}"#,
                &[(LEVEL, "minimal")],
                &[],
            ),
            // Auto, on a model that can't think dynamically, is its middle level.
            (
                "IN11",
                "interactions",
                "level-model",
                r#"{"model":"level-model","generation_config":{"thinking_budget":-1},"input":"hi"}"#,
                &[(LEVEL, "medium")],
                &[],
            ),
        ],
        |body, model, from, catalog| {
            let route = Route {
                model,
                from,
                to: "interactions",
                provider: "interactions",
            };
            apply_thinking::<Interactions>(body, route, |id| {
                find_model(Some(catalog), id, "interactions")
            })
        },
    );
}

/// Ports TestThinkingE2EInteractionsMatrix: the case with an OpenAI target.
#[test]
fn interactions_matrix_to_openai() {
    run_matrix(
        "openai",
        &[(
            "OUT2",
            "interactions",
            "level-model",
            r#"{"model":"level-model","generation_config":{"thinking_level":"high"},"input":"hi"}"#,
            &[("reasoning_effort", "high")],
            &[],
        )],
        |body, model, from, catalog| {
            let route = Route {
                model,
                from,
                to: "openai",
                provider: "openai",
            };
            crate::openai_compat::thinking::apply_request(
                body,
                route,
                &Body::Empty,
                &Body::Empty,
                Some(catalog),
            )
        },
    );
}

/// Ports TestThinkingE2EInteractionsMatrix: the case with a Codex target.
#[test]
fn interactions_matrix_to_codex() {
    run_matrix(
        "codex",
        &[(
            "OUT3",
            "interactions",
            "level-model",
            r#"{"model":"level-model","generation_config":{"thinking_level":"low"},"input":"hi"}"#,
            &[("reasoning.effort", "low")],
            &[],
        )],
        |body, model, from, catalog| {
            let route = Route {
                model,
                from,
                to: "codex",
                provider: "codex",
            };
            crate::codex::thinking::apply_request(
                body,
                route,
                &Body::Empty,
                &Body::Empty,
                Some(catalog),
            )
        },
    );
}

/// Ports TestThinkingE2EInteractionsMatrix: the cases with a Gemini target.
#[test]
fn interactions_matrix_to_gemini() {
    const BUDGET: &str = "generationConfig.thinkingConfig.thinkingBudget";
    run_matrix(
        "gemini",
        &[
            (
                "OUT4",
                "interactions",
                "gemini-budget-model",
                r#"{"model":"gemini-budget-model","generation_config":{"thinking_level":"medium"},"input":"hi"}"#,
                &[(BUDGET, "8192")],
                &[],
            ),
            // A model that can think not at all drops its thinking config.
            (
                "OUT11",
                "interactions",
                "gemini-toggle-mixed-model",
                r#"{"model":"gemini-toggle-mixed-model","generation_config":{"thinking_level":"none"},"input":"hi"}"#,
                &[],
                &["generationConfig.thinkingConfig"],
            ),
            // Auto is dynamic thinking where the model can do it.
            (
                "OUT12",
                "interactions",
                "gemini-budget-model",
                r#"{"model":"gemini-budget-model","generation_config":{"thinking_level":"auto"},"input":"hi"}"#,
                &[(BUDGET, "-1")],
                &[],
            ),
        ],
        |body, model, from, catalog| {
            crate::gemini::thinking::apply_request(
                body,
                model,
                from,
                &Body::Empty,
                &Body::Empty,
                Some(catalog),
                "gemini",
            )
        },
    );
}

/// Ports TestInvalidInteractionsSummaryDoesNotWriteTargetControl: the
/// Gemini case.
#[test]
fn an_invalid_summary_setting_shows_nothing_on_gemini() {
    let body = json!({"model": "model", "generation_config": {"thinking_summaries": "banana"}, "input": "hi"});
    let out = Registry::global().translate_request(
        &Format::INTERACTIONS,
        &Format::GEMINI,
        "model",
        body,
        false,
    );
    assert!(
        !json::exists(&out, "generationConfig.thinkingConfig.includeThoughts"),
        "{out}"
    );
}

/// Ports TestInvalidInteractionsSummaryDoesNotWriteTargetControl: the
/// Codex case.
#[test]
fn an_invalid_summary_setting_shows_nothing_on_codex() {
    let body = json!({"model": "model", "generation_config": {"thinking_summaries": "banana"}, "input": "hi"});
    let out = Registry::global().translate_request(
        &Format::INTERACTIONS,
        &Format::CODEX,
        "model",
        body,
        false,
    );
    assert!(!json::exists(&out, "reasoning.summary"), "{out}");
}

// Not upstream's: a budget becomes the level it stands for, matched to the
// model's levels, and every other spelling of the setting goes.
#[test]
fn a_budget_becomes_a_level_in_place_of_other_spellings() {
    let levels = ["low".to_owned(), "high".to_owned()];
    let mut body = json!({
        "generation_config": {"thinking_budget": 512, "thinkingLevel": "LOW", "thinking_config": {}},
        "generationConfig": {"thinkingConfig": {"thinkingBudget": 1}, "temperature": 1},
    });
    apply(&mut body, &Config::budget(20000), &levels);
    assert_eq!(
        body,
        json!({
            "generation_config": {"thinking_level": "high"},
            "generationConfig": {"temperature": 1},
        })
    );
}

// Not upstream's: whether summaries are shown is kept from the request, its
// `thinking_summaries` first, else its `include_thoughts`.
#[test]
fn keeps_the_requests_summary_setting() {
    let summaries = |body: Value, config: Config| {
        let mut body = body;
        apply(&mut body, &config, &[]);
        json::get(&body, THINKING_SUMMARIES).cloned()
    };
    let include = |include: bool| json!({"generation_config": {"thinking_config": {"includeThoughts": include}}});
    assert_eq!(
        summaries(include(false), Config::level("low")),
        Some(json!("none"))
    );
    assert_eq!(
        summaries(include(true), Config::auto()),
        Some(json!("auto"))
    );
    assert_eq!(
        summaries(
            json!({"generation_config": {"thinking_summaries": " NONE ", "thinkingConfig": {"include_thoughts": true}}}),
            Config::level("high"),
        ),
        Some(json!("none"))
    );
    assert_eq!(
        summaries(
            json!({"generation_config": {"thinking_summaries": "banana"}}),
            Config::level("high"),
        ),
        None
    );
    // Turned off, thinking shows no summaries.
    assert_eq!(summaries(include(true), Config::none()), None);
}

// Not upstream's: without the model's levels, `xhigh` and `max` are
// `high`, and off or auto set no level.
#[test]
fn a_level_without_the_models_levels() {
    let level = |config: Config| {
        let mut body = Value::Null;
        apply(&mut body, &config, &[]);
        json::get(&body, THINKING_LEVEL).cloned()
    };
    assert_eq!(level(Config::level(" XHigh ")), Some(json!("high")));
    assert_eq!(level(Config::level("max")), Some(json!("high")));
    assert_eq!(level(Config::level("medium")), Some(json!("medium")));
    assert_eq!(level(Config::level("auto")), None);
    assert_eq!(level(Config::budget(-1)), None);
    assert_eq!(level(Config::none()), None);
}
