// Ported from CLIProxyAPI internal/runtime/executor/helps/tool_call_ids.go (v8.0.32, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Repairs non-portable caller tool IDs, without changing portable IDs or
//! inventing request or session identities. No process-wide mapping is needed.

use std::collections::{HashMap, HashSet};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;

const PREFIX: &str = "cpa_tid_v1_";

fn portable(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn id_field(block: &Value) -> Option<&'static str> {
    match block.get("type").and_then(Value::as_str) {
        Some("tool_use") => Some("id"),
        Some("tool_result") => Some("tool_use_id"),
        _ => None,
    }
}

// gjson Array() treats a non-array value as a one-item list.
fn items(value: &Value) -> &[Value] {
    match value {
        Value::Array(items) => items,
        _ => std::slice::from_ref(value),
    }
}

fn items_mut(value: &mut Value) -> &mut [Value] {
    match value {
        Value::Array(items) => items,
        _ => std::slice::from_mut(value),
    }
}

pub(super) fn normalize(body: &mut Value) {
    let Some(messages) = body.get_mut("messages") else {
        return;
    };
    let mut used = HashSet::new();
    for message in items(messages) {
        for block in message.get("content").into_iter().flat_map(items) {
            if let Some(id) = id_field(block)
                .and_then(|field| block.get(field))
                .and_then(Value::as_str)
                && portable(id)
            {
                used.insert(id.to_owned());
            }
        }
    }
    let mut mapped = HashMap::<String, String>::new();
    for message in items_mut(messages) {
        for block in message.get_mut("content").into_iter().flat_map(items_mut) {
            let Some(field) = id_field(block) else {
                continue;
            };
            let Some(id) = block.get(field).and_then(Value::as_str) else {
                continue;
            };
            if id.is_empty() || portable(id) {
                continue;
            }
            let replacement = mapped
                .entry(id.to_owned())
                .or_insert_with(|| {
                    let mut alias = format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(id));
                    while used.contains(&alias) {
                        alias.insert(0, '_');
                    }
                    used.insert(alias.clone());
                    alias
                })
                .clone();
            block[field] = Value::String(replacement);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn walks_single_message_and_content_objects() {
        let mut body = json!({"messages":{"content":{"type":"tool_use","id":"call:one"}}});
        normalize(&mut body);
        assert_eq!(body["messages"]["content"]["id"], "cpa_tid_v1_Y2FsbDpvbmU");
        let mut body = json!({"messages":[{"content":{"type":"tool_use","id":"call:one"}},
            {"content":{"type":"tool_result","tool_use_id":"call:one"}}]});
        normalize(&mut body);
        assert_eq!(
            body["messages"][0]["content"]["id"],
            body["messages"][1]["content"]["tool_use_id"]
        );
    }

    #[test]
    fn repairs_calls_and_results_without_colliding_with_portable_ids() {
        let encoded = format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode("call:one"));
        let mut body = json!({"messages":[{"content":[
            {"type":"tool_use","id":"call:one"},
            {"type":"tool_use","id":encoded},
            {"type":"tool_use","id":format!("_{encoded}")},
            {"type":"tool_result","tool_use_id":"call:one"},
            {"type":"text","id":"unchanged:id"},
            {"type":"tool_use","id":""},
            {"type":"tool_use","id":123},
            {"type":"tool_use","id":"こんにちは"}
        ]}]});
        normalize(&mut body);
        let blocks = &body["messages"][0]["content"];
        assert_eq!(blocks[0]["id"], format!("__{encoded}"));
        assert_eq!(blocks[3]["tool_use_id"], blocks[0]["id"]);
        assert_eq!(blocks[1]["id"], encoded);
        assert_eq!(blocks[4]["id"], "unchanged:id");
        assert_eq!(blocks[5]["id"], "");
        assert_eq!(blocks[6]["id"], 123);
        let original = URL_SAFE_NO_PAD
            .decode(
                blocks[7]["id"]
                    .as_str()
                    .unwrap()
                    .strip_prefix(PREFIX)
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(String::from_utf8(original).unwrap(), "こんにちは");
        let once = body.clone();
        normalize(&mut body);
        assert_eq!(body, once);
    }
}
