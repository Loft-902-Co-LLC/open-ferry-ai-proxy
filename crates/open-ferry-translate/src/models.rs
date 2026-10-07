// Ported from CLIProxyAPI internal/registry/model_definitions.go and
// model_registry.go (v8.0.15, MIT). models/models.json is upstream's
// internal/registry/models/models.json as of v8.0.15, unchanged.
// https://github.com/router-for-me/CLIProxyAPI

//! What each model supports, as far as translators need to know.
//!
//! Some translators pick a request's shape from the target model's thinking
//! support: Claude 4.6 and later take adaptive thinking with an effort level,
//! older models a token budget. Some also cap the output tokens a client asks
//! for at the model's limit, and Gemini's web search depends on whether the
//! model has it. Upstream looks this up in a global registry, which holds
//! the models of configured accounts and falls back to a static catalog.
//!
//! The catalog in use is [`ModelCatalog::current`]: the built-in one, until
//! open-ferry-core's `registry::CatalogStore` publishes another, made from
//! the static catalog it read from the file `models.catalog` names, as
//! upstream's translators read the catalog its updater loaded. Readers take
//! it per request, so a new catalog applies from the next request on.
//!
//! Deviations from upstream:
//! - Only the static catalog is searched, not the models of configured
//!   accounts.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, PoisonError, RwLock};

use serde_json::Value;

/// Upstream's static catalog.
const EMBEDDED_CATALOG: &str = include_str!("../models/models.json");

/// The catalog sections upstream's `LookupStaticModelInfo` searches, in order,
/// before its built-in Devin models and then `meta`. The built-in file has no
/// `devin` section.
const SECTIONS: [&str; 9] = [
    "claude",
    "gemini",
    "vertex",
    "aistudio",
    "codex-pro",
    "kimi",
    "antigravity",
    "xai",
    "devin",
];

/// Upstream's built-in Devin models, searched after [`SECTIONS`] and before
/// `meta`: each model's ID, effort levels and output token limit.
const BUILTIN_DEVIN_MODELS: [(&str, &[&str], i64); 12] = [
    ("devin/swe-1-6-slow", &[], 64000),
    ("devin/swe-2", &["medium", "high", "max"], 128000),
    (
        "devin/claude-fable-5-1",
        &["low", "medium", "high", "xhigh", "max"],
        64000,
    ),
    (
        "devin/gpt-6-astra",
        &["low", "medium", "high", "xhigh", "max"],
        64000,
    ),
    ("devin/glm-5-2", &["none", "high"], 64000),
    ("devin/glm-5-3", &["low", "high", "max"], 128000),
    ("devin/glm-5-3-flash", &["low", "high", "max"], 128000),
    (
        "devin/gpt-5-6-sol",
        &["none", "low", "medium", "high", "xhigh", "max"],
        128000,
    ),
    ("devin/gemini-3-8-flash", &["low", "medium", "high"], 65536),
    (
        "devin/grok-4-6",
        &["low", "medium", "high", "xhigh"],
        131072,
    ),
    ("devin/deepseek-v4-flash", &["high", "max"], 64000),
    ("devin/deepseek-v4-1-flash", &["high", "max"], 64000),
];

/// A model's thinking settings (upstream's `ThinkingSupport`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThinkingSupport {
    /// The smallest thinking budget allowed.
    pub min: i64,
    /// The largest thinking budget allowed.
    pub max: i64,
    /// Whether a budget of 0 turns thinking off.
    pub zero_allowed: bool,
    /// Whether a budget of -1 lets the model decide.
    pub dynamic_allowed: bool,
    /// Named effort levels. A model with levels takes one instead of a budget.
    pub levels: Vec<String>,
}

/// The text of upstream's static catalog, `models.json`, as built in.
pub fn embedded_catalog_json() -> &'static str {
    EMBEDDED_CATALOG
}

/// One model in the catalog.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub thinking: Option<ThinkingSupport>,
    /// The most output tokens the model takes, or 0 if the catalog doesn't
    /// say.
    pub max_completion_tokens: i64,
    /// `native_capabilities.web_search`, if the catalog gives it.
    pub native_web_search: Option<bool>,
    /// `supports_web_search`.
    pub supports_web_search: bool,
}

/// Models by ID, each as the first section to list it describes it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelCatalog {
    models: HashMap<String, ModelInfo>,
}

/// The built-in catalog; empty if it doesn't load.
static EMBEDDED: LazyLock<Arc<ModelCatalog>> =
    LazyLock::new(|| Arc::new(ModelCatalog::from_json(EMBEDDED_CATALOG).unwrap_or_default()));

/// The catalog in use.
static CURRENT: LazyLock<RwLock<Arc<ModelCatalog>>> =
    LazyLock::new(|| RwLock::new(Arc::clone(&EMBEDDED)));

impl ModelCatalog {
    /// Upstream's static catalog, as built in.
    pub fn embedded() -> &'static Self {
        &EMBEDDED
    }

    /// The catalog in use: the built-in one until [`ModelCatalog::set_current`]
    /// replaces it.
    pub fn current() -> Arc<Self> {
        Arc::clone(&CURRENT.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Makes `catalog` the one in use, from the next [`ModelCatalog::current`]
    /// on.
    pub fn set_current(catalog: Arc<Self>) {
        *CURRENT.write().unwrap_or_else(PoisonError::into_inner) = catalog;
    }

    /// Reads a catalog in the format of upstream's `models.json`.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        let root: Value = serde_json::from_str(text)?;
        let section = |name: &str| match root.get(name) {
            Some(Value::Array(models)) => models.iter().filter_map(model_info).collect(),
            _ => Vec::new(),
        };
        let searched: Vec<ModelInfo> = SECTIONS.into_iter().flat_map(section).collect();
        Ok(Self::from_models(searched, section("meta")))
    }

    /// The catalog of `searched`, the models of each section upstream's
    /// `LookupStaticModelInfo` searches before its built-in Devin models, in
    /// its order, and then `meta`. Where an ID repeats, the first model
    /// stands; a model without an ID is never found, as upstream's isn't.
    pub fn from_models(
        searched: impl IntoIterator<Item = ModelInfo>,
        meta: impl IntoIterator<Item = ModelInfo>,
    ) -> Self {
        let builtin_devin =
            BUILTIN_DEVIN_MODELS
                .iter()
                .map(|(id, levels, max_completion_tokens)| ModelInfo {
                    id: (*id).to_owned(),
                    thinking: (!levels.is_empty()).then(|| ThinkingSupport {
                        levels: levels.iter().map(|level| (*level).to_owned()).collect(),
                        ..ThinkingSupport::default()
                    }),
                    max_completion_tokens: *max_completion_tokens,
                    ..ModelInfo::default()
                });
        let mut models = HashMap::new();
        for model in searched.into_iter().chain(builtin_devin).chain(meta) {
            if !model.id.is_empty() {
                models.entry(model.id.clone()).or_insert(model);
            }
        }
        Self { models }
    }

    /// `LookupModelInfo`: the model with this ID, ignoring surrounding
    /// whitespace.
    pub fn lookup(&self, id: &str) -> Option<&ModelInfo> {
        self.models.get(id.trim())
    }

    /// Every model, in no particular order.
    pub fn models(&self) -> impl Iterator<Item = &ModelInfo> {
        self.models.values()
    }

    /// The thinking settings of the model with this ID, if it is known and
    /// has any.
    pub fn thinking(&self, id: &str) -> Option<&ThinkingSupport> {
        self.lookup(id)?.thinking.as_ref()
    }
}

fn model_info(model: &Value) -> Option<ModelInfo> {
    let id = model.get("id")?.as_str()?.to_owned();
    let thinking = match model.get("thinking") {
        Some(Value::Object(thinking)) => {
            let int = |key: &str| thinking.get(key).and_then(Value::as_i64).unwrap_or(0);
            let flag = |key: &str| thinking.get(key).and_then(Value::as_bool) == Some(true);
            let levels = match thinking.get("levels") {
                Some(Value::Array(levels)) => levels
                    .iter()
                    .filter_map(|level| Some(level.as_str()?.to_owned()))
                    .collect(),
                _ => Vec::new(),
            };
            Some(ThinkingSupport {
                min: int("min"),
                max: int("max"),
                zero_allowed: flag("zero_allowed"),
                dynamic_allowed: flag("dynamic_allowed"),
                levels,
            })
        }
        _ => None,
    };
    let max_completion_tokens = model
        .get("max_completion_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let native_web_search = model
        .get("native_capabilities")
        .and_then(|capabilities| capabilities.get("web_search"))
        .and_then(Value::as_bool);
    Some(ModelInfo {
        id,
        thinking,
        max_completion_tokens,
        native_web_search,
        supports_web_search: model.get("supports_web_search").and_then(Value::as_bool)
            == Some(true),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_catalog_knows_claude_thinking() {
        let catalog = ModelCatalog::embedded();
        let opus = catalog.thinking("claude-opus-4-6").unwrap();
        assert_eq!(opus.levels, ["low", "medium", "high", "max"]);
        assert_eq!(opus.min, 1024);
        let haiku = catalog.thinking(" claude-haiku-4-5-20251001 ").unwrap();
        assert!(haiku.levels.is_empty());
        assert_eq!(haiku.min, 1024);
        assert!(catalog.lookup("claude-3-5-haiku-20241022").is_some());
        assert_eq!(catalog.thinking("claude-3-5-haiku-20241022"), None);
        assert_eq!(catalog.lookup("claude-opus-4-6(high)"), None);
        assert_eq!(catalog.lookup(""), None);
    }

    #[test]
    fn the_embedded_catalog_knows_output_limits() {
        let catalog = ModelCatalog::embedded();
        let limit = |id: &str| catalog.lookup(id).unwrap().max_completion_tokens;
        assert_eq!(limit("claude-opus-4-6"), 128000);
        assert_eq!(limit("claude-sonnet-4-5-20250929"), 64000);
        assert_eq!(limit("claude-3-5-haiku-20241022"), 8192);
        assert_eq!(limit("devin/grok-4-6"), 131072);
    }

    #[test]
    fn the_first_section_to_list_a_model_wins() {
        let catalog = ModelCatalog::from_json(
            r#"{
                "claude": [{"id": "m", "thinking": {"min": 1}}],
                "gemini": [{"id": "m", "thinking": {"levels": ["high"]}}, {"id": "g", "thinking": {}}],
                "meta": [{"id": "devin/swe-2"}]
            }"#,
        )
        .unwrap();
        assert_eq!(catalog.thinking("m").unwrap().min, 1);
        assert_eq!(catalog.thinking("g"), Some(&ThinkingSupport::default()));
        assert_eq!(catalog.thinking("devin/swe-2").unwrap().levels.len(), 3);
        assert_eq!(catalog.thinking("devin/swe-1-6-slow"), None);
    }

    // Not upstream's: a `devin` section is searched before the built-in
    // Devin models, as upstream's LookupStaticModelInfo searches it, and
    // web search is read.
    #[test]
    fn a_devin_section_comes_before_the_builtin_devin_models() {
        let catalog = ModelCatalog::from_json(
            r#"{
                "devin": [{"id": "devin/swe-2", "max_completion_tokens": 7}],
                "xai": [{"id": "x", "supports_web_search": true,
                         "native_capabilities": {"web_search": false}}]
            }"#,
        )
        .unwrap();
        assert_eq!(
            catalog.lookup("devin/swe-2").unwrap().max_completion_tokens,
            7
        );
        let x = catalog.lookup("x").unwrap();
        assert!(x.supports_web_search);
        assert_eq!(x.native_web_search, Some(false));
    }

    // Not upstream's: the catalog in use starts as the built-in one.
    #[test]
    fn the_current_catalog_starts_as_the_built_in_one() {
        assert_eq!(*ModelCatalog::current(), *ModelCatalog::embedded());
    }
}
