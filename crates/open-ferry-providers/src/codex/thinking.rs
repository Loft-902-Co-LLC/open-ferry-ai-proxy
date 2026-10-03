// Ported from CLIProxyAPI internal/thinking/provider/codex/apply.go,
// internal/thinking/apply.go (ApplyThinkingWithModelInfo) and
// internal/registry/model_registry.go (LookupModelInfo) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Thinking settings on a request going to Codex, or to an OpenAI Responses
//! upstream: a level in `reasoning.effort`. The crate's `thinking` module
//! reads and checks the setting, and handles the `configuration_update`
//! input items that change it mid-conversation.
//!
//! A level goes out as it is. Turning thinking off goes out as `none` if
//! the model can turn it off, else as its lowest level. A model the catalog
//! doesn't know, or one the user defined, gets `none`, `auto`, or a budget
//! as the level nearest to it. A model that doesn't think has
//! `reasoning.effort` removed, and `reasoning` with it if nothing else is
//! left in it.
//!
//! A model is looked up as the executor's provider registered it, in the
//! model registry the executor was given, else in the built-in catalog
//! (upstream's `LookupModelInfo`).
//!
//! Deviations from upstream:
//! - The model the credential manager resolved for an API key
//!   (`ResolvedModelInfo`) isn't ported; see the crate's `thinking` module.
//!   [`apply_with_model_info`] is there for the parity harness.

use open_ferry_core::exec::ExecError;
use open_ferry_core::models::{ModelCatalog, ModelInfo, ThinkingSupport};
use open_ferry_core::registry::StaticCatalog;
use open_ferry_translate::thinking::budget_to_level;
use serde_json::Value;

use crate::json::{self, Body};
use crate::thinking::{self as shared, Config, Mode, Model, Route, Target};

/// Where a Responses request keeps its effort.
const EFFORT: &str = "reasoning.effort";

/// The Codex target, for OpenAI Responses too.
struct Codex;

impl Target for Codex {
    const NAME: &'static str = "codex";

    /// For a Responses target upstream removes only the effort
    /// (`stripResponsesEffort`), not all of `reasoning` as its
    /// `StripThinkingConfig` would.
    fn strip(body: &mut Value) {
        shared::strip_responses_effort(body);
    }

    fn apply_known(body: &mut Value, config: Config, _model: &Model, support: &ThinkingSupport) {
        if let Some(effort) = known_effort(&config, support) {
            json::set(body, EFFORT, Value::String(effort));
        }
    }

    fn apply_compatible(body: &mut Value, config: &Config) {
        if let Some(effort) = compatible_effort(config) {
            json::set(body, EFFORT, Value::String(effort));
        }
    }
}

/// The effort for a model the catalog knows, given a validated setting:
/// a level as it is; off as `none` where the model takes it, else the
/// setting's level or the model's lowest. Budgets and auto, which a
/// validated setting for a level model doesn't have, change nothing.
pub(crate) fn known_effort(config: &Config, support: &ThinkingSupport) -> Option<String> {
    match config.mode {
        Mode::Level => Some(config.level.clone()),
        Mode::None => {
            if config.budget == 0
                && (support.zero_allowed || shared::level_supported("none", &support.levels))
            {
                return Some("none".to_owned());
            }
            if !config.level.is_empty() {
                return Some(config.level.clone());
            }
            support
                .levels
                .first()
                .filter(|level| !level.is_empty())
                .cloned()
        }
        Mode::Budget | Mode::Auto => None,
    }
}

/// The effort for a model the catalog doesn't know or the user defined
/// (`applyCompatibleCodex`): a level as it is, off as `none` (or its
/// level), auto as `auto`, and a budget as the level nearest to it.
pub(crate) fn compatible_effort(config: &Config) -> Option<String> {
    match config.mode {
        Mode::Level if config.level.is_empty() => None,
        Mode::Level => Some(config.level.clone()),
        Mode::None if config.level.is_empty() => Some("none".to_owned()),
        Mode::None => Some(config.level.clone()),
        Mode::Auto => Some("auto".to_owned()),
        Mode::Budget => budget_to_level(config.budget).map(str::to_owned),
    }
}

/// `ApplyRequestThinking` for a request translated from `from` into the
/// Codex or OpenAI Responses (`to`) `body`: applies the thinking setting
/// that `model`'s suffix or the request asks for. `payload` and
/// `original_request` are the client's request as the executor got it and
/// as the client first sent it. Models are looked up as `provider`
/// registered them in `models`, else in the built-in catalog.
///
/// A setting the model can't take is a 400 error.
pub(crate) fn apply_request(
    body: &mut Value,
    route: Route<'_>,
    payload: &Body,
    original_request: &Body,
    models: Option<&dyn ModelCatalog>,
) -> Result<(), ExecError> {
    shared::apply_request_to::<Codex>(body, route, payload, original_request, |id| {
        lookup(models, id, route.provider)
    })
}

/// `LookupModelInfo`: `model` as `provider` registered it in `models`, or
/// as last registered at all, else as the built-in catalog has it.
pub(crate) fn lookup(
    models: Option<&dyn ModelCatalog>,
    model: &str,
    provider: &str,
) -> Option<Model> {
    let model = model.trim();
    if model.is_empty() {
        return None;
    }
    let provider = json::lower_trim(provider);
    models
        .and_then(|models| models.model_info(model, &provider))
        .or_else(|| StaticCatalog::embedded().lookup(model))
        .map(thinking_model)
}

/// What the thinking code needs of `info`.
pub(crate) fn thinking_model(info: ModelInfo) -> Model {
    Model {
        id: info.id,
        model_type: info.model_type,
        thinking: info.thinking,
        user_defined: info.user_defined,
        max_completion_tokens: i64::try_from(info.max_completion_tokens).unwrap_or(i64::MAX),
        support_configuration_update: info.support_configuration_update,
    }
}

/// Upstream's `ApplyThinkingWithModelInfo` for a Codex or OpenAI Responses
/// target (`to`), with `info` as the model bound to the request. `source`
/// is the client's request in format `from`, and `provider` the executor's.
/// On an error, `body` keeps what was already changed and the error's
/// message is returned.
///
/// Only the parity harness calls this.
#[doc(hidden)]
pub fn apply_with_model_info(
    body: &mut Value,
    source: &[u8],
    model: &str,
    from: &str,
    to: &str,
    provider: &str,
    info: Option<ModelInfo>,
) -> Result<(), String> {
    let route = Route {
        model,
        from,
        to,
        provider,
    };
    shared::apply_with_model::<Codex>(body, &Body::parse(source), route, info.map(thinking_model))
        .map_err(|error| error.message)
}

#[cfg(test)]
pub(crate) mod tests;
