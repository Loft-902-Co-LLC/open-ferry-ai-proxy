// Ported from CLIProxyAPI internal/registry/model_definitions.go and the
// catalog loading and checks in internal/registry/model_updater.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The static model catalog: upstream's `models.json`, built in, which lists
//! the models each provider serves, and the image models every Codex plan
//! adds.
//!
//! The catalog lists Codex models by ChatGPT plan; [`CodexPlan`] picks one.
//!
//! Deviations from upstream:
//! - The catalog isn't refreshed from the network (upstream's model updater
//!   fetches a new `models.json` every three hours); the built-in copy is
//!   used.
//! - Only the Claude, Gemini, Vertex, Codex, xAI and Meta sections are
//!   served, and [`StaticCatalog::models_for_channel`] has nothing for the
//!   xAI and Meta channels yet. The others are decoded and checked as
//!   upstream does, so a catalog upstream rejects is rejected here, and are
//!   kept only for [`StaticCatalog::lookup`].
//! - The xAI models don't include upstream's built-in image and video models
//!   (`WithXAIBuiltins`): image and video generation aren't ported.
//! - [`StaticCatalog::lookup`] doesn't search upstream's built-in Devin
//!   models, which no ported provider serves.
//! - A model's `config.override_header` is checked, then dropped: it forces a
//!   client's identity headers, which this project doesn't do. So are
//!   `native_capabilities` and `supports_web_search`.
//! - Decode errors are worded differently from Go's.
//! - Where a key repeats, exactly or in another case, the last one replaces
//!   the earlier ones; Go's decoder merges repeated objects and lists.
//! - If the built-in catalog doesn't load, it is empty; upstream logs a
//!   warning and later fails on the missing catalog.

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::fmt;
use std::sync::OnceLock;

use open_ferry_translate::go;
use open_ferry_translate::models::embedded_catalog_json;
use serde_json::{Map, Value};

use super::json;
use crate::models::{ModelInfo, ThinkingSupport};

/// The sections of `models.json`, in upstream's order.
const SECTIONS: [&str; 13] = [
    "claude",
    "gemini",
    "vertex",
    "aistudio",
    "codex-free",
    "codex-team",
    "codex-plus",
    "codex-pro",
    "kimi",
    "antigravity",
    "xai",
    "devin",
    "meta",
];

/// The one section upstream doesn't check.
const UNCHECKED_SECTION: &str = "devin";

/// The image models every Codex plan serves: ID and display name.
const CODEX_BUILTINS: [(&str, &str); 5] = [
    ("gpt-image-1.5", "GPT Image 1.5"),
    ("gpt-image-2", "GPT Image 2"),
    ("gpt-image-2.5-flare", "GPT Image 2.5 Flare"),
    ("gpt-image-2.5-sunburst", "GPT Image 2.5 Sunburst"),
    ("gpt-image-2.5", "GPT Image 2.5"),
];

/// When the Codex image models came out: 2024-01-01.
const CODEX_BUILTIN_CREATED: i64 = 1_704_067_200;

/// Why a catalog didn't load.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogError {
    /// The text isn't a catalog.
    Decode {
        /// Where the catalog came from, such as `embed`.
        origin: String,
        /// What was wrong.
        detail: String,
    },
    /// A section has a null model, a model without an ID, or a repeated ID.
    Validate {
        /// Where the catalog came from.
        origin: String,
        /// What was wrong.
        detail: String,
    },
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode { origin, detail } => {
                write!(f, "{origin}: decode models catalog: {detail}")
            }
            Self::Validate { origin, detail } => {
                write!(f, "{origin}: validate models catalog: {detail}")
            }
        }
    }
}

impl std::error::Error for CatalogError {}

/// A ChatGPT plan, which decides the Codex models a credential serves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CodexPlan {
    /// The free plan.
    Free,
    /// Team, Business and Go plans.
    Team,
    /// The Plus plan.
    Plus,
    /// The Pro plan, and any plan not known.
    #[default]
    Pro,
}

impl CodexPlan {
    /// The plan for a ChatGPT plan type such as `plus`, ignoring case.
    /// Unknown and empty plan types get the Pro models, as upstream's do.
    pub fn from_plan_type(plan_type: &str) -> Self {
        match go::to_lower(plan_type).as_str() {
            "pro" => Self::Pro,
            "plus" => Self::Plus,
            "team" | "business" | "go" => Self::Team,
            "free" => Self::Free,
            _ => Self::Pro,
        }
    }
}

/// The models of upstream's static catalog that this port serves.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StaticCatalog {
    claude: Vec<ModelInfo>,
    gemini: Vec<ModelInfo>,
    vertex: Vec<ModelInfo>,
    codex_free: Vec<ModelInfo>,
    codex_team: Vec<ModelInfo>,
    codex_plus: Vec<ModelInfo>,
    codex_pro: Vec<ModelInfo>,
    xai: Vec<ModelInfo>,
    meta: Vec<ModelInfo>,
    /// Every section but the Codex Free, Team and Plus ones, in the order
    /// upstream's `LookupStaticModelInfo` searches them.
    lookup: Vec<ModelInfo>,
}

impl StaticCatalog {
    /// Upstream's `models.json`, as built in.
    pub fn embedded() -> &'static Self {
        static CATALOG: OnceLock<StaticCatalog> = OnceLock::new();
        CATALOG
            .get_or_init(|| Self::from_json(embedded_catalog_json(), "embed").unwrap_or_default())
    }

    /// Reads a catalog in the format of upstream's `models.json`, and checks
    /// it as upstream does (upstream's `loadModelsFromBytes`). `origin` names
    /// the catalog in errors.
    pub fn from_json(text: &str, origin: &str) -> Result<Self, CatalogError> {
        let decode_error = |detail: String| CatalogError::Decode {
            origin: origin.to_owned(),
            detail,
        };
        let root: Value =
            serde_json::from_str(text).map_err(|err| decode_error(err.to_string()))?;
        let sections = decode_sections(&root).map_err(decode_error)?;
        validate(&sections).map_err(|detail| CatalogError::Validate {
            origin: origin.to_owned(),
            detail,
        })?;
        let [
            claude,
            gemini,
            vertex,
            aistudio,
            codex_free,
            codex_team,
            codex_plus,
            codex_pro,
            kimi,
            antigravity,
            xai,
            devin,
            meta,
        ] = sections.map(|models| models.into_iter().flatten().collect::<Vec<_>>());
        let lookup = [
            &claude,
            &gemini,
            &vertex,
            &aistudio,
            &codex_pro,
            &kimi,
            &antigravity,
            &xai,
            &devin,
            &meta,
        ]
        .into_iter()
        .flatten()
        .cloned()
        .collect();
        Ok(Self {
            claude,
            gemini,
            vertex,
            codex_free,
            codex_team,
            codex_plus,
            codex_pro,
            xai,
            meta,
            lookup,
        })
    }

    /// The first model with ID `id`, searching every section the way
    /// upstream's `LookupStaticModelInfo` does.
    pub fn lookup(&self, id: &str) -> Option<ModelInfo> {
        if id.is_empty() {
            return None;
        }
        self.lookup.iter().find(|model| model.id == id).cloned()
    }

    /// The Claude models (upstream's `GetClaudeModels`).
    pub fn claude_models(&self) -> Vec<ModelInfo> {
        self.claude.clone()
    }

    /// The Gemini models (upstream's `GetGeminiModels`).
    pub fn gemini_models(&self) -> Vec<ModelInfo> {
        self.gemini.clone()
    }

    /// The Gemini models Vertex AI serves (upstream's
    /// `GetGeminiVertexModels`).
    pub fn vertex_models(&self) -> Vec<ModelInfo> {
        self.vertex.clone()
    }

    /// The xAI models (upstream's `GetXAIModels`, without the built-in image
    /// and video models).
    pub fn xai_models(&self) -> Vec<ModelInfo> {
        self.xai.clone()
    }

    /// The Meta models (upstream's `GetMetaModels`).
    pub fn meta_models(&self) -> Vec<ModelInfo> {
        self.meta.clone()
    }

    /// The Codex models of `plan`, with the image models every plan serves
    /// (upstream's `GetCodexFreeModels` and the like).
    pub fn codex_models(&self, plan: CodexPlan) -> Vec<ModelInfo> {
        let models = match plan {
            CodexPlan::Free => &self.codex_free,
            CodexPlan::Team => &self.codex_team,
            CodexPlan::Plus => &self.codex_plus,
            CodexPlan::Pro => &self.codex_pro,
        };
        with_codex_builtins(models.clone())
    }

    /// The models of a channel such as `claude` or `codex`, ignoring case and
    /// surrounding whitespace; Codex gets the Pro models (upstream's
    /// `GetStaticModelDefinitionsByChannel`). Other channels have none here.
    pub fn models_for_channel(&self, channel: &str) -> Vec<ModelInfo> {
        match go::to_lower(channel.trim()).as_str() {
            "claude" => self.claude_models(),
            "gemini" | "gemini-interactions" => self.gemini_models(),
            "vertex" => self.vertex_models(),
            "codex" => self.codex_models(CodexPlan::Pro),
            _ => Vec::new(),
        }
    }
}

/// `models` without any model of the same ID as a Codex image model, ignoring
/// case, nor any without an ID, followed by the Codex image models (upstream's
/// `WithCodexBuiltins`).
pub fn with_codex_builtins(models: Vec<ModelInfo>) -> Vec<ModelInfo> {
    let builtin_ids: HashSet<String> = CODEX_BUILTINS
        .iter()
        .map(|(id, _)| go::to_lower(id))
        .collect();
    let mut out: Vec<ModelInfo> = models
        .into_iter()
        .filter(|model| {
            let id = model.id.trim();
            !id.is_empty() && !builtin_ids.contains(&go::to_lower(id))
        })
        .collect();
    out.extend(CODEX_BUILTINS.iter().map(|(id, display_name)| ModelInfo {
        id: (*id).to_owned(),
        object: "model".to_owned(),
        created: CODEX_BUILTIN_CREATED,
        owned_by: "openai".to_owned(),
        model_type: "openai".to_owned(),
        display_name: (*display_name).to_owned(),
        version: (*id).to_owned(),
        ..ModelInfo::default()
    }));
    out
}

/// A catalog's sections, in [`SECTIONS`] order. A `None` model is a `null`.
type Sections = [Vec<Option<ModelInfo>>; SECTIONS.len()];

fn decode_sections(root: &Value) -> Result<Sections, String> {
    let mut sections = Sections::default();
    let Some(object) = json::object("catalog", root)? else {
        return Ok(sections);
    };
    for (key, value) in object {
        let Some(index) = SECTIONS
            .iter()
            .position(|section| super::equal_fold(key, section))
        else {
            continue;
        };
        let mut models = Vec::new();
        if let Some(items) = json::array(key, value)? {
            for (position, item) in items.iter().enumerate() {
                let path = format!("{key}[{position}]");
                models.push(match json::object(&path, item)? {
                    Some(object) => Some(decode_model(&path, object)?),
                    None => None,
                });
            }
        }
        if let Some(section) = sections.get_mut(index) {
            *section = models;
        }
    }
    Ok(sections)
}

/// The `models.json` fields of upstream's `ModelInfo`.
const MODEL_FIELDS: [&str; 22] = [
    "id",
    "object",
    "created",
    "owned_by",
    "type",
    "display_name",
    "name",
    "version",
    "description",
    "inputTokenLimit",
    "outputTokenLimit",
    "supportedGenerationMethods",
    "context_length",
    "max_completion_tokens",
    "supported_parameters",
    "supportedInputModalities",
    "supportedOutputModalities",
    "supports_web_search",
    "support_configuration_update",
    "native_capabilities",
    "thinking",
    "config",
];

fn decode_model(path: &str, object: &Map<String, Value>) -> Result<ModelInfo, String> {
    let mut model = ModelInfo::default();
    let mut context_length = 0;
    let mut max_completion_tokens = 0;
    let mut input_token_limit = 0;
    let mut output_token_limit = 0;
    for (key, value) in object {
        let Some(field) = json::field(key, &MODEL_FIELDS) else {
            continue;
        };
        let path = format!("{path}.{key}");
        match field {
            "id" => json::string(&path, value, &mut model.id)?,
            "object" => json::string(&path, value, &mut model.object)?,
            "created" => json::int(&path, value, &mut model.created)?,
            "owned_by" => json::string(&path, value, &mut model.owned_by)?,
            "type" => json::string(&path, value, &mut model.model_type)?,
            "display_name" => json::string(&path, value, &mut model.display_name)?,
            "name" => json::string(&path, value, &mut model.name)?,
            "version" => json::string(&path, value, &mut model.version)?,
            "description" => json::string(&path, value, &mut model.description)?,
            "context_length" => json::int(&path, value, &mut context_length)?,
            "max_completion_tokens" => json::int(&path, value, &mut max_completion_tokens)?,
            "inputTokenLimit" => json::int(&path, value, &mut input_token_limit)?,
            "outputTokenLimit" => json::int(&path, value, &mut output_token_limit)?,
            "supportedGenerationMethods" => {
                json::strings(&path, value, &mut model.supported_generation_methods)?;
            }
            "supported_parameters" => {
                json::strings(&path, value, &mut model.supported_parameters)?;
            }
            "supportedInputModalities" => {
                json::strings(&path, value, &mut model.supported_input_modalities)?;
            }
            "supportedOutputModalities" => {
                json::strings(&path, value, &mut model.supported_output_modalities)?;
            }
            "support_configuration_update" => {
                json::boolean(&path, value, &mut model.support_configuration_update)?;
            }
            "thinking" => model.thinking = decode_thinking(&path, value)?,
            // Checked, then dropped.
            "supports_web_search" => json::boolean(&path, value, &mut false)?,
            "native_capabilities" => check_native_capabilities(&path, value)?,
            "config" => check_config(&path, value)?,
            _ => {}
        }
    }
    // A negative limit means none, as zero does.
    model.context_length = u64::try_from(context_length).unwrap_or(0);
    model.max_completion_tokens = u64::try_from(max_completion_tokens).unwrap_or(0);
    model.input_token_limit = u64::try_from(input_token_limit).unwrap_or(0);
    model.output_token_limit = u64::try_from(output_token_limit).unwrap_or(0);
    Ok(model)
}

fn decode_thinking(path: &str, value: &Value) -> Result<Option<ThinkingSupport>, String> {
    let Some(object) = json::object(path, value)? else {
        return Ok(None);
    };
    let mut thinking = ThinkingSupport::default();
    for (key, value) in object {
        let Some(field) = json::field(
            key,
            &["min", "max", "zero_allowed", "dynamic_allowed", "levels"],
        ) else {
            continue;
        };
        let path = format!("{path}.{key}");
        match field {
            "min" => json::int(&path, value, &mut thinking.min)?,
            "max" => json::int(&path, value, &mut thinking.max)?,
            "zero_allowed" => json::boolean(&path, value, &mut thinking.zero_allowed)?,
            "dynamic_allowed" => json::boolean(&path, value, &mut thinking.dynamic_allowed)?,
            "levels" => json::strings(&path, value, &mut thinking.levels)?,
            _ => {}
        }
    }
    Ok(Some(thinking))
}

/// Checks a model's `native_capabilities`, which isn't ported.
fn check_native_capabilities(path: &str, value: &Value) -> Result<(), String> {
    let Some(object) = json::object(path, value)? else {
        return Ok(());
    };
    for (key, value) in object {
        if json::field(key, &["web_search"]).is_some() {
            json::boolean(&format!("{path}.{key}"), value, &mut false)?;
        }
    }
    Ok(())
}

/// Checks a model's `config`, whose `override_header` is left out by policy.
fn check_config(path: &str, value: &Value) -> Result<(), String> {
    let Some(object) = json::object(path, value)? else {
        return Ok(());
    };
    for (key, value) in object {
        if json::field(key, &["override_header"]).is_none() {
            continue;
        }
        let path = format!("{path}.{key}");
        let Some(headers) = json::object(&path, value)? else {
            continue;
        };
        for (name, value) in headers {
            json::string(&format!("{path}.{name}"), value, &mut String::new())?;
        }
    }
    Ok(())
}

/// Checks each section but `devin` for null models, empty IDs and repeated
/// IDs (upstream's `validateModelsCatalog`). Empty sections pass.
fn validate(sections: &Sections) -> Result<(), String> {
    for (name, models) in SECTIONS.iter().zip(sections) {
        if *name == UNCHECKED_SECTION {
            continue;
        }
        let mut seen = HashSet::with_capacity(models.len());
        for (index, model) in models.iter().enumerate() {
            let Some(model) = model else {
                return Err(format!("{name}[{index}] is null"));
            };
            let id = model.id.trim();
            if id.is_empty() {
                return Err(format!("{name}[{index}] has empty id"));
            }
            if !seen.insert(id) {
                return Err(format!(
                    "{name} contains duplicate model id {}",
                    go::quote(id)
                ));
            }
        }
    }
    Ok(())
}
