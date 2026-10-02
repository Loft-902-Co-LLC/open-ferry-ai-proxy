// Ported from CLIProxyAPI internal/translator/common/responses.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Helpers for reading OpenAI Responses requests, shared by translators that
//! turn them into other providers' formats.

use std::borrow::Cow;
use std::collections::HashMap;

use serde_json::Value;

use crate::json::str_of;

/// `ExtractResponsesCallID`: the tool call an input item belongs to, from
/// `call_id`, `tool_call_id`, `callId` or else `id`. An `fco_` ID names the
/// output item itself, not a call, so it gives `""`.
pub(crate) fn extract_responses_call_id(item: &Value) -> String {
    for key in ["call_id", "tool_call_id", "callId"] {
        let id = str_of(item.get(key));
        if !id.trim().is_empty() {
            return id.trim().to_owned();
        }
    }
    let id = str_of(item.get("id"));
    let id = id.trim();
    if id.starts_with("fco_") {
        return String::new();
    }
    id.to_owned()
}

fn is_tool_output(item: &Value) -> bool {
    matches!(
        &*str_of(item.get("type")),
        "function_call_output" | "custom_tool_call_output"
    )
}

/// `NormalizeResponsesToolCallOutputs`: gives each tool call output the
/// `call_id` of the call it answers, pairing each run of outputs with the
/// calls still waiting for one:
///
/// 1. An output that names a waiting call answers it.
/// 2. An output without an ID answers the first waiting call of the same name.
/// 3. Any other output without an ID answers the first waiting call whose name
///    doesn't differ from its own.
///
/// Passes 2 and 3 skip a call that an output later in the conversation names.
/// An output naming a call that isn't waiting is left alone.
pub(crate) fn normalize_responses_tool_call_outputs(items: &[Value]) -> Vec<Cow<'_, Value>> {
    let mut normalized: Vec<Cow<'_, Value>> = items.iter().map(Cow::Borrowed).collect();

    let mut explicit_output_counts: HashMap<String, i64> = HashMap::new();
    for item in items.iter().filter(|item| is_tool_output(item)) {
        let id = extract_responses_call_id(item);
        if !id.is_empty() {
            *explicit_output_counts.entry(id).or_default() += 1;
        }
    }
    let count = |counts: &HashMap<String, i64>, id: &str| counts.get(id).copied().unwrap_or(0);

    let mut pending_call_ids: Vec<String> = Vec::new();
    let mut pending_call_names: HashMap<String, String> = HashMap::new();

    let mut i = 0;
    while i < items.len() {
        match &*str_of(items[i].get("type")) {
            "function_call" | "custom_tool_call" => {
                let call_id = extract_responses_call_id(&items[i]);
                if !call_id.is_empty() {
                    let name = str_of(items[i].get("name")).into_owned();
                    pending_call_names.insert(call_id.clone(), name);
                    pending_call_ids.push(call_id);
                }
                i += 1;
            }
            "function_call_output" | "custom_tool_call_output" => {
                let start = i;
                while i < items.len() && is_tool_output(&items[i]) {
                    i += 1;
                }
                if pending_call_ids.is_empty() {
                    continue;
                }
                let outputs = &items[start..i];
                let output_ids: Vec<String> =
                    outputs.iter().map(extract_responses_call_id).collect();
                let output_name =
                    |index: usize| str_of(outputs[index].get("name")).trim().to_owned();
                let mut used = vec![false; outputs.len()];
                let mut matched: Vec<Option<usize>> = vec![None; pending_call_ids.len()];

                // Pass 1: the output names the call.
                for (pending, pending_id) in pending_call_ids.iter().enumerate() {
                    if let Some(index) = (0..outputs.len())
                        .find(|&index| !used[index] && output_ids[index] == *pending_id)
                    {
                        used[index] = true;
                        matched[pending] = Some(index);
                        *explicit_output_counts
                            .entry(pending_id.clone())
                            .or_default() -= 1;
                    }
                }

                // Pass 2: an output without an ID has the call's name.
                for (pending, pending_id) in pending_call_ids.iter().enumerate() {
                    if matched[pending].is_some() || count(&explicit_output_counts, pending_id) > 0
                    {
                        continue;
                    }
                    let expected_name = &pending_call_names[pending_id];
                    if expected_name.is_empty() {
                        continue;
                    }
                    if let Some(index) = (0..outputs.len()).find(|&index| {
                        !used[index]
                            && output_ids[index].is_empty()
                            && output_name(index) == *expected_name
                    }) {
                        used[index] = true;
                        matched[pending] = Some(index);
                    }
                }

                // Pass 3: first in, first out, unless the names differ.
                for (pending, pending_id) in pending_call_ids.iter().enumerate() {
                    if matched[pending].is_some() || count(&explicit_output_counts, pending_id) > 0
                    {
                        continue;
                    }
                    let expected_name = &pending_call_names[pending_id];
                    if let Some(index) = (0..outputs.len()).find(|&index| {
                        if used[index] || !output_ids[index].is_empty() {
                            return false;
                        }
                        let name = output_name(index);
                        name.is_empty() || expected_name.is_empty() || name == *expected_name
                    }) {
                        used[index] = true;
                        matched[pending] = Some(index);
                    }
                }

                let mut remaining = Vec::new();
                for (pending_id, matched) in pending_call_ids.into_iter().zip(matched) {
                    let Some(index) = matched else {
                        remaining.push(pending_id);
                        continue;
                    };
                    let output = &outputs[index];
                    if str_of(output.get("call_id")) != pending_id.as_str() {
                        let mut output = output.clone();
                        if let Some(fields) = output.as_object_mut() {
                            fields.insert("call_id".to_owned(), pending_id.into());
                        }
                        normalized[start + index] = Cow::Owned(output);
                    }
                }
                pending_call_ids = remaining;
            }
            _ => i += 1,
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn call_ids_prefer_dedicated_fields() {
        let cases = [
            (json!({"call_id": "call_1"}), "call_1"),
            (json!({"tool_call_id": "call_2"}), "call_2"),
            (json!({"callId": "call_3"}), "call_3"),
            (json!({"id": "call_4"}), "call_4"),
            (json!({"id": "item_5", "call_id": "call_5"}), "call_5"),
            (json!({"id": "item_6", "tool_call_id": "call_6"}), "call_6"),
            (json!({"id": "item_7", "callId": "call_7"}), "call_7"),
            (json!({"output": "result"}), ""),
            (
                json!({"id": "fco_01a08664-2d16-7a91-8ab2-2eccd49e4c3e"}),
                "",
            ),
            (
                json!({"id": "fco_01a08664-2d16-7a91-8ab2-2eccd49e4c3e", "call_id": "call_1"}),
                "call_1",
            ),
            (json!({"call_id": "  ", "id": " call_8 "}), "call_8"),
        ];
        for (item, want) in cases {
            assert_eq!(extract_responses_call_id(&item), want, "{item}");
        }
    }

    fn call_ids(items: &Value) -> Vec<String> {
        normalize_responses_tool_call_outputs(items.as_array().unwrap())
            .iter()
            .map(|item| str_of(item.get("call_id")).into_owned())
            .collect()
    }

    #[test]
    fn an_output_named_later_keeps_its_call() {
        let items = json!([
            {"type": "function_call", "call_id": "call_a", "name": "tool_a"},
            {"type": "function_call", "call_id": "call_b", "name": "tool_b"},
            {"type": "function_call_output", "output": "result_b"},
            {"type": "function_call_output", "tool_call_id": "call_a", "output": "result_a"}
        ]);
        assert_eq!(call_ids(&items), ["call_a", "call_b", "call_b", "call_a"]);
    }

    #[test]
    fn a_named_output_keeps_its_call_across_messages() {
        let items = json!([
            {"type": "function_call", "call_id": "call_a", "name": "tool_a"},
            {"type": "function_call", "call_id": "call_b", "name": "tool_b"},
            {"type": "function_call_output", "output": "result_b"},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "intervening"}]},
            {"type": "function_call_output", "call_id": "call_a", "output": "result_a"}
        ]);
        assert_eq!(
            call_ids(&items),
            ["call_a", "call_b", "call_b", "", "call_a"]
        );
    }

    #[test]
    fn an_output_for_another_call_is_left_alone() {
        let items = json!([
            {"type": "function_call", "call_id": "call_a", "name": "tool_a"},
            {"type": "function_call_output", "call_id": "call_other", "output": "result_other"}
        ]);
        assert_eq!(call_ids(&items), ["call_a", "call_other"]);
    }

    #[test]
    fn an_output_item_id_is_not_a_call_id() {
        let items = json!([
            {"type": "function_call", "call_id": "call_1788961125480214178_817", "name": "Bash"},
            {"type": "function_call_output", "id": "fco_01a08664-2d16-7a91-8ab2-2eccd49e4c3e", "output": "result"}
        ]);
        assert_eq!(
            call_ids(&items),
            [
                "call_1788961125480214178_817",
                "call_1788961125480214178_817"
            ]
        );

        let items = json!([
            {"type": "function_call", "call_id": "call_a", "name": "tool_a"},
            {"type": "function_call", "call_id": "call_b", "name": "tool_b"},
            {"type": "function_call_output", "id": "fco_b", "output": "result_b"},
            {"type": "function_call_output", "call_id": "call_a", "output": "result_a"}
        ]);
        assert_eq!(call_ids(&items), ["call_a", "call_b", "call_b", "call_a"]);
    }

    #[test]
    fn outputs_match_by_name_before_order() {
        let items = json!([
            {"type": "function_call", "call_id": "call_a", "name": "tool_a"},
            {"type": "custom_tool_call", "call_id": "call_b", "name": "tool_b"},
            {"type": "custom_tool_call_output", "name": " tool_b ", "output": "b"},
            {"type": "function_call_output", "name": "other", "output": "x"},
            {"type": "function_call_output", "output": "a"}
        ]);
        let normalized = normalize_responses_tool_call_outputs(items.as_array().unwrap());
        assert_eq!(normalized[2]["call_id"], "call_b");
        assert!(normalized[3].get("call_id").is_none());
        assert_eq!(normalized[4]["call_id"], "call_a");
        assert!(matches!(normalized[3], Cow::Borrowed(_)));
    }
}
