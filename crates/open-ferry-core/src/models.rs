// Modelled on the parts of CLIProxyAPI internal/registry/model_registry.go
// that the HTTP handlers use (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Which models the configured credentials serve, and through which
//! providers. [`crate::registry`] keeps them.

pub use open_ferry_translate::models::ThinkingSupport;

use crate::exec::ProviderId;

/// A model a credential serves (upstream's `ModelInfo`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelInfo {
    /// The model's ID.
    pub id: String,
    /// Who makes it, such as `openai` or `anthropic`.
    pub owned_by: String,
    /// When it was released, in Unix seconds, or 0 when unknown.
    pub created: i64,
    /// A name for people, or empty.
    pub display_name: String,
    /// The most input tokens it takes, or 0 when unknown.
    pub context_length: u64,
    /// The most output tokens it gives, or 0 when unknown.
    pub max_completion_tokens: u64,
    /// The object type, usually `model`, or empty.
    pub object: String,
    /// The model's family, such as `claude` or `openai` (upstream's `Type`).
    pub model_type: String,
    /// A Gemini-style name such as `models/gemini-2.5-pro`, or empty.
    pub name: String,
    /// The model's version, or empty.
    pub version: String,
    /// A description, or empty.
    pub description: String,
    /// A context window set in the config, or 0.
    pub max_context_length: u64,
    /// The request parameters the model takes.
    pub supported_parameters: Vec<String>,
    /// The kinds of input it takes, such as `TEXT` and `IMAGE`.
    pub supported_input_modalities: Vec<String>,
    /// The kinds of output it gives.
    pub supported_output_modalities: Vec<String>,
    /// The most input tokens it takes, for Gemini model lists, or 0 when
    /// unknown (upstream's `InputTokenLimit`).
    pub input_token_limit: u64,
    /// The most output tokens it gives, for Gemini model lists, or 0 when
    /// unknown (upstream's `OutputTokenLimit`).
    pub output_token_limit: u64,
    /// The Gemini methods it takes, such as `generateContent`
    /// (upstream's `SupportedGenerationMethods`).
    pub supported_generation_methods: Vec<String>,
    /// Its thinking settings, if it thinks.
    pub thinking: Option<ThinkingSupport>,
    /// The thinking settings were set in the config. Internal.
    pub explicit_thinking: bool,
    /// The model whose metadata this one uses, when this one is an alias or
    /// a prefixed name. Internal; model lists don't show it.
    pub metadata_model_id: String,
    /// Defined in the config's model list rather than the static catalog.
    pub user_defined: bool,
    /// Compatibility handling is on for this configured API-key model.
    pub is_compat: bool,
    /// The model takes Codex's `configuration_update`. Internal.
    pub support_configuration_update: bool,
}

/// The models the configured credentials serve (upstream's model registry).
pub trait ModelCatalog: Send + Sync + 'static {
    /// The providers that serve `model`, exactly as named, in order of
    /// preference (upstream's `GetModelProviders`).
    fn model_providers(&self, model: &str) -> Vec<ProviderId>;

    /// The model `auto` stands for, if any model is available.
    fn first_available_model(&self) -> Option<String>;

    /// The models available now, in no particular order.
    fn available_models(&self) -> Vec<ModelInfo>;

    /// `model`'s details as registered under `provider`, or as last
    /// registered at all (upstream's `GetModelInfo`). None by default.
    fn model_info(&self, _model: &str, _provider: &str) -> Option<ModelInfo> {
        None
    }
}
