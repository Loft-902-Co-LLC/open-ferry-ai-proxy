// Ported from CLIProxyAPI internal/client/codex/optimize-multi-agent-v2/
// optimize_multi_agent_v2.go (codexSpawnAgentModelsAndMarkdownForRequest,
// codexSpawnAgentModelsFromTemplates, codexSpawnAgentModelFromMetadata,
// applyCodexSpawnAgentThinking, codexReasoningMetadata,
// normalizeCodexReasoningEffort, codexServiceTierIDs, mapString, mapInt)
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The models a Codex client's `spawn_agent` tool may pick, for step 2 of
//! multi-agent v2 (`client.codex.optimize-multi-agent-v2`), which writes
//! them into the tool's description with
//! [`open_ferry_translate::codex_client::multi_agent_v2::prepare_tools`].
//!
//! Each available model is described by its entry in the Codex client
//! catalog ([`crate::registry::codex_client`]) when it has one. Otherwise
//! the default entry gives it a place, and it takes its own description and
//! reasoning levels, as registered or else as the static catalog has them,
//! with no service tiers. Models with an entry come first, by priority, then
//! the rest by display name.
//!
//! Deviations from upstream:
//! - The Home model fetch isn't ported, so the models are always the
//!   proxy's own.
//! - Upstream keeps the list until the catalog or the model registry
//!   changes. It isn't kept here: it is made again for each request with a
//!   `spawn_agent` tool, which gives the same list.
//! - The catalog is the built-in one, as it can't be replaced at run time
//!   here. When it doesn't load the list is empty, as upstream's is when its
//!   catalog has no default entry.
//! - Models come as [`ModelInfo`]s, read as upstream reads the maps of its
//!   OpenAI model list: the ID, description and display name, trimmed.

#[cfg(test)]
mod tests;

use std::collections::HashSet;

use open_ferry_translate::codex_client::multi_agent_v2::{
    SpawnAgentModel, format_spawn_agent_models,
};
use open_ferry_translate::go;
use serde_json::{Map, Value};

use crate::models::{ModelCatalog, ModelInfo, ThinkingSupport};
use crate::registry::StaticCatalog;
use crate::registry::codex_client::CodexClientCatalog;

/// The models `spawn_agent` may pick, from `catalog`'s available models
/// (`codexSpawnAgentModelsForRequest` without Home).
pub fn spawn_agent_models(catalog: &dyn ModelCatalog) -> Vec<SpawnAgentModel> {
    let Some(templates) = CodexClientCatalog::current() else {
        return Vec::new();
    };
    let statics = StaticCatalog::current();
    let lookup = |id: &str| catalog.model_info(id, "").or_else(|| statics.lookup(id));
    models_from_templates(
        &catalog.available_models(),
        |slug| templates.template(slug),
        templates.default_template(),
        lookup,
    )
}

/// [`spawn_agent_models`] as the Markdown list `spawn_agent`'s description
/// takes, empty when there are none (`formatCodexSpawnAgentModelsForRequest`
/// without Home).
pub fn spawn_agent_model_list(catalog: &dyn ModelCatalog) -> String {
    format_spawn_agent_models(&spawn_agent_models(catalog))
}

/// `codexSpawnAgentModelsFromTemplates`: one entry for each of `available`
/// with an ID, the first of each ID kept, made from its `template` or else
/// from `default_template` and what `lookup` finds.
fn models_from_templates<'t>(
    available: &[ModelInfo],
    template: impl Fn(&str) -> Option<&'t Map<String, Value>>,
    default_template: &Map<String, Value>,
    lookup: impl Fn(&str) -> Option<ModelInfo>,
) -> Vec<SpawnAgentModel> {
    let mut seen = HashSet::with_capacity(available.len());
    let mut template_models = Vec::with_capacity(available.len());
    let mut synthesized_models = Vec::with_capacity(available.len());
    for model in available {
        let id = model.id.trim();
        if id.is_empty() || !seen.insert(id) {
            continue;
        }
        if let Some(template) = template(id) {
            template_models.push(model_from_metadata(id, template));
            continue;
        }
        let mut profile = model_from_metadata(id, default_template);
        profile.description = model.description.trim().to_owned();
        profile.display_name = match model.display_name.trim() {
            "" => id.to_owned(),
            name => name.to_owned(),
        };
        if let Some(info) = lookup(id) {
            let description = info.description.trim();
            if !description.is_empty() {
                profile.description = description.to_owned();
            }
            if let Some(thinking) = &info.thinking {
                apply_thinking(&mut profile, thinking);
            }
        }
        if profile.description.is_empty() {
            profile.description = id.to_owned();
        }
        profile.service_tiers = Vec::new();
        synthesized_models.push(profile);
    }
    template_models.sort_by(|a, b| a.priority.cmp(&b.priority).then_with(|| a.id.cmp(&b.id)));
    // The display names are lowered once each, not at every comparison.
    let mut synthesized: Vec<(String, SpawnAgentModel)> = synthesized_models
        .into_iter()
        .map(|model| (go::to_lower(&model.display_name), model))
        .collect();
    synthesized
        .sort_by(|(a_name, a), (b_name, b)| a_name.cmp(b_name).then_with(|| a.id.cmp(&b.id)));
    template_models.extend(synthesized.into_iter().map(|(_, model)| model));
    template_models
}

/// A model's entry from a catalog entry (`codexSpawnAgentModelFromMetadata`).
fn model_from_metadata(id: &str, metadata: &Map<String, Value>) -> SpawnAgentModel {
    let (reasoning_efforts, default_reasoning_effort) = reasoning_metadata(metadata);
    SpawnAgentModel {
        id: id.to_owned(),
        description: map_string(metadata, "description").to_owned(),
        reasoning_efforts,
        default_reasoning_effort,
        service_tiers: service_tier_ids(metadata),
        priority: map_int(metadata, "priority"),
        display_name: map_string(metadata, "display_name").to_owned(),
    }
}

/// Gives `profile` the reasoning levels of a registered model, the default
/// being `medium` if listed, else the first besides `none`, else the first
/// (`applyCodexSpawnAgentThinking`).
fn apply_thinking(profile: &mut SpawnAgentModel, thinking: &ThinkingSupport) {
    let mut efforts = Vec::with_capacity(thinking.levels.len());
    let mut default_effort = String::new();
    let mut first_effort = String::new();
    for raw in &thinking.levels {
        let effort = normalize_reasoning_effort(raw);
        if effort.is_empty() {
            continue;
        }
        if first_effort.is_empty() {
            first_effort = effort.to_owned();
        }
        if (default_effort.is_empty() && effort != "none") || effort == "medium" {
            default_effort = effort.to_owned();
        }
        efforts.push(effort.to_owned());
    }
    if efforts.is_empty() {
        return;
    }
    if default_effort.is_empty() {
        default_effort = first_effort;
    }
    profile.reasoning_efforts = efforts;
    profile.default_reasoning_effort = default_effort;
}

/// A catalog entry's reasoning levels and default, the first level when
/// the default isn't one of them (`codexReasoningMetadata`).
fn reasoning_metadata(metadata: &Map<String, Value>) -> (Vec<String>, String) {
    let efforts: Vec<String> = array(metadata, "supported_reasoning_levels")
        .map(|level| normalize_reasoning_effort(object_string(level, "effort")))
        .filter(|effort| !effort.is_empty())
        .map(str::to_owned)
        .collect();
    let Some(first) = efforts.first() else {
        return (Vec::new(), String::new());
    };
    let default_effort =
        normalize_reasoning_effort(map_string(metadata, "default_reasoning_level"));
    let default_effort = if efforts.iter().any(|effort| effort == default_effort) {
        default_effort.to_owned()
    } else {
        first.clone()
    };
    (efforts, default_effort)
}

/// `effort` if it is a reasoning effort Codex knows, in lower case, else
/// empty (`normalizeCodexReasoningEffort`).
fn normalize_reasoning_effort(effort: &str) -> &'static str {
    match go::to_lower(effort.trim()).as_str() {
        "none" => "none",
        "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "xhigh" => "xhigh",
        "max" => "max",
        "ultra" => "ultra",
        _ => "",
    }
}

/// The IDs of a catalog entry's service tiers, each once
/// (`codexServiceTierIDs`).
fn service_tier_ids(metadata: &Map<String, Value>) -> Vec<String> {
    let mut seen = HashSet::new();
    array(metadata, "service_tiers")
        .map(|tier| object_string(tier, "id"))
        .filter(|id| !id.is_empty() && seen.insert(*id))
        .map(str::to_owned)
        .collect()
}

/// The items of `values[key]` when it is an array.
fn array<'v>(values: &'v Map<String, Value>, key: &str) -> impl Iterator<Item = &'v Value> {
    values
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

/// `value[key]`, trimmed, when `value` is an object and that is a string.
fn object_string<'v>(value: &'v Value, key: &str) -> &'v str {
    value
        .as_object()
        .map_or("", |values| map_string(values, key))
}

/// `values[key]`, trimmed, when it is a string (`mapString`).
fn map_string<'v>(values: &'v Map<String, Value>, key: &str) -> &'v str {
    values.get(key).and_then(Value::as_str).unwrap_or("").trim()
}

/// `values[key]` as a whole number, cut toward zero, when it is a number
/// (`mapInt`, where JSON numbers are read as `float64`).
fn map_int(values: &Map<String, Value>, key: &str) -> i64 {
    values
        .get(key)
        .and_then(Value::as_f64)
        .map_or(0, |number| number as i64)
}
