// Ported from CLIProxyAPI internal/thinking/provider/interactions/apply.go
// (Apply, applyInteractionsBudget, applyInteractionsLevel,
// applyInteractionsNone, stripInteractionsThinkingFields,
// setInteractionsThinkingSummaries, originalInteractionsThinkingSummaries,
// originalInteractionsIncludeThoughts, normalizeInteractionsLevel),
// internal/thinking/strip.go (the Interactions case of StripThinkingConfig)
// and internal/runtime/executor/gemini_executor.go
// (applyGeminiInteractionsThinking) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Thinking settings on a request going to Gemini's Interactions API, as
//! `generation_config.thinking_level` and
//! `generation_config.thinking_summaries`. [`crate::thinking`] reads and
//! checks the setting.
//!
//! The API takes levels only: a budget becomes the level it stands for, and
//! a level is matched to the model's own, or the last of them when it has
//! none such. Every other spelling of the setting, in snake or camel case,
//! under `generation_config` or `generationConfig`, is removed. Whether
//! summaries are shown is kept from the request as it came: its
//! `thinking_summaries` when that is `auto` or `none`, else its
//! `include_thoughts`.
//!
//! Models are looked up as the `gemini` provider registered them, as
//! upstream's executor asks for them.
//!
//! Deviations from upstream:
//! - A model the user defined gets the setting with no levels to match it
//!   to, as for a model the catalog doesn't know; upstream matches it to
//!   the levels the user gave the model.

use open_ferry_core::exec::ExecError;
use open_ferry_core::models::{ModelCatalog, ThinkingSupport};
use open_ferry_translate::go::{equal_fold, to_lower};
use open_ferry_translate::thinking::budget_to_level;
use serde_json::{Map, Value};

use crate::gemini::thinking::lookup;
use crate::json::{self, Body};
use crate::thinking::{self as shared, Config, Mode, Model, Route, Target};

/// The fields `StripThinkingConfig` removes for Interactions.
const STRIPPED: [&str; 8] = [
    "generation_config.thinking_level",
    "generation_config.thinkingLevel",
    "generation_config.thinking_budget",
    "generation_config.thinkingBudget",
    "generation_config.thinking_summaries",
    "generation_config.thinkingSummaries",
    "generation_config.thinking_config",
    "generation_config.thinkingConfig",
];

/// What `stripInteractionsThinkingFields` removes before a setting is
/// written: [`STRIPPED`] and Gemini's camel-case config.
const REPLACED: [&str; 7] = [
    "generationConfig.thinkingLevel",
    "generationConfig.thinking_level",
    "generationConfig.thinkingBudget",
    "generationConfig.thinking_budget",
    "generationConfig.thinkingSummaries",
    "generationConfig.thinking_summaries",
    "generationConfig.thinkingConfig",
];

/// Where the level goes.
const THINKING_LEVEL: &str = "generation_config.thinking_level";
/// Where the summary setting goes.
const THINKING_SUMMARIES: &str = "generation_config.thinking_summaries";

/// The Interactions target.
pub(super) struct Interactions;

impl Target for Interactions {
    const NAME: &'static str = "interactions";

    /// `StripThinkingConfig` for Interactions.
    fn strip(body: &mut Value) {
        for path in STRIPPED {
            json::delete(body, path);
        }
    }

    fn apply_known(body: &mut Value, config: Config, _model: &Model, support: &ThinkingSupport) {
        apply(body, &config, &support.levels);
    }

    fn apply_compatible(body: &mut Value, config: &Config) {
        apply(body, config, &[]);
    }
}

/// `applyGeminiInteractionsThinking`: applies the thinking setting that
/// `model`'s suffix or the request asks for to the Interactions `body`,
/// translated from `from` (Interactions when empty). `payload` and
/// `original_request` are the client's request as the executor got it and
/// as the client first sent it. Models are looked up as the `gemini`
/// provider registered them in `models`, else in the built-in catalog.
///
/// A setting the model can't take is a 400 error.
pub(super) fn apply_request(
    body: &mut Value,
    model: &str,
    from: &str,
    payload: &Body,
    original_request: &Body,
    models: Option<&dyn ModelCatalog>,
) -> Result<(), ExecError> {
    let from = if from.trim().is_empty() {
        Interactions::NAME
    } else {
        from
    };
    let route = Route {
        model,
        from,
        to: Interactions::NAME,
        provider: "gemini",
    };
    shared::apply_request_to::<Interactions>(body, route, payload, original_request, |id| {
        lookup(models, id, "gemini").map(|info| Model {
            id: info.id,
            model_type: info.model_type,
            thinking: info.thinking,
            user_defined: info.user_defined,
            max_completion_tokens: i64::try_from(info.max_completion_tokens).unwrap_or(i64::MAX),
            support_configuration_update: info.support_configuration_update,
        })
    })
}

/// The applier's `Apply`: `config` written on `body`, with the model's
/// `levels` to match a level to.
fn apply(body: &mut Value, config: &Config, levels: &[String]) {
    if !body.is_object() {
        *body = Value::Object(Map::new());
    }
    let original = body.clone();
    for path in STRIPPED.iter().chain(&REPLACED) {
        json::delete(body, path);
    }
    match config.mode {
        Mode::Level => apply_level(body, &original, &config.level, levels),
        Mode::Budget => apply_budget(body, &original, config.budget, levels),
        Mode::Auto => set_summaries(body, &original),
        Mode::None => {
            if !config.level.is_empty() {
                apply_level(body, &original, &config.level, levels);
            } else if config.budget > 0 {
                apply_budget(body, &original, config.budget, levels);
            }
            // With thinking turned off, showing summaries alone could make
            // a model that thinks by default think and summarize anyway.
        }
    }
}

/// `applyInteractionsBudget`: the level a budget stands for. Off and auto
/// have no level the API takes, so only the summary setting is kept.
fn apply_budget(body: &mut Value, original: &Value, budget: i64, levels: &[String]) {
    match budget_to_level(budget) {
        Some(level) if level != "none" && level != "auto" => {
            apply_level(body, original, level, levels);
        }
        _ => set_summaries(body, original),
    }
}

/// `applyInteractionsLevel`.
fn apply_level(body: &mut Value, original: &Value, level: &str, levels: &[String]) {
    let level = normalize_level(level, levels);
    if !level.is_empty() {
        json::set(body, THINKING_LEVEL, Value::from(level));
    }
    set_summaries(body, original);
}

/// `normalizeInteractionsLevel`: no level for off or auto; else the
/// model's level of that name, or its last; without levels, `xhigh` and
/// `max` become `high`.
fn normalize_level(level: &str, levels: &[String]) -> String {
    let level = json::lower_trim(level);
    if level.is_empty() || level == "none" || level == "auto" {
        return String::new();
    }
    if let Some(last) = levels.last() {
        let matched = levels
            .iter()
            .find(|candidate| equal_fold(candidate, &level))
            .unwrap_or(last);
        return to_lower(matched);
    }
    match level.as_str() {
        "max" | "xhigh" => "high".to_owned(),
        _ => level,
    }
}

/// `setInteractionsThinkingSummaries`: the summary setting of the request
/// as it came, if it has one.
fn set_summaries(body: &mut Value, original: &Value) {
    let value = original_summaries(original).or_else(|| {
        include_thoughts(original).map(|include| if include { "auto" } else { "none" })
    });
    if let Some(value) = value {
        json::set(body, THINKING_SUMMARIES, Value::from(value));
    }
}

/// `originalInteractionsThinkingSummaries`: the first summary setting that
/// is a string saying `auto` or `none`.
fn original_summaries(body: &Value) -> Option<&'static str> {
    [
        "generation_config.thinking_summaries",
        "generation_config.thinkingSummaries",
    ]
    .iter()
    .find_map(|path| match json::get(body, path) {
        Some(Value::String(value)) => match json::lower_trim(value).as_str() {
            "auto" => Some("auto"),
            "none" => Some("none"),
            _ => None,
        },
        _ => None,
    })
}

/// `originalInteractionsIncludeThoughts`: the first `include_thoughts`
/// that is a boolean.
fn include_thoughts(body: &Value) -> Option<bool> {
    [
        "generation_config.thinking_config.include_thoughts",
        "generation_config.thinking_config.includeThoughts",
        "generation_config.thinkingConfig.include_thoughts",
        "generation_config.thinkingConfig.includeThoughts",
    ]
    .iter()
    .find_map(|path| json::get(body, path)?.as_bool())
}

#[cfg(test)]
mod tests;
