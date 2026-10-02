// Ported from CLIProxyAPI internal/thinking/summary.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Whether a client asked to see reasoning summaries, and asking the
//! upstream for the same.
//!
//! Only reading a Chat Completions request and writing a Claude one are
//! ported so far.

use serde_json::{Map, Value};

use crate::go;
use crate::json::{int_of, path, str_of};
use crate::models::ModelCatalog;
use crate::thinking::base_model_name;

/// Chat Completions fields that say whether to show summaries, as booleans.
/// The first one present decides. Google's documented extension comes first,
/// then aliases other clients send.
const OPENAI_BOOL_PATHS: [&str; 16] = [
    "extra_body.google.thinking_config.include_thoughts",
    "extra_body.google.thinking_config.includeThoughts",
    "extra_body.google.thinkingConfig.include_thoughts",
    "extra_body.google.thinkingConfig.includeThoughts",
    "extra_body.extra_body.google.thinking_config.include_thoughts",
    "extra_body.extra_body.google.thinking_config.includeThoughts",
    "google.thinking_config.include_thoughts",
    "google.thinking_config.includeThoughts",
    "thinking.includeThoughts",
    "thinking.include_thoughts",
    "reasoning.includeThoughts",
    "reasoning.include_thoughts",
    "generationConfig.thinkingConfig.includeThoughts",
    "generationConfig.thinkingConfig.include_thoughts",
    "generation_config.thinking_config.include_thoughts",
    "generation_config.thinking_config.includeThoughts",
];

/// Whether a Chat Completions request explicitly asks to show reasoning
/// summaries (`Some(true)`) or to hide them (`Some(false)`).
///
/// `reasoning_effort` sets how hard the model thinks, not whether the client
/// sees it, so it doesn't count here.
pub(crate) fn openai_chat_explicit_summary(request: &Value) -> Option<bool> {
    let flag = |key: &str| path(request, key).and_then(Value::as_bool);
    OPENAI_BOOL_PATHS
        .into_iter()
        .find_map(flag)
        .or_else(|| responses_summary(path(request, "reasoning.summary")))
        .or_else(|| responses_summary(path(request, "reasoning.generate_summary")))
        // OpenRouter's "reason but hide" switch, its older inverse alias, and
        // its switch for reasoning with nothing hidden.
        .or_else(|| flag("reasoning.exclude").map(|exclude| !exclude))
        .or_else(|| flag("include_reasoning"))
        .or_else(|| flag("reasoning.enabled"))
}

/// A Responses-style summary setting: `auto`, `concise` or `detailed` shows
/// summaries; `none` or `null` hides them.
fn responses_summary(value: Option<&Value>) -> Option<bool> {
    match value? {
        Value::Null => Some(false),
        Value::String(summary) => match go::to_lower(summary.trim()).as_str() {
            "auto" | "concise" | "detailed" => Some(true),
            "none" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Sets `thinking.display` on a Claude request body to show or hide summaries.
///
/// Claude only takes `display` alongside active thinking, so hiding them
/// changes nothing when thinking is off. Showing them turns thinking on if the
/// request doesn't say, in whichever form `model` supports.
pub(crate) fn apply_to_claude(body: &mut Value, show: bool, model: &str, models: &ModelCatalog) {
    if show && path(body, "thinking.type").is_none() {
        enable_claude_thinking(body, model, models);
    }
    if !claude_thinking_accepts_display(body) {
        return;
    }
    let display = if show { "summarized" } else { "omitted" };
    if let Some(Value::Object(thinking)) = body.get_mut("thinking") {
        thinking.insert("display".into(), display.into());
    }
}

/// Turns on thinking so summaries can come back: adaptive for a model with
/// effort levels, otherwise the model's smallest budget, if `max_tokens`
/// leaves room for it.
fn enable_claude_thinking(body: &mut Value, model: &str, models: &ModelCatalog) {
    let mut base = base_model_name(model);
    let body_model = str_of(body.get("model")).into_owned();
    if base.is_empty() {
        base = base_model_name(&body_model);
    }
    let Some(support) = models.thinking(base) else {
        return;
    };
    let adaptive = !support.levels.is_empty();
    let budget = support.min;
    if !adaptive
        && (budget <= 0
            || body
                .get("max_tokens")
                .is_some_and(|max| int_of(max) <= budget))
    {
        return;
    }
    let Some(Value::Object(thinking)) = body.as_object_mut().map(|body| {
        body.entry("thinking")
            .or_insert_with(|| Value::Object(Map::new()))
    }) else {
        return;
    };
    if adaptive {
        thinking.insert("type".into(), "adaptive".into());
        thinking.shift_remove("budget_tokens");
    } else {
        thinking.insert("type".into(), "enabled".into());
        thinking.insert("budget_tokens".into(), budget.into());
    }
}

/// Reports whether the request's thinking is on, so it can take `display`.
/// A budget of -1 means the model decides, so it counts as on, and so does
/// a missing budget, which is filled in later.
fn claude_thinking_accepts_display(body: &Value) -> bool {
    match go::to_lower(str_of(path(body, "thinking.type")).trim()).as_str() {
        "adaptive" => true,
        "enabled" => match path(body, "thinking.budget_tokens") {
            Some(budget @ Value::Number(_)) => {
                let budget = int_of(budget);
                budget == -1 || budget > 0
            }
            _ => true,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn summary(request: Value) -> Option<bool> {
        openai_chat_explicit_summary(&request)
    }

    #[test]
    fn explicit_chat_fields_decide_in_order() {
        assert_eq!(summary(json!({"reasoning_effort": "high"})), None);
        assert_eq!(
            summary(json!({
                "extra_body": {"google": {"thinking_config": {"include_thoughts": false}}},
                "reasoning": {"summary": "auto"}
            })),
            Some(false)
        );
        assert_eq!(
            summary(json!({"reasoning": {"summary": " Detailed "}})),
            Some(true)
        );
        assert_eq!(
            summary(json!({"reasoning": {"summary": null}})),
            Some(false)
        );
        assert_eq!(
            summary(json!({"reasoning": {"summary": "bogus", "generate_summary": "none"}})),
            Some(false)
        );
        assert_eq!(
            summary(json!({"reasoning": {"summary": 1, "exclude": true}})),
            Some(false)
        );
        assert_eq!(summary(json!({"include_reasoning": true})), Some(true));
        assert_eq!(
            summary(json!({"reasoning": {"enabled": false}})),
            Some(false)
        );
        assert_eq!(summary(json!({"include_reasoning": "true"})), None);
    }

    fn apply(mut body: Value, show: bool, model: &str) -> Value {
        apply_to_claude(&mut body, show, model, ModelCatalog::embedded());
        body
    }

    #[test]
    fn showing_summaries_turns_thinking_on_by_model() {
        assert_eq!(
            apply(json!({"max_tokens": 32000}), true, "claude-opus-4-6"),
            json!({"max_tokens": 32000, "thinking": {"type": "adaptive", "display": "summarized"}})
        );
        assert_eq!(
            apply(
                json!({"max_tokens": 32000}),
                true,
                "claude-sonnet-4-5-20250929(high)"
            ),
            json!({
                "max_tokens": 32000,
                "thinking": {"type": "enabled", "budget_tokens": 1024, "display": "summarized"}
            })
        );
        // No room for the smallest budget, or an unknown model: left alone.
        assert_eq!(
            apply(
                json!({"max_tokens": 1024}),
                true,
                "claude-sonnet-4-5-20250929"
            ),
            json!({"max_tokens": 1024})
        );
        assert_eq!(apply(json!({}), true, "gpt-x"), json!({}));
    }

    #[test]
    fn hiding_summaries_needs_active_thinking() {
        assert_eq!(
            apply(
                json!({"thinking": {"type": "disabled"}}),
                false,
                "claude-opus-4-6"
            ),
            json!({"thinking": {"type": "disabled"}})
        );
        assert_eq!(apply(json!({}), false, "claude-opus-4-6"), json!({}));
        assert_eq!(
            apply(json!({"thinking": {"type": "enabled"}}), false, "x"),
            json!({"thinking": {"type": "enabled", "display": "omitted"}})
        );
        assert_eq!(
            apply(
                json!({"thinking": {"type": "enabled", "budget_tokens": 0}}),
                false,
                "x"
            ),
            json!({"thinking": {"type": "enabled", "budget_tokens": 0}})
        );
        assert_eq!(
            apply(
                json!({"thinking": {"type": "Enabled", "budget_tokens": -1}}),
                true,
                "x"
            ),
            json!({"thinking": {"type": "Enabled", "budget_tokens": -1, "display": "summarized"}})
        );
    }
}
