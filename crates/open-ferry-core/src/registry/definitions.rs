// Ported from CLIProxyAPI internal/registry/model_definitions.go and the
// catalog loading, checks and change detection in
// internal/registry/model_updater.go (loadModelsFromBytes,
// validateModelsCatalog, validateModelSection, detectChangedProviders,
// modelSectionChanged) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The static model catalog: upstream's `models.json`, which lists the
//! models each provider serves, and the image models every Codex plan adds.
//!
//! The catalog in use is [`StaticCatalog::current`]: the built-in one, or
//! the last valid one read from the file `models.catalog` names (see
//! [`super::catalog_sources`]), published to [`super::CatalogStore`].
//!
//! The catalog lists Codex models by ChatGPT plan; [`CodexPlan`] picks one.
//!
//! Deviations from upstream:
//! - Only the Claude, Gemini, Vertex, Codex, xAI and Meta sections are
//!   served, and [`StaticCatalog::models_for_channel`] has nothing for the
//!   xAI and Meta channels yet. The others are decoded and checked as
//!   upstream does, so a catalog upstream rejects is rejected here, and are
//!   kept for [`StaticCatalog::lookup`] and to tell which providers changed.
//! - [`StaticCatalog::lookup`] doesn't search upstream's built-in Devin
//!   models, which no ported provider serves.
//! - A model's `config.override_header` is checked, then dropped: it forces a
//!   client's identity headers, which this project doesn't do.
//! - [`StaticCatalog::changed_providers`] compares what is kept of each
//!   model, so a change only to a model's `config.override_header`, or
//!   between a negative limit and zero, changes no provider; upstream
//!   compares the models' JSON. A `null` in the unchecked `devin` section
//!   is dropped, so adding or removing one isn't a change.
//! - Decode errors are worded differently from Go's.
//! - Where a key repeats, exactly or in another case, the last one replaces
//!   the earlier ones; Go's decoder merges repeated objects and lists.
//! - If the built-in catalog doesn't load, it is empty; upstream logs a
//!   warning and later fails on the missing catalog.

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, LazyLock};

use open_ferry_translate::go;
use open_ferry_translate::models::{
    ModelCatalog as TranslatorCatalog, ModelInfo as TranslatorModel, embedded_catalog_json,
};
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

/// The image, video and speech models every xAI credential serves: ID,
/// display name, description and when the model came out (upstream's
/// `xaiBuiltinImageModelInfo` and the like).
const XAI_BUILTINS: [(&str, &str, &str, i64); 8] = [
    (
        "grok-imagine-image",
        "Grok Imagine Image",
        "xAI Grok image generation model.",
        XAI_2025,
    ),
    (
        "grok-imagine-image-quality",
        "Grok Imagine Image Quality",
        "xAI Grok higher-fidelity image generation model.",
        XAI_2025,
    ),
    (
        "grok-imagine-image-2.0",
        "Grok Imagine Image 2.0",
        "xAI Grok image generation model.",
        // 2026-08-07.
        1_786_060_800,
    ),
    (
        "grok-imagine-video",
        "Grok Imagine Video",
        "xAI Grok video generation model.",
        XAI_2025,
    ),
    (
        "grok-imagine-video-1.5",
        "Grok Imagine Video 1.5",
        "xAI Grok video generation model.",
        XAI_2025,
    ),
    (
        "grok-imagine-video-1.5-preview",
        "Grok Imagine Video 1.5 Preview",
        "Compatibility alias for the xAI Grok video generation model.",
        XAI_2025,
    ),
    (
        "grok-tts",
        "Grok TTS",
        "xAI Grok unary text-to-speech model.",
        XAI_SPEECH,
    ),
    (
        "grok-voice-tts-1.0",
        "Grok Voice TTS 1.0",
        "xAI Grok unary text-to-speech model.",
        XAI_SPEECH,
    ),
];

/// When most of the xAI built-in models came out: 2025-01-01.
const XAI_2025: i64 = 1_735_689_600;

/// When the xAI speech models came out: 2026-03-16.
const XAI_SPEECH: i64 = 1_773_619_200;

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

/// Upstream's static catalog: the models of each section of `models.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StaticCatalog {
    claude: Vec<ModelInfo>,
    gemini: Vec<ModelInfo>,
    vertex: Vec<ModelInfo>,
    aistudio: Vec<ModelInfo>,
    codex_free: Vec<ModelInfo>,
    codex_team: Vec<ModelInfo>,
    codex_plus: Vec<ModelInfo>,
    codex_pro: Vec<ModelInfo>,
    kimi: Vec<ModelInfo>,
    antigravity: Vec<ModelInfo>,
    xai: Vec<ModelInfo>,
    devin: Vec<ModelInfo>,
    meta: Vec<ModelInfo>,
}

/// The built-in catalog; empty if it doesn't load.
static EMBEDDED: LazyLock<Arc<StaticCatalog>> = LazyLock::new(|| {
    Arc::new(StaticCatalog::from_json(embedded_catalog_json(), "embed").unwrap_or_default())
});

impl StaticCatalog {
    /// Upstream's `models.json`, as built in.
    pub fn embedded() -> &'static Self {
        &EMBEDDED
    }

    /// The built-in catalog, shared.
    pub(crate) fn embedded_shared() -> Arc<Self> {
        Arc::clone(&EMBEDDED)
    }

    /// The catalog in use (upstream's `getModels`): the built-in one, or the
    /// last valid one published. Take it once per task and
    /// keep it: it is a whole catalog, which a later one replaces without
    /// changing it.
    pub fn current() -> Arc<Self> {
        super::CatalogStore::global().general()
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
        Ok(Self {
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
        })
    }

    /// The sections upstream's `LookupStaticModelInfo` searches before its
    /// built-in Devin models, in its order. It searches `meta` after them.
    fn searched(&self) -> [&[ModelInfo]; 9] {
        [
            &self.claude,
            &self.gemini,
            &self.vertex,
            &self.aistudio,
            &self.codex_pro,
            &self.kimi,
            &self.antigravity,
            &self.xai,
            &self.devin,
        ]
    }

    /// The first model with ID `id`, searching every section the way
    /// upstream's `LookupStaticModelInfo` does.
    pub fn lookup(&self, id: &str) -> Option<ModelInfo> {
        if id.is_empty() {
            return None;
        }
        self.searched()
            .into_iter()
            .chain([self.meta.as_slice()])
            .flatten()
            .find(|model| model.id == id)
            .cloned()
    }

    /// What the translators need of this catalog: each model's thinking
    /// settings, output limit and web search, as `LookupStaticModelInfo`
    /// finds them.
    pub fn translator_catalog(&self) -> TranslatorCatalog {
        TranslatorCatalog::from_models(
            self.searched().into_iter().flatten().map(translator_model),
            self.meta.iter().map(translator_model),
        )
    }

    /// Keeps `previous`'s Meta models if this catalog has none, as
    /// upstream's `publishCatalogBytes` does.
    pub(crate) fn keep_meta_of(&mut self, previous: &Self) {
        if self.meta.is_empty() {
            self.meta.clone_from(&previous.meta);
        }
    }

    /// The providers whose models differ from this catalog's in `new`, each
    /// named once, in upstream's order (upstream's `detectChangedProviders`).
    /// Gemini's section is both `gemini`'s and `gemini-interactions`', the
    /// four Codex plans' are `codex`'s, and Kimi's is `kimi`'s, `kimi-ai`'s,
    /// `kimi.ai`'s and `kimi.com`'s.
    pub fn changed_providers(&self, new: &Self) -> Vec<String> {
        let sections: [(&str, &[ModelInfo], &[ModelInfo]); 17] = [
            ("claude", &self.claude, &new.claude),
            ("gemini", &self.gemini, &new.gemini),
            ("gemini-interactions", &self.gemini, &new.gemini),
            ("vertex", &self.vertex, &new.vertex),
            ("aistudio", &self.aistudio, &new.aistudio),
            ("codex", &self.codex_free, &new.codex_free),
            ("codex", &self.codex_team, &new.codex_team),
            ("codex", &self.codex_plus, &new.codex_plus),
            ("codex", &self.codex_pro, &new.codex_pro),
            ("kimi", &self.kimi, &new.kimi),
            ("kimi-ai", &self.kimi, &new.kimi),
            ("kimi.ai", &self.kimi, &new.kimi),
            ("kimi.com", &self.kimi, &new.kimi),
            ("antigravity", &self.antigravity, &new.antigravity),
            ("xai", &self.xai, &new.xai),
            ("devin", &self.devin, &new.devin),
            ("meta", &self.meta, &new.meta),
        ];
        let mut changed: Vec<String> = Vec::new();
        for (provider, old, new) in sections {
            if old != new && !changed.iter().any(|seen| seen == provider) {
                changed.push(provider.to_owned());
            }
        }
        changed
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

    /// The xAI models, with the image, video and speech models every xAI
    /// credential
    /// serves (upstream's `GetXAIModels`).
    pub fn xai_models(&self) -> Vec<ModelInfo> {
        with_xai_builtins(self.xai.clone())
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

/// A model as the translators see it.
fn translator_model(model: &ModelInfo) -> TranslatorModel {
    TranslatorModel {
        id: model.id.clone(),
        thinking: model.thinking.clone(),
        max_completion_tokens: i64::try_from(model.max_completion_tokens).unwrap_or(i64::MAX),
        native_web_search: model.native_web_search,
        supports_web_search: model.supports_web_search,
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

/// `models` without any model of the same ID as an xAI image, video or
/// speech model, ignoring case, nor any without an ID, followed by the xAI
/// image, video and speech models (upstream's `WithXAIBuiltins`).
pub fn with_xai_builtins(models: Vec<ModelInfo>) -> Vec<ModelInfo> {
    let builtin_ids: HashSet<String> = XAI_BUILTINS
        .iter()
        .map(|(id, ..)| go::to_lower(id))
        .collect();
    let mut out: Vec<ModelInfo> = models
        .into_iter()
        .filter(|model| {
            let id = model.id.trim();
            !id.is_empty() && !builtin_ids.contains(&go::to_lower(id))
        })
        .collect();
    out.extend(
        XAI_BUILTINS
            .iter()
            .map(|(id, display_name, description, created)| ModelInfo {
                id: (*id).to_owned(),
                object: "model".to_owned(),
                created: *created,
                owned_by: "xai".to_owned(),
                model_type: "xai".to_owned(),
                display_name: (*display_name).to_owned(),
                name: (*id).to_owned(),
                description: (*description).to_owned(),
                ..ModelInfo::default()
            }),
    );
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
            "supports_web_search" => {
                json::boolean(&path, value, &mut model.supports_web_search)?;
            }
            "native_capabilities" => {
                model.native_web_search = decode_native_capabilities(&path, value)?;
            }
            // Checked, then dropped.
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

/// A model's `native_capabilities.web_search`: `None` when either is
/// missing or `null`, as Go leaves its pointers.
fn decode_native_capabilities(path: &str, value: &Value) -> Result<Option<bool>, String> {
    let Some(object) = json::object(path, value)? else {
        return Ok(None);
    };
    let mut web_search = None;
    for (key, value) in object {
        if json::field(key, &["web_search"]).is_none() {
            continue;
        }
        web_search = match value {
            Value::Null => None,
            value => {
                let mut flag = false;
                json::boolean(&format!("{path}.{key}"), value, &mut flag)?;
                Some(flag)
            }
        };
    }
    Ok(web_search)
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
/// IDs (upstream's `validateModelsCatalog`). An empty section passes, with
/// a warning.
fn validate(sections: &Sections) -> Result<(), String> {
    for (name, models) in SECTIONS.iter().zip(sections) {
        if *name == UNCHECKED_SECTION {
            continue;
        }
        if models.is_empty() {
            tracing::warn!(
                "models catalog: {name} section is empty, continuing without those model definitions"
            );
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
