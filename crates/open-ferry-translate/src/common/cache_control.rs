// Ported from CLIProxyAPI internal/translator/common/cache_control.go (v8.0.38, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Carrying a client's `cache_control` markers onto Claude content blocks.
//!
//! Claude only accepts `{"type": "ephemeral", …}`, so any other marker is
//! dropped.

use serde_json::{Map, Value};

/// The client's marker on `src`, if Claude accepts it.
fn valid(src: &Value) -> Option<&Value> {
    let cache_control = src.get("cache_control")?;
    match cache_control.get("type") {
        Some(Value::String(kind)) if cache_control.is_object() && kind == "ephemeral" => {
            Some(cache_control)
        }
        _ => None,
    }
}

/// Whether a translated Claude block can carry a cache marker.
fn can_host(block: &Map<String, Value>) -> bool {
    match block
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "thinking" | "redacted_thinking" => false,
        "text" => !crate::json::str_of(block.get("text")).trim().is_empty(),
        _ => true,
    }
}

/// `AttachCacheControl`: copies `src`'s marker onto `block`.
pub(crate) fn attach(block: &mut Map<String, Value>, src: &Value) {
    if let Some(cache_control) = valid(src)
        && can_host(block)
    {
        block.insert("cache_control".into(), cache_control.clone());
    }
}

/// [`attach`] for a block known to be an object.
pub(crate) fn attach_to(block: &mut Value, src: &Value) {
    if let Value::Object(block) = block {
        attach(block, src);
    }
}

/// `AttachMessageCacheControl` for array content: a message's marker goes on
/// its last eligible block, unless that block has one of its own.
pub(crate) fn attach_to_last_block(blocks: &mut [Value], message: &Value) {
    let Some(cache_control) = valid(message) else {
        return;
    };
    if let Some(last) = blocks
        .iter_mut()
        .rev()
        .filter_map(Value::as_object_mut)
        .find(|block| can_host(block))
        && !last.contains_key("cache_control")
    {
        last.insert("cache_control".into(), cache_control.clone());
    }
}

/// A dropped part's marker stays at its preceding surviving block, preserving
/// TTL order. Do not search farther back or replace that block's own marker.
pub(crate) fn attach_to_preceding_block(blocks: &mut [Value], source: &Value) {
    if let Some(block) = blocks.last_mut()
        && block.get("cache_control").is_none()
    {
        attach_to(block, source);
    }
}

/// `AttachToolMessageCacheControl`: Claude rejects markers inside a
/// `tool_result`'s content, so the first marker on a part of the tool
/// message's content, or else the message's own, goes on the first
/// `tool_result` block instead.
pub(crate) fn attach_to_tool_result(blocks: &mut [Value], message: &Value) {
    let Some(cache_control) = first_part_marker(message).or_else(|| valid(message)) else {
        return;
    };
    let tool_result = blocks.iter_mut().find_map(|block| match block {
        Value::Object(block)
            if block.get("type").and_then(Value::as_str) == Some("tool_result") =>
        {
            Some(block)
        }
        _ => None,
    });
    if let Some(tool_result) = tool_result {
        tool_result.insert("cache_control".into(), cache_control.clone());
    }
}

/// The first accepted marker on a part of `message`'s content. A message
/// without content is read as its own content.
fn first_part_marker(message: &Value) -> Option<&Value> {
    match message.get("content").unwrap_or(message) {
        Value::Array(parts) => parts.iter().find_map(valid),
        content @ Value::Object(_) => valid(content),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_ephemeral_markers_are_copied() {
        let mut block = Map::new();
        attach(
            &mut block,
            &json!({"cache_control": {"type": "persistent"}}),
        );
        attach(&mut block, &json!({"cache_control": "ephemeral"}));
        assert!(block.is_empty());
        attach(
            &mut block,
            &json!({"cache_control": {"type": "ephemeral", "ttl": "1h"}}),
        );
        assert_eq!(
            block["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"})
        );
    }

    #[test]
    fn a_message_marker_yields_to_the_last_blocks_own() {
        let message = json!({"cache_control": {"type": "ephemeral"}});
        let mut blocks = vec![
            json!({"type": "text", "text": "a"}),
            json!({"type": "text", "text": "b"}),
        ];
        attach_to_last_block(&mut blocks, &message);
        assert_eq!(blocks[1]["cache_control"], json!({"type": "ephemeral"}));
        assert!(blocks[0].get("cache_control").is_none());

        let mut blocks = vec![json!({"type": "text", "cache_control": null})];
        attach_to_last_block(&mut blocks, &message);
        assert_eq!(blocks[0]["cache_control"], json!(null));
    }

    #[test]
    fn preceding_block_markers_never_walk_back_or_replace_a_marker() {
        let source = json!({"cache_control":{"type":"ephemeral","ttl":"5m"}});
        for tail in [
            json!({"type":"thinking"}),
            json!({"type":"redacted_thinking"}),
            json!({"type":"text","text":" "}),
            json!({"type":"text","text":"B","cache_control":{"type":"ephemeral","ttl":"1h"}}),
        ] {
            let mut blocks = vec![json!({"type":"text","text":"A"}), tail.clone()];
            attach_to_preceding_block(&mut blocks, &source);
            assert_eq!(blocks, vec![json!({"type":"text","text":"A"}), tail]);
        }
    }

    #[test]
    fn tool_messages_take_the_first_part_marker() {
        let message = json!({
            "cache_control": {"type": "ephemeral", "ttl": "5m"},
            "content": [{"type": "text"}, {"cache_control": {"type": "ephemeral", "ttl": "1h"}}]
        });
        let mut blocks = vec![json!({"type": "text"}), json!({"type": "tool_result"})];
        attach_to_tool_result(&mut blocks, &message);
        assert_eq!(blocks[1]["cache_control"]["ttl"], "1h");

        let message = json!({"cache_control": {"type": "ephemeral"}});
        let mut blocks = vec![json!({"type": "tool_result"})];
        attach_to_tool_result(&mut blocks, &message);
        assert_eq!(blocks[0]["cache_control"], json!({"type": "ephemeral"}));
    }

    #[test]
    fn markers_skip_blank_and_thinking_blocks() {
        let source = json!({"cache_control":{"type":"ephemeral"}});
        for block in [
            json!({"type":"text","text":" \n\t"}),
            json!({"type":"thinking"}),
            json!({"type":"redacted_thinking"}),
        ] {
            let mut block = block;
            attach_to(&mut block, &source);
            assert!(block.get("cache_control").is_none());
        }
        let mut blocks = vec![
            json!({"type":"text","text":"keep"}),
            json!({"type":"thinking"}),
            json!({"type":"text","text":" "}),
            Value::Null,
        ];
        attach_to_last_block(&mut blocks, &source);
        assert_eq!(blocks[0]["cache_control"], source["cache_control"]);
        assert!(
            blocks[1..]
                .iter()
                .all(|block| block.get("cache_control").is_none())
        );
        attach_to_last_block(&mut blocks[1..], &source);
        assert!(
            blocks[1..]
                .iter()
                .all(|block| block.get("cache_control").is_none())
        );
    }
}
