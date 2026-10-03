// Modelled on the parts of CLIProxyAPI internal/registry/model_registry.go
// that the HTTP handlers use (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Which models the configured credentials serve, and through which
//! providers.

use crate::exec::ProviderId;

/// A model a credential serves (upstream's `ModelInfo`, as far as model
/// lists show it).
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
}
