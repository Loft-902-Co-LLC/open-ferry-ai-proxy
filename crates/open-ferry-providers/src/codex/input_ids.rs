// Ported from CLIProxyAPI internal/runtime/executor/helps/codex_input_ids.go
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Input item IDs as Codex takes them: at most 64 characters, and with the
//! prefix of their type (`msg`, `rs`, `fc`, `ctc`, `ctco`).
//!
//! An ID without its type's prefix gets it. An ID that is too long is cut
//! and given a hash suffix, unless it belongs to a reasoning item with
//! encrypted content, which is dropped instead, as Codex would refuse it.
//! New IDs never collide with IDs already in the input, and the same input
//! always gives the same IDs.

use std::collections::HashMap;
use std::fmt::Write as _;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::json::{set, str_of};

/// The longest ID Codex takes, in characters.
const ID_LIMIT: usize = 64;

/// The ID is in use, and short enough to stay.
const OCCUPIED: u8 = 1 << 0;
/// The ID is in the input as it is.
const PRESERVED: u8 = 1 << 1;

/// Normalizes the IDs of the body's `input` items, drops encrypted reasoning
/// items whose IDs are too long, and shortens other long IDs
/// (`SanitizeCodexInputItemIDs`). Returns whether anything changed.
pub(crate) fn sanitize_input_item_ids(body: &mut Value) -> bool {
    let Some(Value::Array(items)) = body.get_mut("input") else {
        return false;
    };

    let mut states: HashMap<String, u8> = HashMap::with_capacity(items.len());
    for item in items.iter() {
        if should_drop(item) {
            continue;
        }
        let Some(Value::String(original)) = item.get("id") else {
            continue;
        };
        let id = normalize(item, original);
        let mut state = states.get(&id).copied().unwrap_or(0);
        if id == *original {
            state |= PRESERVED;
        }
        if rune_len(&id) <= ID_LIMIT {
            state |= OCCUPIED;
        }
        if state != 0 {
            states.insert(id, state);
        }
    }

    let occupied = |states: &HashMap<String, u8>, id: &str| {
        states.get(id).is_some_and(|state| state & OCCUPIED != 0)
    };
    let mut collisions: HashMap<String, String> = HashMap::new();
    let mut shortened: HashMap<String, String> = HashMap::new();
    let mut changed = false;
    let old = std::mem::take(items);
    let mut rebuilt = Vec::with_capacity(old.len());
    for mut item in old {
        if should_drop(&item) {
            changed = true;
            continue;
        }
        if let Some(Value::String(original)) = item.get("id") {
            let original = original.clone();
            let mut id = normalize(&item, &original);
            if id != original && states.get(&id).is_some_and(|state| state & PRESERVED != 0) {
                id = match collisions.get(&id) {
                    Some(collision) => collision.clone(),
                    None => {
                        let mut attempt = 0;
                        let collision = loop {
                            let candidate = with_hash_suffix(&id, attempt);
                            if !occupied(&states, &candidate) {
                                break candidate;
                            }
                            attempt += 1;
                        };
                        *states.entry(collision.clone()).or_default() |= OCCUPIED;
                        collisions.insert(id, collision.clone());
                        collision
                    }
                };
            }
            if rune_len(&id) > ID_LIMIT {
                id = match shortened.get(&id) {
                    Some(short) => short.clone(),
                    None => {
                        let mut short = with_hash_suffix(&id, 0);
                        let mut attempt = 1;
                        while occupied(&states, &short) {
                            short = with_hash_suffix(&id, attempt);
                            attempt += 1;
                        }
                        *states.entry(short.clone()).or_default() |= OCCUPIED;
                        shortened.insert(id, short.clone());
                        short
                    }
                };
            }
            if id != original && set(&mut item, "id", Value::String(id)) {
                changed = true;
            }
        }
        rebuilt.push(item);
    }
    *items = rebuilt;
    changed
}

fn rune_len(text: &str) -> usize {
    text.chars().count()
}

/// The ID with its item type's prefix (`normalizeCodexInputItemID`).
fn normalize(item: &Value, id: &str) -> String {
    let prefix = match str_of(item.get("type")).as_str() {
        "message" => "msg",
        "reasoning" => "rs",
        "function_call" => "fc",
        "custom_tool_call" => "ctc",
        "custom_tool_call_output" => "ctco",
        _ => return id.to_owned(),
    };
    if id.is_empty() || id.starts_with(prefix) {
        id.to_owned()
    } else {
        format!("{prefix}_{id}")
    }
}

/// Whether the item is a reasoning item with encrypted content and an ID
/// that is too long (`shouldDropCodexEncryptedReasoningItem`).
fn should_drop(item: &Value) -> bool {
    if str_of(item.get("type")) != "reasoning" {
        return false;
    }
    let Some(Value::String(id)) = item.get("id") else {
        return false;
    };
    rune_len(id) > ID_LIMIT
        && matches!(item.get("encrypted_content"), Some(Value::String(content)) if !content.is_empty())
}

/// The ID cut to fit with `_` and 16 hex digits of its hash, the hash
/// varied by `attempt` (`codexInputItemIDWithHashSuffix`). A short ID keeps
/// its whole text.
fn with_hash_suffix(id: &str, attempt: u32) -> String {
    let mut hasher = Sha256::new();
    hasher.update(id.as_bytes());
    if attempt > 0 {
        hasher.update(format!("\0{attempt}").as_bytes());
    }
    let mut suffix = String::from("_");
    for byte in hasher.finalize().iter().take(8) {
        let _ = write!(suffix, "{byte:02x}");
    }
    let mut out: String = id.chars().take(ID_LIMIT - suffix.len()).collect();
    out.push_str(&suffix);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::str_at;
    use serde_json::json;

    fn sanitized(body: &str) -> Value {
        let mut value: Value = serde_json::from_str(body).unwrap();
        sanitize_input_item_ids(&mut value);
        value
    }

    fn id_at(value: &Value, index: usize) -> String {
        str_at(value, &format!("input.{index}.id"))
    }

    #[test]
    fn boundaries() {
        let id64 = "a".repeat(64);
        let id65 = "b".repeat(65);
        let unicode65 = "界".repeat(65);
        let got = sanitized(
            &json!({"input": [{"id": id64}, {"id": id65}, {"id": unicode65}]}).to_string(),
        );
        assert_eq!(id_at(&got, 0), id64);
        assert_eq!(rune_len(&id_at(&got, 1)), 64);
        assert_eq!(rune_len(&id_at(&got, 2)), 64);
    }

    #[test]
    fn normalizes_message_ids() {
        let invalid = "item_74ec40c883248ebb4885ec84";
        let body = json!({"input": [
            {"type": "message", "id": invalid, "role": "user"},
            {"type": "message", "id": "msg-1", "role": "assistant"},
            {"type": "function_call", "id": "item_call", "call_id": "call-1"},
        ]})
        .to_string();
        let first = sanitized(&body);
        assert_eq!(id_at(&first, 0), format!("msg_{invalid}"));
        assert_eq!(id_at(&first, 1), "msg-1");
        assert_eq!(id_at(&first, 2), "fc_item_call");
        assert_eq!(first, sanitized(&body));
    }

    #[test]
    fn normalizes_response_item_ids() {
        let body = json!({"input": [
            {"type": "message", "id": "item_message"},
            {"type": "reasoning", "id": "item_reasoning"},
            {"type": "function_call", "id": "item_function_call", "call_id": "call-1"},
            {"type": "function_call_output", "id": "item_function_call_output", "call_id": "call-1"},
            {"type": "reasoning", "id": "rs-existing"},
            {"type": "function_call", "id": "fc-existing", "call_id": "call-2"},
            {"type": "message", "id": "msg-existing"},
        ]})
        .to_string();
        let got = sanitized(&body);
        let want = [
            "msg_item_message",
            "rs_item_reasoning",
            "fc_item_function_call",
            "item_function_call_output",
            "rs-existing",
            "fc-existing",
            "msg-existing",
        ];
        for (index, want) in want.iter().enumerate() {
            assert_eq!(id_at(&got, index), *want);
        }
        assert_eq!(got, sanitized(&body));
    }

    #[test]
    fn avoids_normalization_collisions() {
        let cases = [
            ("message", "msg_"),
            ("reasoning", "rs_"),
            ("function_call", "fc_"),
            ("custom_tool_call", "ctc_"),
            ("custom_tool_call_output", "ctco_"),
        ];
        for (item_type, prefix) in cases {
            let overlong = "x".repeat(ID_LIMIT - prefix.len() + 1);
            for invalid in ["item_collision".to_owned(), overlong] {
                let prefixed = format!("{prefix}{invalid}");
                for (ids, prefixed_index) in
                    [([&invalid, &prefixed], 1), ([&prefixed, &invalid], 0)]
                {
                    let body = json!({"input": [
                        {"type": item_type, "id": ids[0]},
                        {"type": item_type, "id": ids[1]},
                    ]})
                    .to_string();
                    let first = sanitized(&body);
                    let again = sanitized(&first.to_string());
                    let got = [id_at(&first, 0), id_at(&first, 1)];
                    assert_ne!(got[0], got[1], "{first}");
                    for id in &got {
                        assert!(id.starts_with(prefix), "{id}");
                        assert!(rune_len(id) <= ID_LIMIT, "{id}");
                    }
                    if rune_len(&prefixed) <= ID_LIMIT {
                        assert_eq!(got[prefixed_index], prefixed);
                    }
                    assert_eq!(first, sanitized(&body));
                    assert_eq!(first, again);
                }
            }
        }
    }

    #[test]
    fn normalizes_custom_tool_call_ids() {
        let invalid = "item_44e13caebc1ddf25f1337cbe";
        let got = sanitized(
            &json!({"input": [{"type": "custom_tool_call", "id": invalid, "call_id": "call-1", "name": "lookup", "input": "{}"}]})
                .to_string(),
        );
        assert_eq!(id_at(&got, 0), format!("ctc_{invalid}"));
    }

    #[test]
    fn normalizes_custom_tool_call_output_ids() {
        let invalid = "item_44e13caebc1ddf25f1337cbe_output";
        let body = json!({"input": [
            {"type": "custom_tool_call_output", "id": invalid, "call_id": "call-1", "output": "done"},
            {"type": "custom_tool_call_output", "id": "ctco-existing", "call_id": "call-2", "output": "done"},
        ]})
        .to_string();
        let first = sanitized(&body);
        assert_eq!(id_at(&first, 0), format!("ctco_{invalid}"));
        assert_eq!(id_at(&first, 1), "ctco-existing");
        assert_eq!(first, sanitized(&body));
        assert_eq!(first, sanitized(&first.to_string()));
    }

    #[test]
    fn drops_overlong_encrypted_reasoning_items() {
        let long_reasoning = format!("rs_{}", "a".repeat(64));
        let short_reasoning = format!("rs_{}", "b".repeat(48));
        let long_call = "call-item-".repeat(8);
        let got = sanitized(
            &json!({"input": [
                {"type": "message", "id": "msg-1", "role": "user", "content": "before"},
                {"type": "reasoning", "id": long_reasoning, "encrypted_content": "gAAAA-encrypted", "summary": [{"type": "summary_text", "text": "drop me"}]},
                {"type": "reasoning", "id": short_reasoning, "encrypted_content": "gAAAA-encrypted", "summary": []},
                {"type": "function_call", "id": long_call, "call_id": "call-1", "name": "lookup", "arguments": "{}"},
            ]})
            .to_string(),
        );
        assert_eq!(got["input"].as_array().map(Vec::len), Some(3));
        assert_eq!(id_at(&got, 0), "msg-1");
        assert_eq!(id_at(&got, 1), short_reasoning);
        let call = id_at(&got, 2);
        assert_ne!(call, long_call);
        assert_eq!(rune_len(&call), 64);
    }

    #[test]
    fn shortens_overlong_reasoning_without_encrypted_content() {
        let long_reasoning = format!("rs_{}", "a".repeat(64));
        for extra in [
            json!({}),
            json!({"encrypted_content": ""}),
            json!({"encrypted_content": null}),
        ] {
            let mut item = json!({"type": "reasoning", "id": long_reasoning});
            for (key, value) in extra.as_object().unwrap() {
                item[key] = value.clone();
            }
            item["summary"] = json!([]);
            let got = sanitized(&json!({"input": [item]}).to_string());
            assert_eq!(got["input"].as_array().map(Vec::len), Some(1));
            let id = id_at(&got, 0);
            assert_ne!(id, long_reasoning);
            assert_eq!(rune_len(&id), 64);
        }
    }

    #[test]
    fn avoids_existing_id_collision() {
        let long = "grok-item-".repeat(10);
        let colliding = with_hash_suffix(&long, 0);
        let body = json!({"input": [{"id": long}, {"id": colliding}]}).to_string();
        let first = sanitized(&body);
        let short = id_at(&first, 0);
        assert_ne!(short, colliding);
        assert!(rune_len(&short) <= 64);
        assert_eq!(id_at(&first, 1), colliding);
        assert_eq!(id_at(&sanitized(&body), 0), short);
    }

    #[test]
    fn leaves_unsupported_payloads_unchanged() {
        for body in [
            json!("not-json"),
            json!({"input": {"id": "item-1"}}),
            json!({"input": [1, {"id": 2}, {"id": "item-1"}]}),
        ] {
            let mut value = body.clone();
            assert!(!sanitize_input_item_ids(&mut value));
            assert_eq!(value, body);
        }
    }

    #[test]
    fn hash_suffix_is_sha256() {
        // sha256("abc") starts ba7816bf8f01cfea.
        assert_eq!(with_hash_suffix("abc", 0), "abc_ba7816bf8f01cfea");
        let attempt1 = with_hash_suffix("abc", 1);
        assert!(
            attempt1.starts_with("abc_") && attempt1.len() == 20,
            "{attempt1}"
        );
        assert_ne!(attempt1, with_hash_suffix("abc", 0));
    }
}
