// Ported from CLIProxyAPI internal/translator/common/claude_native_response.go
// (ClaudeMessagesJSONToSSE) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A whole Claude Messages response, as the events of a stream that build it.
//!
//! Claude may answer a request that doesn't stream with one `message`
//! object, where the translators of whole responses read the `data:` lines
//! of an event stream. [`messages_json_to_sse`] writes such a message as
//! those lines, so the translators read both alike, and leaves anything
//! else, an event stream included, as it is.
//!
//! The message is read as gjson reads it: of a key that repeats, the first
//! counts, and a value read as text that isn't a string is its JSON text as
//! written. A tool call's `input` is passed on as written. A block that has
//! no delta, such as `redacted_thinking` or `server_tool_use`, is written as
//! Go's `json.Marshal` writes it, since the Responses translator reads a web
//! search's `input` as written.
//!
//! Deviations from upstream:
//! - A message that isn't UTF-8, or that serde_json can't read because it
//!   holds an unpaired surrogate escape or nests more than 128 levels deep,
//!   is [`Native::Unreadable`]. The translators treat it as a `data:` line
//!   they can't read. Upstream writes its events.
//! - The message `message_start` carries, and each text, tool use or
//!   thinking block, is read with serde_json, so of a key that repeats in
//!   it, the last counts.

use serde_json::{Value, json};

use crate::json::{exact, go_marshaled, raw, set_path, str_of};

/// What [`messages_json_to_sse`] made of a response.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Native {
    /// Not a whole Messages response: read the response as it is.
    Other,
    /// A whole Messages response that gjson reads but serde_json can't.
    Unreadable,
    /// A whole Messages response, as SSE `data:` lines, and its model.
    Events { sse: String, model: String },
}

/// `ClaudeMessagesJSONToSSE`: `response` as the `data:` lines of the events
/// that build it, if it is a whole Messages response: valid JSON whose
/// `type` is `message` and whose `content` is an array.
pub(crate) fn messages_json_to_sse(response: &[u8]) -> Native {
    // Bytes that aren't UTF-8 never form JSON's structure, so replacing them
    // keeps it.
    let lossy = String::from_utf8_lossy(response);
    if !raw::valid(&lossy)
        || string(raw::member(&lossy, "type")) != "message"
        || !raw::member(&lossy, "content").is_some_and(|content| content.starts_with('['))
    {
        return Native::Other;
    }
    let Ok(text) = std::str::from_utf8(response) else {
        return Native::Unreadable;
    };
    let Ok(mut message) = exact::from_str(text) else {
        return Native::Unreadable;
    };

    let mut sse = String::new();
    let mut emit = |event: &str| {
        sse.push_str("data: ");
        sse.push_str(event);
        sse.push_str("\n\n");
    };
    set_path(&mut message, "content", json!([]));
    set_path(&mut message, "stop_reason", Value::Null);
    set_path(&mut message, "stop_sequence", Value::Null);
    emit(&json!({ "message": message, "type": "message_start" }).to_string());

    let content = raw::member(text, "content").unwrap_or_default();
    for (index, block) in raw::elements(content).into_iter().enumerate() {
        let kind = string(raw::member(block, "type"));
        let (start, delta) = match kind.as_str() {
            "text" => (
                edited(block, &["text"]),
                Some(json!({ "text": string(raw::member(block, "text")), "type": "text_delta" })),
            ),
            "tool_use" => (
                edited(block, &["input"]),
                Some(json!({
                    "partial_json": raw::member(block, "input").unwrap_or("{}"),
                    "type": "input_json_delta",
                })),
            ),
            "thinking" => (
                edited(block, &["thinking", "signature"]),
                Some(json!({
                    "thinking": string(raw::member(block, "thinking")),
                    "type": "thinking_delta",
                })),
            ),
            _ => (go_compact(block), None),
        };
        emit(&format!(
            r#"{{"content_block":{start},"index":{index},"type":"content_block_start"}}"#
        ));
        let mut delta_event = |delta: Value| {
            emit(
                &json!({ "delta": delta, "index": index, "type": "content_block_delta" })
                    .to_string(),
            );
        };
        if let Some(delta) = delta {
            delta_event(delta);
        }
        if kind == "text" {
            for citation in array(raw::member(block, "citations")) {
                let citation = exact::from_str(citation).unwrap_or_default();
                delta_event(json!({ "citation": citation, "type": "citations_delta" }));
            }
        }
        if kind == "thinking"
            && let Some(signature) = raw::member(block, "signature")
        {
            delta_event(json!({ "signature": string(Some(signature)), "type": "signature_delta" }));
        }
        emit(&json!({ "index": index, "type": "content_block_stop" }).to_string());
    }

    let read = |key: &str| raw::member(text, key).and_then(|value| exact::from_str(value).ok());
    let stop = |key: &str| read(key).map_or(Value::Null, |value| go_marshaled(&value));
    let usage = read("usage").unwrap_or_else(|| json!({}));
    emit(
        &json!({
            "delta": { "stop_reason": stop("stop_reason"), "stop_sequence": stop("stop_sequence") },
            "type": "message_delta",
            "usage": usage,
        })
        .to_string(),
    );
    emit(&json!({ "type": "message_stop" }).to_string());
    Native::Events {
        sse,
        model: string(raw::member(text, "model")),
    }
}

/// The block `block` with each of `fields` set to an empty value: `{}` for
/// `input`, else `""`.
fn edited(block: &str, fields: &[&str]) -> String {
    let Ok(mut block) = exact::from_str(block) else {
        return go_compact(block);
    };
    for &field in fields {
        let empty = if field == "input" {
            json!({})
        } else {
            json!("")
        };
        set_path(&mut block, field, empty);
    }
    block.to_string()
}

/// gjson `String()` of the value `raw`, as written.
fn string(raw: Option<&str>) -> String {
    match raw {
        None => String::new(),
        Some(raw) if raw.starts_with(['{', '[']) => raw.to_owned(),
        Some(raw) => exact::from_str(raw)
            .map(|value| str_of(Some(&value)).into_owned())
            .unwrap_or_default(),
    }
}

/// gjson `Array()` of the value `raw`: an array's elements, nothing for a
/// missing value or `null`, and any other value alone.
fn array(raw: Option<&str>) -> Vec<&str> {
    match raw {
        None | Some("null") => Vec::new(),
        Some(raw) if raw.starts_with('[') => raw::elements(raw),
        Some(raw) => vec![raw],
    }
}

/// The valid JSON `raw` as Go's `json.Marshal` writes a `json.RawMessage`:
/// without the white space between tokens, and with `<`, `>`, `&`, U+2028
/// and U+2029 escaped.
fn go_compact(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let (mut in_string, mut escaped) = (false, false);
    for c in raw.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
        } else if matches!(c, ' ' | '\t' | '\n' | '\r') {
            continue;
        } else if c == '"' {
            in_string = true;
        }
        if matches!(c, '<' | '>' | '&' | '\u{2028}' | '\u{2029}') {
            out.push('\\');
            out.push('u');
            out.push_str(&format!("{:04x}", u32::from(c)));
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests;
