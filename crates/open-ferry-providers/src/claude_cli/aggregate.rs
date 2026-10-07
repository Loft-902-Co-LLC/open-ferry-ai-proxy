//! Claude's stream events made into the one message a call that isn't
//! streamed gets, as Anthropic answers such a call.

use serde_json::{Map, Value};

use crate::json::str_at;

/// The message the events build: `message_start`'s, with each block's
/// deltas joined into it and `message_delta`'s stop reason and usage set.
/// `None` without a `message_start`.
pub(crate) fn message(events: &[Value]) -> Option<Value> {
    let mut message: Option<Value> = None;
    let mut blocks: Vec<Value> = Vec::new();
    let mut inputs: Vec<Option<String>> = Vec::new();
    for event in events {
        match str_at(event, "type").as_str() {
            "message_start" => {
                message = event.get("message").cloned();
                blocks.clear();
                inputs.clear();
            }
            "content_block_start" => {
                let Some(index) = index_of(event) else {
                    continue;
                };
                if blocks.len() <= index {
                    blocks.resize(index + 1, Value::Null);
                    inputs.resize(index + 1, None);
                }
                blocks[index] = event.get("content_block").cloned().unwrap_or(Value::Null);
            }
            "content_block_delta" => {
                let (Some(index), Some(delta)) = (index_of(event), event.get("delta")) else {
                    continue;
                };
                let (Some(block), Some(input)) = (blocks.get_mut(index), inputs.get_mut(index))
                else {
                    continue;
                };
                apply_delta(block, input, delta);
            }
            "content_block_stop" => {
                let Some(index) = index_of(event) else {
                    continue;
                };
                if let (Some(block), Some(input)) = (blocks.get_mut(index), inputs.get_mut(index))
                    && let Some(json) = input.take()
                {
                    block["input"] = serde_json::from_str(&json).unwrap_or(Value::Null);
                }
            }
            "message_delta" => {
                let Some(message) = message.as_mut() else {
                    continue;
                };
                merge(message, event.get("delta"));
                if !message.get("usage").is_some_and(Value::is_object) {
                    message["usage"] = Value::Object(Map::new());
                }
                merge(&mut message["usage"], event.get("usage"));
            }
            _ => {}
        }
    }
    let mut message = message?;
    message["content"] = Value::Array(blocks.into_iter().filter(|b| !b.is_null()).collect());
    Some(message)
}

fn index_of(event: &Value) -> Option<usize> {
    event
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
}

/// Sets each of `from`'s fields on `target`.
fn merge(target: &mut Value, from: Option<&Value>) {
    let (Some(target), Some(from)) = (target.as_object_mut(), from.and_then(Value::as_object))
    else {
        return;
    };
    for (key, value) in from {
        target.insert(key.clone(), value.clone());
    }
}

/// Joins one delta into its block; a tool input's JSON gathers in `input`
/// until the block stops.
fn apply_delta(block: &mut Value, input: &mut Option<String>, delta: &Value) {
    let append = |block: &mut Value, field: &str, text: String| {
        let joined = format!("{}{text}", str_at(block, field));
        block[field] = Value::String(joined);
    };
    match str_at(delta, "type").as_str() {
        "text_delta" => append(block, "text", str_at(delta, "text")),
        "thinking_delta" => append(block, "thinking", str_at(delta, "thinking")),
        "signature_delta" => block["signature"] = Value::String(str_at(delta, "signature")),
        "input_json_delta" => input
            .get_or_insert_with(String::new)
            .push_str(&str_at(delta, "partial_json")),
        "citations_delta" => {
            let Some(citation) = delta.get("citation") else {
                return;
            };
            if !block.get("citations").is_some_and(Value::is_array) {
                block["citations"] = Value::Array(Vec::new());
            }
            if let Some(citations) = block["citations"].as_array_mut() {
                citations.push(citation.clone());
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn joins_the_events_into_a_message() {
        let events = [
            json!({"type": "message_start", "message": {"id": "msg_1", "type": "message", "role": "assistant", "model": "claude-sonnet-5-5", "content": [], "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 3, "output_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Let me "}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "see."}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "ping"}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Hello"}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "citations_delta", "citation": {"cited_text": "x"}}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": ", world"}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "server_tool_use", "id": "srv", "name": "web_search", "input": {}}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"query\":"}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "\"x\"}"}}),
            json!({"type": "content_block_stop", "index": 2}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null}, "usage": {"output_tokens": 9, "input_tokens": 10}}),
            json!({"type": "message_stop"}),
        ];
        assert_eq!(
            message(&events).unwrap(),
            json!({
                "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-sonnet-5-5",
                "content": [
                    {"type": "thinking", "thinking": "Let me see.", "signature": "sig"},
                    {"type": "text", "text": "Hello, world", "citations": [{"cited_text": "x"}]},
                    {"type": "server_tool_use", "id": "srv", "name": "web_search", "input": {"query": "x"}},
                ],
                "stop_reason": "end_turn", "stop_sequence": null,
                "usage": {"input_tokens": 10, "output_tokens": 9},
            })
        );
        assert_eq!(message(&events[1..]), None);
    }
}
