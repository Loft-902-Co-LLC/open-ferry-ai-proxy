//! Our side of the harness's `multi-agent/*` entries: a Codex client's
//! multi-agent v2 request readied for other upstreams, its orphan
//! delegation outputs rewritten, and the namespace rename undone in an
//! upstream's event, with the case's headers, settings and registered
//! models (see `go/parity_multi_agent.go`).

use open_ferry_core::codex_models::spawn_agent::spawn_agent_model_list;
use open_ferry_core::models::{ModelInfo, ThinkingSupport};
use open_ferry_core::registry::ModelRegistry;
use open_ferry_translate::codex_client::{header_value, multi_agent_v2, orphan_delegation};
use serde_json::{Value, json};

/// `multi-agent/prepare`: the request with its collaboration tools readied.
pub fn prepare(mut body: Value, options: &Value) -> Value {
    let registry = registry(options);
    multi_agent_v2::prepare_tools(
        &mut body,
        &header(options, "user_agent"),
        options["enabled"] == true,
        || spawn_agent_model_list(&registry),
    );
    body
}

/// `multi-agent/optimize`: the request as the Codex executor sends it to
/// another upstream, and whether its namespace was renamed.
pub fn optimize(mut body: Value, options: &Value) -> Value {
    let registry = registry(options);
    let optimized = multi_agent_v2::optimize(
        &mut body,
        &header(options, "user_agent"),
        options["enabled"] == true,
        || spawn_agent_model_list(&registry),
    );
    json!({ "body": body, "optimized": optimized })
}

/// `multi-agent/input`: the request with its agent messages rewritten for
/// another target format or a compatibility model.
pub fn input(mut body: Value, options: &Value) -> Value {
    multi_agent_v2::rewrite_input(
        &mut body,
        &header(options, "user_agent"),
        options["enabled"] == true,
        options["compat"] == true,
    );
    body
}

/// `multi-agent/orphan`: the request with its orphan delegation outputs
/// turned into user messages.
pub fn orphan(mut body: Value, options: &Value) -> Value {
    orphan_delegation::rewrite(
        &mut body,
        &header(options, "subagent"),
        options["enabled"] == true,
    );
    body
}

/// `multi-agent/restore`: the event's data with the namespace renamed back,
/// as text.
pub fn restore(data: &str, options: &Value) -> Value {
    let restored = multi_agent_v2::restore_response(data.as_bytes(), options["optimized"] == true);
    json!({ "output": String::from_utf8_lossy(&restored) })
}

/// A header from the case's options, read as the server reads it.
fn header(options: &Value, key: &str) -> String {
    header_value(options[key].as_str().map(str::as_bytes))
}

/// A registry holding the case's registrations.
fn registry(options: &Value) -> ModelRegistry {
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
    registry
}

/// A model from the case, with the fields the harness's Go side reads.
fn model_info(model: &Value) -> ModelInfo {
    ModelInfo {
        id: text(&model["id"]).to_owned(),
        display_name: text(&model["display_name"]).to_owned(),
        description: text(&model["description"]).to_owned(),
        thinking: model["thinking"]
            .as_object()
            .map(|thinking| ThinkingSupport {
                levels: thinking
                    .get("levels")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|level| text(level).to_owned())
                    .collect(),
                ..ThinkingSupport::default()
            }),
        ..ModelInfo::default()
    }
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}
