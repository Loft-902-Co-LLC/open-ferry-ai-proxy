// Ported from CLIProxyAPI internal/runtime/executor/xai_executor_tokens.go
// (countXAIInputTokens, xaiCollectInputTokenSegments,
// xaiCollectContentTokenSegments, xaiCollectToolTokenSegments,
// xaiAppendTokenString, xaiAppendTokenJSON) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Estimates a prepared xAI request's input tokens with `o200k_base`.
//!
//! Only the text the model reads counts: the instructions, each input
//! message's text (and image, file and audio references), function calls
//! and their output, reasoning summaries, function tools, and a structured
//! output format's name and schema. The request's settings and wrappers
//! don't.
//!
//! Deviations from upstream:
//! - A JSON value counted as JSON (a schema, a tool's parameters, an
//!   object's arguments or output) is written by `serde_json`, compactly
//!   and in its keys' order, where upstream counts the bytes as they were.

use serde_json::Value;

use crate::json::{get, str_of};

/// The type of a function tool (`xaiFunctionToolType`).
const FUNCTION_TOOL_TYPE: &str = "function";

/// Estimates `body`'s input tokens (`countXAIInputTokens`).
pub(crate) fn count_input_tokens(body: &Value) -> i64 {
    let mut segments = Vec::new();
    append_string(&mut segments, get(body, "instructions"));
    collect_input(get(body, "input"), &mut segments);
    collect_tools(get(body, "tools"), &mut segments);
    if let Some(format) = get(body, "text.format") {
        append_string(&mut segments, format.get("name"));
        append_json(&mut segments, format.get("schema"));
    }
    if segments.is_empty() {
        return 0;
    }
    let count = tiktoken_rs::o200k_base_singleton()
        .encode_ordinary(&segments.join("\n"))
        .len();
    i64::try_from(count).unwrap_or(i64::MAX)
}

/// The text of the request's input (`xaiCollectInputTokenSegments`).
fn collect_input(input: Option<&Value>, segments: &mut Vec<String>) {
    let items = match input {
        Some(text @ Value::String(_)) => return append_string(segments, Some(text)),
        Some(Value::Array(items)) => items,
        _ => return,
    };
    for item in items {
        match str_of(item.get("type")).as_str() {
            "message" => collect_content(item.get("content"), segments),
            "function_call" => {
                append_string(segments, item.get("name"));
                append_json(segments, item.get("arguments"));
            }
            "function_call_output" => append_json(segments, item.get("output")),
            "reasoning" => {
                if let Some(Value::Array(parts)) = item.get("summary") {
                    for part in parts {
                        append_string(segments, part.get("text"));
                    }
                }
            }
            _ => {}
        }
    }
}

/// The text of a message's content (`xaiCollectContentTokenSegments`).
fn collect_content(content: Option<&Value>, segments: &mut Vec<String>) {
    let parts = match content {
        Some(text @ Value::String(_)) => return append_string(segments, Some(text)),
        Some(Value::Array(parts)) => parts,
        _ => return,
    };
    for part in parts {
        let fields: &[&str] = match str_of(part.get("type")).as_str() {
            "text" | "input_text" | "output_text" => &["text"],
            "refusal" => &["refusal"],
            "input_image" => &["image_url", "file_id"],
            "input_file" => &["file_data", "file_url", "file_id", "filename"],
            "input_audio" => &["data", "input_audio.data"],
            _ => &[],
        };
        for field in fields {
            append_string(segments, get(part, field));
        }
    }
}

/// The name, description and parameters of each function tool
/// (`xaiCollectToolTokenSegments`).
fn collect_tools(tools: Option<&Value>, segments: &mut Vec<String>) {
    let Some(Value::Array(tools)) = tools else {
        return;
    };
    for tool in tools {
        if str_of(tool.get("type")) != FUNCTION_TOOL_TYPE {
            continue;
        }
        append_string(segments, tool.get("name"));
        append_string(segments, tool.get("description"));
        append_json(segments, tool.get("parameters"));
    }
}

/// Adds `value`'s text, trimmed, unless it is empty
/// (`xaiAppendTokenString`).
fn append_string(segments: &mut Vec<String>, value: Option<&Value>) {
    let text = str_of(value);
    let text = text.trim();
    if !text.is_empty() {
        segments.push(text.to_owned());
    }
}

/// Adds a string's text, or another value's JSON (`xaiAppendTokenJSON`).
fn append_json(segments: &mut Vec<String>, value: Option<&Value>) {
    match value {
        None => {}
        Some(Value::String(_)) => append_string(segments, value),
        Some(value) => {
            let text = value.to_string();
            let text = text.trim();
            if !text.is_empty() {
                segments.push(text.to_owned());
            }
        }
    }
}

#[cfg(test)]
mod tests;
