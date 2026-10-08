// Ported from CLIProxyAPI internal/translator/claude/interactions/interactions_claude_request.go
// (ConvertInteractionsRequestToClaude) (v8.0.15, MIT), with the v8.0.20 user turn checks.
// https://github.com/router-for-me/CLIProxyAPI

//! Gemini Interactions request → Claude Messages request.
//!
//! `input` steps become Claude messages: `model_output` and `thought` steps
//! become assistant turns and the rest user turns, and consecutive turns of
//! one role are joined ([`MessageAccumulator`]). `function_call` and
//! `function_result` steps become `tool_use` and `tool_result` blocks, with
//! IDs and names made valid for Claude. Thinking parts are kept only in
//! assistant turns, without a signature. Function declarations become Claude
//! tools, with their schemas normalized for Claude
//! ([`normalize_claude_tool_input_schema`]).
//!
//! A thinking level becomes Claude's thinking: `none` turns it off, `auto`
//! makes it adaptive, a known level gives a token budget, and any other level
//! becomes an adaptive effort.
//!
//! An image or document is sent with its bytes, or else by its `http(s)`
//! URL. A user part Claude can't carry (audio, video, media with neither, or
//! another part with only bytes) is dropped rather than replaced by a note;
//! assistant and tool result content keeps the note. Consecutive user content
//! makes one turn, as Claude joins it, and a turn left with nothing to send
//! is refused with an [`UnsupportedPartError`]. Developer and system steps
//! are sent as user content but close the user's turn rather than join it.
//!
//! Deviations from upstream:
//! - Where upstream copies the client's JSON text into a string (a text,
//!   name, ID, media type or data that is an object or an array, and a
//!   function result that isn't a list, which is stored as JSON text), we
//!   write the same JSON compactly.

use std::borrow::Cow;

use serde_json::{Value, json};

use crate::common::claude::{MessageAccumulator, sanitize_function_name, sanitize_tool_id};
use crate::common::parts::{
    UserRun, interactions_attachment_type, is_http_url, is_interactions_instruction_step,
};
use crate::go;
use crate::json::{bool_of, delete_path, path, set_path, str_of};
use crate::registry::UnsupportedPartError;
use crate::schema::normalize_claude_tool_input_schema;
use crate::thinking::level_to_budget;

/// The generation settings copied as they are, in upstream's order. A later
/// name for the same setting wins.
const COPIED_CONFIG: [(&str, &str); 7] = [
    ("max_output_tokens", "max_tokens"),
    ("maxOutputTokens", "max_tokens"),
    ("top_p", "top_p"),
    ("topP", "top_p"),
    ("temperature", "temperature"),
    ("stop_sequences", "stop_sequences"),
    ("stopSequences", "stop_sequences"),
];

/// Converts an Interactions request body into a Claude Messages request for
/// `model_name`. The request streams if `stream` is set or the request asks
/// to.
///
/// The error is set when a user turn had only parts Claude can't take; the
/// body is still returned.
pub fn convert_interactions_request_to_claude(
    model_name: &str,
    request: &Value,
    stream: bool,
) -> (Value, Option<UnsupportedPartError>) {
    let mut out = json!({"model": model_name, "max_tokens": 32000, "messages": []});
    if stream || request.get("stream").is_some_and(bool_of) {
        out["stream"] = true.into();
    }

    let system = request
        .get("system_instruction")
        .or_else(|| request.get("systemInstruction"));
    let system = claude_text(system);
    if !system.is_empty() {
        out["system"] = system.into();
    }

    let config = request
        .get("generation_config")
        .or_else(|| request.get("generationConfig"));
    if let Some(config) = config {
        for (from, to) in COPIED_CONFIG {
            if let Some(value) = config.get(from) {
                out[to] = value.clone();
            }
        }
        if let Some(level) = first_existing(
            config,
            &["thinking_level", "thinkingLevel", "reasoning.effort"],
        ) {
            set_thinking(&mut out, &str_of(Some(level)));
        }
        apply_tool_choice(&mut out, config.get("tool_choice"));
        apply_tool_choice(&mut out, config.get("toolChoice"));
    }
    if let Some(reasoning) = request.get("reasoning") {
        if let Some(effort) = reasoning.get("effort") {
            set_thinking(&mut out, &str_of(Some(effort)));
        } else if let Some(level) = reasoning.get("thinking_level") {
            set_thinking(&mut out, &str_of(Some(level)));
        }
    }
    apply_tool_choice(&mut out, request.get("tool_choice"));
    apply_tool_choice(&mut out, request.get("toolChoice"));

    let mut messages = Messages::default();
    messages.append_input(request.get("input"));
    messages.run.end();
    let refusal = messages.run.err();
    let messages = messages.claude.into_messages();
    if !messages.is_empty() {
        out["messages"] = Value::Array(messages);
    }

    if let Some(Value::Array(tools)) = request.get("tools") {
        let mut converted = Vec::new();
        for tool in tools {
            let declarations = match tool.get("function_declarations") {
                Some(Value::Array(declarations)) => Some(declarations),
                _ => match tool.get("functionDeclarations") {
                    Some(Value::Array(declarations)) => Some(declarations),
                    _ => None,
                },
            };
            match declarations {
                Some(declarations) => converted.extend(declarations.iter().filter_map(claude_tool)),
                None => converted.extend(claude_tool(tool)),
            }
        }
        if !converted.is_empty() {
            out["tools"] = Value::Array(converted);
        }
    }
    (out, refusal)
}

/// The first of `paths` that `value` has, whatever its value.
fn first_existing<'v>(value: &'v Value, paths: &[&str]) -> Option<&'v Value> {
    paths.iter().find_map(|key| path(value, key))
}

/// Sets Claude's thinking from a thinking level or reasoning effort.
fn set_thinking(out: &mut Value, level: &str) {
    let level = go::to_lower(level.trim());
    match level.as_str() {
        "" => return,
        "none" | "disabled" | "off" | "false" => {
            set_path(out, "thinking.type", "disabled".into());
            delete_path(out, "thinking.budget_tokens");
            return;
        }
        "auto" | "adaptive" => {
            set_path(out, "thinking.type", "adaptive".into());
            delete_path(out, "thinking.budget_tokens");
            return;
        }
        _ => {}
    }
    match level_to_budget(&level) {
        Some(0) => {
            set_path(out, "thinking.type", "disabled".into());
        }
        Some(budget) if budget < 0 => {
            set_path(out, "thinking.type", "enabled".into());
        }
        Some(budget) => {
            set_path(out, "thinking.type", "enabled".into());
            set_path(out, "thinking.budget_tokens", budget.into());
        }
        None => {
            set_path(out, "thinking.type", "adaptive".into());
            set_path(out, "output_config.effort", level.into());
        }
    }
}

/// Sets Claude's `tool_choice` from an Interactions one: `auto`, `any` for
/// `required`, or a named tool. Anything else leaves it as it was.
fn apply_tool_choice(out: &mut Value, choice: Option<&Value>) {
    let kind = match choice {
        Some(Value::String(text)) => go::to_lower(text.trim()),
        Some(choice @ (Value::Object(_) | Value::Array(_))) => {
            let kind = go::to_lower(str_of(choice.get("type")).trim());
            if matches!(kind.as_str(), "function" | "tool") {
                let mut name = str_of(choice.get("name"));
                if name.is_empty() {
                    name = str_of(path(choice, "function.name"));
                }
                if !name.is_empty() {
                    out["tool_choice"] =
                        json!({"type": "tool", "name": sanitize_function_name(&name)});
                }
                return;
            }
            kind
        }
        _ => return,
    };
    match kind.as_str() {
        "auto" => out["tool_choice"] = json!({"type": "auto"}),
        "required" | "any" => out["tool_choice"] = json!({"type": "any"}),
        _ => {}
    }
}

/// The Claude messages being built, and the user turn they're in.
#[derive(Default)]
struct Messages {
    claude: MessageAccumulator,
    /// `interactionsClaudeUserRun`: the consecutive user content Claude
    /// joins into one user message. A part Claude can't carry is refused
    /// only when that whole turn is left with nothing to send, so text or a
    /// tool result in a neighbouring step keeps the turn.
    run: UserRun,
}

impl Messages {
    /// Adds the request's `input`: a string as one user turn, one step, or a
    /// list of steps.
    fn append_input(&mut self, input: Option<&Value>) {
        match input {
            None => {}
            Some(Value::String(text)) => {
                let step =
                    json!({"type": "user_input", "content": [{"type": "text", "text": text}]});
                self.append_step(&step, "user", false);
            }
            Some(Value::Array(items)) => {
                for item in items {
                    self.append_item(item);
                }
            }
            Some(item) => self.append_item(item),
        }
    }

    /// Adds one input item: a turn holding `steps`, a Gemini-style turn
    /// holding `parts`, or a single step.
    fn append_item(&mut self, item: &Value) {
        let model_role = matches!(str_of(item.get("role")).as_ref(), "model" | "assistant");
        let instruction = is_interactions_instruction_step(item, false);
        if let Some(Value::Array(steps)) = item.get("steps") {
            let role = if model_role { "assistant" } else { "user" };
            for step in steps {
                self.append_step(step, role, instruction);
            }
            return;
        }
        if let Some(parts) = item.get("parts") {
            // Upstream wraps the parts in a step without a role, so they
            // always join a user turn.
            let step_type = if model_role {
                "model_output"
            } else {
                "user_input"
            };
            let step = json!({"type": step_type, "content": parts.clone()});
            self.append_step(&step, "user", instruction);
            return;
        }
        match str_of(item.get("type")).as_ref() {
            "function_call" => append_function_call(self, item),
            "function_result" => append_function_result(self, item),
            "model_output" | "thought" => self.append_step(item, "assistant", false),
            _ => self.append_step(item, "user", false),
        }
    }

    /// Adds a step's content (a string or a list of parts, or else its
    /// `text`, or else the step itself if it is a media part) as one message
    /// of the step's own role, if it is `user` or `assistant`, or else of
    /// `default_role`.
    ///
    /// `instruction` says the step sits in a developer or system wrapper.
    /// Developer and system content is sent as user content, but it isn't
    /// the user's own turn: it closes the open user turn and never keeps an
    /// emptied one alive.
    fn append_step(&mut self, step: &Value, default_role: &'static str, instruction: bool) {
        let role = match str_of(step.get("role")).as_ref() {
            "user" => "user",
            "assistant" => "assistant",
            _ => default_role,
        };
        let user_content = role == "user" && !is_interactions_instruction_step(step, instruction);
        let mut blocks = Vec::new();
        match step.get("content") {
            Some(Value::String(content)) => {
                let part = json!({"type": "text", "text": content});
                self.append_part(&mut blocks, &part, role, user_content);
            }
            Some(Value::Array(parts)) => {
                for part in parts {
                    self.append_part(&mut blocks, part, role, user_content);
                }
            }
            _ => {
                if let Some(text) = step.get("text") {
                    let part = json!({"type": "text", "text": str_of(Some(text))});
                    self.append_part(&mut blocks, &part, role, user_content);
                } else if dropped_media(step).is_some() {
                    // A bare media part stands for a step of its own.
                    self.append_part(&mut blocks, step, role, user_content);
                }
            }
        }
        if blocks.is_empty() {
            return;
        }
        if !user_content {
            self.run.end();
        }
        self.claude.push(role, blocks);
    }

    /// Adds one part to `blocks`, counting it in the user's turn when it is
    /// sendable user content, or recording it there as dropped when Claude
    /// can't carry it.
    fn append_part(
        &mut self,
        blocks: &mut Vec<Value>,
        part: &Value,
        role: &str,
        user_content: bool,
    ) {
        let Some(block) = content_to_claude(part, role) else {
            if user_content {
                let dropped = dropped_part(part);
                if !dropped.is_empty() {
                    self.run.drop_part(&dropped);
                }
            }
            return;
        };
        if user_content && is_sendable(&block) {
            self.run.add();
        }
        blocks.push(block);
    }
}

/// One content part as a Claude block. Thinking is kept only for assistant
/// turns. Other parts that only carry bytes become a note that they were
/// left out, except in a user turn (`role` is `tool_result` inside one).
fn content_to_claude(part: &Value, role: &str) -> Option<Value> {
    let mut part_type = str_of(part.get("type"));
    if part_type.is_empty() && part.get("text").is_some() {
        part_type = Cow::Borrowed("text");
    }
    match part_type.as_ref() {
        "text" => Some(json!({"type": "text", "text": str_of(part.get("text"))})),
        "thinking" | "reasoning" => (role == "assistant")
            .then(|| json!({"type": "thinking", "thinking": claude_text(Some(part))})),
        "image" => media_part(part, "image"),
        "document" | "file" => media_part(part, "document"),
        _ => {
            let text = claude_text(Some(part));
            if !text.is_empty() {
                return Some(json!({"type": "text", "text": text}));
            }
            // A user attachment Claude can't carry is never replaced by a
            // note; the caller records it as dropped. Assistant and tool
            // result content only echoes earlier output, so it keeps one.
            (role != "user" && has_data(part))
                .then(|| json!({"type": "text", "text": format!("[{part_type} content omitted]")}))
        }
    }
}

/// Whether a part carries `data` or `file_data`.
fn has_data(part: &Value) -> bool {
    !str_of(part.get("data")).is_empty() || !str_of(part.get("file_data")).is_empty()
}

/// `interactionsClaudeDroppedMedia`: the media type of a part
/// [`content_to_claude`] left out, or `None` when the part isn't media.
fn dropped_media(part: &Value) -> Option<&'static str> {
    match str_of(part.get("type")).as_ref() {
        "image" => Some("image"),
        "audio" => Some("audio"),
        "video" => Some("video"),
        "document" | "file" => Some("document"),
        _ => None,
    }
}

/// `interactionsClaudeDroppedPart`: the type of a user part
/// [`content_to_claude`] left out, media or any other part with bytes, or
/// `""` when it holds nothing to report.
fn dropped_part(part: &Value) -> String {
    match dropped_media(part) {
        Some(media) => media.to_owned(),
        None if has_data(part) => interactions_attachment_type(part),
        None => String::new(),
    }
}

/// Whether a Claude block gives the model something to read. Blank text
/// doesn't.
fn is_sendable(block: &Value) -> bool {
    str_of(block.get("type")) != "text" || !str_of(block.get("text")).trim().is_empty()
}

fn append_function_call(messages: &mut Messages, step: &Value) {
    let mut tool_use = json!({
        "type": "tool_use",
        "id": tool_id(step),
        "name": sanitize_function_name(&str_of(step.get("name"))),
        "input": {},
    });
    if let Some(arguments @ Value::Object(_)) = step.get("arguments").or_else(|| step.get("args")) {
        tool_use["input"] = arguments.clone();
    }
    messages.run.end();
    messages.claude.push("assistant", vec![tool_use]);
}

fn append_function_result(messages: &mut Messages, step: &Value) {
    let mut tool_result =
        json!({"type": "tool_result", "tool_use_id": tool_id(step), "content": ""});
    if step.get("is_error").is_some_and(bool_of) {
        tool_result["is_error"] = true.into();
    }
    match step.get("result").or_else(|| step.get("output")) {
        Some(Value::Array(parts)) => {
            tool_result["content"] = parts
                .iter()
                .filter_map(|part| content_to_claude(part, "tool_result"))
                .collect();
        }
        // Upstream stores any other result as its JSON text.
        Some(result) => tool_result["content"] = result.to_string().into(),
        None => {}
    }
    // A tool result is content the model reads, so it keeps the user's turn.
    messages.run.add();
    messages.claude.push("user", vec![tool_result]);
}

/// A function declaration as a Claude tool, if it has a name.
fn claude_tool(tool: &Value) -> Option<Value> {
    let mut name = str_of(tool.get("name"));
    if name.is_empty() {
        name = str_of(path(tool, "function.name"));
    }
    if name.is_empty() {
        return None;
    }
    let mut converted = json!({
        "name": sanitize_function_name(&name),
        "input_schema": {"type": "object", "properties": {}},
    });
    if let Some(description) = tool
        .get("description")
        .or_else(|| path(tool, "function.description"))
    {
        converted["description"] = str_of(Some(description)).into();
    }
    let parameters = first_existing(
        tool,
        &[
            "parameters",
            "parametersJsonSchema",
            "parameters_json_schema",
            "input_schema",
        ],
    );
    if let Some(parameters @ Value::Object(_)) = parameters {
        converted["input_schema"] = normalize_claude_tool_input_schema(Some(parameters));
    }
    Some(converted)
}

/// A step's call ID made valid for Claude, or one made from its name.
fn tool_id(step: &Value) -> String {
    for key in ["call_id", "id", "tool_use_id"] {
        let id = str_of(step.get(key));
        if !id.is_empty() {
            return sanitize_tool_id(&id);
        }
    }
    let name = str_of(step.get("name"));
    if name.is_empty() {
        "toolu_interactions".to_owned()
    } else {
        sanitize_tool_id(&format!("toolu_{name}"))
    }
}

/// `interactionsClaudeText`: a string as it is, or else a part's `text`,
/// `thinking` or `content`, or its `parts`' texts one per line.
fn claude_text(value: Option<&Value>) -> Cow<'_, str> {
    let Some(value) = value else {
        return Cow::Borrowed("");
    };
    if let Value::String(text) = value {
        return Cow::Borrowed(text);
    }
    if let Some(text) = value.get("text").or_else(|| value.get("thinking")) {
        return str_of(Some(text));
    }
    if let Some(content) = value.get("content") {
        return claude_text(Some(content));
    }
    let Some(Value::Array(parts)) = value.get("parts") else {
        return Cow::Borrowed("");
    };
    let texts: Vec<Cow<'_, str>> = parts
        .iter()
        .map(|part| claude_text(Some(part)))
        .filter(|text| !text.is_empty())
        .collect();
    Cow::Owned(texts.join("\n"))
}

/// An image or document part as a Claude block with base64 data, if it has
/// both a media type and data, or else with a URL source, if it has an
/// `http(s)` URL.
fn media_part(part: &Value, claude_type: &str) -> Option<Value> {
    let mut media_type = str_of(first_existing(
        part,
        &["mime_type", "mimeType", "media_type", "mediaType"],
    ));
    let mut data = str_of(first_existing(part, &["data", "file_data", "fileData"]));
    if let Some(source) = part.get("source") {
        if media_type.is_empty() {
            media_type = str_of(source.get("media_type"));
        }
        if data.is_empty() {
            data = str_of(source.get("data"));
        }
    }
    if !media_type.is_empty() && !data.is_empty() {
        return Some(json!({
            "type": claude_type,
            "source": {"type": "base64", "media_type": media_type, "data": data},
        }));
    }
    ["uri", "file_uri", "fileUri", "url"]
        .into_iter()
        .map(|key| str_of(part.get(key)))
        .find(|uri| is_http_url(uri))
        .map(|uri| json!({"type": claude_type, "source": {"type": "url", "url": uri.trim()}}))
}

#[cfg(test)]
mod tests;
