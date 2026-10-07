// Ported from CLIProxyAPI internal/client/codex/models/models.go
// (BuildResponseForClientWithToolCapabilities, MarshalCompact,
// buildCodexClientModelsWithToolCapabilities and what they call) and
// apply_patch.go (applyCodexClientApplyPatchCapability) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The model list Codex clients fetch: `GET /v1/models?client_version=…`.
//!
//! Each available model gets an entry made from a template in the Codex
//! client catalog ([`crate::registry::codex_client`]): its own when the
//! catalog lists it, or the model it is an alias or prefixed name of;
//! otherwise the default template, filled in with the model's own name,
//! description, context window and reasoning levels, and with short
//! instructions so the list stays under Codex's 1 MiB limit. Then:
//! - input modalities and reasoning levels are narrowed to what every
//!   provider serving the model supports, where the config says or the
//!   provider serves the model under another name;
//! - only models Codex alone serves keep the search tool, WebSockets,
//!   service tiers, upgrade and availability notices;
//! - reasoning levels the client is too old for (`max` and `ultra` before
//!   0.144.0) are dropped;
//! - image and video models are hidden;
//! - models without a template of their own come after the catalog's, in
//!   order of display name;
//! - `apply_patch_tool_type` is `freeform` only for a model that takes text
//!   and that the caller says every provider routing it supports, or, when
//!   the caller gives no capability, whose entry's template declares it.
//!
//! [`marshal_compact`] writes the list as upstream's `MarshalCompact` does.
//!
//! Deviations from upstream:
//! - Models come as [`ModelInfo`]s, read as upstream reads the maps of its
//!   OpenAI model list. A model map's `thinking`, `base_instructions` and
//!   `available_in_plans`, which only Home's model maps carry, aren't read:
//!   Home isn't ported.
//! - Devin isn't ported: a Devin model's display name gets no ` (Devin)`.
//! - `cpa_capabilities` isn't ported: it is removed from every entry, and
//!   `client_version=cpa` gets nothing in its place.
//! - Model details are looked up in the given catalog, then the static
//!   catalog, which leaves out upstream's built-in Devin models.
//! - [`marshal_compact`] can't fail. Numbers keep their text; the built-in
//!   catalog's are all whole numbers, which Go writes the same way.

pub mod spawn_agent;

#[cfg(test)]
mod apply_patch_tests;
#[cfg(test)]
mod tests;

use std::cmp::Ordering;
use std::fmt::Write as _;
use std::sync::Arc;

use open_ferry_translate::go;
use serde_json::{Map, Value, json};

use crate::models::{ModelCatalog, ModelInfo, ThinkingSupport};
use crate::registry::StaticCatalog;
use crate::registry::codex_client::CodexClientCatalog;
use crate::registry::registration::OPENAI_IMAGE_MODEL_TYPE;

/// The providers registered for a model (upstream's
/// `ProvidersForModelFunc`).
pub type ProvidersForModel<'a> = &'a dyn Fn(&str) -> Vec<String>;

/// Whether every provider routing a model, by its exact public ID, supports
/// the freeform `apply_patch` tool (upstream's
/// `ApplyPatchCapabilityForModelFunc`). False also stands for unknown.
pub type ApplyPatchCapability<'a> = &'a dyn Fn(&str) -> bool;

/// The instructions an entry without a template of its own gets
/// (`codexClientFallbackInstructions`).
const FALLBACK_INSTRUCTIONS: &str =
    "You are Codex, a coding agent. You and the user share one workspace.";

/// The reasoning levels clients from 0.144.0 on take.
const REASONING_LEVELS: [&str; 8] = [
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];

/// The reasoning levels older clients take.
const LEGACY_REASONING_LEVELS: [&str; 6] = ["none", "minimal", "low", "medium", "high", "xhigh"];

/// The client version that brought the `max` and `ultra` reasoning levels.
const EXTENDED_REASONING_VERSION: &str = "0.144.0";

/// Image and video models, which Codex clients don't list
/// (`isCodexClientImageOrVideoModel`).
const IMAGE_AND_VIDEO_MODELS: [&str; 11] = [
    "grok-imagine-image-quality",
    "gpt-image-1.5",
    "gpt-image-2",
    "gpt-image-2.5-flare",
    "gpt-image-2.5-sunburst",
    "gpt-image-2.5",
    "grok-imagine-image",
    "grok-imagine-image-2.0",
    "grok-imagine-video",
    "grok-imagine-video-1.5",
    "grok-imagine-video-1.5-preview",
];

/// The priority of an entry without one (`codexClientModelPriority`).
const DEFAULT_PRIORITY: i64 = 100;

/// The Codex client model list for `models` (upstream's
/// `BuildResponseForClientWithToolCapabilities`): `{"models": [...]}`, or
/// `{"models": null}` if no Codex client catalog is in use
/// ([`CodexClientCatalog::current`]).
///
/// `catalog` gives the models' registered details. `providers_for_model`,
/// if given, narrows capabilities to the providers serving each model;
/// `apply_patch`, if given, decides `apply_patch_tool_type`; otherwise a
/// model keeps what its template declares.
/// `optimize_multi_agent_v2` advertises multi-agent v2 for every model, and
/// `client_version` is the client's, which decides its reasoning levels.
pub fn build_response(
    catalog: &dyn ModelCatalog,
    models: &[ModelInfo],
    providers_for_model: Option<ProvidersForModel<'_>>,
    apply_patch: Option<ApplyPatchCapability<'_>>,
    optimize_multi_agent_v2: bool,
    client_version: &str,
) -> Value {
    let builder = Builder {
        catalog,
        statics: StaticCatalog::current(),
        providers_for_model,
        apply_patch,
        optimize_multi_agent_v2,
        client_version,
    };
    let models = CodexClientCatalog::current()
        .map_or(Value::Null, |templates| builder.build(&templates, models));
    json!({ "models": models })
}

/// `value` as one line of JSON, as upstream's `MarshalCompact` writes it:
/// object keys sorted, and strings escaped as Go escapes them without its
/// HTML escapes, so `<`, `>` and `&` stay as they are.
pub fn marshal_compact(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value);
    out
}

/// What each entry is built with.
struct Builder<'a> {
    catalog: &'a dyn ModelCatalog,
    /// The static catalog in use, taken once for the whole list.
    statics: Arc<StaticCatalog>,
    providers_for_model: Option<ProvidersForModel<'a>>,
    apply_patch: Option<ApplyPatchCapability<'a>>,
    optimize_multi_agent_v2: bool,
    client_version: &'a str,
}

impl Builder<'_> {
    /// Upstream's `buildCodexClientModelsWithToolCapabilities`.
    fn build(&self, templates: &CodexClientCatalog, models: &[ModelInfo]) -> Value {
        let mut result: Vec<Map<String, Value>> = Vec::with_capacity(models.len());
        for model in models {
            let id = model.id.trim();
            if id.is_empty() {
                continue;
            }
            let metadata_id = self.metadata_model_id(id);
            let entry = match templates.template(&metadata_id) {
                Some(template) => self.template_entry(template.clone(), id, &metadata_id, model),
                None => self.synthesized_entry(templates.default_template().clone(), id, model),
            };
            result.push(entry);
        }
        self.apply_non_template_priorities(&mut result, templates);
        result.sort_by_key(model_priority);
        Value::Array(result.into_iter().map(Value::Object).collect())
    }

    /// The entry of a model with a template of its own.
    fn template_entry(
        &self,
        mut entry: Map<String, Value>,
        id: &str,
        metadata_id: &str,
        model: &ModelInfo,
    ) -> Map<String, Value> {
        entry.insert("slug".into(), id.into());
        let info = self.lookup(id, "");
        self.apply_model_capabilities(&mut entry, id, metadata_id, info.as_ref());
        let display_name = model.display_name.trim();
        if !display_name.is_empty() {
            entry.insert("display_name".into(), display_name.into());
        }
        let description = model.description.trim();
        if !description.is_empty() {
            entry.insert("description".into(), description.into());
        }
        if model.max_context_length > 0 {
            entry.insert("context_window".into(), model.max_context_length.into());
            entry.insert("max_context_window".into(), model.max_context_length.into());
        }
        apply_max_tokens(&mut entry, model);
        self.apply_provider_capabilities(&mut entry, id, true);
        entry.shift_remove("cpa_capabilities");
        sanitize_reasoning_metadata(&mut entry, self.client_version);
        apply_visibility_override(&mut entry, id);
        if self.optimize_multi_agent_v2 {
            entry.insert("multi_agent_version".into(), "v2".into());
        }
        self.apply_apply_patch_capability(&mut entry, id);
        entry
    }

    /// The entry of a model without a template, made from the default one.
    fn synthesized_entry(
        &self,
        mut entry: Map<String, Value>,
        id: &str,
        model: &ModelInfo,
    ) -> Map<String, Value> {
        self.apply_model_metadata(&mut entry, id, model);
        apply_max_tokens(&mut entry, model);
        self.apply_provider_capabilities(&mut entry, id, false);
        entry.shift_remove("cpa_capabilities");
        sanitize_reasoning_metadata(&mut entry, self.client_version);
        apply_visibility_override(&mut entry, id);
        self.apply_apply_patch_capability(&mut entry, id);
        entry
    }

    /// Upstream's `LookupModelInfo`: `id`'s details as registered under
    /// `provider`, or as last registered at all, or from the static catalog.
    fn lookup(&self, id: &str, provider: &str) -> Option<ModelInfo> {
        let id = id.trim();
        if id.is_empty() {
            return None;
        }
        let provider = go::to_lower(provider.trim());
        self.catalog
            .model_info(id, &provider)
            .or_else(|| self.statics.lookup(id))
    }

    /// The providers of `id`, or of what follows its first `/` when it has
    /// none; `None` when there is no provider lookup.
    fn providers(&self, id: &str) -> Option<Vec<String>> {
        let providers_for_model = self.providers_for_model?;
        let mut providers = providers_for_model(id);
        if providers.is_empty()
            && let Some(slash) = id.find('/')
        {
            providers = providers_for_model(id[slash + 1..].trim());
        }
        Some(providers)
    }

    /// The model whose template `id` takes: the one it names as its metadata
    /// model, else, for a prefixed ID, the metadata model of what follows the
    /// prefix or that itself (`codexClientMetadataModelID`).
    fn metadata_model_id(&self, id: &str) -> String {
        let id = id.trim();
        if let Some(info) = self.lookup(id, "") {
            let metadata_id = info.metadata_model_id.trim();
            if !metadata_id.is_empty() {
                return metadata_id.to_owned();
            }
        }
        let Some(slash) = id.find('/') else {
            return id.to_owned();
        };
        let base = id[slash + 1..].trim();
        if let Some(info) = self.lookup(base, "") {
            let metadata_id = info.metadata_model_id.trim();
            if !metadata_id.is_empty() {
                return metadata_id.to_owned();
            }
        }
        base.to_owned()
    }

    /// Narrows a template's input modalities and reasoning levels to what
    /// the model's providers support (`applyCodexClientModelCapabilities`).
    /// A provider constrains them when it serves the model as an alias of
    /// another, unless it is Codex, or when its config sets them.
    fn apply_model_capabilities(
        &self,
        entry: &mut Map<String, Value>,
        id: &str,
        metadata_id: &str,
        info: Option<&ModelInfo>,
    ) {
        if info.is_some_and(|info| info.model_type == OPENAI_IMAGE_MODEL_TYPE) {
            hide_image_model(entry);
            return;
        }
        let providers = self.providers(id).unwrap_or_default();
        let is_alias = !metadata_id.is_empty() && !go::equal_fold(id, metadata_id);
        let provider_infos: Vec<(bool, ModelInfo)> = providers
            .iter()
            .filter_map(|provider| {
                let info = self.lookup(id, provider).or_else(|| {
                    let slash = id.find('/')?;
                    self.lookup(id[slash + 1..].trim(), provider)
                })?;
                Some((go::equal_fold(provider.trim(), "codex"), info))
            })
            .collect();

        let mut modalities: Option<Vec<String>> = None;
        for (is_codex, provider_info) in &provider_infos {
            if (!is_codex && is_alias) || provider_info.explicit_input_modalities {
                let supported = &provider_info.supported_input_modalities;
                modalities = Some(match modalities {
                    None => supported.clone(),
                    Some(constrained) => intersect_string_slices(&constrained, supported),
                });
            }
        }
        if modalities.is_none()
            && let Some(info) = info.filter(|info| info.explicit_input_modalities)
        {
            modalities = Some(info.supported_input_modalities.clone());
        }
        if let Some(modalities) = modalities {
            set_input_modalities(entry, filter_codex_input_modalities(&modalities));
        }

        let mut thinking: Option<ThinkingSupport> = None;
        for (is_codex, provider_info) in &provider_infos {
            if (!is_codex && is_alias) || provider_info.explicit_thinking {
                let supported = thinking_or_none(provider_info.thinking.as_ref());
                thinking = Some(match thinking {
                    None => supported,
                    Some(constrained) => intersect_thinking_support(&constrained, &supported),
                });
            }
        }
        if thinking.is_none()
            && let Some(info) = info.filter(|info| info.explicit_thinking)
        {
            thinking = Some(thinking_or_none(info.thinking.as_ref()));
        }
        if let Some(thinking) = thinking {
            apply_thinking_metadata(entry, Some(&thinking), self.client_version);
        }
    }

    /// Fills the default template in for a model without one of its own
    /// (`applyCodexClientModelMetadata`).
    fn apply_model_metadata(&self, entry: &mut Map<String, Value>, id: &str, model: &ModelInfo) {
        let info = self.lookup(id, "");
        let mut display_name = model.display_name.trim().to_owned();
        let mut description = model.description.trim().to_owned();
        let mut context_window = model.context_length;
        let mut thinking = None;
        if let Some(info) = &info {
            if !info.display_name.is_empty() {
                display_name.clone_from(&info.display_name);
            }
            if !info.description.is_empty() {
                description.clone_from(&info.description);
            }
            if context_window == 0 && info.context_length > 0 {
                context_window = info.context_length;
            }
            if info.model_type == OPENAI_IMAGE_MODEL_TYPE {
                hide_image_model(entry);
            } else {
                apply_input_modalities_metadata(entry, &info.supported_input_modalities);
            }
            thinking = info.thinking.as_ref();
        }
        apply_thinking_metadata(entry, thinking, self.client_version);

        if model.max_context_length > 0 {
            context_window = model.max_context_length;
        }
        if display_name.is_empty() {
            display_name = id.to_owned();
        }
        if description.is_empty() {
            description = id.to_owned();
        }

        entry.insert("slug".into(), id.into());
        entry.insert("display_name".into(), display_name.into());
        entry.insert("description".into(), description.into());
        entry.insert("prefer_websockets".into(), false.into());
        if self.optimize_multi_agent_v2 {
            entry.insert("multi_agent_version".into(), "v2".into());
        }
        entry.insert("service_tiers".into(), json!([]));
        null_required_options(entry);
        if context_window > 0 {
            entry.insert("context_window".into(), context_window.into());
            entry.insert("max_context_window".into(), context_window.into());
        }
        // Codex 0.156 and later take at most 1 MiB of model list; the
        // template's full instructions on every such entry would pass that.
        use_compact_instructions(entry);
    }

    /// Keeps Codex-only capabilities for models Codex alone serves
    /// (`applyCodexClientProviderCapabilities`).
    fn apply_provider_capabilities(
        &self,
        entry: &mut Map<String, Value>,
        id: &str,
        template: bool,
    ) {
        if !template {
            self.apply_search_tool_support(entry, id, false);
            return;
        }
        if self.providers_for_model.is_some() && !self.is_pure_codex_provider(id) {
            entry.insert("supports_search_tool".into(), false.into());
            entry.insert("prefer_websockets".into(), false.into());
            entry.insert("service_tiers".into(), json!([]));
            null_required_options(entry);
            return;
        }
        self.apply_search_tool_support(entry, id, true);
    }

    /// Whether Codex is the only provider serving `id`; true without a
    /// provider lookup (`isPureCodexProvider`).
    fn is_pure_codex_provider(&self, id: &str) -> bool {
        let Some(providers) = self.providers(id) else {
            return true;
        };
        !providers.is_empty() && providers.iter().all(|p| go::equal_fold(p.trim(), "codex"))
    }

    /// Keeps the search tool only for a template model that Codex alone
    /// serves, or any template model without a provider lookup
    /// (`applyCodexClientSearchToolSupport`).
    fn apply_search_tool_support(&self, entry: &mut Map<String, Value>, id: &str, template: bool) {
        if entry.get("supports_search_tool") != Some(&Value::Bool(true)) {
            return;
        }
        if !template {
            entry.insert("supports_search_tool".into(), false.into());
            return;
        }
        let Some(providers) = self.providers(id) else {
            return;
        };
        if providers.is_empty() || !providers.iter().all(|p| go::equal_fold(p.trim(), "codex")) {
            entry.insert("supports_search_tool".into(), false.into());
        }
    }

    /// Ranks the entries without a template of their own after the
    /// catalog's, in order of display name, then slug
    /// (`applyCodexClientNonTemplatePriorities`).
    fn apply_non_template_priorities(
        &self,
        result: &mut [Map<String, Value>],
        templates: &CodexClientCatalog,
    ) {
        if result.is_empty() {
            return;
        }
        let base = templates.templates().map(model_priority).fold(0, i64::max);
        let mut pending: Vec<(usize, String, String)> = Vec::new();
        for (index, entry) in result.iter().enumerate() {
            let slug = string_value(entry, "slug");
            if templates.template(&self.metadata_model_id(slug)).is_some() {
                continue;
            }
            let display_name = match string_value(entry, "display_name") {
                "" => slug,
                name => name,
            };
            pending.push((index, go::to_lower(display_name), slug.to_owned()));
        }
        pending.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.2.cmp(&b.2)));
        for (rank, (index, ..)) in (1..).zip(&pending) {
            result[*index].insert("priority".into(), (base + 100 * rank).into());
        }
    }

    /// Sets `apply_patch_tool_type` for a text model that isn't an image or
    /// video model: `freeform` when the capability says so, or, without a
    /// capability, when the entry still declares `freeform` from its
    /// template; else `null` (`applyCodexClientApplyPatchCapability`). Hidden
    /// text models keep the tool; entries that don't take text don't get it.
    fn apply_apply_patch_capability(&self, entry: &mut Map<String, Value>, id: &str) {
        let template_supported =
            entry.get("apply_patch_tool_type").and_then(Value::as_str) == Some("freeform");
        entry.insert("apply_patch_tool_type".into(), Value::Null);
        let lower = go::to_lower(id.trim());
        let base_id = match lower.rfind('/') {
            Some(slash) => lower[slash + 1..].trim(),
            None => lower.as_str(),
        };
        if is_image_or_video_model(base_id) {
            return;
        }
        let (has_modalities, supports_text) = match entry.get("input_modalities") {
            Some(Value::Array(modalities)) => (
                !modalities.is_empty(),
                modalities.iter().any(|modality| modality == "text"),
            ),
            _ => (false, false),
        };
        let hidden = entry.get("visibility").and_then(Value::as_str) == Some("hide");
        if !supports_text && (has_modalities || hidden) {
            return;
        }
        let Some(capability) = self.apply_patch else {
            if template_supported {
                entry.insert("apply_patch_tool_type".into(), "freeform".into());
            }
            return;
        };
        if capability(id.trim()) {
            entry.insert("apply_patch_tool_type".into(), "freeform".into());
        }
    }
}

/// `thinking`, or support for no levels at all when there is none.
fn thinking_or_none(thinking: Option<&ThinkingSupport>) -> ThinkingSupport {
    thinking.cloned().unwrap_or_default()
}

/// What both `a` and `b` allow (`intersectThinkingSupport`).
fn intersect_thinking_support(a: &ThinkingSupport, b: &ThinkingSupport) -> ThinkingSupport {
    let mut max = a.max;
    if b.max > 0 && (max == 0 || b.max < max) {
        max = b.max;
    }
    ThinkingSupport {
        min: a.min.max(b.min),
        max,
        zero_allowed: a.zero_allowed && b.zero_allowed,
        dynamic_allowed: a.dynamic_allowed && b.dynamic_allowed,
        levels: intersect_string_slices(&a.levels, &b.levels),
    }
}

/// The items of `a` also in `b`, each once, compared trimmed and in lower
/// case (`intersectStringSlices`).
fn intersect_string_slices(a: &[String], b: &[String]) -> Vec<String> {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    let key = |item: &str| go::to_lower(item.trim());
    let in_b: Vec<String> = b.iter().map(|item| key(item)).collect();
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for item in a {
        let item_key = key(item);
        if in_b.contains(&item_key) && !seen.contains(&item_key) {
            seen.push(item_key);
            out.push(item.clone());
        }
    }
    out
}

/// Hides an entry for a model the image endpoints serve.
fn hide_image_model(entry: &mut Map<String, Value>) {
    entry.insert("visibility".into(), "hide".into());
    entry.shift_remove("input_modalities");
    entry.shift_remove("supports_image_detail_original");
}

/// Sets `input_modalities`, and `supports_image_detail_original` when they
/// include images.
fn set_input_modalities(entry: &mut Map<String, Value>, modalities: Vec<Value>) {
    let has_image = modalities.iter().any(|modality| modality == "image");
    entry.insert("input_modalities".into(), Value::Array(modalities));
    if has_image {
        entry.insert("supports_image_detail_original".into(), true.into());
    } else {
        entry.shift_remove("supports_image_detail_original");
    }
}

/// Sets a registered model's input modalities, unless it has none Codex
/// knows (`applyCodexClientInputModalitiesMetadata`).
fn apply_input_modalities_metadata(entry: &mut Map<String, Value>, modalities: &[String]) {
    if modalities.is_empty() {
        return;
    }
    let modalities = filter_codex_input_modalities(modalities);
    if !modalities.is_empty() {
        set_input_modalities(entry, modalities);
    }
}

/// The modalities Codex knows, `text` and `image`, each once, trimmed and in
/// lower case (`filterCodexInputModalities`).
fn filter_codex_input_modalities(modalities: &[String]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for modality in modalities {
        let modality = go::to_lower(modality.trim());
        if matches!(modality.as_str(), "text" | "image") && !out.iter().any(|m| *m == *modality) {
            out.push(modality.into());
        }
    }
    out
}

/// `max_tokens` from the model's output limit (`applyCodexClientMaxTokens`).
fn apply_max_tokens(entry: &mut Map<String, Value>, model: &ModelInfo) {
    if model.max_completion_tokens > 0 {
        entry.insert("max_tokens".into(), model.max_completion_tokens.into());
    }
}

/// Clears capabilities other providers' models must not advertise. Codex
/// rejects a list without these keys, so they stay, as `null`
/// (`nullCodexClientRequiredOptions`).
fn null_required_options(entry: &mut Map<String, Value>) {
    for key in ["apply_patch_tool_type", "upgrade", "availability_nux"] {
        entry.insert(key.into(), Value::Null);
    }
}

/// Swaps the template's instructions for a short line, in both fields Codex
/// reads (`useCompactCodexClientInstructions`).
fn use_compact_instructions(entry: &mut Map<String, Value>) {
    entry.insert("base_instructions".into(), FALLBACK_INSTRUCTIONS.into());
    entry.insert(
        "model_messages".into(),
        json!({
            "instructions_template": FALLBACK_INSTRUCTIONS,
            "instructions_variables": null,
            "approvals": null,
            "collaboration_modes": null,
            "auto_review": null,
            "permissions": null,
            "multi_agent": null,
        }),
    );
}

/// Hides image and video models (`applyCodexClientVisibilityOverride`).
fn apply_visibility_override(entry: &mut Map<String, Value>, id: &str) {
    if is_image_or_video_model(id) {
        entry.insert("visibility".into(), "hide".into());
    }
}

/// Whether `id`, or what follows its first `/`, is an image or video model
/// (`isCodexClientImageOrVideoModel`). The dashboard's client setup uses it
/// too, to tell chat models from others.
pub fn is_image_or_video_model(id: &str) -> bool {
    let mut target = id.trim();
    if let Some(slash) = target.find('/') {
        target = target[slash + 1..].trim();
    }
    IMAGE_AND_VIDEO_MODELS.contains(&target)
}

/// Sets the reasoning levels of `thinking` the client takes, defaulting to
/// `medium`, else the first that isn't `none`, else the first
/// (`applyCodexClientThinkingMetadata`).
fn apply_thinking_metadata(
    entry: &mut Map<String, Value>,
    thinking: Option<&ThinkingSupport>,
    client_version: &str,
) {
    let Some(thinking) = thinking else {
        return;
    };
    let mut levels = Vec::with_capacity(thinking.levels.len());
    let mut default_level = String::new();
    let mut first_level = String::new();
    for raw in &thinking.levels {
        let level = normalize_reasoning_level(raw, client_version);
        if level.is_empty() {
            continue;
        }
        if first_level.is_empty() {
            first_level.clone_from(&level);
        }
        if (default_level.is_empty() && level != "none") || level == "medium" {
            default_level.clone_from(&level);
        }
        levels.push(json!({
            "effort": level,
            "description": reasoning_description(&level),
        }));
    }
    if levels.is_empty() {
        entry.insert("supported_reasoning_levels".into(), json!([]));
        entry.shift_remove("default_reasoning_level");
        return;
    }
    if default_level.is_empty() {
        default_level = first_level;
    }
    entry.insert("supported_reasoning_levels".into(), Value::Array(levels));
    entry.insert("default_reasoning_level".into(), default_level.into());
}

/// Drops the reasoning levels the client doesn't take, and makes the default
/// one it does (`sanitizeCodexClientReasoningMetadata`).
fn sanitize_reasoning_metadata(entry: &mut Map<String, Value>, client_version: &str) {
    let Some(Value::Array(raw_levels)) = entry.get("supported_reasoning_levels") else {
        return;
    };
    let mut levels: Vec<Value> = Vec::with_capacity(raw_levels.len());
    let mut allowed: Vec<String> = Vec::with_capacity(raw_levels.len());
    for raw in raw_levels {
        let Value::Object(level_entry) = raw else {
            continue;
        };
        let level = normalize_reasoning_level(string_value(level_entry, "effort"), client_version);
        if level.is_empty() {
            continue;
        }
        let mut cloned = level_entry.clone();
        cloned.insert("effort".into(), level.clone().into());
        levels.push(Value::Object(cloned));
        allowed.push(level);
    }
    if levels.is_empty() {
        entry.insert("supported_reasoning_levels".into(), json!([]));
        entry.shift_remove("default_reasoning_level");
        return;
    }
    let mut default_level = normalize_reasoning_level(
        string_value(entry, "default_reasoning_level"),
        client_version,
    );
    if !allowed.contains(&default_level) {
        default_level.clone_from(&allowed[0]);
    }
    entry.insert("supported_reasoning_levels".into(), Value::Array(levels));
    entry.insert("default_reasoning_level".into(), default_level.into());
}

/// `raw` trimmed and in lower case if the client takes it as a reasoning
/// level, else empty (`normalizeCodexClientReasoningLevel`).
fn normalize_reasoning_level(raw: &str, client_version: &str) -> String {
    let level = go::to_lower(raw.trim());
    let allowed: &[&str] = if supports_extended_reasoning_levels(client_version) {
        &REASONING_LEVELS
    } else {
        &LEGACY_REASONING_LEVELS
    };
    if allowed.contains(&level.as_str()) {
        level
    } else {
        String::new()
    }
}

/// Whether the client takes the `max` and `ultra` reasoning levels: it is
/// 0.144.0 or later, or its version is missing or unreadable
/// (`supportsExtendedReasoningLevels`).
fn supports_extended_reasoning_levels(client_version: &str) -> bool {
    let client_version = client_version.trim();
    if client_version.is_empty() {
        return true;
    }
    compare_dotted_versions(client_version, EXTENDED_REASONING_VERSION)
        .is_none_or(|order| order != Ordering::Less)
}

/// The numbers of a version such as `v1.2.3-beta`, ignoring a leading `v`,
/// anything from a `-` or `+`, and empty parts; empty if a part isn't a
/// number that fits in 64 bits, or is negative (`parseDottedVersion`).
fn parse_dotted_version(version: &str) -> Vec<i64> {
    let mut version = version.trim();
    if let Some(rest) = version.strip_prefix(['v', 'V']) {
        version = rest;
    }
    if let Some(end) = version.find(['-', '+']) {
        version = &version[..end];
    }
    let mut numbers = Vec::new();
    for part in version.split('.') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.parse::<i64>() {
            Ok(number) if number >= 0 => numbers.push(number),
            _ => return Vec::new(),
        }
    }
    numbers
}

/// How version `a` compares to `b`, missing parts counting as 0, or `None`
/// if either has no numbers (`compareDottedVersions`).
fn compare_dotted_versions(a: &str, b: &str) -> Option<Ordering> {
    let (a, b) = (parse_dotted_version(a), parse_dotted_version(b));
    if a.is_empty() || b.is_empty() {
        return None;
    }
    let part = |numbers: &[i64], i: usize| numbers.get(i).copied().unwrap_or(0);
    let order = (0..a.len().max(b.len()))
        .map(|i| part(&a, i).cmp(&part(&b, i)))
        .find(|order| order.is_ne())
        .unwrap_or(Ordering::Equal);
    Some(order)
}

/// What a reasoning level does, for the client's picker
/// (`codexClientReasoningDescription`).
fn reasoning_description(level: &str) -> &str {
    match level {
        "none" => "No reasoning",
        "minimal" => "Fastest responses with minimal reasoning",
        "low" => "Fast responses with lighter reasoning",
        "medium" => "Balances speed and reasoning depth for everyday tasks",
        "high" => "Greater reasoning depth for complex problems",
        "xhigh" => "Extra high reasoning depth for complex problems",
        "max" => "Maximum available reasoning depth for complex problems",
        other => other,
    }
}

/// An entry's `priority`, a fraction cut off, or 100 when it has none
/// (`codexClientModelPriority`).
fn model_priority(model: &Map<String, Value>) -> i64 {
    match model.get("priority") {
        Some(Value::Number(number)) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|float| float as i64))
            .unwrap_or(DEFAULT_PRIORITY),
        _ => DEFAULT_PRIORITY,
    }
}

/// `key`'s string, trimmed, or empty when it isn't a string
/// (`stringModelValue`).
fn string_value<'a>(model: &'a Map<String, Value>, key: &str) -> &'a str {
    model.get(key).and_then(Value::as_str).unwrap_or("").trim()
}

fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::String(text) => write_string(out, text),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::Object(fields) => {
            let mut entries: Vec<(&String, &Value)> = fields.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (i, (key, value)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, key);
                out.push(':');
                write_value(out, value);
            }
            out.push('}');
        }
    }
}

/// A JSON string as Go's encoder writes it with HTML escaping off: control
/// characters, quotes, backslashes, U+2028 and U+2029 escaped, nothing else.
fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' || c == '\u{2028}' || c == '\u{2029}' => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}
