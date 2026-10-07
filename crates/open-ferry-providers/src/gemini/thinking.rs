// Ported from CLIProxyAPI internal/thinking/provider/gemini/apply.go,
// internal/thinking/strip.go (the Gemini case of StripThinkingConfig) and
// internal/registry/model_registry.go (LookupModelInfo) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Thinking settings on a request going to Gemini or Vertex AI, under
//! `generationConfig.thinkingConfig`: a token budget (`thinkingBudget`, as
//! Gemini 2.5 takes it) or a level (`thinkingLevel`, as Gemini 3 takes it).
//! [`crate::thinking`] reads and checks the setting.
//!
//! A level goes out as a level; turning thinking off goes out as a level
//! for a model with levels, else as a budget; anything else goes out as a
//! budget, `-1` meaning auto. `includeThoughts` is kept from the request
//! only when it is a boolean, under its camel-case name, unless thinking is
//! turned off entirely, which removes the whole `thinkingConfig`.
//!
//! A model is looked up as the credential's provider registered it, in the
//! model registry the executor was given, else in the static catalog
//! (upstream's `LookupModelInfo`).
//!
//! Deviations from upstream:
//! - The model the credential manager resolved for an API key
//!   (`ResolvedModelInfo`) isn't ported; see [`crate::thinking`].

use open_ferry_core::exec::ExecError;
use open_ferry_core::models::{ModelCatalog, ModelInfo, ThinkingSupport};
use open_ferry_core::registry::StaticCatalog;
use serde_json::Value;

use crate::json::{self, Body};
use crate::thinking::{self as shared, Config, Mode, Model, Target};

/// Where a Gemini request keeps its thinking setting.
const THINKING_CONFIG: &str = "generationConfig.thinkingConfig";

/// The Gemini target, for Vertex AI too.
struct Gemini;

impl Target for Gemini {
    const NAME: &'static str = "gemini";

    /// `StripThinkingConfig` for Gemini.
    fn strip(body: &mut Value) {
        json::delete(body, THINKING_CONFIG);
    }

    fn apply_known(body: &mut Value, config: Config, _model: &Model, support: &ThinkingSupport) {
        match config.mode {
            Mode::Level => apply_level(body, &config),
            Mode::None if !support.levels.is_empty() => apply_level(body, &config),
            _ => apply_budget(body, &config),
        }
    }

    fn apply_compatible(body: &mut Value, config: &Config) {
        match config.mode {
            Mode::Auto => apply_budget(body, config),
            Mode::Level => apply_level(body, config),
            Mode::None if !config.level.is_empty() => apply_level(body, config),
            _ => apply_budget(body, config),
        }
    }
}

/// `ApplyRequestThinking` for a request translated from `from` into the
/// Gemini `body`: applies the thinking setting that `model`'s suffix or the
/// request asks for. `payload` and `original_request` are the client's
/// request as the executor got it and as the client first sent it. Models
/// are looked up as `provider` (`gemini` or `vertex`) registered them in
/// `models`, else in the static catalog.
///
/// A setting the model can't take is a 400 error.
pub(crate) fn apply_request(
    body: &mut Value,
    model: &str,
    from: &str,
    payload: &Body,
    original_request: &Body,
    models: Option<&dyn ModelCatalog>,
    provider: &str,
) -> Result<(), ExecError> {
    shared::apply_request::<Gemini>(body, model, from, payload, original_request, |id| {
        lookup(models, id, provider).map(|info| Model {
            id: info.id,
            model_type: info.model_type,
            thinking: info.thinking,
            user_defined: info.user_defined,
            max_completion_tokens: i64::try_from(info.max_completion_tokens).unwrap_or(i64::MAX),
            support_configuration_update: info.support_configuration_update,
        })
    })
}

/// `LookupModelInfo`: `model` as `provider` registered it in `models`, or
/// as last registered at all, else as the static catalog in use has it.
pub(crate) fn lookup(
    models: Option<&dyn ModelCatalog>,
    model: &str,
    provider: &str,
) -> Option<ModelInfo> {
    let model = model.trim();
    if model.is_empty() {
        return None;
    }
    let provider = json::lower_trim(provider);
    models
        .and_then(|models| models.model_info(model, &provider))
        .or_else(|| StaticCatalog::current().lookup(model))
}

/// The request's own `includeThoughts`: the first of the camel and snake
/// case fields that is a boolean.
fn include_thoughts(body: &Value) -> Option<bool> {
    ["includeThoughts", "include_thoughts"]
        .iter()
        .find_map(|field| json::get(body, &format!("{THINKING_CONFIG}.{field}"))?.as_bool())
}

fn delete_fields(body: &mut Value, fields: &[&str]) {
    for field in fields {
        json::delete(body, &format!("{THINKING_CONFIG}.{field}"));
    }
}

/// `applyGeminiIncludeThoughts`.
fn restore_include_thoughts(body: &mut Value, include: Option<bool>) {
    if let Some(include) = include {
        json::set(
            body,
            &format!("{THINKING_CONFIG}.includeThoughts"),
            Value::Bool(include),
        );
    }
}

/// `applyLevelFormat`: a level, or thinking turned off.
fn apply_level(body: &mut Value, config: &Config) {
    if !matches!(config.mode, Mode::Level | Mode::None) {
        return;
    }
    let include = include_thoughts(body);
    delete_fields(
        body,
        &[
            "thinkingBudget",
            "thinking_budget",
            "thinking_level",
            "includeThoughts",
            "include_thoughts",
        ],
    );
    if config.mode == Mode::None {
        if config.fully_disabled() {
            // Showing thoughts alone would turn thinking back on for a model
            // that thinks by default.
            json::delete(body, THINKING_CONFIG);
            return;
        }
        if !config.level.is_empty() {
            set_level(body, &config.level);
        }
    } else {
        set_level(body, &config.level);
    }
    restore_include_thoughts(body, include);
}

fn set_level(body: &mut Value, level: &str) {
    json::set(
        body,
        &format!("{THINKING_CONFIG}.thinkingLevel"),
        Value::from(level),
    );
}

/// `applyBudgetFormat`.
fn apply_budget(body: &mut Value, config: &Config) {
    let include = include_thoughts(body);
    delete_fields(
        body,
        &[
            "thinkingLevel",
            "thinking_level",
            "thinking_budget",
            "includeThoughts",
            "include_thoughts",
        ],
    );
    json::set(
        body,
        &format!("{THINKING_CONFIG}.thinkingBudget"),
        Value::from(config.budget),
    );
    restore_include_thoughts(body, include);
}

#[cfg(test)]
mod tests;
