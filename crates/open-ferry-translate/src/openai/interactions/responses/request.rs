// Ported from CLIProxyAPI internal/translator/openai/interactions/responses/interactions_openai_responses_request.go
// (ConvertOpenAIResponsesRequestToInteractions, ConvertInteractionsRequestToOpenAIResponses)
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! OpenAI Responses requests to Gemini Interactions requests, and back.
//!
//! Responses to Interactions: `instructions` becomes `system_instruction`,
//! `previous_response_id` becomes `previous_interaction_id`, and `input`
//! becomes Interactions steps. Messages become `user_input` or
//! `model_output` steps, function and custom tool calls become
//! `function_call` steps with namespaced names qualified, and their outputs
//! become `function_result` steps, named after their call when they carry no
//! name of their own. Tools declared at the top level, in `additional_tools`
//! input items and inside namespaces are flattened into functions, one per
//! name (see [`crate::responses_tools`]). The sampling knobs, reasoning and
//! `tool_choice` go into `generation_config`.
//!
//! The `automation_update` tool of the `mcp__codex_app` namespace is left
//! out, and so is a `tool_choice` that names it.
//!
//! Interactions to Responses goes the other way: steps become messages,
//! `reasoning` items, `function_call` and `function_call_output` items, and
//! `generation_config` becomes `tool_choice` and `reasoning`.
//!
//! The helpers marked `pub(super)` are the ones upstream's response
//! translators share.
//!
//! Deviations from upstream:
//! - A model whose name contains `antigravity` is handled like any other.
//!   Upstream renames the tools whose names clash with Antigravity's own,
//!   moves the token limit to `agent_config.max_total_tokens`, drops the
//!   sampling knobs and skips the `automation_update` filter for it.
//! - Tool descriptions are passed on as written. For every model but an
//!   Antigravity one, upstream rewrites the wording of two well-known tool
//!   descriptions (`SanitizeDevinToolDescription`), which only disguises the
//!   client, so it is not ported.
//! - Where upstream copies the client's JSON text into a string, such as an
//!   object given where a name, an id or a text belongs, or a function
//!   call's `arguments` or result that isn't a string, we write the same JSON
//!   compactly.
//! - When an object repeats a key, the last value counts; gjson reads the
//!   first, and gjson's `ForEach` visits every repeat of a key.
//! - A function call's `arguments`, or an output's `output` or `result`,
//!   that is a string holding JSON `serde_json` can't read, nested over 128
//!   levels or with an unpaired surrogate escape, is passed on as a string.
//!   Upstream embeds it as JSON.
//! - A `temperature`, `top_p`, `presence_penalty` or `frequency_penalty`
//!   that isn't a finite number, such as `1e400` or the string `"NaN"`, is
//!   left out. Go writes it as `+Inf` or `NaN`, which isn't JSON.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};

use crate::apply_patch;
use crate::go;
use crate::json::{bool_of, exact, float_of, int_of, object, path, set_path, str_of};
use crate::responses_tools::{
    ToolIdentity, collect_tool_descriptors, collect_tool_winners, qualify_namespace_tool_name,
    tool_description, tool_parameters, unwrap_responses_custom_tool_input,
};

/// `ConvertOpenAIResponsesRequestToInteractions`: an OpenAI Responses request
/// as a Gemini Interactions request.
pub fn convert_openai_responses_request_to_interactions(
    model_name: &str,
    request: &Value,
    stream: bool,
) -> Value {
    let root = request;
    let mut out = object([
        ("model", request_model(model_name, root).into()),
        ("input", Value::Array(Vec::new())),
    ]);
    if let Some(value) = root.get("stream") {
        set_path(&mut out, "stream", bool_of(value).into());
    } else if stream {
        set_path(&mut out, "stream", true.into());
    }
    if let Some(instructions) = root.get("instructions") {
        set_path(
            &mut out,
            "system_instruction",
            responses_instructions_text(instructions).into(),
        );
    }
    let previous = first_non_empty([
        &str_of(root.get("previous_response_id")),
        &str_of(root.get("previous_interaction_id")),
    ])
    .to_owned();
    if !previous.is_empty() {
        set_path(&mut out, "previous_interaction_id", previous.into());
    }
    let environment = first_non_empty([
        &str_of(root.get("environment_id")),
        &str_of(path(root, "environment.id")),
    ])
    .to_owned();
    if !environment.is_empty() {
        set_path(&mut out, "environment_id", environment.into());
    }
    if let Some(agent_config) = root.get("agent_config") {
        set_path(&mut out, "agent_config", agent_config.clone());
    }
    if let Some(input) = root.get("input") {
        set_responses_input_on_interactions(&mut out, input);
    }
    append_responses_tools_to_interactions(&mut out, root);
    if let Some(tool_choice) = root.get("tool_choice")
        && let Some(choice) = responses_tool_choice_to_interactions(tool_choice)
    {
        set_path(&mut out, "generation_config.tool_choice", choice);
    }
    if let Some(Value::String(effort)) = path(root, "reasoning.effort") {
        set_path(
            &mut out,
            "generation_config.thinking_level",
            go::to_lower(effort.trim()).into(),
        );
    }
    if let Some(Value::String(summary)) = path(root, "reasoning.summary") {
        set_path(
            &mut out,
            "generation_config.thinking_summaries",
            summary.clone().into(),
        );
    }
    if let Some(format) = root
        .get("response_format")
        .or_else(|| path(root, "text.format"))
    {
        set_path(&mut out, "response_format", format.clone());
    }
    if let Some(max_output_tokens) = first_existing([
        root.get("max_output_tokens"),
        root.get("max_tokens"),
        root.get("max_completion_tokens"),
    ]) {
        set_path(
            &mut out,
            "generation_config.max_output_tokens",
            int_of(max_output_tokens).into(),
        );
    }
    for knob in [
        "temperature",
        "top_p",
        "presence_penalty",
        "frequency_penalty",
    ] {
        if let Some(value) = root.get(knob).and_then(float_of) {
            set_path(&mut out, &format!("generation_config.{knob}"), value);
        }
    }
    if let Some(stop) = root.get("stop") {
        set_path(&mut out, "generation_config.stop_sequences", stop.clone());
    }
    out
}

/// `ConvertInteractionsRequestToOpenAIResponses`: a Gemini Interactions
/// request as an OpenAI Responses request.
pub fn convert_interactions_request_to_openai_responses(
    model_name: &str,
    request: &Value,
    stream: bool,
) -> Value {
    let root = request;
    let mut out = object([
        ("model", request_model(model_name, root).into()),
        ("input", Value::Array(Vec::new())),
    ]);
    if stream || root.get("stream").is_some_and(bool_of) {
        set_path(&mut out, "stream", true.into());
    }
    let instructions = interactions_system_instruction_text(root);
    if !instructions.is_empty() {
        set_path(&mut out, "instructions", instructions.into());
    }
    let previous = first_non_empty([
        &str_of(root.get("previous_interaction_id")),
        &str_of(root.get("previous_response_id")),
    ])
    .to_owned();
    if !previous.is_empty() {
        set_path(&mut out, "previous_response_id", previous.into());
    }
    let environment = first_non_empty([
        &str_of(root.get("environment_id")),
        &str_of(path(root, "environment.id")),
    ])
    .to_owned();
    if !environment.is_empty() {
        set_path(&mut out, "environment_id", environment.into());
    }
    if let Some(agent_config) = root.get("agent_config") {
        set_path(&mut out, "agent_config", agent_config.clone());
    }
    if let Some(input) = root.get("input") {
        set_interactions_input_on_responses(&mut out, input);
    }
    append_interactions_tools_to_responses(&mut out, root.get("tools"));
    if let Some(tool_choice) =
        path(root, "generation_config.tool_choice").or_else(|| root.get("tool_choice"))
    {
        set_path(&mut out, "tool_choice", tool_choice.clone());
    }
    let effort = interactions_thinking_effort(root);
    if !effort.is_empty() {
        set_path(&mut out, "reasoning.effort", effort.into());
    }
    if let Some(Value::String(summary)) = path(root, "generation_config.thinking_summaries") {
        set_path(&mut out, "reasoning.summary", summary.clone().into());
    }
    if let Some(modalities) = root.get("response_modalities") {
        set_path(&mut out, "modalities", modalities.clone());
    }
    if let Some(Value::String(tier)) = root.get("service_tier") {
        set_path(&mut out, "service_tier", tier.clone().into());
    }
    if let Some(format) = root.get("response_format") {
        set_path(&mut out, "text.format", format.clone());
    }
    out
}

/// `requestModel`: the model asked for, or else the request's `model`.
fn request_model(model_name: &str, root: &Value) -> String {
    if !model_name.trim().is_empty() {
        return model_name.to_owned();
    }
    str_of(root.get("model")).into_owned()
}

/// `responsesInstructionsText`: a string as it is, else its `text`, else
/// the texts of its `content` parts run together, else its JSON.
fn responses_instructions_text(instructions: &Value) -> String {
    if let Value::String(text) = instructions {
        return text.clone();
    }
    if let Some(text) = instructions.get("text") {
        return str_of(Some(text)).into_owned();
    }
    if let Some(Value::Array(parts)) = instructions.get("content") {
        return parts.iter().map(|part| str_of(part.get("text"))).collect();
    }
    str_of(Some(instructions)).into_owned()
}

/// `interactionsSystemInstructionText`: `system_instruction` as a string, its
/// `text`, or the texts of its `parts` run together.
fn interactions_system_instruction_text(root: &Value) -> String {
    let Some(system) = root.get("system_instruction") else {
        return String::new();
    };
    if let Value::String(text) = system {
        return text.clone();
    }
    if let Some(text) = system.get("text") {
        return str_of(Some(text)).into_owned();
    }
    if let Some(Value::Array(parts)) = system.get("parts") {
        return parts.iter().map(|part| str_of(part.get("text"))).collect();
    }
    String::new()
}

/// `interactionsThinkingEffort`: the first thinking level given as a string,
/// lowercased and trimmed.
fn interactions_thinking_effort(root: &Value) -> String {
    [
        "generation_config.thinking_level",
        "generation_config.thinkingConfig.thinkingLevel",
        "generation_config.thinkingConfig.thinking_level",
        "generation_config.thinking_config.thinking_level",
    ]
    .into_iter()
    .find_map(|at| match path(root, at) {
        Some(Value::String(level)) => Some(go::to_lower(level.trim())),
        _ => None,
    })
    .unwrap_or_default()
}

/// The `tool_choice` written to `generation_config`, or `None` when it names
/// the `automation_update` tool. An object's function name is qualified with
/// its namespace.
fn responses_tool_choice_to_interactions(tool_choice: &Value) -> Option<Value> {
    if !tool_choice.is_object() {
        return Some(tool_choice.clone());
    }
    let mut name = first_non_empty([
        &str_of(path(tool_choice, "function.name")),
        &str_of(tool_choice.get("name")),
        &str_of(path(tool_choice, "custom.name")),
    ])
    .to_owned();
    let namespace = first_non_empty([
        &str_of(tool_choice.get("namespace")),
        &str_of(path(tool_choice, "function.namespace")),
        &str_of(path(tool_choice, "custom.namespace")),
    ])
    .to_owned();
    if !namespace.is_empty() && !name.is_empty() {
        name = qualify_namespace_tool_name(&namespace, &name);
    }
    if is_devin_codex_app_automation_update(&namespace, &name)
        || is_devin_codex_app_automation_update("", &name)
    {
        return None;
    }
    let mut choice = tool_choice.clone();
    if !name.is_empty()
        && let Some(at) = ["function.name", "name", "custom.name"]
            .into_iter()
            .find(|at| path(tool_choice, at).is_some())
    {
        set_path(&mut choice, at, name.into());
    }
    Some(choice)
}

/// `IsDevinCodexAppAutomationUpdate`: whether a tool is the
/// `automation_update` tool of the `mcp__codex_app` namespace.
fn is_devin_codex_app_automation_update(namespace: &str, tool: &str) -> bool {
    let namespace = namespace.trim();
    let tool = tool.trim();
    (go::equal_fold(namespace, "mcp__codex_app") && go::equal_fold(tool, "automation_update"))
        || go::equal_fold(tool, "mcp__codex_app__automation_update")
}

/// `setResponsesInputOnInteractions`: `input` as Interactions steps.
fn set_responses_input_on_interactions(out: &mut Value, input: &Value) {
    let mut names_by_call_id = HashMap::new();
    let mut items = Vec::new();
    match input {
        Value::String(text) => items.push(interactions_text_step("user_input", text)),
        Value::Array(list) => {
            items.extend(list.iter().filter_map(|item| {
                responses_input_item_to_interactions(item, &mut names_by_call_id)
            }))
        }
        Value::Object(_) => {
            items.extend(responses_input_item_to_interactions(
                input,
                &mut names_by_call_id,
            ));
        }
        _ => {}
    }
    if !items.is_empty() {
        set_path(out, "input", Value::Array(items));
    }
}

/// `responsesInputItemToInteractions`: one Responses input item as a step.
fn responses_input_item_to_interactions(
    item: &Value,
    names_by_call_id: &mut HashMap<String, String>,
) -> Option<Value> {
    let item_type = str_of(item.get("type"));
    match item_type.as_ref() {
        "message" => {
            let role = str_of(item.get("role"));
            let step_type = if role == "assistant" || role == "model" {
                "model_output"
            } else {
                "user_input"
            };
            let mut step = empty_step(step_type);
            append_responses_content_to_interactions(&mut step, item.get("content"));
            Some(step)
        }
        "function_call" | "custom_tool_call" => {
            let call_id = call_id(item);
            let name = qualified_name(item);
            if !call_id.is_empty() && !name.is_empty() {
                names_by_call_id.insert(call_id, name);
            }
            Some(if item_type == "function_call" {
                responses_function_call_to_interactions(item)
            } else {
                responses_custom_tool_call_to_interactions(item)
            })
        }
        "function_call_output" | "custom_tool_call_output" => Some(
            responses_function_output_to_interactions(item, names_by_call_id),
        ),
        "input_text" | "output_text" | "text" => {
            let step_type = if item_type == "output_text" {
                "model_output"
            } else {
                "user_input"
            };
            Some(interactions_text_step(step_type, &str_of(item.get("text"))))
        }
        "input_image" | "output_image" => {
            let step_type = if item_type == "output_image" {
                "model_output"
            } else {
                "user_input"
            };
            let mut step = empty_step(step_type);
            if let Some(part) = responses_content_part_to_interactions(item) {
                set_path(&mut step, "content", Value::Array(vec![part]));
            }
            Some(step)
        }
        _ => item.get("content").map(|content| {
            let mut step = empty_step("user_input");
            append_responses_content_to_interactions(&mut step, Some(content));
            step
        }),
    }
}

/// A step with no content yet.
fn empty_step(step_type: &str) -> Value {
    object([
        ("type", step_type.into()),
        ("content", Value::Array(Vec::new())),
    ])
}

/// `appendResponsesContentToInteractions`: a message's content as the step's
/// content parts. The step keeps its empty `content` if none convert.
fn append_responses_content_to_interactions(step: &mut Value, content: Option<&Value>) {
    let parts: Vec<Value> = match content {
        Some(Value::String(text)) => vec![text_part(text)],
        Some(Value::Array(list)) => list
            .iter()
            .filter_map(responses_content_part_to_interactions)
            .collect(),
        Some(part @ Value::Object(_)) => responses_content_part_to_interactions(part)
            .into_iter()
            .collect(),
        _ => Vec::new(),
    };
    if !parts.is_empty() {
        set_path(step, "content", Value::Array(parts));
    }
}

/// An Interactions text part.
fn text_part(text: &str) -> Value {
    object([("type", "text".into()), ("text", text.into())])
}

/// `responsesContentPartToInteractions`: a Responses content part as an
/// Interactions content part: text and images, and anything else with a
/// `text` as text.
pub(super) fn responses_content_part_to_interactions(part: &Value) -> Option<Value> {
    match str_of(part.get("type")).as_ref() {
        "input_text" | "output_text" | "text" => return Some(text_part(&str_of(part.get("text")))),
        "input_image" | "output_image" => return Some(responses_image_part_to_interactions(part)),
        _ => {}
    }
    part.get("text").map(|text| text_part(&str_of(Some(text))))
}

/// `responsesImagePartToInteractions`: an image part as inline data when its
/// URL is a data URL or it carries `data`, and else by URL.
fn responses_image_part_to_interactions(part: &Value) -> Value {
    let mut out = object([("type", "image".into())]);
    let (image_url, url) = (str_of(part.get("image_url")), str_of(part.get("url")));
    let image_url = first_non_empty([&image_url, &url]);
    if let Some((mime_type, data)) = parse_data_url(image_url) {
        set_path(&mut out, "mime_type", mime_type.into());
        set_path(&mut out, "data", data.into());
        return out;
    }
    let data = str_of(part.get("data"));
    if !data.is_empty() {
        set_path(&mut out, "data", data.into());
        let mime_type = str_of(part.get("mime_type"));
        if !mime_type.is_empty() {
            set_path(&mut out, "mime_type", mime_type.into());
        }
        return out;
    }
    if !image_url.is_empty() {
        set_path(&mut out, "image_url", image_url.into());
    }
    out
}

/// `parseDataURL`: a `data:` URL's media type, `application/octet-stream` if
/// it names none, and its data.
fn parse_data_url(value: &str) -> Option<(&str, &str)> {
    let (header, data) = value.strip_prefix("data:")?.split_once(',')?;
    let mime_type = header
        .split_once(';')
        .map_or(header, |(mime_type, _)| mime_type);
    if mime_type.is_empty() {
        return Some(("application/octet-stream", data));
    }
    Some((mime_type, data))
}

/// An item's `name`, qualified with its `namespace` when both are given.
fn qualified_name(item: &Value) -> String {
    let name = str_of(item.get("name"));
    let namespace = str_of(item.get("namespace"));
    if !namespace.is_empty() && !name.is_empty() {
        return qualify_namespace_tool_name(&namespace, &name);
    }
    name.into_owned()
}

/// `responsesFunctionCallToInteractions`: a Responses `function_call` item as
/// an Interactions `function_call` step.
pub(super) fn responses_function_call_to_interactions(item: &Value) -> Value {
    let mut out = function_call_step(item);
    set_json_value(&mut out, "arguments", item.get("arguments"), json!({}));
    out
}

/// `responsesCustomToolCallToInteractions`: a `custom_tool_call` item as a
/// `function_call` step, its `input` wrapped as `{"input": ...}`.
fn responses_custom_tool_call_to_interactions(item: &Value) -> Value {
    let mut out = function_call_step(item);
    if let Some(input) = item.get("input") {
        set_path(&mut out, "arguments.input", str_of(Some(input)).into());
    } else {
        set_json_value(&mut out, "arguments", item.get("arguments"), json!({}));
    }
    out
}

/// A `function_call` step with the call's name and id, and empty arguments.
fn function_call_step(item: &Value) -> Value {
    let mut out = object([
        ("type", "function_call".into()),
        ("name", qualified_name(item).into()),
        ("arguments", Value::Object(Map::new())),
    ]);
    let call_id = call_id(item);
    if !call_id.is_empty() {
        set_path(&mut out, "call_id", call_id.into());
    }
    out
}

/// `responsesFunctionOutputToInteractions`: a function or custom tool call
/// output as a `function_result` step, named after its call if it has no name.
fn responses_function_output_to_interactions(
    item: &Value,
    names_by_call_id: &HashMap<String, String>,
) -> Value {
    let mut out = object([
        ("type", "function_result".into()),
        ("name", "".into()),
        ("result", Value::Object(Map::new())),
    ]);
    let call_id = call_id(item);
    let mut name = qualified_name(item);
    if name.is_empty() && !call_id.is_empty() {
        name = names_by_call_id.get(&call_id).cloned().unwrap_or_default();
    }
    if !name.is_empty() {
        set_path(&mut out, "name", name.into());
    }
    if !call_id.is_empty() {
        set_path(&mut out, "call_id", call_id.into());
    }
    let result = item.get("output").or_else(|| item.get("result"));
    set_json_value(&mut out, "result", result, json!({}));
    out
}

/// `interactionsTextStep`: a step holding one text part.
fn interactions_text_step(step_type: &str, text: &str) -> Value {
    object([
        ("type", step_type.into()),
        ("content", Value::Array(vec![text_part(text)])),
    ])
}

/// `appendResponsesToolsToInteractions`: every winning tool declaration as an
/// Interactions function, but the `automation_update` tool. A request that
/// is an array is read as its tools.
fn append_responses_tools_to_interactions(out: &mut Value, root: &Value) {
    let wrapped;
    let root = if root.is_array() {
        wrapped = object([("tools", root.clone())]);
        &wrapped
    } else {
        root
    };
    let descriptors = collect_tool_descriptors(root);
    if descriptors.is_empty() {
        return;
    }
    let winners = collect_tool_winners(root);
    let mut seen = HashSet::new();
    let mut tools = Vec::new();
    for (name, descriptor) in &descriptors {
        if !winners
            .get(name)
            .is_some_and(|winner| winner.order == descriptor.order)
        {
            continue;
        }
        if !seen.insert(name.as_str()) {
            continue;
        }
        if is_devin_codex_app_automation_update(&descriptor.namespace, &descriptor.local_name)
            || is_devin_codex_app_automation_update("", name)
        {
            continue;
        }
        let mut tool = object([("type", "function".into()), ("name", name.as_str().into())]);
        let apply_patch = apply_patch::is_custom_tool(descriptor.tool);
        let description = if apply_patch {
            apply_patch::description(descriptor.tool)
        } else {
            tool_description(descriptor.tool)
        };
        if !description.is_empty() {
            set_path(&mut tool, "description", description.into());
        }
        let parameters = if apply_patch {
            Some(apply_patch::parameters())
        } else if descriptor.custom {
            Some(json!({
                "type": "object",
                "properties": {"input": {"type": "string"}},
                "required": ["input"],
            }))
        } else {
            tool_parameters(descriptor.tool).cloned()
        };
        if let Some(parameters) = parameters {
            set_path(&mut tool, "parameters", parameters);
        }
        tools.push(tool);
    }
    if !tools.is_empty() {
        set_path(out, "tools", Value::Array(tools));
    }
}

/// `setInteractionsInputOnResponses`: `input` as Responses input items.
fn set_interactions_input_on_responses(out: &mut Value, input: &Value) {
    let items: Vec<Value> = match input {
        Value::String(text) => vec![interactions_text_message(text)],
        Value::Array(list) => list
            .iter()
            .filter_map(interactions_input_item_to_responses)
            .collect(),
        Value::Object(_) => interactions_input_item_to_responses(input)
            .into_iter()
            .collect(),
        _ => Vec::new(),
    };
    if !items.is_empty() {
        set_path(out, "input", Value::Array(items));
    }
}

/// `interactionsTextMessage`: a user message holding one text part.
fn interactions_text_message(text: &str) -> Value {
    object([
        ("type", "message".into()),
        ("role", "user".into()),
        ("content", json!([{"type": "input_text", "text": text}])),
    ])
}

/// `interactionsInputItemToResponses`: one Interactions step as a Responses
/// input item.
fn interactions_input_item_to_responses(item: &Value) -> Option<Value> {
    match str_of(item.get("type")).as_ref() {
        "user_input" => Some(interactions_message_to_responses(item, "user")),
        "model_output" => Some(interactions_message_to_responses(item, "assistant")),
        "thought" => Some(interactions_thought_to_responses(item)),
        "function_call" => Some(interactions_function_call_to_responses_with_identity(
            item, None,
        )),
        "function_result" => Some(interactions_function_result_to_responses(item)),
        _ => match item {
            Value::String(text) => Some(interactions_text_message(text)),
            _ => None,
        },
    }
}

/// `interactionsMessageToResponses`: a step as a message from `role`.
fn interactions_message_to_responses(item: &Value, role: &str) -> Value {
    let content = item.get("content");
    let parts: Vec<Value> = if let Some(Value::String(text)) = content {
        let part_type = if role == "assistant" {
            "output_text"
        } else {
            "input_text"
        };
        vec![object([
            ("type", part_type.into()),
            ("text", text.as_str().into()),
        ])]
    } else {
        // gjson's ForEach visits an object's values too. A scalar gives no
        // part, so it isn't visited here.
        let parts: Vec<&Value> = match content {
            Some(Value::Array(list)) => list.iter().collect(),
            Some(Value::Object(fields)) => fields.values().collect(),
            _ => Vec::new(),
        };
        parts
            .into_iter()
            .filter_map(|part| interactions_content_part_to_responses(part, role))
            .collect()
    };
    object([
        ("type", "message".into()),
        ("role", role.into()),
        ("content", Value::Array(parts)),
    ])
}

/// `interactionsThoughtToResponses`: a thought step as a reasoning item with
/// its texts as summary parts.
fn interactions_thought_to_responses(item: &Value) -> Value {
    let summary = interactions_content_texts(item.get("content"))
        .into_iter()
        .map(|text| object([("type", "summary_text".into()), ("text", text.into())]))
        .collect();
    object([
        ("type", "reasoning".into()),
        ("summary", Value::Array(summary)),
    ])
}

/// `interactionsContentPartToResponses`: an Interactions content part as a
/// Responses content part of a message from `role`. Audio becomes a note in
/// text; video and documents become files.
pub(super) fn interactions_content_part_to_responses(part: &Value, role: &str) -> Option<Value> {
    let mut part_type = str_of(part.get("type"));
    if part_type.is_empty() && part.get("text").is_some() {
        part_type = Cow::Borrowed("text");
    }
    let assistant = role == "assistant";
    match part_type.as_ref() {
        "text" => {
            let out_type = if assistant {
                "output_text"
            } else {
                "input_text"
            };
            Some(object([
                ("type", out_type.into()),
                ("text", str_of(part.get("text")).into()),
            ]))
        }
        "image" => {
            let out_type = if assistant {
                "output_image"
            } else {
                "input_image"
            };
            let mut out = object([("type", out_type.into())]);
            let image_url = interactions_media_data_url(part);
            if !image_url.is_empty() {
                set_path(&mut out, "image_url", image_url.into());
            }
            Some(out)
        }
        "audio" => {
            let mime_type = str_of(part.get("mime_type"));
            let text = format!(
                "Audio content: inline data (Format: {})",
                media_format(&mime_type)
            );
            Some(object([
                ("type", "output_text".into()),
                ("text", text.into()),
            ]))
        }
        "video" | "document" => {
            let out_type = if assistant {
                "output_file"
            } else {
                "input_file"
            };
            let mut out = object([("type", out_type.into())]);
            let data_url = interactions_media_data_url(part);
            if !data_url.is_empty() {
                set_path(&mut out, "file_data", data_url.into());
            }
            let filename = str_of(part.get("filename"));
            if !filename.is_empty() {
                set_path(&mut out, "filename", filename.into());
            }
            Some(out)
        }
        _ => None,
    }
}

/// `interactionsFunctionCallToResponsesWithIdentity`: a `function_call` step
/// as a Responses `function_call` item, or a `custom_tool_call` one when
/// `identities` says its name is a custom tool's. A name found in
/// `identities` is replaced by the tool's own name and namespace.
pub(super) fn interactions_function_call_to_responses_with_identity(
    item: &Value,
    identities: Option<&HashMap<String, ToolIdentity>>,
) -> Value {
    let raw_name = str_of(item.get("name"));
    let (name, namespace, custom) =
        match identities.and_then(|identities| identities.get(raw_name.as_ref())) {
            Some(identity) => (
                identity.name.clone(),
                identity.namespace.clone(),
                identity.custom,
            ),
            None => (raw_name.into_owned(), String::new(), false),
        };
    let call_id = call_id(item);
    let arguments = json_string_value(item.get("arguments"), "{}");
    let (mut out, value_key, value) = if custom {
        (
            object([
                ("type", "custom_tool_call".into()),
                ("call_id", "".into()),
                ("name", "".into()),
                ("input", "".into()),
            ]),
            "input",
            unwrap_responses_custom_tool_input(&arguments),
        )
    } else {
        (
            object([
                ("type", "function_call".into()),
                ("call_id", "".into()),
                ("name", "".into()),
                ("arguments", "{}".into()),
            ]),
            "arguments",
            arguments,
        )
    };
    if !call_id.is_empty() {
        set_path(&mut out, "call_id", call_id.into());
    }
    if !namespace.is_empty() {
        set_path(&mut out, "namespace", namespace.into());
    }
    set_path(&mut out, "name", name.into());
    set_path(&mut out, value_key, value.into());
    out
}

/// `interactionsFunctionResultToResponses`: a `function_result` step as a
/// `function_call_output` item.
fn interactions_function_result_to_responses(item: &Value) -> Value {
    let mut out = object([
        ("type", "function_call_output".into()),
        ("call_id", "".into()),
        ("output", "".into()),
    ]);
    let call_id = call_id(item);
    if !call_id.is_empty() {
        set_path(&mut out, "call_id", call_id.into());
    }
    let name = str_of(item.get("name"));
    if !name.is_empty() {
        set_path(&mut out, "name", name.into());
    }
    let result = item.get("result").or_else(|| item.get("output"));
    set_path(&mut out, "output", json_string_value(result, "").into());
    out
}

/// `appendInteractionsToolsToResponses`: each tool, and each of its
/// `function_declarations`, as a Responses function tool.
fn append_interactions_tools_to_responses(out: &mut Value, tools: Option<&Value>) {
    let Some(Value::Array(tools)) = tools else {
        return;
    };
    let mut items = Vec::new();
    for tool in tools {
        items.extend(responses_tool_from_interactions_tool(tool));
        if let Some(Value::Array(declarations)) = tool.get("function_declarations") {
            items.extend(
                declarations
                    .iter()
                    .filter_map(responses_tool_from_interactions_tool),
            );
        }
    }
    if !items.is_empty() {
        set_path(out, "tools", Value::Array(items));
    }
}

/// `responsesToolFromInteractionsTool`: a named tool as a Responses function
/// tool, or `None` if it has no name.
fn responses_tool_from_interactions_tool(tool: &Value) -> Option<Value> {
    let name = first_non_empty([
        &str_of(tool.get("name")),
        &str_of(path(tool, "function.name")),
    ])
    .to_owned();
    if name.is_empty() {
        return None;
    }
    let mut out = object([("type", "function".into()), ("name", name.into())]);
    if let Some(description) =
        first_existing([tool.get("description"), path(tool, "function.description")])
    {
        set_path(&mut out, "description", str_of(Some(description)).into());
    }
    if let Some(parameters) = first_existing([
        tool.get("parameters"),
        path(tool, "function.parameters"),
        tool.get("parametersJsonSchema"),
    ]) {
        set_path(&mut out, "parameters", parameters.clone());
    }
    Some(out)
}

/// `interactionsContentTexts`: a string content as its one text, or the
/// non-empty texts of its parts.
pub(super) fn interactions_content_texts(content: Option<&Value>) -> Vec<String> {
    match content {
        Some(Value::String(text)) => vec![text.clone()],
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                let (own, nested) = (str_of(part.get("text")), str_of(path(part, "content.text")));
                let text = first_non_empty([&own, &nested]);
                (!text.is_empty()).then(|| text.to_owned())
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// `interactionsMediaDataURL`: a part's URL, or its inline data as a data
/// URL.
fn interactions_media_data_url(part: &Value) -> String {
    let urls = ["image_url", "file_data", "url"].map(|at| str_of(part.get(at)));
    let url = first_non_empty([&urls[0], &urls[1], &urls[2]]);
    if !url.is_empty() {
        return url.to_owned();
    }
    let data = str_of(part.get("data"));
    if data.is_empty() {
        return String::new();
    }
    let mime_type = str_of(part.get("mime_type"));
    let mime_type = if mime_type.is_empty() {
        "application/octet-stream"
    } else {
        &mime_type
    };
    format!("data:{mime_type};base64,{data}")
}

/// `mediaFormat`: the subtype of a media type, `unknown` if there is none.
fn media_format(mime_type: &str) -> &str {
    if mime_type.is_empty() {
        return "unknown";
    }
    match mime_type.split_once('/') {
        Some((_, format)) if !format.is_empty() => format,
        _ => mime_type,
    }
}

/// `setJSONValue`: sets `at` to `value`, a string holding JSON as that JSON,
/// or `default` when it is missing.
fn set_json_value(out: &mut Value, at: &str, value: Option<&Value>, default: Value) {
    let value = match value {
        None => default,
        Some(Value::String(text)) => {
            let embedded = if go::gjson_valid(text.as_bytes()) {
                exact::from_str(text).ok()
            } else {
                None
            };
            embedded.unwrap_or_else(|| Value::String(text.clone()))
        }
        Some(other) => other.clone(),
    };
    set_path(out, at, value);
}

/// `jsonStringValue`: a string as it is, other JSON as its text, or
/// `fallback` when it is missing.
pub(super) fn json_string_value(value: Option<&Value>, fallback: &str) -> String {
    match value {
        None => fallback.to_owned(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

/// `firstExisting`: the first value given.
pub(super) fn first_existing<const N: usize>(values: [Option<&Value>; N]) -> Option<&Value> {
    values.into_iter().flatten().next()
}

/// `firstNonEmpty`: the first string that isn't blank, as it is, or `""`.
pub(super) fn first_non_empty<const N: usize>(values: [&str; N]) -> &str {
    values
        .into_iter()
        .find(|value| !value.trim().is_empty())
        .unwrap_or_default()
}

/// [`first_non_empty`] of the gjson `String()` of `item`'s `call_id` and
/// `id`.
fn call_id(item: &Value) -> String {
    let (own, id) = (str_of(item.get("call_id")), str_of(item.get("id")));
    first_non_empty([&own, &id]).to_owned()
}

#[cfg(test)]
mod tests;
