//! Whole Claude Messages responses, as Claude answers a call that doesn't
//! stream, for the Chat Completions and Responses translators of whole
//! responses, which read one as the events of a stream that build it.
//!
//! [`body`] builds the message a generated event stream builds: the message
//! of its `message_start`, each block as its start gives it with what its
//! deltas carry added (text, thinking, a signature, citations, and a tool
//! call's or search's input when its pieces make JSON), and the stop reason,
//! stop sequence and usage of its `message_delta`. Loosely typed values stay
//! loose. The message is written compact, pretty-printed or with its
//! non-ASCII characters escaped, and now and then has a `type` or `content`
//! that keeps it from being read as one.
//!
//! No key repeats: the message `message_start` carries, and a block with a
//! delta, are read with serde_json, where upstream reads the first of a
//! repeated key.

use open_ferry_translate::json::exact;
use serde_json::{Map, Value, json};

use super::{Rng, escape_text};

/// The whole Messages response the stream `events` builds, as Claude might
/// write it.
pub(super) fn body(rng: &mut Rng, events: &[Value]) -> String {
    let mut message = message(events);
    damage(rng, &mut message);
    render(rng, &Value::Object(message))
}

/// The message `events` build.
fn message(events: &[Value]) -> Map<String, Value> {
    let mut message = Map::new();
    // Each block, with the `partial_json` pieces it was given.
    let mut blocks: Vec<(Value, String)> = Vec::new();
    for event in events {
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if let Some(Value::Object(start)) = event.get("message") {
                    message = start.clone();
                }
            }
            Some("content_block_start") => {
                if let Some(block) = event.get("content_block") {
                    blocks.push((block.clone(), String::new()));
                }
            }
            Some("content_block_delta") => {
                if let (Some((Value::Object(block), input)), Some(delta)) =
                    (blocks.last_mut(), event.get("delta"))
                {
                    add_delta(block, delta, input);
                }
            }
            Some("message_delta") => {
                if let Some(Value::Object(delta)) = event.get("delta") {
                    for key in ["stop_reason", "stop_sequence"] {
                        if let Some(value) = delta.get(key) {
                            message.insert(key.to_owned(), value.clone());
                        }
                    }
                }
                if let Some(usage) = event.get("usage") {
                    message.insert("usage".to_owned(), usage.clone());
                }
            }
            _ => {}
        }
    }
    let content: Vec<Value> = blocks
        .into_iter()
        .map(|(mut block, input)| {
            if let (Value::Object(fields), Ok(input)) = (&mut block, exact::from_str(&input)) {
                fields.insert("input".to_owned(), input);
            }
            block
        })
        .collect();
    if !message.contains_key("type") {
        message.insert("type".to_owned(), json!("message"));
    }
    message.insert("content".to_owned(), Value::Array(content));
    message
}

/// Now and then gives `message` a `type` or `content` that keeps it from
/// being read as a message.
fn damage(rng: &mut Rng, message: &mut Map<String, Value>) {
    match rng.below(100) {
        0..=2 => {
            let kind = rng.pick(&[
                json!("Message"),
                json!("message_start"),
                json!(5),
                Value::Null,
            ]);
            message.insert("type".to_owned(), kind);
        }
        3..=5 => {
            let content = rng.pick(&[json!("text"), json!({}), Value::Null]);
            message.insert("content".to_owned(), content);
        }
        6 => {
            message.remove("content");
        }
        _ => {}
    }
}

/// Adds what `delta` carries to `block`, or to the `partial_json` pieces of
/// its `input`.
fn add_delta(block: &mut Map<String, Value>, delta: &Value, input: &mut String) {
    match delta.get("type").and_then(Value::as_str) {
        Some("text_delta") => append(block, "text", delta.get("text")),
        Some("thinking_delta") => append(block, "thinking", delta.get("thinking")),
        Some("signature_delta") => {
            if let Some(signature) = delta.get("signature") {
                block.insert("signature".to_owned(), signature.clone());
            }
        }
        Some("citations_delta") => {
            let citation = delta.get("citation").cloned().unwrap_or(Value::Null);
            match block.get_mut("citations") {
                Some(Value::Array(citations)) => citations.push(citation),
                _ => {
                    block.insert("citations".to_owned(), json!([citation]));
                }
            }
        }
        Some("input_json_delta") => {
            if let Some(Value::String(piece)) = delta.get("partial_json") {
                input.push_str(piece);
            }
        }
        _ => {}
    }
}

/// Appends the text `piece` to the text at `key`, or puts a piece that isn't
/// text, or text where there was none, in its place.
fn append(block: &mut Map<String, Value>, key: &str, piece: Option<&Value>) {
    let Some(piece) = piece else {
        return;
    };
    match (block.get_mut(key), piece) {
        (Some(Value::String(text)), Value::String(piece)) => text.push_str(piece),
        _ => {
            block.insert(key.to_owned(), piece.clone());
        }
    }
}

/// `message` compact, pretty-printed with spaces or with tabs and CRLF line
/// ends, sometimes with every non-ASCII character escaped or with white
/// space around it.
fn render(rng: &mut Rng, message: &Value) -> String {
    let text = match rng.below(10) {
        0..=5 => message.to_string(),
        6 | 7 => serde_json::to_string_pretty(message).expect("a Value always serializes"),
        _ => {
            let pretty = serde_json::to_string_pretty(message).expect("a Value always serializes");
            // JSON text breaks lines only between tokens, so each line starts
            // with nothing but its indent.
            pretty
                .lines()
                .map(|line| {
                    let token = line.trim_start_matches(' ');
                    "\t".repeat((line.len() - token.len()) / 2) + token
                })
                .collect::<Vec<_>>()
                .join("\r\n")
        }
    };
    let text = if rng.chance(20) {
        escape_text(&text)
    } else {
        text
    };
    if rng.chance(10) {
        format!("\n {text}\r\n")
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_is_what_the_events_build() {
        let events = [
            json!({ "type": "message_start", "message": { "id": "msg_1", "type": "message", "content": [], "stop_reason": null, "usage": { "input_tokens": 3 } } }),
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Hel" } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "lo" } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "citations_delta", "citation": { "url": "u" } } }),
            json!({ "type": "content_block_stop", "index": 0 }),
            json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "tool_use", "id": "toolu_1", "input": {} } }),
            json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "input_json_delta", "partial_json": "{\"n\":" } }),
            json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "input_json_delta", "partial_json": "1.50}" } }),
            json!({ "type": "content_block_start", "index": 2, "content_block": { "type": "thinking", "thinking": "" } }),
            json!({ "type": "content_block_delta", "index": 2, "delta": { "type": "thinking_delta", "thinking": 5 } }),
            json!({ "type": "content_block_delta", "index": 2, "delta": { "type": "signature_delta", "signature": "sig" } }),
            json!({ "type": "content_block_start", "index": 3, "content_block": { "type": "tool_use", "input": {} } }),
            json!({ "type": "content_block_delta", "index": 3, "delta": { "type": "input_json_delta", "partial_json": "{\"a\":" } }),
            json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 2 } }),
            json!({ "type": "message_stop" }),
        ];
        let message = Value::Object(message(&events));
        let want = r#"{"id":"msg_1","type":"message","content":[{"type":"text","text":"Hello","citations":[{"url":"u"}]},{"type":"tool_use","id":"toolu_1","input":{"n":1.50}},{"type":"thinking","thinking":5,"signature":"sig"},{"type":"tool_use","input":{}}],"stop_reason":"tool_use","usage":{"output_tokens":2}}"#;
        assert_eq!(message.to_string(), want);
    }

    #[test]
    fn renderings_are_the_same_json() {
        let message = json!({ "type": "message", "content": [{ "type": "text", "text": "café / 🚀" }], "usage": { "n": [1, {}] } });
        for seed in 0..64 {
            let text = render(&mut Rng(seed), &message);
            let read: Value = serde_json::from_str(&text).expect("valid JSON");
            assert_eq!(read, message, "{text}");
        }
    }
}
