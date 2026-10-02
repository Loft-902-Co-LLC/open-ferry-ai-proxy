// Ported from CLIProxyAPI internal/registry/model_definitions.go and
// model_registry.go (v8.0.10, MIT). models/models.json is upstream's
// internal/registry/models/models.json, unchanged.
// https://github.com/router-for-me/CLIProxyAPI

//! What each model supports, as far as translators need to know.
//!
//! Some translators pick a request's shape from the target model's thinking
//! support: Claude 4.6 and later take adaptive thinking with an effort level,
//! older models a token budget. Some also cap the output tokens a client asks
//! for at the model's limit. Upstream looks this up in a global registry,
//! which holds the models of configured accounts and falls back to a static
//! catalog. Only the static catalog is ported so far.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde_json::Value;

/// Upstream's static catalog.
const EMBEDDED_CATALOG: &str = include_str!("../models/models.json");

/// The catalog sections upstream's `LookupStaticModelInfo` searches, in order.
/// `devin` is also searched but missing from the file, so the built-in Devin
/// models below come in its place.
const SECTIONS: [&str; 8] = [
    "claude",
    "gemini",
    "vertex",
    "aistudio",
    "codex-pro",
    "kimi",
    "antigravity",
    "xai",
];

/// Searched after [`SECTIONS`], then `meta`: each model's ID, effort levels
/// and output token limit.
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

/// One model in the catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub thinking: Option<ThinkingSupport>,
    /// The most output tokens the model takes, or 0 if the catalog doesn't
    /// say.
    pub max_completion_tokens: i64,
}

/// Models by ID, each as the first section to list it describes it.
#[derive(Clone, Debug, Default)]
pub struct ModelCatalog {
    models: HashMap<String, ModelInfo>,
}

impl ModelCatalog {
    /// Upstream's static catalog, as built in.
    pub fn embedded() -> &'static Self {
        static CATALOG: OnceLock<ModelCatalog> = OnceLock::new();
        CATALOG.get_or_init(|| {
            Self::from_json(EMBEDDED_CATALOG).expect("the embedded catalog is valid JSON")
        })
    }

    /// Reads a catalog in the format of upstream's `models.json`.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        let root: Value = serde_json::from_str(text)?;
        let section = |name: &str| match root.get(name) {
            Some(Value::Array(models)) => models.iter().filter_map(model_info).collect(),
            _ => Vec::new(),
        };
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
                });

        let mut models = HashMap::new();
        let all = SECTIONS
            .into_iter()
            .flat_map(section)
            .chain(builtin_devin)
            .chain(section("meta"));
        for model in all {
            models.entry(model.id.clone()).or_insert(model);
        }
        Ok(Self { models })
    }

    /// `LookupModelInfo`: the model with this ID, ignoring surrounding
    /// whitespace.
    pub fn lookup(&self, id: &str) -> Option<&ModelInfo> {
        self.models.get(id.trim())
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
    Some(ModelInfo {
        id,
        thinking,
        max_completion_tokens,
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
}
