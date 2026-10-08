// Ported from CLIProxyAPI internal/runtime/executor/helps/token_helpers.go
// (TokenizerForModel, CountOpenAIChatTokens, BuildOpenAIUsageJSON)
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Counts an OpenAI Chat Completions request's input tokens locally, with
//! the model's tokenizer, over its messages, tools, functions, tool choice,
//! response format, `input` and `prompt`.
//!
//! Deviations from upstream:
//! - `tiktoken-rs` replaces `tiktoken-go`, with the encodings upstream's
//!   model table gives: `cl100k_base` for GPT-4 (but not GPT-4.1 or GPT-4o),
//!   GPT-3 and an empty model, `o200k_base` for everything else.
//! - Raw JSON (tool parameters, schemas, object content) is counted as
//!   `serde_json` writes it, compact, where upstream counts the client's
//!   bytes.

use open_ferry_translate::go::to_lower;
use serde_json::Value;
use tiktoken_rs::CoreBPE;

use crate::json::{get, str_at, str_of};

/// The tokenizer for `model` (`TokenizerForModel`).
pub(crate) fn tokenizer_for(model: &str) -> &'static CoreBPE {
    let model = to_lower(model.trim());
    let o200k = ["gpt-5", "gpt-4.1", "gpt-4o"]
        .iter()
        .any(|prefix| model.starts_with(prefix));
    let cl100k = model.is_empty() || model.starts_with("gpt-4") || model.starts_with("gpt-3");
    if cl100k && !o200k {
        tiktoken_rs::cl100k_base_singleton()
    } else {
        tiktoken_rs::o200k_base_singleton()
    }
}

/// Counts the input tokens of a Chat Completions body
/// (`CountOpenAIChatTokens`).
pub(crate) fn count_chat_tokens(tokenizer: &CoreBPE, body: &Value) -> i64 {
    let mut segments = Vec::new();
    collect_messages(get(body, "messages"), &mut segments);
    collect_tools(get(body, "tools"), &mut segments);
    collect_functions(get(body, "functions"), &mut segments);
    collect_tool_choice(get(body, "tool_choice"), &mut segments);
    collect_response_format(get(body, "response_format"), &mut segments);
    add(&mut segments, &str_at(body, "input"));
    add(&mut segments, &str_at(body, "prompt"));
    let joined = segments.join("\n");
    let joined = joined.trim();
    if joined.is_empty() {
        return 0;
    }
    i64::try_from(tokenizer.encode_ordinary(joined).len()).unwrap_or(i64::MAX)
}

/// The usage a token count answers with, before translation
/// (`BuildOpenAIUsageJSON`).
pub(crate) fn usage_json(count: i64) -> String {
    format!(
        r#"{{"usage":{{"prompt_tokens":{count},"completion_tokens":0,"total_tokens":{count}}}}}"#
    )
}

/// JSON as text, as gjson's `Raw` gives it.
fn raw(value: &Value) -> String {
    value.to_string()
}

fn collect_messages(messages: Option<&Value>, segments: &mut Vec<String>) {
    let Some(Value::Array(messages)) = messages else {
        return;
    };
    for message in messages {
        add(segments, &str_at(message, "role"));
        add(segments, &str_at(message, "name"));
        collect_content(get(message, "content"), segments);
        collect_tool_calls(get(message, "tool_calls"), segments);
        collect_function_call(get(message, "function_call"), segments);
    }
}

fn collect_content(content: Option<&Value>, segments: &mut Vec<String>) {
    match content {
        Some(Value::String(text)) => add(segments, text),
        Some(Value::Array(parts)) => {
            for part in parts {
                match str_at(part, "type").as_str() {
                    "text" | "input_text" | "output_text" => add(segments, &str_at(part, "text")),
                    "image_url" => add(segments, &str_at(part, "image_url.url")),
                    "input_audio" | "output_audio" | "audio" => add(segments, &str_at(part, "id")),
                    "tool_result" => {
                        add(segments, &str_at(part, "name"));
                        collect_content(get(part, "content"), segments);
                    }
                    _ => match part {
                        Value::Array(_) => collect_content(Some(part), segments),
                        Value::Object(_) => add(segments, &raw(part)),
                        other => add(segments, &str_of(Some(other))),
                    },
                }
            }
        }
        Some(object @ Value::Object(_)) => add(segments, &raw(object)),
        _ => {}
    }
}

fn collect_tool_calls(calls: Option<&Value>, segments: &mut Vec<String>) {
    let Some(Value::Array(calls)) = calls else {
        return;
    };
    for call in calls {
        add(segments, &str_at(call, "id"));
        add(segments, &str_at(call, "type"));
        if let Some(function) = get(call, "function") {
            add(segments, &str_at(function, "name"));
            add(segments, &str_at(function, "description"));
            add(segments, &str_at(function, "arguments"));
            if let Some(parameters) = get(function, "parameters") {
                add(segments, &raw(parameters));
            }
        }
    }
}

fn collect_function_call(call: Option<&Value>, segments: &mut Vec<String>) {
    if let Some(call) = call {
        add(segments, &str_at(call, "name"));
        add(segments, &str_at(call, "arguments"));
    }
}

fn collect_tools(tools: Option<&Value>, segments: &mut Vec<String>) {
    match tools {
        Some(Value::Array(tools)) => {
            for tool in tools {
                append_tool(tool, segments);
            }
        }
        Some(tool) => append_tool(tool, segments),
        None => {}
    }
}

fn collect_functions(functions: Option<&Value>, segments: &mut Vec<String>) {
    let Some(Value::Array(functions)) = functions else {
        return;
    };
    for function in functions {
        add(segments, &str_at(function, "name"));
        add(segments, &str_at(function, "description"));
        if let Some(parameters) = get(function, "parameters") {
            add(segments, &raw(parameters));
        }
    }
}

fn collect_tool_choice(choice: Option<&Value>, segments: &mut Vec<String>) {
    match choice {
        Some(Value::String(text)) => add(segments, text),
        Some(other) => add(segments, &raw(other)),
        None => {}
    }
}

fn collect_response_format(format: Option<&Value>, segments: &mut Vec<String>) {
    let Some(format) = format else {
        return;
    };
    add(segments, &str_at(format, "type"));
    add(segments, &str_at(format, "name"));
    for path in ["json_schema", "schema"] {
        if let Some(schema) = get(format, path) {
            add(segments, &raw(schema));
        }
    }
}

/// `appendToolPayload`.
fn append_tool(tool: &Value, segments: &mut Vec<String>) {
    add(segments, &str_at(tool, "type"));
    add(segments, &str_at(tool, "name"));
    add(segments, &str_at(tool, "description"));
    if let Some(function) = get(tool, "function") {
        add(segments, &str_at(function, "name"));
        add(segments, &str_at(function, "description"));
        if let Some(parameters) = get(function, "parameters") {
            add(segments, &raw(parameters));
        }
    }
}

/// `addIfNotEmpty`: the value without the spaces around it, unless that
/// leaves nothing.
fn add(segments: &mut Vec<String>, value: &str) {
    let trimmed = value.trim();
    if !trimmed.is_empty() {
        segments.push(trimmed.to_owned());
    }
}

#[cfg(test)]
mod tests {
    // Upstream has no tests of these helpers; these check the segments and
    // the tokenizer table.
    use super::*;
    use serde_json::json;

    fn count(body: Value) -> i64 {
        count_chat_tokens(tiktoken_rs::o200k_base_singleton(), &body)
    }

    fn tokens(text: &str) -> i64 {
        tiktoken_rs::o200k_base_singleton()
            .encode_ordinary(text)
            .len() as i64
    }

    #[test]
    fn tokenizer_table() {
        let o200k: *const CoreBPE = tiktoken_rs::o200k_base_singleton();
        let cl100k: *const CoreBPE = tiktoken_rs::cl100k_base_singleton();
        for (model, want) in [
            ("", cl100k),
            (" GPT-5.1 ", o200k),
            ("gpt-4.1-mini", o200k),
            ("gpt-4o", o200k),
            ("gpt-4-turbo", cl100k),
            ("gpt-3.5-turbo", cl100k),
            ("gpt-3", cl100k),
            ("o1-mini", o200k),
            ("o3", o200k),
            ("o4-mini", o200k),
            ("deepseek-chat", o200k),
        ] {
            assert!(std::ptr::eq(tokenizer_for(model), want), "{model}");
        }
    }

    #[test]
    fn counts_the_request_segments() {
        assert_eq!(count(json!({})), 0);
        assert_eq!(count(json!({"messages":[{"role":" "}]})), 0);
        let body = json!({
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "name": "ann", "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "image_url", "image_url": {"url": "https://x/y.png"}},
                    {"type": "input_audio", "id": "aud"},
                    {"type": "tool_result", "name": "t", "content": "r"},
                    {"type": "other", "k": 1},
                    ["nested"],
                    7
                ]},
                {"role": "assistant", "content": {"a": 1}, "tool_calls": [
                    {"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{}", "parameters": {"p": 1}}}
                ], "function_call": {"name": "g", "arguments": "x"}}
            ],
            "tools": [{"type": "function", "function": {"name": "f", "description": "d", "parameters": {"q": 2}}}],
            "functions": [{"name": "h", "description": "e", "parameters": {}}],
            "tool_choice": {"type": "auto"},
            "response_format": {"type": "json_schema", "json_schema": {"s": 1}},
            "input": "in",
            "prompt": " pr "
        });
        let want = [
            "system",
            "be brief",
            "user",
            "ann",
            "hello",
            "https://x/y.png",
            "aud",
            "t",
            "r",
            r#"{"type":"other","k":1}"#,
            "nested",
            "7",
            "assistant",
            r#"{"a":1}"#,
            "c1",
            "function",
            "f",
            "{}",
            r#"{"p":1}"#,
            "g",
            "x",
            "function",
            "f",
            "d",
            r#"{"q":2}"#,
            "h",
            "e",
            "{}",
            r#"{"type":"auto"}"#,
            "json_schema",
            r#"{"s":1}"#,
            "in",
            "pr",
        ]
        .join("\n");
        assert_eq!(count(body), tokens(&want));
        assert_eq!(
            count(json!({"tool_choice": "none", "tools": {"name": "solo"}})),
            tokens("solo\nnone")
        );
    }

    #[test]
    fn usage() {
        assert_eq!(
            usage_json(12),
            r#"{"usage":{"prompt_tokens":12,"completion_tokens":0,"total_tokens":12}}"#
        );
    }
}
