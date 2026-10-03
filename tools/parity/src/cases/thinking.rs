//! Hand-written cases for upstream's `ApplyThinkingWithModelInfo` on a Codex
//! or Responses target ([`codex`]) and on a Chat Completions one
//! ([`openai`]), after upstream's thinking tests.
//!
//! A case's request is the body already translated for the target, and its
//! model the model with its suffix. Its options hold the rest (see
//! `go/parity_thinking.go`): the client's request as sent (`source`, empty
//! for none), its format (`from`), the target (`to`), the executor's
//! provider (`provider`), and the model the request is bound to
//! (`model_info`, null for one nobody registered), which [`model_info`]
//! reads as our side takes it.

use open_ferry_core::models::{ModelInfo, ThinkingSupport};
use serde_json::{Value, json};

use super::Case;

/// A thinking case: `body` going to `to` for a client of format `from`
/// that sent `source`, through an executor of `provider`.
#[expect(clippy::too_many_arguments)]
pub fn case(
    name: &str,
    model: &str,
    body: &Value,
    source: &str,
    from: &str,
    to: &str,
    provider: &str,
    info: Value,
) -> Case {
    Case::new(name, model, body.to_string()).with_options(json!({
        "source": source,
        "from": from,
        "to": to,
        "provider": provider,
        "model_info": info,
    }))
}

/// The model a case binds its request to, as the harness builds upstream's
/// `registry.ModelInfo` from it: `None` for null.
pub fn model_info(value: &Value) -> Option<ModelInfo> {
    let object = value.as_object()?;
    let text = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let flag = |key: &str| object.get(key).and_then(Value::as_bool).unwrap_or(false);
    let thinking = object
        .get("thinking")
        .filter(|thinking| thinking.is_object())
        .map(|thinking| ThinkingSupport {
            min: thinking["min"].as_i64().unwrap_or(0),
            max: thinking["max"].as_i64().unwrap_or(0),
            zero_allowed: thinking["zero_allowed"].as_bool().unwrap_or(false),
            dynamic_allowed: thinking["dynamic_allowed"].as_bool().unwrap_or(false),
            levels: thinking["levels"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
        });
    Some(ModelInfo {
        id: text("id"),
        model_type: text("type"),
        user_defined: flag("user_defined"),
        support_configuration_update: flag("support_configuration_update"),
        thinking,
        ..ModelInfo::default()
    })
}

/// `gpt-5.5` as the built-in catalog has it.
fn gpt_5_5() -> Value {
    json!({
        "id": "gpt-5.5",
        "type": "openai",
        "thinking": { "levels": ["low", "medium", "high", "xhigh"] },
    })
}

/// `gpt-6-sol`, which takes Responses effort updates.
fn gpt_6_sol() -> Value {
    json!({
        "id": "gpt-6-sol",
        "type": "openai",
        "support_configuration_update": true,
        "thinking": { "levels": ["low", "medium", "high", "xhigh", "max"] },
    })
}

/// A model that can't think.
fn plain() -> Value {
    json!({ "id": "plain-model", "type": "openai" })
}

/// A model from the config's model list.
fn user_defined() -> Value {
    json!({ "id": "custom-model", "user_defined": true })
}

/// A Claude model with adaptive levels, for mapping high levels across
/// families.
fn claude_levels() -> Value {
    json!({
        "id": "claude-opus-4-6",
        "type": "claude",
        "thinking": { "min": 1024, "max": 128000, "levels": ["low", "medium", "high", "max"] },
    })
}

/// A model that takes only a budget.
fn budget_only() -> Value {
    json!({
        "id": "budget-model",
        "type": "gemini",
        "thinking": { "min": 128, "max": 32768, "zero_allowed": true, "dynamic_allowed": true },
    })
}

/// A request with an effort update between two messages.
fn with_update(effort: &str, top: Option<&str>) -> Value {
    let mut body = json!({
        "model": "gpt",
        "input": [
            { "type": "message", "role": "user", "content": "hi" },
            { "type": "configuration_update", "reasoning": { "effort": effort } },
            { "type": "message", "role": "user", "content": "again" },
        ],
    });
    if let Some(top) = top {
        body["reasoning"] = json!({ "effort": top, "summary": "auto" });
    }
    body
}

pub fn codex() -> Vec<Case> {
    let responses = "openai-response";
    let hi = json!({ "model": "gpt", "input": "hi" });
    let low = json!({ "input": "hi", "reasoning": { "effort": "low" } });
    vec![
        case(
            "suffix-level",
            "gpt-5.5(high)",
            &hi,
            "",
            responses,
            "codex",
            "codex",
            gpt_5_5(),
        ),
        case(
            "suffix-level-not-supported",
            "gpt-5.5(max)",
            &hi,
            "",
            responses,
            "codex",
            "codex",
            gpt_5_5(),
        ),
        case(
            "suffix-level-clamped-across-families",
            "gpt-5.5(max)",
            &hi,
            r#"{"thinking":{"type":"enabled","budget_tokens":4096}}"#,
            "claude",
            "codex",
            "codex",
            gpt_5_5(),
        ),
        case(
            "suffix-budget-as-level",
            "gpt-5.5(8192)",
            &hi,
            "",
            "openai",
            "codex",
            "codex",
            gpt_5_5(),
        ),
        case(
            "suffix-none-without-none-level",
            "gpt-5.5(none)",
            &low,
            "",
            responses,
            "codex",
            "codex",
            gpt_5_5(),
        ),
        case(
            "suffix-auto-without-dynamic",
            "gpt-5.5(auto)",
            &hi,
            "",
            responses,
            "codex",
            "codex",
            gpt_5_5(),
        ),
        case(
            "suffix-unknown",
            "gpt-5.5(fast)",
            &low,
            "",
            responses,
            "codex",
            "codex",
            gpt_5_5(),
        ),
        case(
            "chat-effort-carried-over",
            "gpt-5.5",
            &json!({ "input": [], "reasoning": { "effort": "xhigh" } }),
            r#"{"messages":[],"reasoning_effort":"xhigh"}"#,
            "openai",
            "codex",
            "codex",
            gpt_5_5(),
        ),
        case(
            "chat-effort-not-supported",
            "gpt-5.5",
            &json!({ "input": [], "reasoning": { "effort": "max" } }),
            r#"{"messages":[],"reasoning_effort":"max"}"#,
            "openai",
            "codex",
            "codex",
            gpt_5_5(),
        ),
        case(
            "chat-source-effort-only",
            "gpt-5.5",
            &json!({ "input": [] }),
            r#"{"messages":[],"reasoning_effort":"high"}"#,
            "openai",
            "codex",
            "codex",
            gpt_5_5(),
        ),
        case(
            "claude-xhigh-mapped-to-max",
            "claude-opus-4-6",
            &json!({ "input": [], "reasoning": { "effort": "xhigh" } }),
            r#"{"thinking":{"type":"adaptive"},"output_config":{"effort":"xhigh"}}"#,
            "claude",
            "codex",
            "codex",
            claude_levels(),
        ),
        case(
            "native-responses-left-alone",
            "gpt-6-sol",
            &with_update("max", Some("low")),
            "",
            responses,
            "codex",
            "codex",
            gpt_6_sol(),
        ),
        case(
            "native-responses-with-suffix",
            "gpt-6-sol(medium)",
            &with_update("max", Some("low")),
            "",
            responses,
            "codex",
            "codex",
            gpt_6_sol(),
        ),
        case(
            "native-responses-through-openai-response",
            "gpt-6-sol",
            &with_update("high", None),
            &with_update("high", None).to_string(),
            responses,
            "openai-response",
            "",
            gpt_6_sol(),
        ),
        case(
            "updates-stripped-and-applied",
            "gpt-5.5",
            &with_update("high", Some("low")),
            "",
            responses,
            "codex",
            "codex",
            gpt_5_5(),
        ),
        case(
            "updates-stripped-then-rejected",
            "gpt-5.5",
            &with_update("max", Some("low")),
            "",
            responses,
            "codex",
            "codex",
            gpt_5_5(),
        ),
        case(
            "update-in-source-only",
            "gpt-5.5",
            &low,
            &with_update(" XHigh ", None).to_string(),
            responses,
            "codex",
            "codex",
            gpt_5_5(),
        ),
        case(
            "updates-for-an-unknown-model",
            "custom-model",
            &with_update("high", Some("low")),
            "",
            responses,
            "codex",
            "codex",
            Value::Null,
        ),
        case(
            "updates-from-a-chat-client",
            "gpt-6-sol",
            &with_update("high", Some("low")),
            r#"{"messages":[]}"#,
            "openai",
            "codex",
            "codex",
            gpt_6_sol(),
        ),
        case(
            "summary-from-source",
            "gpt-5.5",
            &low,
            r#"{"input":"hi","reasoning":{"effort":"low","summary":"detailed"}}"#,
            responses,
            "codex",
            "codex",
            gpt_5_5(),
        ),
        case(
            "summary-hidden-by-source",
            "gpt-5.5(high)",
            &json!({ "input": "hi", "reasoning": { "summary": "auto", "generate_summary": "auto" } }),
            r#"{"input":"hi","reasoning":{"summary":"none"}}"#,
            responses,
            "codex",
            "codex",
            gpt_5_5(),
        ),
        case(
            "summary-hidden-empties-reasoning",
            "gpt-5.5",
            &json!({ "input": "hi", "reasoning": { "summary": "auto" } }),
            r#"{"reasoning_effort":"none"}"#,
            "openai",
            "codex",
            "codex",
            gpt_5_5(),
        ),
        case(
            "summary-from-body-without-source",
            "gpt-5.5",
            &json!({ "input": "hi", "reasoning": { "effort": "high", "summary": "concise" } }),
            "",
            "claude",
            "codex",
            "codex",
            gpt_5_5(),
        ),
        case(
            "model-without-thinking",
            "plain-model",
            &json!({ "input": "hi", "reasoning": { "effort": "high", "summary": "auto" } }),
            "",
            responses,
            "codex",
            "codex",
            plain(),
        ),
        case(
            "model-without-thinking-or-settings",
            "plain-model(high)",
            &hi,
            "",
            responses,
            "codex",
            "codex",
            plain(),
        ),
        case(
            "user-defined-claude-budget",
            "custom-model",
            &json!({ "input": [], "thinking": { "type": "enabled", "budget_tokens": 20000 } }),
            r#"{"messages":[],"thinking":{"type":"enabled","budget_tokens":20000}}"#,
            "claude",
            "codex",
            "codex",
            user_defined(),
        ),
        case(
            "unknown-model-auto",
            "custom-model(auto)",
            &hi,
            "",
            "openai",
            "codex",
            "codex",
            Value::Null,
        ),
        case(
            "budget-model-level",
            "budget-model(high)",
            &hi,
            "",
            "gemini",
            "codex",
            "codex",
            budget_only(),
        ),
        case(
            "invalid-source",
            "gpt-5.5",
            &low,
            "not json",
            "openai",
            "codex",
            "codex",
            gpt_5_5(),
        ),
        case(
            "invalid-responses-source-with-an-effort",
            "gpt-5.5",
            &hi,
            r#"{"reasoning":{"effort":"high"}"#,
            responses,
            "codex",
            "codex",
            gpt_5_5(),
        )
        .known_difference(
            "upstream reads an invalid Responses request's effort leniently with gjson; we read none",
        ),
        case(
            "formats-in-other-cases",
            "gpt-5.5(HIGH)",
            &hi,
            r#"{"input":"hi","reasoning":{"summary":"auto"}}"#,
            " OpenAI-Response ",
            " Codex ",
            " CODEX ",
            gpt_5_5(),
        ),
    ]
}

pub fn openai() -> Vec<Case> {
    let messages = json!({ "model": "gpt", "messages": [] });
    vec![
        case(
            "suffix-level",
            "gpt-5.5(high)",
            &messages,
            "",
            "openai",
            "openai",
            "openai",
            gpt_5_5(),
        ),
        case(
            "suffix-level-on-openrouter",
            "gpt-5.5(high)",
            &messages,
            r#"{"messages":[],"reasoning_effort":"low"}"#,
            "openai",
            "openai",
            "openrouter",
            gpt_5_5(),
        ),
        case(
            "summary-shown-on-openrouter",
            "gpt-5.5",
            &json!({ "messages": [], "reasoning_effort": "low" }),
            r#"{"messages":[],"reasoning_effort":"low"}"#,
            "openai",
            "openai",
            "my-openrouter",
            gpt_5_5(),
        ),
        case(
            "summary-hidden-on-openrouter",
            "gpt-5.5",
            &json!({ "messages": [], "reasoning_effort": "high" }),
            r#"{"input":"hi","reasoning":{"effort":"high","summary":"none"}}"#,
            "openai-response",
            "openai",
            "openrouter.ai",
            gpt_5_5(),
        ),
        case(
            "summary-in-existing-fields",
            "gpt-5.5",
            &json!({ "messages": [], "reasoning_effort": "high", "include_reasoning": false, "reasoning": { "exclude": true } }),
            r#"{"input":"hi","reasoning":{"effort":"high","summary":"detailed"}}"#,
            "openai-response",
            "openai",
            "deepseek",
            gpt_5_5(),
        ),
        case(
            "summary-not-for-another-provider",
            "gpt-5.5",
            &json!({ "messages": [], "reasoning_effort": "high" }),
            r#"{"input":"hi","reasoning":{"effort":"high","summary":"detailed"}}"#,
            "openai-response",
            "openai",
            "openrouterx",
            gpt_5_5(),
        ),
        case(
            "turned-off-wins-over-summary",
            "gpt-5.5(none)",
            &json!({ "messages": [], "include_reasoning": true }),
            r#"{"input":"hi","reasoning":{"summary":"auto"}}"#,
            "openai-response",
            "openai",
            "openrouter",
            json!({ "id": "zero", "type": "openai", "thinking": { "zero_allowed": true, "levels": ["low", "high"] } }),
        ),
        case(
            "level-not-supported",
            "gpt-5.5(minimal)",
            &messages,
            "",
            "openai",
            "openai",
            "openai",
            gpt_5_5(),
        ),
        case(
            "claude-max-mapped",
            "gpt-5.5",
            &json!({ "messages": [], "reasoning_effort": "max" }),
            r#"{"thinking":{"type":"adaptive"},"output_config":{"effort":"max"}}"#,
            "claude",
            "openai",
            "openai",
            gpt_5_5(),
        ),
        case(
            "model-without-thinking",
            "plain-model",
            &json!({ "messages": [], "reasoning_effort": "high", "reasoning": { "exclude": true } }),
            "",
            "openai",
            "openai",
            "openrouter",
            plain(),
        ),
        case(
            "unknown-model-budget",
            "custom-model(8192)",
            &messages,
            "",
            "openai",
            "openai",
            "openai",
            Value::Null,
        ),
        case(
            "unknown-model-gemini-budget",
            "custom-model",
            &json!({ "messages": [], "generationConfig": { "thinkingConfig": { "thinkingBudget": -1 } } }),
            r#"{"contents":[],"generationConfig":{"thinkingConfig":{"thinkingBudget":-1,"includeThoughts":true}}}"#,
            "gemini",
            "openai",
            "openrouter",
            Value::Null,
        ),
        case(
            "user-defined-level",
            "custom-model",
            &json!({ "messages": [], "reasoning_effort": "Turbo" }),
            "",
            "openai",
            "openai",
            "openai",
            user_defined(),
        ),
        case(
            "responses-update-in-source",
            "gpt-5.5",
            &json!({ "messages": [], "reasoning_effort": "low" }),
            &with_update("xhigh", Some("low")).to_string(),
            "openai-response",
            "openai",
            "openai",
            gpt_5_5(),
        ),
        case(
            "reasoning-not-an-object",
            "gpt-5.5(low)",
            &json!({ "messages": [], "reasoning": "high" }),
            r#"{"messages":[],"reasoning_effort":"low"}"#,
            "openai",
            "openai",
            "openrouter",
            gpt_5_5(),
        ),
        case(
            "budget-model",
            "budget-model(medium)",
            &messages,
            "",
            "openai",
            "openai",
            "openai",
            budget_only(),
        ),
        case(
            "formats-in-other-cases",
            "gpt-5.5( High )",
            &messages,
            r#"{"messages":[],"reasoning_effort":"low"}"#,
            " OpenAI ",
            "OPENAI",
            " OpenRouter ",
            gpt_5_5(),
        ),
    ]
}
