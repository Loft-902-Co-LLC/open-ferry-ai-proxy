// Ported from CLIProxyAPI internal/runtime/executor/codex_executor_tokens.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Counts a Codex request's input tokens locally, with the model's
//! tokenizer, over its instructions, input, tools and output schema.
//!
//! Deviations from upstream:
//! - `tiktoken-rs` replaces `tiktoken-go`. GPT-5, GPT-4.1 and GPT-4o models
//!   use `o200k_base`; everything else uses `cl100k_base`, as upstream's
//!   model table gives.
//! - Raw JSON (tool parameters, the output schema) is counted as
//!   `serde_json` writes it, compact, where upstream counts the client's
//!   bytes.

use open_ferry_translate::go::to_lower;
use serde_json::Value;
use tiktoken_rs::CoreBPE;

use super::gjson::{get, str_of};

/// The tokenizer for `model` (`tokenizerForCodexModel`).
pub(crate) fn tokenizer_for(model: &str) -> &'static CoreBPE {
    let model = to_lower(model.trim());
    let o200k = ["gpt-5", "gpt-4.1", "gpt-4o"]
        .iter()
        .any(|prefix| model.starts_with(prefix));
    if o200k {
        tiktoken_rs::o200k_base_singleton()
    } else {
        tiktoken_rs::cl100k_base_singleton()
    }
}

/// Counts the input tokens of a Codex request body (`countCodexInputTokens`).
pub(crate) fn count_input_tokens(tokenizer: &CoreBPE, body: &Value) -> i64 {
    let text = segments(body).join("\n");
    if text.is_empty() {
        return 0;
    }
    i64::try_from(tokenizer.encode_ordinary(&text).len()).unwrap_or(i64::MAX)
}

/// The text that counts toward the input.
fn segments(body: &Value) -> Vec<String> {
    let mut segments = Vec::new();
    let mut push = |text: String| {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            segments.push(trimmed.to_owned());
        }
    };
    push(str_of(get(body, "instructions")));

    if let Some(Value::Array(items)) = get(body, "input") {
        for item in items {
            match str_of(get(item, "type")).as_str() {
                "message" => {
                    if let Some(Value::Array(parts)) = get(item, "content") {
                        for part in parts {
                            push(str_of(get(part, "text")));
                        }
                    }
                }
                "function_call" => {
                    push(str_of(get(item, "name")));
                    push(str_of(get(item, "arguments")));
                }
                "function_call_output" => push(str_of(get(item, "output"))),
                _ => push(str_of(get(item, "text"))),
            }
        }
    }

    if let Some(Value::Array(tools)) = get(body, "tools") {
        for tool in tools {
            push(str_of(get(tool, "name")));
            push(str_of(get(tool, "description")));
            if let Some(parameters) = get(tool, "parameters") {
                push(raw_or_string(parameters));
            }
        }
    }

    if let Some(format) = get(body, "text.format") {
        push(str_of(get(format, "name")));
        if let Some(schema) = get(format, "schema") {
            push(raw_or_string(schema));
        }
    }
    segments
}

/// A string as it is, anything else as JSON.
fn raw_or_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn picks_tokenizer_by_model() {
        let o200k: *const CoreBPE = tiktoken_rs::o200k_base_singleton();
        let cl100k: *const CoreBPE = tiktoken_rs::cl100k_base_singleton();
        for (model, want) in [
            ("", cl100k),
            ("gpt-5.4", o200k),
            (" GPT-5-codex ", o200k),
            ("gpt-4.1-mini", o200k),
            ("gpt-4o", o200k),
            ("gpt-4", cl100k),
            ("gpt-3.5-turbo", cl100k),
            ("o3", cl100k),
        ] {
            assert!(std::ptr::eq(tokenizer_for(model), want), "{model}");
        }
    }

    #[test]
    fn counts_every_segment() {
        let body = json!({
            "instructions": " be brief ",
            "input": [
                {"type": "message", "content": [{"type": "input_text", "text": "hello"}, {"type": "input_image"}]},
                {"type": "function_call", "name": "lookup", "arguments": "{\"q\":1}"},
                {"type": "function_call_output", "output": "found"},
                {"type": "reasoning", "text": "why"},
                {"type": "message", "content": "not an array"}
            ],
            "tools": [{"name": "lookup", "description": "Finds", "parameters": {"type": "object"}}],
            "text": {"format": {"name": "answer", "schema": "{\"type\":\"string\"}"}}
        });
        assert_eq!(
            segments(&body),
            [
                "be brief",
                "hello",
                "lookup",
                "{\"q\":1}",
                "found",
                "why",
                "lookup",
                "Finds",
                "{\"type\":\"object\"}",
                "answer",
                "{\"type\":\"string\"}"
            ]
        );
        let tokenizer = tokenizer_for("gpt-5");
        let want = tokenizer.encode_ordinary(&segments(&body).join("\n")).len();
        assert_eq!(
            count_input_tokens(tokenizer, &body),
            i64::try_from(want).unwrap()
        );
        assert_eq!(count_input_tokens(tokenizer, &json!({})), 0);
    }
}
