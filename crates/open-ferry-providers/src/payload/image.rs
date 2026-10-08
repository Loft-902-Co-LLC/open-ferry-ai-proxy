// Ported from CLIProxyAPI internal/runtime/executor/helps/payload_helpers.go
// (isImagesEndpointRequestPath, shouldStripImageGeneration,
// removeToolTypeFromPayloadWithRoot, removeToolChoiceFromPayloadWithRoot,
// removeToolChoiceFromPayload, removeToolTypeFromToolsArray) (v8.0.20,
// MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! `disable-image-generation`: the built-in `image_generation` tool taken
//! out of `tools`, and a `tool_choice` that picks it removed, before the
//! payload rules run, so that a rule can put it back.
//!
//! Deviations from upstream: none.

use std::borrow::Cow;

use open_ferry_core::config::DisableImageGeneration;
use open_ferry_translate::go::equal_fold;
use serde_json::Value;

use super::gjson::{self, Found};
use super::path::build_path;
use super::sjson;

/// The tool `disable-image-generation` takes out.
const IMAGE_GENERATION: &str = "image_generation";

/// Whether `mode` takes the tool out of a request to `request_path`
/// (`shouldStripImageGeneration`): `true` everywhere, `chat` except on the
/// images endpoints.
pub(super) fn should_strip(mode: DisableImageGeneration, request_path: &str) -> bool {
    match mode {
        DisableImageGeneration::All => true,
        DisableImageGeneration::Chat => !is_images_endpoint(request_path),
        DisableImageGeneration::Off | DisableImageGeneration::Passthrough => false,
    }
}

/// Whether `path` is an images endpoint, also under a longer route
/// (`isImagesEndpointRequestPath`).
fn is_images_endpoint(path: &str) -> bool {
    let path = path.trim();
    path.ends_with("/images/generations") || path.ends_with("/images/edits")
}

/// What taking the tool out changes: the `tools` without it, and whether
/// to remove `tool_choice`, with their paths.
#[derive(Debug)]
pub(super) struct Strip {
    tools: Option<(String, Value)>,
    tool_choice: Option<String>,
}

/// What taking the tool out of the `tools` and `tool_choice` under `root`
/// would change, or `None` for nothing.
pub(super) fn plan(body: &Value, root: &str) -> Option<Strip> {
    let tools_path = build_path(root, "tools");
    let tools = filtered_tools(body, &tools_path).map(|tools| (tools_path, tools));
    let choice_path = build_path(root, "tool_choice");
    let tool_choice = picks_tool(body, &choice_path).then_some(choice_path);
    (tools.is_some() || tool_choice.is_some()).then_some(Strip { tools, tool_choice })
}

impl Strip {
    /// Makes the changes, each skipped where its path can't be written.
    pub(super) fn apply(self, body: &mut Value) {
        if let Some((path, tools)) = self.tools {
            let _ = sjson::set(body, &path, &tools);
        }
        if let Some(path) = self.tool_choice {
            let _ = sjson::delete(body, &path);
        }
    }
}

/// Whether `tool` is the image generation tool: an object whose `type` is
/// its name.
fn is_image_tool(tool: &Value) -> bool {
    tool.get("type").and_then(Value::as_str) == Some(IMAGE_GENERATION)
}

/// The array at `path` without the tool, if it has it
/// (`removeToolTypeFromToolsArray`).
fn filtered_tools(body: &Value, path: &str) -> Option<Value> {
    let found = gjson::get(body, path)?;
    let items = found.array_items()?;
    if !items.iter().any(|tool| is_image_tool(tool)) {
        return None;
    }
    Some(Value::Array(
        items
            .into_iter()
            .filter(|tool| !is_image_tool(tool))
            .map(Cow::into_owned)
            .collect(),
    ))
}

/// Whether the `tool_choice` at `path` picks the tool: a string naming it,
/// or an object whose `type` is its name, or is `tool` with its `name`, all
/// in any case (`removeToolChoiceFromPayload`).
fn picks_tool(body: &Value, path: &str) -> bool {
    match gjson::get(body, path) {
        Some(Found::At(Value::String(choice), _)) => equal_fold(choice.trim(), IMAGE_GENERATION),
        Some(Found::At(Value::Object(choice), _)) => {
            let text = |key: &str| {
                choice
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
            };
            let kind = text("type");
            equal_fold(kind, IMAGE_GENERATION)
                || (equal_fold(kind, "tool") && equal_fold(text("name"), IMAGE_GENERATION))
        }
        _ => false,
    }
}
