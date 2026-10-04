// Ported from CLIProxyAPI sdk/api/handlers/openai/codex_client_models.go
// (codexClientModelsResponse), sdk/api/handlers/apply_patch_capability.go
// (SupportsApplyPatchModel) and sdk/cliproxy/auth/apply_patch_capability.go
// (SupportsApplyPatchForProviders) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The model list Codex clients fetch, `GET /v1/models?client_version=…`,
//! built by [`open_ferry_core::codex_models`] from the available models and
//! the `client.codex` settings.
//!
//! With `client.codex.enable-apply-patch` on, a model gets
//! `"apply_patch_tool_type": "freeform"` when it routes as a call to it
//! would and every provider serving it takes Codex's freeform `apply_patch`
//! tool.
//!
//! Deviations from upstream:
//! - Whether a provider takes the tool goes by its name: `codex`, `claude`,
//!   `gemini`, `gemini-interactions`, `vertex`, `meta` and the
//!   OpenAI-compatible providers (`openai-compatibility` and
//!   `openai-compatible-<name>`) do, the
//!   providers whose executors say so upstream and are ported. Upstream asks
//!   the provider's executor, which would take a new executor method, and
//!   says no when none is registered; here those executors are registered at
//!   start, or with the first credential of an OpenAI-compatible provider.
//! - No web search capability is given, as `cpa_capabilities` isn't ported.
//! - The models come sorted by ID. Upstream's order varies, and decides the
//!   order of models with the same priority.

#[cfg(test)]
mod tests;

use open_ferry_core::auth::compat::OPENAI_COMPATIBILITY;
use open_ferry_core::auth::synthesizer::vertex::VERTEX;
use open_ferry_core::codex_models::{
    ApplyPatchCapability, ProvidersForModel, build_response, marshal_compact,
};
use open_ferry_core::models::ModelCatalog;
use open_ferry_translate::go;

use crate::entry_protocol::GEMINI_INTERACTIONS;
use crate::routing;
use crate::state::AppState;

/// The provider of Gemini API keys, as the Gemini executor names it.
const GEMINI: &str = "gemini";

/// The prefix of a named OpenAI-compatible provider's key.
const OPENAI_COMPATIBLE_PREFIX: &str = "openai-compatible-";

/// The Codex client model list for a client at `client_version`, written as
/// upstream writes it (`codexClientModelsResponse`, then `MarshalCompact`).
pub(super) fn response(state: &AppState, client_version: &str) -> String {
    let settings = state.settings();
    let codex = &settings.config.codex_client;
    let catalog = state.catalog();
    let providers_for_model = |model: &str| catalog.model_providers(model);
    let apply_patch = |model: &str| supports_apply_patch(catalog, model);
    let response = build_response(
        catalog,
        &catalog.available_models(),
        Some(&providers_for_model as ProvidersForModel<'_>),
        codex
            .enable_apply_patch
            .then_some(&apply_patch as ApplyPatchCapability<'_>),
        codex.optimize_multi_agent_v2,
        client_version,
    );
    marshal_compact(&response)
}

/// Whether `model` routes as a call to it would, and every provider it
/// routes to takes the freeform `apply_patch` tool (upstream's
/// `SupportsApplyPatchModel`).
fn supports_apply_patch(catalog: &dyn ModelCatalog, model: &str) -> bool {
    routing::route(catalog, model)
        .is_ok_and(|route| supports_apply_patch_for_providers(&route.providers))
}

/// Whether there are providers and each takes the tool (upstream's
/// `SupportsApplyPatchForProviders`).
fn supports_apply_patch_for_providers(providers: &[String]) -> bool {
    !providers.is_empty()
        && providers
            .iter()
            .all(|provider| provider_supports_apply_patch(provider))
}

/// Whether `provider` takes the tool, as upstream's executors for it say
/// (`SupportsApplyPatch`). Names match as upstream looks up executors:
/// trimmed, and in any case.
fn provider_supports_apply_patch(provider: &str) -> bool {
    let provider = go::to_lower(provider.trim());
    match provider.as_str() {
        "codex" | "claude" | "xai" | GEMINI | GEMINI_INTERACTIONS | VERTEX | "meta"
        | OPENAI_COMPATIBILITY => true,
        name => name
            .strip_prefix(OPENAI_COMPATIBLE_PREFIX)
            .is_some_and(|rest| !rest.is_empty()),
    }
}
