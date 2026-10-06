// Ported from CLIProxyAPI internal/api/handlers/management/
// model_definitions.go (GetStaticModelDefinitions), the channel names of
// internal/registry/model_definitions.go (GetStaticModelDefinitionsByChannel)
// and the JSON layout of internal/registry/model_registry.go (ModelInfo)
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! `GET /v0/management/model-definitions/:channel` (also
//! `/v8/management/routing/model-definitions/:channel`): the models a
//! channel serves, from the static model catalog.
//!
//! The channel is the path's, or the `channel` query parameter when the
//! path's is blank. The answer is `{"channel":<channel>,"models":[...]}`
//! with the channel in lower case; a channel the catalog doesn't know is
//! answered with a 400.
//!
//! Deviations from upstream:
//! - Only the channels open-ferry serves are known: `claude`, `gemini`,
//!   `gemini-interactions`, `vertex`, `codex` (with the Pro plan's models),
//!   `xai` (also `x-ai` and `grok`) and `meta` (also `muse`). Upstream also
//!   knows `aistudio`, `kimi` (also `kimi-ai`, `kimi.ai` and `kimi.com`),
//!   `antigravity` and `devin`, which are a 400 here.
//! - `xai` lacks the three image models upstream adds to its list (see
//!   [`StaticCatalog::xai_models`]).
//! - A model's `supports_web_search` and `config` aren't written. No model
//!   of these channels has the first; the second holds the client
//!   headers upstream sends for a model, which open-ferry doesn't send.
//! - A channel whose `%`-escapes don't decode to UTF-8 is named in the
//!   answer as it was sent.

use axum::extract::rejection::PathRejection;
use axum::extract::{Path, RawQuery};
use axum::http::Uri;
use axum::response::Response;
use axum::routing::get;
use http::StatusCode;
use open_ferry_core::models::ModelInfo;
use open_ferry_core::registry::StaticCatalog;
use open_ferry_translate::go::to_lower;

use crate::Route;
use crate::config_read::{Fields, strings, thinking_fields};
use crate::go::lossy;
use crate::json::{self, Json};
use crate::query::Query;

/// The routes this module serves.
pub(crate) fn routes() -> Vec<Route> {
    vec![
        Route::key(
            "/v0/management/model-definitions/{channel}",
            get(definitions),
        ),
        Route::key(
            "/v8/management/routing/model-definitions/{channel}",
            get(definitions),
        ),
    ]
}

/// `GET /v0/management/model-definitions/:channel` (upstream's
/// `GetStaticModelDefinitions`).
async fn definitions(
    channel: Result<Path<String>, PathRejection>,
    uri: Uri,
    RawQuery(raw): RawQuery,
) -> Response {
    let channel = match channel {
        Ok(Path(channel)) => channel,
        Err(_) => uri.path().rsplit('/').next().unwrap_or_default().to_owned(),
    };
    let mut channel = channel.trim().to_owned();
    if channel.is_empty() {
        channel = lossy(Query::parse(raw.as_deref()).value("channel"))
            .trim()
            .to_owned();
    }
    if channel.is_empty() {
        return json::error(StatusCode::BAD_REQUEST, "channel is required");
    }
    let models = channel_models(StaticCatalog::embedded(), &channel);
    if models.is_empty() {
        return json::response(
            StatusCode::BAD_REQUEST,
            &Json::map([
                ("error", Json::Str("unknown channel".into())),
                ("channel", Json::Str(channel)),
            ]),
        );
    }
    json::response(
        StatusCode::OK,
        &Json::map([
            ("channel", Json::Str(to_lower(&channel))),
            ("models", Json::Array(models.iter().map(model).collect())),
        ]),
    )
}

/// The models of a channel, in upstream's spellings of its name (its
/// `GetStaticModelDefinitionsByChannel`). The catalog's own
/// `models_for_channel` has no xAI or Meta channel.
fn channel_models(catalog: &StaticCatalog, channel: &str) -> Vec<ModelInfo> {
    match to_lower(channel).as_str() {
        "xai" | "x-ai" | "grok" => catalog.xai_models(),
        "meta" | "muse" => catalog.meta_models(),
        _ => catalog.models_for_channel(channel),
    }
}

/// A model as Go's encoder writes upstream's `ModelInfo`.
fn model(model: &ModelInfo) -> Json {
    let count = |n: u64| Json::Int(i64::try_from(n).unwrap_or(i64::MAX));
    let string = |s: &str| Json::Str(s.to_owned());
    Fields::new()
        .with("id", string(&model.id))
        .with("object", string(&model.object))
        .with("created", Json::Int(model.created))
        .with("owned_by", string(&model.owned_by))
        .with("type", string(&model.model_type))
        .omit_empty("display_name", string(&model.display_name))
        .omit_empty("name", string(&model.name))
        .omit_empty("version", string(&model.version))
        .omit_empty("description", string(&model.description))
        .omit_empty("inputTokenLimit", count(model.input_token_limit))
        .omit_empty("outputTokenLimit", count(model.output_token_limit))
        .omit_empty(
            "supportedGenerationMethods",
            strings(&model.supported_generation_methods),
        )
        .omit_empty("context_length", count(model.context_length))
        .omit_empty("max_completion_tokens", count(model.max_completion_tokens))
        .omit_empty("supported_parameters", strings(&model.supported_parameters))
        .omit_empty(
            "supportedInputModalities",
            strings(&model.supported_input_modalities),
        )
        .omit_empty(
            "supportedOutputModalities",
            strings(&model.supported_output_modalities),
        )
        .omit_nil(
            "thinking",
            model.thinking.as_ref().map(|thinking| {
                thinking_fields(
                    thinking.min,
                    thinking.max,
                    thinking.zero_allowed,
                    thinking.dynamic_allowed,
                    &thinking.levels,
                )
            }),
        )
        .done()
}
