// Ported from CLIProxyAPI internal/thinking/provider/xai/apply.go (Applier)
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Thinking settings on a request going to xAI: a level in
//! `reasoning.effort`, written as for Codex (upstream's xAI applier embeds
//! Codex's; see [`crate::codex::thinking`]).
//!
//! A model is looked up as provider `xai` registered it, in the model
//! registry the executor was given, else in the built-in catalog.
//!
//! Deviations from upstream:
//! - The model the credential manager resolved for an API key
//!   (`ResolvedModelInfo`) isn't ported; see the crate's `thinking` module.

use open_ferry_core::exec::ExecError;
use open_ferry_core::models::{ModelCatalog, ThinkingSupport};
use serde_json::Value;

use crate::codex::thinking::{compatible_effort, known_effort, lookup};
use crate::json::{self, Body};
use crate::thinking::{self as shared, Config, Model, Target};

/// Where a Responses request keeps its effort.
const EFFORT: &str = "reasoning.effort";

/// The xAI target.
struct Xai;

impl Target for Xai {
    const NAME: &'static str = "xai";

    /// As for Codex, only the effort is removed (`stripResponsesEffort`).
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

/// `ApplyRequestThinking` for a request translated from `from` into the
/// xAI `body`: applies the thinking setting that `model`'s suffix or the
/// request asks for. `payload` and `original_request` are the client's
/// request as the executor got it and as the client first sent it.
///
/// A setting the model can't take is a 400 error.
pub(crate) fn apply_request(
    body: &mut Value,
    model: &str,
    from: &str,
    payload: &Body,
    original_request: &Body,
    models: Option<&dyn ModelCatalog>,
) -> Result<(), ExecError> {
    shared::apply_request::<Xai>(body, model, from, payload, original_request, |id| {
        lookup(models, id, Xai::NAME)
    })
}
