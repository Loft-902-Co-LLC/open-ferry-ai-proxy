// Ported from CLIProxyAPI internal/thinking/provider/openai/apply.go,
// internal/thinking/strip.go (the OpenAI case of StripThinkingConfig),
// internal/thinking/summary.go (applyOpenAIChatSummaryConfig,
// isOpenRouterProvider) and internal/thinking/apply.go
// (ApplyThinkingWithModelInfo) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Thinking settings on a request going to an OpenAI-compatible provider
//! as Chat Completions: a level in `reasoning_effort`, written as the Codex
//! target writes `reasoning.effort` ([`crate::codex::thinking`]). The
//! crate's `thinking` module reads and checks the setting. A model that
//! doesn't think has `reasoning_effort` and `reasoning` removed.
//!
//! Whether summaries are shown goes in OpenRouter's `reasoning.exclude`,
//! for a provider named `openrouter` (or with it as a `-`, `_`, `/`, `.` or
//! `:` separated part of its name) or a request that already has it, and in
//! `include_reasoning` if the request has it.
//!
//! Deviations from upstream:
//! - The model the credential manager resolved for an API key
//!   (`ResolvedModelInfo`) isn't ported; see the crate's `thinking` module.
//!   [`apply_with_model_info`] is there for the parity harness.

use open_ferry_core::exec::ExecError;
use open_ferry_core::models::{ModelCatalog, ModelInfo, ThinkingSupport};
use open_ferry_translate::thinking::summary::Summary;
use serde_json::Value;

use crate::codex::thinking::{compatible_effort, known_effort, lookup, thinking_model};
use crate::json::{self, Body};
use crate::thinking::{self as shared, Config, Model, Route, Target};

/// Where a Chat Completions request keeps its effort.
const EFFORT: &str = "reasoning_effort";

/// The OpenAI Chat Completions target.
struct OpenAi;

impl Target for OpenAi {
    const NAME: &'static str = "openai";

    fn strip(body: &mut Value) {
        json::delete(body, EFFORT);
        json::delete(body, "reasoning");
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

    /// `applyOpenAIChatSummaryConfig`.
    fn apply_summary(body: &mut Value, _model: &str, provider: &str, summary: Summary) {
        let show = match summary {
            Summary::Unspecified => return,
            Summary::Hidden => false,
            Summary::Shown(_) => true,
        };
        if is_openrouter(provider)
            || json::get(body, "reasoning.exclude").is_some_and(Value::is_boolean)
        {
            json::set(body, "reasoning.exclude", Value::Bool(!show));
        }
        if json::get(body, "include_reasoning").is_some_and(Value::is_boolean) {
            json::set(body, "include_reasoning", Value::Bool(show));
        }
    }
}

/// `isOpenRouterProvider`: `openrouter`, alone or as a part of the name.
fn is_openrouter(provider: &str) -> bool {
    json::lower_trim(provider)
        .split(['-', '_', '/', '.', ':'])
        .any(|part| part == "openrouter")
}

/// `ApplyRequestThinking` for a request translated from `from` into the
/// Chat Completions `body`: applies the thinking setting that `model`'s
/// suffix or the request asks for. `payload` and `original_request` are the
/// client's request as the executor got it and as the client first sent
/// it. Models are looked up as `provider` registered them in `models`, else
/// in the built-in catalog.
///
/// A setting the model can't take is a 400 error.
pub(crate) fn apply_request(
    body: &mut Value,
    route: Route<'_>,
    payload: &Body,
    original_request: &Body,
    models: Option<&dyn ModelCatalog>,
) -> Result<(), ExecError> {
    shared::apply_request_to::<OpenAi>(body, route, payload, original_request, |id| {
        lookup(models, id, route.provider)
    })
}

/// Upstream's `ApplyThinkingWithModelInfo` for a Chat Completions target,
/// with `info` as the model bound to the request. `source` is the client's
/// request in format `from`, and `provider` the executor's. On an error,
/// `body` keeps what was already changed and the error's message is
/// returned.
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
    shared::apply_with_model::<OpenAi>(body, &Body::parse(source), route, info.map(thinking_model))
        .map_err(|error| error.message)
}

#[cfg(test)]
mod tests;
