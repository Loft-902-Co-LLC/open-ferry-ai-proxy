//! Our side of the harness's `codex-models/list` entry: the Codex client
//! model list built from the case's registrations, summarized as the
//! harness summarizes upstream's (see `go/parity_codex_models.go`).

use std::fmt::Write as _;

use open_ferry_core::codex_models::{
    ApplyPatchCapability, ProvidersForModel, build_response, marshal_compact,
};
use open_ferry_core::models::{ModelInfo, ThinkingSupport};
use open_ferry_core::registry::ModelRegistry;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The length past which a string in the summary's body is replaced by its
/// hash (`codexModelsLongString`).
const LONG_STRING: usize = 120;

/// The list for the case's `options`, summarized.
pub fn list(options: &Value) -> Value {
    let registry = ModelRegistry::new();
    for registration in options["registrations"].as_array().into_iter().flatten() {
        let models: Vec<ModelInfo> = registration["models"]
            .as_array()
            .into_iter()
            .flatten()
            .map(model_info)
            .collect();
        registry.register_client(
            text(&registration["client"]),
            text(&registration["provider"]),
            &models,
        );
    }
    let models = registry.available_model_infos();
    let providers = |model: &str| registry.providers_for_model(model);
    let supported: Option<Vec<&str>> = options["apply_patch"]
        .as_array()
        .map(|ids| ids.iter().map(text).collect());
    let apply_patch = |model: &str| supported.as_ref().is_some_and(|ids| ids.contains(&model));
    let response = build_response(
        &registry,
        &models,
        (options["providers"] == true).then_some(&providers as ProvidersForModel<'_>),
        supported
            .is_some()
            .then_some(&apply_patch as ApplyPatchCapability<'_>),
        options["optimize_multi_agent_v2"] == true,
        text(&options["client_version"]),
    );
    summary(&marshal_compact(&response))
}

/// The length, SHA-256 and shortened body of a marshalled list.
fn summary(body: &str) -> Value {
    let mut value: Value = serde_json::from_str(body).expect("the list is JSON");
    shorten(&mut value);
    json!({
        "body": value,
        "bytes": body.len(),
        "sha256": sha256_hex(body.as_bytes()),
    })
}

/// Replaces each long string in `value` by its hash.
fn shorten(value: &mut Value) {
    match value {
        Value::String(text) if text.len() > LONG_STRING => {
            *text = format!("sha256:{}", sha256_hex(text.as_bytes()));
        }
        Value::Array(items) => items.iter_mut().for_each(shorten),
        Value::Object(map) => map.values_mut().for_each(shorten),
        _ => {}
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// A model from the case, with the fields `codexModelsModel` reads.
fn model_info(model: &Value) -> ModelInfo {
    let strings = |key: &str| -> Vec<String> {
        model[key]
            .as_array()
            .into_iter()
            .flatten()
            .map(|item| text(item).to_owned())
            .collect()
    };
    let number = |key: &str| model[key].as_u64().unwrap_or_default();
    let thinking = model["thinking"]
        .as_object()
        .map(|thinking| ThinkingSupport {
            min: thinking
                .get("min")
                .and_then(Value::as_i64)
                .unwrap_or_default(),
            max: thinking
                .get("max")
                .and_then(Value::as_i64)
                .unwrap_or_default(),
            zero_allowed: thinking.get("zero_allowed") == Some(&Value::Bool(true)),
            dynamic_allowed: thinking.get("dynamic_allowed") == Some(&Value::Bool(true)),
            levels: thinking
                .get("levels")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|level| text(level).to_owned())
                .collect(),
        });
    ModelInfo {
        id: text(&model["id"]).to_owned(),
        metadata_model_id: text(&model["metadata_model_id"]).to_owned(),
        object: text(&model["object"]).to_owned(),
        created: model["created"].as_i64().unwrap_or_default(),
        owned_by: text(&model["owned_by"]).to_owned(),
        model_type: text(&model["type"]).to_owned(),
        display_name: text(&model["display_name"]).to_owned(),
        version: text(&model["version"]).to_owned(),
        description: text(&model["description"]).to_owned(),
        context_length: number("context_length"),
        max_context_length: number("max_context_length"),
        max_completion_tokens: number("max_completion_tokens"),
        supported_parameters: strings("supported_parameters"),
        supported_input_modalities: strings("supported_input_modalities"),
        thinking,
        explicit_thinking: model["explicit_thinking"] == true,
        explicit_input_modalities: model["explicit_input_modalities"] == true,
        ..ModelInfo::default()
    }
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}
