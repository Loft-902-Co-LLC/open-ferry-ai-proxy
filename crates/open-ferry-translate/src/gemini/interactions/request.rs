// Ported from CLIProxyAPI internal/translator/gemini/interactions/interactions_gemini_common.go
// (ConvertInteractionsRequestToGemini, ConvertGeminiRequestToInteractions and the
// helpers they call, geminiPartToInteractionsSteps, interactionsThoughtSignature,
// interactionsContentPartToGeminiPart, geminiTextPartJSON, geminiInlineDataPartJSON,
// geminiFileDataPartJSON, geminiInlineDataPartFromDataURL,
// interactionsInputAudioMimeType, geminiInlineDataToInteractionsContent and
// geminiThoughtStepJSON; with the user turn refusals, geminiPartFileData,
// geminiFileDataToInteractionsContent and geminiInteractionsMediaType)
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Gemini Interactions request → Gemini request, and Gemini request →
//! Gemini Interactions request.
//!
//! Interactions `input` steps become Gemini `contents`. Model output, thoughts
//! and function calls in a row share one `model` turn. A thought's signature
//! waits for the step it signs (a function call, or the end of the model
//! turn), and is carried on an empty text part when nothing takes it.
//! Function results in a row share one `user` turn. The generation config's
//! snake_case keys become camelCase ([`super::case`]), and the thinking
//! settings move into `thinkingConfig`.
//!
//! The other way, each Gemini part becomes a step of its own, and the
//! generation config's keys become snake_case, with the thinking settings
//! copied to the top-level names Interactions uses. A `fileData` part
//! becomes a media part that names the file by `uri`.
//!
//! Both ways refuse a request whose user turn is left with nothing to send
//! because its only attachment has no equivalent on the other side
//! ([`UnsupportedPartError`]). Text or any other sendable part beside it
//! keeps the turn; blank text doesn't. In Interactions input, consecutive
//! user steps make one turn, and a tool result keeps it; developer and
//! system steps close it without keeping it.

use std::borrow::Cow;

use serde_json::{Map, Value};

use super::case::{camel_to_snake, snake_to_camel};
use crate::common::file_data::normalize_openai_file_data;
use crate::common::gemini::{reorder_gemini_user_parts, set_gemini_function_response_result};
use crate::common::parts::{
    UserRun, UserTurnDrops, gemini_part_is_sendable, interactions_attachment_type,
    is_interactions_instruction_step,
};
use crate::go;
use crate::json::{bool_of, go_marshaled, object, path, set_path, str_of};
use crate::registry::UnsupportedPartError;

/// `ConvertInteractionsRequestToGemini`: a Gemini Interactions request as a
/// Gemini request. `model` replaces the client's model when the client named
/// one. The refusal names the first user turn left with nothing to send
/// because its only attachment has no Gemini equivalent.
pub fn convert_interactions_request_to_gemini(
    model: &str,
    body: &Value,
    _stream: bool,
) -> (Value, Option<UnsupportedPartError>) {
    let mut out = object([
        ("model", Value::String(String::new())),
        ("contents", Value::Array(Vec::new())),
    ]);
    if !model.is_empty() && body.get("model").is_some() {
        set_path(&mut out, "model", Value::String(model.to_owned()));
    }
    copy_system_instruction(&mut out, body);
    copy_generation_config(&mut out, body);
    copy_response_modalities(&mut out, body);
    copy_tools(&mut out, body);
    copy_tool_choice(&mut out, body);
    copy_service_tier(&mut out, body);
    let (contents, err) = input_contents(body.get("input"));
    set_path(&mut out, "contents", Value::Array(contents));
    (out, err)
}

/// `ConvertGeminiRequestToInteractions`: a Gemini request as a Gemini
/// Interactions request for `model`. The refusal names the first user turn
/// left with nothing to send because its only part couldn't be carried over.
pub fn convert_gemini_request_to_interactions(
    model: &str,
    body: &Value,
    stream: bool,
) -> (Value, Option<UnsupportedPartError>) {
    let mut out = object([
        ("model", Value::String(model.to_owned())),
        ("input", Value::Array(Vec::new())),
    ]);
    let system = gemini_system_instruction_text(
        body.get("systemInstruction")
            .or_else(|| body.get("system_instruction")),
    );
    if !system.is_empty() {
        set_path(&mut out, "system_instruction", Value::String(system));
    }
    if let Some(config) = body.get("generationConfig") {
        set_path(&mut out, "generation_config", camel_to_snake(config));
        normalize_gemini_thinking_config(&mut out);
    }
    copy_gemini_tools(&mut out, body);
    let mut drops = UserTurnDrops::default();
    let mut input = Vec::new();
    for content in each(body.get("contents")) {
        let role = str_of(content.get("role"));
        let step_type = if role == "model" {
            "model_output"
        } else {
            "user_input"
        };
        // What this turn sends; empty text isn't sendable.
        let mut sendable = 0;
        for part in each(content.get("parts")) {
            if part.get("functionCall").is_some() || part.get("functionResponse").is_some() {
                let steps = gemini_part_to_steps(part);
                sendable += steps.len();
                input.extend(steps);
                continue;
            }
            if part
                .get("text")
                .is_some_and(|text| str_of(Some(text)).is_empty())
            {
                input.extend(gemini_part_to_steps(part));
                continue;
            }
            let Some(item) = gemini_part_to_content(part) else {
                if role != "model" && gemini_part_file_data(part).is_some() {
                    drops.drop_part("fileData");
                }
                continue;
            };
            let step_type = if part.get("thought").is_some_and(bool_of) && role == "model" {
                "thought"
            } else {
                step_type
            };
            input.push(object([
                ("type", Value::String(step_type.to_owned())),
                ("content", Value::Array(vec![item])),
            ]));
            sendable += 1;
        }
        if role != "model" {
            drops.end_turn(sendable);
        }
    }
    set_path(&mut out, "input", Value::Array(input));
    set_path(&mut out, "stream", Value::Bool(stream));
    (out, drops.err())
}

/// gjson's `ForEach`: an array's items, an object's values, or anything else
/// once, itself. Nothing when `value` is missing.
pub(super) fn each(value: Option<&Value>) -> impl Iterator<Item = &Value> {
    let (items, fields, other) = match value {
        Some(Value::Array(items)) => (Some(items.iter()), None, None),
        Some(Value::Object(fields)) => (None, Some(fields.values()), None),
        other => (None, None, other),
    };
    items
        .into_iter()
        .flatten()
        .chain(fields.into_iter().flatten())
        .chain(other)
}

/// `firstNonEmptyString`: the first value that isn't blank, trimmed.
fn first_trimmed<'v>(values: impl IntoIterator<Item = Cow<'v, str>>) -> String {
    values
        .into_iter()
        .find_map(|value| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        })
        .unwrap_or_default()
}

/// A step's signature: its first non-blank `signature`, `thought_signature`
/// or `thoughtSignature`, trimmed.
fn step_signature(item: &Value) -> String {
    first_trimmed(
        ["signature", "thought_signature", "thoughtSignature"].map(|key| str_of(item.get(key))),
    )
}

/// `copyInteractionsSystemInstruction`.
fn copy_system_instruction(out: &mut Value, root: &Value) {
    let Some(system) = root.get("system_instruction") else {
        return;
    };
    let instruction = match system {
        Value::String(text) => text_instruction(text.clone()),
        _ => match system.get("text") {
            Some(text) if system.get("parts").is_none() => {
                text_instruction(str_of(Some(text)).into_owned())
            }
            _ => system.clone(),
        },
    };
    set_path(out, "systemInstruction", instruction);
}

/// A Gemini system instruction holding `text`.
fn text_instruction(text: String) -> Value {
    object([(
        "parts",
        Value::Array(vec![object([("text", Value::String(text))])]),
    )])
}

/// `copyInteractionsGenerationConfig`: `generation_config` with camelCase
/// keys, or else `generationConfig` as it is, then
/// `normalizeInteractionsGenerationConfig`.
fn copy_generation_config(out: &mut Value, root: &Value) {
    let config = if let Some(config) = root.get("generation_config") {
        snake_to_camel(config)
    } else if let Some(config) = root.get("generationConfig") {
        config.clone()
    } else {
        return;
    };
    set_path(out, "generationConfig", config);

    if path(out, "generationConfig.toolChoice").is_some() {
        delete(out, "generationConfig.toolChoice");
    }
    for key in ["thinkingLevel", "thinkingBudget", "includeThoughts"] {
        let from = format!("generationConfig.{key}");
        if let Some(value) = path(out, &from).cloned() {
            set_path(
                out,
                &format!("generationConfig.thinkingConfig.{key}"),
                value,
            );
            delete(out, &from);
        }
    }
    if let Some(summaries) = path(out, "generationConfig.thinkingSummaries") {
        if let Some(include) = thinking_summaries_include_thoughts(summaries) {
            set_path(
                out,
                "generationConfig.thinkingConfig.includeThoughts",
                Value::Bool(include),
            );
        }
        delete(out, "generationConfig.thinkingSummaries");
    }
}

/// sjson `Delete` for a dotted path of object keys.
fn delete(out: &mut Value, path: &str) {
    crate::json::delete_path(out, path);
}

/// `interactionsThinkingSummariesIncludeThoughts`: whether `"auto"` or
/// `"none"` asks for thoughts; `None` for anything else.
fn thinking_summaries_include_thoughts(summary: &Value) -> Option<bool> {
    let Value::String(summary) = summary else {
        return None;
    };
    match go::to_lower(summary.trim()).as_str() {
        "auto" => Some(true),
        "none" => Some(false),
        _ => None,
    }
}

/// `copyInteractionsResponseModalities`.
fn copy_response_modalities(out: &mut Value, root: &Value) {
    let modalities = root
        .get("response_modalities")
        .or_else(|| root.get("responseModalities"));
    let Some(Value::Array(modalities)) = modalities else {
        return;
    };
    let modalities: Vec<Value> = modalities
        .iter()
        .filter_map(
            |modality| match go::to_lower(str_of(Some(modality)).trim()).as_str() {
                "text" => Some("TEXT"),
                "image" => Some("IMAGE"),
                "audio" => Some("AUDIO"),
                _ => None,
            },
        )
        .map(|modality| Value::String(modality.to_owned()))
        .collect();
    if !modalities.is_empty() {
        set_path(
            out,
            "generationConfig.responseModalities",
            Value::Array(modalities),
        );
    }
}

/// `copyInteractionsToolChoice`.
fn copy_tool_choice(out: &mut Value, root: &Value) {
    let Some(choice) = root
        .get("tool_choice")
        .or_else(|| path(root, "generation_config.tool_choice"))
        .or_else(|| path(root, "generationConfig.toolChoice"))
    else {
        return;
    };
    let mut allowed = None;
    let mode = match choice {
        Value::String(choice) => match go::to_lower(choice.trim()).as_str() {
            "none" => "NONE",
            "auto" => "AUTO",
            "required" | "any" => "ANY",
            _ => return,
        },
        Value::Object(_) => match go::to_lower(str_of(choice.get("type")).trim()).as_str() {
            "none" => "NONE",
            "auto" => "AUTO",
            "required" | "any" => "ANY",
            kind @ ("function" | "tool") => {
                let name = if kind == "function" {
                    str_of(path(choice, "function.name"))
                } else {
                    str_of(choice.get("name"))
                };
                let name = name.trim();
                if !name.is_empty() {
                    allowed = Some(name.to_owned());
                }
                "ANY"
            }
            _ => return,
        },
        _ => return,
    };
    set_path(
        out,
        "toolConfig.functionCallingConfig.mode",
        Value::String(mode.to_owned()),
    );
    if let Some(name) = allowed {
        set_path(
            out,
            "toolConfig.functionCallingConfig.allowedFunctionNames",
            Value::Array(vec![Value::String(name)]),
        );
    }
}

/// `copyInteractionsServiceTier`: a string tier only.
fn copy_service_tier(out: &mut Value, root: &Value) {
    if let Some(Value::String(tier)) = root.get("service_tier") {
        set_path(out, "service_tier", Value::String(tier.clone()));
    }
}

/// `copyInteractionsTools`: built-in tools by `type`, function tools as
/// function declarations. Tools that already hold `functionDeclarations`
/// pass through as they are.
fn copy_tools(out: &mut Value, root: &Value) {
    let Some(tools) = root.get("tools") else {
        return;
    };
    let Value::Array(list) = tools else {
        set_path(out, "tools", tools.clone());
        return;
    };
    let mut normalized = Vec::new();
    for tool in list {
        if tool.get("functionDeclarations").is_some() {
            set_path(out, "tools", tools.clone());
            return;
        }
        let kind = str_of(tool.get("type"));
        let entry = match kind.as_ref() {
            "url_context" => Some(object([(
                "urlContext",
                object_or_empty(tool, "url_context", "urlContext"),
            )])),
            "code_execution" => Some(object([(
                "codeExecution",
                object_or_empty(tool, "code_execution", "codeExecution"),
            )])),
            "google_search" | "web_search" => Some(object([(
                "googleSearch",
                object_or_empty(tool, "google_search", "googleSearch"),
            )])),
            _ => {
                if let Some(declarations @ Value::Array(_)) = tool.get("function_declarations") {
                    Some(object([("functionDeclarations", declarations.clone())]))
                } else if let Some(name) = tool.get("name") {
                    // json.Marshal writes a map's keys sorted.
                    let mut declaration = Vec::new();
                    if let Some(description) = tool.get("description") {
                        declaration.push((
                            "description",
                            Value::String(str_of(Some(description)).into_owned()),
                        ));
                    }
                    declaration.push(("name", Value::String(str_of(Some(name)).into_owned())));
                    if let Some(parameters) = tool.get("parameters") {
                        declaration.push(("parameters", parameters.clone()));
                    }
                    Some(object([(
                        "functionDeclarations",
                        Value::Array(vec![fields(declaration)]),
                    )]))
                } else {
                    generic_tool(tool, kind.is_empty())
                }
            }
        };
        normalized.extend(entry);
    }
    let tools = if normalized.is_empty() {
        tools.clone()
    } else {
        Value::Array(normalized)
    };
    set_path(out, "tools", tools);
}

/// The first of `tool`'s `first` and `second` that is an object, or `{}`.
fn object_or_empty(tool: &Value, first: &str, second: &str) -> Value {
    [first, second]
        .into_iter()
        .find_map(|key| tool.get(key).filter(|value| value.is_object()))
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()))
}

/// A tool upstream decodes into a Go map and writes back: keys sorted,
/// numbers as `float64`. With no `type`, the built-in tools' snake_case keys
/// become camelCase. `None` where upstream's decoding fails or gives no map:
/// a tool that isn't an object, or holds a number too large for `float64`.
fn generic_tool(tool: &Value, rename: bool) -> Option<Value> {
    let Value::Object(fields) = tool else {
        return None;
    };
    if has_infinite_number(tool) {
        return None;
    }
    let mut fields = fields.clone();
    if rename {
        for (from, to) in [
            ("url_context", "urlContext"),
            ("code_execution", "codeExecution"),
            ("google_search", "googleSearch"),
            ("web_search", "googleSearch"),
        ] {
            if let Some(value) = fields.shift_remove(from) {
                fields.insert(to.to_owned(), value);
            }
        }
    }
    Some(go_marshaled(&Value::Object(fields)))
}

/// Whether `value` holds a number that is infinite as an `f64`, which Go's
/// `json.Unmarshal` refuses.
fn has_infinite_number(value: &Value) -> bool {
    match value {
        Value::Number(number) => !number.to_string().parse::<f64>().is_ok_and(f64::is_finite),
        Value::Array(items) => items.iter().any(has_infinite_number),
        Value::Object(fields) => fields.values().any(has_infinite_number),
        _ => false,
    }
}

/// An object of `entries`, with its keys sorted as `json.Marshal` writes a
/// Go map's. The values keep their own order, as a `json.RawMessage` does.
fn fields(mut entries: Vec<(&str, Value)>) -> Value {
    entries.sort_by(|a, b| a.0.cmp(b.0));
    Value::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

/// Builds the Gemini `contents` from Interactions steps
/// (`geminiInteractionsInputContext`).
#[derive(Default)]
struct Input {
    items: Vec<Value>,
    in_model_turn: bool,
    last_step_type: &'static str,
    pending_signature: String,
    /// Follows the consecutive user content, so that an attachment Gemini
    /// can't carry is refused only when its whole user turn is left with
    /// nothing to send.
    run: UserRun,
    /// Set while the step being added is developer or system content.
    instruction: bool,
}

/// `appendInteractionsInput`: the Gemini contents for an Interactions
/// `input`: text, a list of steps, a turn holding `steps`, or one step; and
/// the refusal for the first user turn it emptied.
fn input_contents(input: Option<&Value>) -> (Vec<Value>, Option<UnsupportedPartError>) {
    let Some(input) = input else {
        return (Vec::new(), None);
    };
    let mut ctx = Input::default();
    match input {
        Value::String(text) => return (vec![text_content("user", text)], None),
        Value::Array(items) => {
            for item in items {
                ctx.step(item, "user");
            }
        }
        _ => match input.get("steps") {
            Some(Value::Array(steps)) => {
                let role = match str_of(input.get("role")).as_ref() {
                    "model" | "assistant" => "model",
                    _ => "user",
                };
                ctx.instruction = is_interactions_instruction_step(input, false);
                for step in steps {
                    ctx.step(step, role);
                }
                ctx.instruction = false;
            }
            _ => ctx.step(input, "user"),
        },
    }
    ctx.flush_pending_signature();
    ctx.run.end();
    let err = ctx.run.err();
    (ctx.items, err)
}

impl Input {
    /// `appendInteractionsStepToGemini`. A step that names no role or type
    /// is instruction content if the step around it is.
    fn step(&mut self, item: &Value, default_role: &'static str) {
        let inherited = self.instruction;
        self.instruction = is_interactions_instruction_step(item, inherited);
        self.add_step(item, default_role);
        self.instruction = inherited;
    }

    /// [`Self::step`] without the instruction bookkeeping.
    fn add_step(&mut self, item: &Value, default_role: &'static str) {
        if let Value::String(text) = item {
            self.leave_model_turn();
            self.append_text(default_role, text);
            self.last_step_type = "text";
            return;
        }
        if let Some(Value::Array(steps)) = item.get("steps") {
            let role = match str_of(item.get("role")).as_ref() {
                "model" | "assistant" => "model",
                "user" => "user",
                _ => default_role,
            };
            for child in steps {
                self.step(child, role);
            }
            return;
        }
        match str_of(item.get("type")).as_ref() {
            "model_output" => {
                self.run.end();
                if !self.pending_signature.is_empty() {
                    let carrier = signature_carrier(std::mem::take(&mut self.pending_signature));
                    self.append_to_model_turn(vec![carrier]);
                }
                let parts = parts_of(item.get("content").or_else(|| item.get("text")), false);
                if !parts.is_empty() {
                    self.append_to_model_turn(parts);
                }
                self.in_model_turn = true;
                self.last_step_type = "model_output";
            }
            "thought" => {
                self.run.end();
                let signature = step_signature(item);
                if !signature.is_empty() {
                    if !self.pending_signature.is_empty() && self.pending_signature != signature {
                        let carrier = signature_carrier(self.pending_signature.clone());
                        self.append_to_model_turn(vec![carrier]);
                    }
                    self.pending_signature = signature;
                }
                let content = item
                    .get("content")
                    .or_else(|| item.get("summary"))
                    .or_else(|| item.get("text"));
                let parts = parts_of(content, true);
                if !parts.is_empty() {
                    self.append_to_model_turn(parts);
                }
                self.in_model_turn = true;
                self.last_step_type = "thought";
            }
            "function_call" => {
                self.run.end();
                let mut part = function_call_part(item);
                let mut signature = step_signature(item);
                if signature.is_empty() {
                    signature = std::mem::take(&mut self.pending_signature);
                } else if !self.pending_signature.is_empty() {
                    let pending = std::mem::take(&mut self.pending_signature);
                    if pending != signature {
                        self.append_to_model_turn(vec![signature_carrier(pending)]);
                    }
                }
                if !signature.is_empty() {
                    set_path(&mut part, "thoughtSignature", Value::String(signature));
                }
                self.append_to_model_turn(vec![part]);
                self.in_model_turn = true;
                self.last_step_type = "function_call";
            }
            "function_result" => {
                self.leave_model_turn();
                let part = function_result_part(item);
                // A tool result is content the model reads, so it keeps the
                // user turn around it.
                self.run.add();
                match self.items.last_mut() {
                    Some(last)
                        if self.last_step_type == "function_result" && role_is(last, "user") =>
                    {
                        if let Some(Value::Array(parts)) = last.get_mut("parts") {
                            let mut merged = std::mem::take(parts);
                            merged.push(part);
                            *parts = reorder_gemini_user_parts(merged);
                        }
                    }
                    _ => self.items.push(content("user", vec![part])),
                }
                self.last_step_type = "function_result";
            }
            "user_input" | "" => {
                self.leave_model_turn();
                if item.get("parts").is_some() {
                    self.native_content(item, default_role);
                } else {
                    self.content_list(default_role, item.get("content"));
                }
                self.last_step_type = "user_input";
            }
            _ => {
                self.leave_model_turn();
                if item.get("parts").is_some() {
                    self.native_content(item, default_role);
                } else if let Some(list) = item.get("content") {
                    self.content_list(default_role, Some(list));
                } else if let Some(text) = item.get("text") {
                    self.append_text(default_role, &str_of(Some(text)));
                }
                self.last_step_type = "default";
            }
        }
    }

    /// `userRun`: whether content added with `role` is the user's, which
    /// [`Self::run`] follows. Model content and developer or system content
    /// close the open user turn instead, so they can never keep an emptied
    /// one alive.
    fn tracks(&mut self, role: &str) -> bool {
        if role == "user" && !self.instruction {
            return true;
        }
        self.run.end();
        false
    }

    /// `appendText`: a content holding one text part. Blank text doesn't
    /// keep a user turn alive.
    fn append_text(&mut self, role: &str, text: &str) {
        self.items.push(text_content(role, text));
        if self.tracks(role) && !text.trim().is_empty() {
            self.run.add();
        }
    }

    /// Ends a model turn, leaving a pending signature on it.
    fn leave_model_turn(&mut self) {
        if self.in_model_turn {
            self.flush_pending_signature();
            self.in_model_turn = false;
        }
    }

    /// Adds `parts` to the model turn in progress, or starts one.
    fn append_to_model_turn(&mut self, parts: Vec<Value>) {
        if self.in_model_turn
            && let Some(last) = self.items.last_mut().filter(|last| role_is(last, "model"))
        {
            if let Some(Value::Array(existing)) = last.get_mut("parts") {
                existing.extend(parts);
            }
            return;
        }
        self.items.push(content("model", parts));
    }

    /// `flushPendingGeminiSignature`: carries a pending signature on an empty
    /// text part at the end of the last model turn, or a new one.
    fn flush_pending_signature(&mut self) {
        if self.pending_signature.is_empty() {
            return;
        }
        let carrier = signature_carrier(std::mem::take(&mut self.pending_signature));
        match self.items.last_mut().filter(|last| role_is(last, "model")) {
            Some(last) => {
                if let Some(Value::Array(parts)) = last.get_mut("parts") {
                    parts.push(carrier);
                }
            }
            None => self.items.push(content("model", vec![carrier])),
        }
    }

    /// `appendInteractionsNativeContent`: a step holding Gemini `parts`.
    fn native_content(&mut self, item: &Value, default_role: &str) {
        let Some(Value::Array(parts)) = item.get("parts") else {
            return;
        };
        let role = content_role(&str_of(item.get("role")), default_role);
        let tracked = self.tracks(role);
        let mut kept = Vec::new();
        for part in parts {
            match native_part(part) {
                Some(part) => {
                    if tracked && gemini_part_is_sendable(&part) {
                        self.run.add();
                    }
                    kept.push(part);
                }
                None => self.drop_part(part, tracked),
            }
        }
        if kept.is_empty() {
            return;
        }
        self.items.push(content(role, kept));
    }

    /// `appendInteractionsContentList`: a content of its own for each part.
    fn content_list(&mut self, role: &str, list: Option<&Value>) {
        let Some(list) = list else {
            return;
        };
        let tracked = self.tracks(role);
        match list {
            Value::Array(list) => {
                for part in list {
                    self.add_content_part(role, part, tracked);
                }
            }
            Value::Object(_) => self.add_content_part(role, list, tracked),
            Value::String(text) => self.append_text(role, text),
            _ => {}
        }
    }

    /// `appendInteractionsContentPart`: a content holding one part.
    /// `tracked` is whether it's user content, whose sendable parts and
    /// dropped attachments [`Self::run`] records.
    fn add_content_part(&mut self, role: &str, part: &Value, tracked: bool) {
        let Some(converted) = content_part(part, false) else {
            self.drop_part(part, tracked);
            return;
        };
        let sendable = gemini_part_is_sendable(&converted);
        self.items.push(content(role, vec![converted]));
        if tracked && sendable {
            self.run.add();
        }
    }

    /// Records a user part Gemini can't take, if it's an attachment.
    fn drop_part(&mut self, part: &Value, tracked: bool) {
        let dropped = interactions_attachment_type(part);
        if tracked && !dropped.is_empty() {
            self.run.drop_part(&dropped);
        }
    }
}

/// Whether a content's `role` is `role`.
fn role_is(content: &Value, role: &str) -> bool {
    content.get("role").and_then(Value::as_str) == Some(role)
}

/// `interactionsGeminiContent`.
fn content(role: &str, parts: Vec<Value>) -> Value {
    object([
        ("role", Value::String(role.to_owned())),
        ("parts", Value::Array(parts)),
    ])
}

/// `appendGeminiTextContent`: a content holding one text part.
fn text_content(role: &str, text: &str) -> Value {
    content(role, vec![text_part(text, false)])
}

/// An empty text part carrying a thought signature.
fn signature_carrier(signature: String) -> Value {
    object([
        ("text", Value::String(String::new())),
        ("thoughtSignature", Value::String(signature)),
    ])
}

/// `interactionsGeminiContentRole`.
fn content_role(role: &str, default_role: &str) -> &'static str {
    match go::to_lower(role.trim()).as_str() {
        "model" | "assistant" => "model",
        "user" => "user",
        _ if default_role == "model" => "model",
        _ => "user",
    }
}

/// `interactionsNativeGeminiPart`: a text, function call or function
/// response part as it is, and inline or file data rebuilt.
fn native_part(part: &Value) -> Option<Value> {
    if ["text", "functionCall", "functionResponse"]
        .iter()
        .any(|key| part.get(key).is_some())
    {
        return Some(part.clone());
    }
    if let Some(inline) = part.get("inlineData") {
        return inline_data_part(inline);
    }
    if let Some(file) = part.get("fileData") {
        return file_data_part(file);
    }
    if let Some(inline) = part.get("inline_data") {
        return inline_data_part(inline);
    }
    file_data_part(part.get("file_data")?)
}

/// The Gemini parts for an Interactions step's content: a string, one part
/// or a list of them (`extractInteractionsStepContentPartsToGemini`,
/// `interactionsContentToGeminiParts`).
pub(super) fn parts_of(content: Option<&Value>, thought: bool) -> Vec<Value> {
    match content {
        Some(Value::Array(list)) => list
            .iter()
            .filter_map(|part| content_part(part, thought))
            .collect(),
        Some(part @ Value::Object(_)) => content_part(part, thought).into_iter().collect(),
        Some(Value::String(text)) => vec![text_part(text, thought)],
        _ => Vec::new(),
    }
}

/// `buildGeminiFunctionCallPart`.
fn function_call_part(item: &Value) -> Value {
    let mut call = object([
        ("name", Value::String(str_of(item.get("name")).into_owned())),
        ("args", Value::Object(Map::new())),
    ]);
    if let Some(id) = item.get("call_id").or_else(|| item.get("id")) {
        set_path(
            &mut call,
            "id",
            Value::String(str_of(Some(id)).into_owned()),
        );
    }
    if let Some(arguments) = item.get("arguments") {
        set_path(&mut call, "args", arguments.clone());
    }
    object([("functionCall", call)])
}

/// `buildGeminiFunctionResultPart`.
fn function_result_part(item: &Value) -> Value {
    let mut response = object([
        ("name", Value::String(str_of(item.get("name")).into_owned())),
        ("response", Value::Object(Map::new())),
    ]);
    if let Some(id) = item.get("call_id").or_else(|| item.get("id")) {
        set_path(
            &mut response,
            "id",
            Value::String(str_of(Some(id)).into_owned()),
        );
    }
    let mut part = object([("functionResponse", response)]);
    if let Some(result) = item.get("result") {
        set_gemini_function_response_result(
            &mut part,
            "functionResponse.response",
            Some(result.clone()),
        );
    }
    part
}

/// `interactionsContentPartToGeminiPart`: an Interactions content part (or a
/// Chat Completions one: `image_url`, `input_audio`, `file`) as a Gemini
/// part. `None` if it has nothing Gemini takes.
pub(super) fn content_part(part: &Value, thought: bool) -> Option<Value> {
    if let Some(text) = part.get("text") {
        return Some(text_part(&str_of(Some(text)), thought));
    }
    if let Some(inline) = part.get("inline_data").or_else(|| part.get("inlineData")) {
        return inline_data_part(inline);
    }
    match go::to_lower(str_of(part.get("type")).trim()).as_str() {
        "image" | "audio" | "video" | "document" => {
            let mime_type = || {
                let mime_type = str_of(part.get("mime_type"));
                if mime_type.is_empty() {
                    str_of(part.get("mimeType"))
                } else {
                    mime_type
                }
            };
            if part.get("mime_type").is_some() || part.get("mimeType").is_some() {
                let data = str_of(part.get("data"));
                if !data.is_empty() {
                    return inline_data(&mime_type(), &data);
                }
            }
            // `uri` is the Interactions spelling of `file_uri`.
            let uri =
                first_trimmed(["file_uri", "fileUri", "uri"].map(|key| str_of(part.get(key))));
            if !uri.is_empty() {
                return file_data(&mime_type(), &uri);
            }
            part.get("url")
                .and_then(|url| inline_data_from_url(&str_of(Some(url))))
        }
        "image_url" => inline_data_from_url(&str_of(path(part, "image_url.url"))),
        "input_audio" => {
            let mime_type = input_audio_mime_type(&str_of(path(part, "input_audio.format")));
            inline_data(mime_type, &str_of(path(part, "input_audio.data")))
        }
        "file" => {
            let file = normalize_openai_file_data(
                &str_of(path(part, "file.filename")),
                "",
                &str_of(path(part, "file.file_data")),
            )?;
            inline_data(&file.mime_type, &file.data)
        }
        _ => None,
    }
}

/// `geminiTextPartJSON`.
pub(super) fn text_part(text: &str, thought: bool) -> Value {
    let mut part = object([("text", Value::String(text.to_owned()))]);
    if thought {
        set_path(&mut part, "thought", Value::Bool(true));
    }
    part
}

/// `geminiInlineDataPartJSON`: `None` without a MIME type or data.
fn inline_data_part(inline: &Value) -> Option<Value> {
    let mut mime_type = str_of(inline.get("mimeType"));
    if mime_type.is_empty() {
        mime_type = str_of(inline.get("mime_type"));
    }
    inline_part(&mime_type, &str_of(inline.get("data")))
}

/// An inline data part, if neither field is empty.
fn inline_part(mime_type: &str, data: &str) -> Option<Value> {
    if mime_type.is_empty() || data.is_empty() {
        return None;
    }
    Some(object([(
        "inlineData",
        object([
            ("mimeType", Value::String(mime_type.to_owned())),
            ("data", Value::String(data.to_owned())),
        ]),
    )]))
}

/// `geminiFileDataPartJSON`: `None` without a MIME type or URI.
fn file_data_part(file: &Value) -> Option<Value> {
    let mut mime_type = str_of(file.get("mimeType"));
    if mime_type.is_empty() {
        mime_type = str_of(file.get("mime_type"));
    }
    let mut uri = str_of(file.get("fileUri"));
    if uri.is_empty() {
        uri = str_of(file.get("file_uri"));
    }
    file_part(&mime_type, &uri)
}

/// A file data part, if neither field is empty.
fn file_part(mime_type: &str, uri: &str) -> Option<Value> {
    if mime_type.is_empty() || uri.is_empty() {
        return None;
    }
    Some(object([(
        "fileData",
        object([
            ("mimeType", Value::String(mime_type.to_owned())),
            ("fileUri", Value::String(uri.to_owned())),
        ]),
    )]))
}

/// An inline data part made as upstream makes it: the two fields written
/// into JSON with Go's `%q`, then read back with gjson (see [`go_quoted`]).
fn inline_data(mime_type: &str, data: &str) -> Option<Value> {
    inline_part(go_quoted(mime_type), go_quoted(data))
}

/// [`inline_data`] for a file data part.
fn file_data(mime_type: &str, uri: &str) -> Option<Value> {
    file_part(go_quoted(mime_type), go_quoted(uri))
}

/// `text` after Go's `%q` writes it and gjson reads it back. gjson stops
/// reading a string at an escape JSON doesn't have, so the text ends before
/// the first character `%q` writes as `\a`, `\v`, `\xNN` or `\UXXXXXXXX`.
fn go_quoted(text: &str) -> &str {
    let end = text
        .char_indices()
        .find(|&(_, c)| match c {
            '\u{8}' | '\t' | '\n' | '\u{c}' | '\r' => false,
            c if c < ' ' || c == '\u{7f}' => true,
            c if u32::from(c) < 0x1_0000 => false,
            c => go::quote(c.encode_utf8(&mut [0; 4])).starts_with("\"\\U"),
        })
        .map_or(text.len(), |(index, _)| index);
    text.get(..end).unwrap_or(text)
}

/// `geminiInlineDataPartFromDataURL`: a `data:<type>;base64,<data>` URL as
/// an inline data part.
fn inline_data_from_url(url: &str) -> Option<Value> {
    let (mime_type, rest) = url.strip_prefix("data:")?.split_once(';')?;
    inline_data(mime_type, rest.strip_prefix("base64,")?)
}

/// `interactionsInputAudioMimeType`.
fn input_audio_mime_type(format: &str) -> &'static str {
    match go::to_lower(format.trim()).as_str() {
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "opus" => "audio/opus",
        "pcm16" => "audio/pcm",
        _ => "audio/mpeg",
    }
}

/// `geminiSystemInstructionText`: a string, a `text` string, or the
/// non-empty texts of `parts` joined by newlines.
fn gemini_system_instruction_text(system: Option<&Value>) -> String {
    let Some(system) = system else {
        return String::new();
    };
    if let Value::String(text) = system {
        return text.clone();
    }
    if let Some(Value::String(text)) = system.get("text") {
        return text.clone();
    }
    let Some(Value::Array(parts)) = system.get("parts") else {
        return String::new();
    };
    let texts: Vec<Cow<'_, str>> = parts
        .iter()
        .map(|part| str_of(part.get("text")))
        .filter(|text| !text.is_empty())
        .collect();
    texts.join("\n")
}

/// `normalizeGeminiThinkingConfigForInteractions`: copies the thinking
/// level, budget and summary setting to the top of `generation_config`.
fn normalize_gemini_thinking_config(out: &mut Value) {
    let level = first_existing(
        out,
        &[
            "generation_config.thinking_config.thinking_level",
            "generation_config.thinkingConfig.thinkingLevel",
            "generation_config.thinkingConfig.thinking_level",
        ],
    )
    .map(|level| go::to_lower(str_of(Some(level)).trim()));
    if let Some(level) = level {
        set_path(
            out,
            "generation_config.thinking_level",
            Value::String(level),
        );
    }
    let budget = first_existing(
        out,
        &[
            "generation_config.thinking_config.thinking_budget",
            "generation_config.thinkingConfig.thinkingBudget",
            "generation_config.thinkingConfig.thinking_budget",
        ],
    )
    .cloned();
    if let Some(budget) = budget {
        set_path(out, "generation_config.thinking_budget", budget);
    }
    if path(out, "generation_config.thinking_summaries").is_some() {
        return;
    }
    let include = first_existing(
        out,
        &[
            "generation_config.thinking_config.include_thoughts",
            "generation_config.thinking_config.includeThoughts",
            "generation_config.thinkingConfig.include_thoughts",
            "generation_config.thinkingConfig.includeThoughts",
        ],
    )
    .map(bool_of);
    if let Some(include) = include {
        let summary = if include { "auto" } else { "none" };
        set_path(
            out,
            "generation_config.thinking_summaries",
            Value::String(summary.to_owned()),
        );
    }
}

/// `firstExistingPath`.
fn first_existing<'v>(root: &'v Value, paths: &[&str]) -> Option<&'v Value> {
    paths.iter().find_map(|at| path(root, at))
}

/// `copyGeminiToolsToInteractions`: built-in tools and function
/// declarations as Interactions tools.
fn copy_gemini_tools(out: &mut Value, root: &Value) {
    let Some(tools) = root.get("tools") else {
        return;
    };
    let Value::Array(list) = tools else {
        set_path(out, "tools", tools.clone());
        return;
    };
    let mut normalized = Vec::new();
    for tool in list {
        for (camel, kind) in [
            ("urlContext", "url_context"),
            ("codeExecution", "code_execution"),
            ("googleSearch", "google_search"),
        ] {
            let Some(config) = tool.get(camel).or_else(|| tool.get(kind)) else {
                continue;
            };
            let mut entry = vec![("type", Value::String(kind.to_owned()))];
            if config.as_object().is_some_and(|config| !config.is_empty()) {
                entry.push((kind, config.clone()));
            }
            normalized.push(fields(entry));
        }
        if let Some(name) = tool.get("name") {
            normalized.push(function_tool(tool, name));
            continue;
        }
        let declarations = tool
            .get("functionDeclarations")
            .or_else(|| tool.get("function_declarations"));
        for declaration in each(declarations) {
            if let Some(name) = declaration.get("name") {
                normalized.push(function_tool(declaration, name));
            }
        }
    }
    let tools = if normalized.is_empty() {
        tools.clone()
    } else {
        Value::Array(normalized)
    };
    set_path(out, "tools", tools);
}

/// An Interactions function tool for a Gemini declaration, with its keys
/// sorted as `json.Marshal` writes a Go map's.
fn function_tool(declaration: &Value, name: &Value) -> Value {
    let mut entry = vec![
        ("type", Value::String("function".to_owned())),
        ("name", Value::String(str_of(Some(name)).into_owned())),
    ];
    if let Some(description) = declaration.get("description") {
        entry.push((
            "description",
            Value::String(str_of(Some(description)).into_owned()),
        ));
    }
    if let Some(parameters) = declaration
        .get("parameters")
        .or_else(|| declaration.get("parametersJsonSchema"))
    {
        entry.push(("parameters", parameters.clone()));
    }
    fields(entry)
}

/// `geminiPartToInteractionsContent`: a Gemini text, inline data or file
/// data part as Interactions content.
fn gemini_part_to_content(part: &Value) -> Option<Value> {
    if let Some(text) = part.get("text") {
        return Some(object([
            ("type", Value::String("text".to_owned())),
            ("text", Value::String(str_of(Some(text)).into_owned())),
        ]));
    }
    if let Some(inline) = part.get("inlineData") {
        let mut mime_type = str_of(inline.get("mimeType"));
        if mime_type.is_empty() {
            mime_type = str_of(inline.get("mime_type"));
        }
        return Some(inline_data_content(&mime_type, &str_of(inline.get("data"))));
    }
    if let Some(inline) = part.get("inline_data") {
        return Some(inline_data_content(
            &str_of(inline.get("mime_type")),
            &str_of(inline.get("data")),
        ));
    }
    file_data_content(gemini_part_file_data(part)?)
}

/// `geminiPartFileData`: a Gemini part's `fileData`, in either spelling.
fn gemini_part_file_data(part: &Value) -> Option<&Value> {
    part.get("fileData").or_else(|| part.get("file_data"))
}

/// `geminiInlineDataToInteractionsContent`: typed by the MIME type's family.
fn inline_data_content(mime_type: &str, data: &str) -> Value {
    object([
        ("type", Value::String(media_type(mime_type).to_owned())),
        ("mime_type", Value::String(mime_type.to_owned())),
        ("data", Value::String(data.to_owned())),
    ])
}

/// `geminiFileDataToInteractionsContent`: a Gemini `fileData` as an
/// Interactions media part that names the file by `uri`. `None` without a
/// URI, which leaves nothing to send.
fn file_data_content(file: &Value) -> Option<Value> {
    let mut uri = str_of(file.get("fileUri")).trim().to_owned();
    if uri.is_empty() {
        uri = str_of(file.get("file_uri")).trim().to_owned();
    }
    if uri.is_empty() {
        return None;
    }
    let mut mime_type = str_of(file.get("mimeType"));
    if mime_type.is_empty() {
        mime_type = str_of(file.get("mime_type"));
    }
    let mut item = object([
        ("type", Value::String(media_type(&mime_type).to_owned())),
        ("uri", Value::String(uri)),
    ]);
    if !mime_type.is_empty() {
        set_path(
            &mut item,
            "mime_type",
            Value::String(mime_type.into_owned()),
        );
    }
    Some(item)
}

/// `geminiInteractionsMediaType`: the Interactions content type for a MIME
/// type's family.
fn media_type(mime_type: &str) -> &'static str {
    let lower = go::to_lower(mime_type);
    if lower.starts_with("image/") {
        "image"
    } else if lower.starts_with("audio/") {
        "audio"
    } else if lower.starts_with("video/") {
        "video"
    } else {
        "document"
    }
}

/// `interactionsThoughtSignature`: a part's first non-blank
/// `thoughtSignature`, `thought_signature` or
/// `extra_content.google.thought_signature`, trimmed.
pub(super) fn thought_signature(part: &Value) -> String {
    first_trimmed(
        [
            "thoughtSignature",
            "thought_signature",
            "extra_content.google.thought_signature",
        ]
        .map(|at| str_of(path(part, at))),
    )
}

/// `geminiPartToInteractionsSteps`: the Interactions steps for a Gemini
/// part. A signature becomes a thought step of its own, before a function
/// call and after anything else.
pub(super) fn gemini_part_to_steps(part: &Value) -> Vec<Value> {
    let signature = thought_signature(part);
    let signed = |step: Value| {
        let mut steps = vec![step];
        if !signature.is_empty() {
            steps.push(thought_step(&signature, ""));
        }
        steps
    };
    if let Some(call) = part.get("functionCall") {
        let mut step = object([
            ("type", Value::String("function_call".to_owned())),
            ("name", Value::String(str_of(call.get("name")).into_owned())),
            ("arguments", Value::Object(Map::new())),
        ]);
        if let Some(id) = call.get("id").or_else(|| call.get("call_id")) {
            set_path(
                &mut step,
                "call_id",
                Value::String(str_of(Some(id)).into_owned()),
            );
        }
        if let Some(args) = call.get("args") {
            set_path(&mut step, "arguments", args.clone());
        }
        let mut steps = Vec::new();
        if !signature.is_empty() {
            steps.push(thought_step(&signature, ""));
        }
        steps.push(step);
        return steps;
    }
    if let Some(response) = part.get("functionResponse") {
        let mut step = object([
            ("type", Value::String("function_result".to_owned())),
            (
                "name",
                Value::String(str_of(response.get("name")).into_owned()),
            ),
            ("result", Value::Object(Map::new())),
        ]);
        if let Some(id) = response.get("id").or_else(|| response.get("call_id")) {
            set_path(
                &mut step,
                "call_id",
                Value::String(str_of(Some(id)).into_owned()),
            );
        }
        if let Some(result) = response.get("response") {
            set_path(&mut step, "result", result.clone());
        }
        return vec![step];
    }
    if let Some(text) = part.get("text") {
        let text = str_of(Some(text));
        if part.get("thought").is_some_and(bool_of) {
            return vec![thought_step(&signature, &text)];
        }
        if text.is_empty() {
            if signature.is_empty() {
                return Vec::new();
            }
            return vec![thought_step(&signature, "")];
        }
        return signed(object([
            ("type", Value::String("model_output".to_owned())),
            (
                "content",
                Value::Array(vec![object([("text", Value::String(text.into_owned()))])]),
            ),
        ]));
    }
    let inline = if let Some(inline) = part.get("inlineData") {
        let mut mime_type = str_of(inline.get("mimeType"));
        if mime_type.is_empty() {
            mime_type = str_of(inline.get("mime_type"));
        }
        Some(inline_data_content(&mime_type, &str_of(inline.get("data"))))
    } else {
        part.get("inline_data").map(|inline| {
            inline_data_content(
                &str_of(inline.get("mime_type")),
                &str_of(inline.get("data")),
            )
        })
    };
    if let Some(item) = inline {
        return signed(object([
            ("type", Value::String("model_output".to_owned())),
            ("content", Value::Array(vec![item])),
        ]));
    }
    if signature.is_empty() {
        Vec::new()
    } else {
        vec![thought_step(&signature, "")]
    }
}

/// `geminiThoughtStepJSON`.
fn thought_step(signature: &str, text: &str) -> Value {
    let mut step = object([("type", Value::String("thought".to_owned()))]);
    if !signature.is_empty() {
        set_path(&mut step, "signature", Value::String(signature.to_owned()));
    }
    if !text.is_empty() {
        set_path(
            &mut step,
            "content",
            Value::Array(vec![object([("text", Value::String(text.to_owned()))])]),
        );
    }
    step
}

#[cfg(test)]
mod tests;
