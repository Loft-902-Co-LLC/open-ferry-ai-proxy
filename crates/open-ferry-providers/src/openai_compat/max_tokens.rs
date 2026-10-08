// Ported from CLIProxyAPI
// internal/runtime/executor/helps/openai_compat_max_tokens.go and
// normalizeOpenAICompatibilityModelName in
// helps/openai_compat_tool_results.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! `max_tokens` or `max_completion_tokens`, whichever the model takes.
//!
//! A model configured with `use-max-completion-tokens` gets
//! `max_completion_tokens`; any other gets `max_tokens`. A limit the client
//! set in the other field moves over, `null` included, unless the right one
//! is set too, and the other field is dropped.
//!
//! The model is looked up in the provider's `models` by the upstream model,
//! then by the model the client asked for: by name first, then by alias,
//! regardless of case and without a thinking suffix.
//!
//! Deviations from upstream: none.

use open_ferry_core::config::{OpenAiCompatibility, OpenAiCompatibilityModel};
use serde_json::Value;

use crate::codex::request::base_model;
use crate::json::{delete, eq_fold, set};

/// Whether the model takes `max_completion_tokens`
/// (`ShouldUseMaxCompletionTokensForModel`).
pub(crate) fn should_use_max_completion_tokens(
    compat: Option<&OpenAiCompatibility>,
    upstream_model: &str,
    requested_model: &str,
) -> bool {
    let Some(compat) = compat else {
        return false;
    };
    if let Some(use_max_completion_tokens) =
        model_uses_max_completion_tokens(&compat.models, upstream_model)
    {
        return use_max_completion_tokens;
    }
    model_uses_max_completion_tokens(&compat.models, requested_model).unwrap_or(false)
}

/// The `use-max-completion-tokens` of the model named `model`, if one is
/// (`openAICompatibilityModelUsesMaxCompletionTokens`).
fn model_uses_max_completion_tokens(
    models: &[OpenAiCompatibilityModel],
    model: &str,
) -> Option<bool> {
    let model = normalize_model_name(model);
    if model.is_empty() {
        return None;
    }
    models
        .iter()
        .find(|entry| eq_fold(model, normalize_model_name(&entry.name)))
        .or_else(|| {
            models
                .iter()
                .find(|entry| eq_fold(model, normalize_model_name(&entry.alias)))
        })
        .map(|entry| entry.use_max_completion_tokens)
}

/// A model name without the spaces around it or a thinking suffix
/// (`normalizeOpenAICompatibilityModelName`).
pub(crate) fn normalize_model_name(model: &str) -> &str {
    let model = model.trim();
    if model.is_empty() {
        return "";
    }
    base_model(model).trim()
}

/// Moves the output limit to the field the model takes
/// (`NormalizeOpenAIMaxTokens`): with `use_max_completion_tokens`,
/// `max_tokens` becomes `max_completion_tokens`, else the other way round.
pub(crate) fn normalize_max_tokens(body: &mut Value, use_max_completion_tokens: bool) {
    let (wanted, unwanted) = if use_max_completion_tokens {
        ("max_completion_tokens", "max_tokens")
    } else {
        ("max_tokens", "max_completion_tokens")
    };
    let Some(object) = body.as_object() else {
        return;
    };
    let Some(value) = object.get(unwanted).cloned() else {
        return;
    };
    if !object.contains_key(wanted) {
        set(body, wanted, value);
    }
    delete(body, unwanted);
}

#[cfg(test)]
mod tests {
    // Ports internal/runtime/executor/helps/openai_compat_max_tokens_test.go.
    // Changed: "empty payload returns empty" checks that a body that isn't an
    // object is left as it is, since bodies here are parsed values; field
    // values and order are checked on the whole body.
    use super::*;
    use serde_json::json;

    fn model(name: &str, alias: &str, use_max_completion_tokens: bool) -> OpenAiCompatibilityModel {
        OpenAiCompatibilityModel {
            name: name.into(),
            alias: alias.into(),
            use_max_completion_tokens,
            ..OpenAiCompatibilityModel::default()
        }
    }

    #[test]
    fn should_use_max_completion_tokens_for_model() {
        let compat = OpenAiCompatibility {
            models: vec![
                model("azure/o1-preview", "o1-preview", true),
                model("deepseek/v3", "deepseek-chat", false),
                model("legacy/gpt-3.5", "gpt-3.5", false),
            ],
            ..OpenAiCompatibility::default()
        };
        let cases = [
            (
                "nil compat returns false",
                None,
                "azure/o1-preview",
                "o1-preview",
                false,
            ),
            (
                "match upstreamModel by name with true",
                Some(&compat),
                "azure/o1-preview",
                "other-alias",
                true,
            ),
            (
                "match requestedModel by alias with true",
                Some(&compat),
                "unknown-upstream",
                "o1-preview",
                true,
            ),
            (
                "match with thinking suffix on upstreamModel",
                Some(&compat),
                "azure/o1-preview(high)",
                "unknown-alias",
                true,
            ),
            (
                "match with thinking suffix on requestedModel",
                Some(&compat),
                "unknown",
                "o1-preview(medium)",
                true,
            ),
            (
                "case-insensitive match",
                Some(&compat),
                "AZURE/O1-PREVIEW",
                "O1-PREVIEW",
                true,
            ),
            (
                "explicit false returns false",
                Some(&compat),
                "deepseek/v3",
                "deepseek-chat",
                false,
            ),
            (
                "omitted default returns false",
                Some(&compat),
                "legacy/gpt-3.5",
                "gpt-3.5",
                false,
            ),
            (
                "unmatched model returns false",
                Some(&compat),
                "completely-unknown",
                "unknown-alias",
                false,
            ),
        ];
        for (name, compat, upstream, requested, want) in cases {
            assert_eq!(
                should_use_max_completion_tokens(compat, upstream, requested),
                want,
                "{name}"
            );
        }
    }

    #[test]
    fn upstream_model_match_decides_first() {
        let compat = OpenAiCompatibility {
            models: vec![model("a", "shared", false), model("b", "x", true)],
            ..OpenAiCompatibility::default()
        };
        assert!(!should_use_max_completion_tokens(Some(&compat), "a", "b"));
        assert!(should_use_max_completion_tokens(Some(&compat), " ", "b"));
        assert!(!should_use_max_completion_tokens(
            Some(&compat),
            "shared",
            "b"
        ));
    }

    fn normalized(input: Value, use_max_completion_tokens: bool) -> Value {
        let mut body = input;
        normalize_max_tokens(&mut body, use_max_completion_tokens);
        body
    }

    #[test]
    fn normalize_openai_max_tokens() {
        // useMaxCompletionTokens=true converts max_tokens
        assert_eq!(
            normalized(json!({"model":"o1","max_tokens":1024,"messages":[]}), true).to_string(),
            r#"{"model":"o1","messages":[],"max_completion_tokens":1024}"#
        );
        // useMaxCompletionTokens=true preserves existing max_completion_tokens
        // and removes max_tokens
        assert_eq!(
            normalized(
                json!({"model":"o1","max_tokens":512,"max_completion_tokens":2048}),
                true
            ),
            json!({"model":"o1","max_completion_tokens":2048})
        );
        // useMaxCompletionTokens=true keeps max_completion_tokens when
        // max_tokens is absent
        assert_eq!(
            normalized(json!({"model":"o1","max_completion_tokens":4096}), true),
            json!({"model":"o1","max_completion_tokens":4096})
        );
        // useMaxCompletionTokens=false converts max_completion_tokens to
        // max_tokens
        assert_eq!(
            normalized(
                json!({"model":"deepseek","max_completion_tokens":1024,"messages":[]}),
                false
            )
            .to_string(),
            r#"{"model":"deepseek","messages":[],"max_tokens":1024}"#
        );
        // useMaxCompletionTokens=false preserves existing max_tokens and
        // removes max_completion_tokens
        assert_eq!(
            normalized(
                json!({"model":"deepseek","max_tokens":512,"max_completion_tokens":2048}),
                false
            ),
            json!({"model":"deepseek","max_tokens":512})
        );
        // useMaxCompletionTokens=false keeps max_tokens when
        // max_completion_tokens is absent
        assert_eq!(
            normalized(json!({"model":"deepseek","max_tokens":4096}), false),
            json!({"model":"deepseek","max_tokens":4096})
        );
        // useMaxCompletionTokens=true preserves null value
        assert_eq!(
            normalized(json!({"model":"o1","max_tokens":null}), true),
            json!({"model":"o1","max_completion_tokens":null})
        );
        // useMaxCompletionTokens=false preserves null value
        assert_eq!(
            normalized(
                json!({"model":"deepseek","max_completion_tokens":null}),
                false
            ),
            json!({"model":"deepseek","max_tokens":null})
        );
        // payload without max tokens remains unchanged
        let plain = json!({"model":"gpt-4","messages":[{"role":"user","content":"hi"}]});
        assert_eq!(normalized(plain.clone(), true), plain);
        assert_eq!(normalized(plain.clone(), false), plain);
        // A body that isn't an object is left as it is.
        assert_eq!(normalized(json!([1]), true), json!([1]));
        assert_eq!(normalized(Value::Null, false), Value::Null);
    }
}
