// Ported from CLIProxyAPI internal/translator/common/openai_tools.go
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Helpers for Chat Completions tool calls, shared by translators that write
//! Chat Completions requests.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::json::str_of;

/// An assistant message with tool calls, as `assistantRecord`.
struct Assistant {
    index: usize,
    call_ids: Vec<String>,
    has_empty_id: bool,
}

/// `AlignOpenAIToolCallMessages`: moves each assistant message's tool results
/// to right after it, keeping their relative order. Only an assistant whose
/// every call has exactly one result, later in the conversation, is aligned.
/// A call ID that is empty, repeated, answered more than once or listed in
/// `extra_ambiguous_ids` leaves its assistant's results where they are.
pub(crate) fn align_openai_tool_call_messages<'a>(
    messages: Vec<Value>,
    extra_ambiguous_ids: impl IntoIterator<Item = &'a str>,
) -> Vec<Value> {
    if messages.len() <= 1 {
        return messages;
    }

    let mut ambiguous: HashSet<String> = extra_ambiguous_ids
        .into_iter()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect();
    let mut assistants: Vec<Assistant> = Vec::new();
    let mut assistant_by_call_id: HashMap<String, usize> = HashMap::new();
    let mut tool_messages_by_call_id: HashMap<String, Vec<usize>> = HashMap::new();

    for (i, message) in messages.iter().enumerate() {
        match &*str_of(message.get("role")) {
            "assistant" => {
                let Some(Value::Array(calls)) = message.get("tool_calls") else {
                    continue;
                };
                if calls.is_empty() {
                    continue;
                }
                let mut call_ids = Vec::with_capacity(calls.len());
                let mut has_empty_id = false;
                for call in calls {
                    let id = str_of(call.get("id")).into_owned();
                    if id.is_empty() {
                        // An empty ID can't be matched safely.
                        ambiguous.insert(String::new());
                        has_empty_id = true;
                        continue;
                    }
                    if assistant_by_call_id.insert(id.clone(), i).is_some() {
                        ambiguous.insert(id.clone());
                    }
                    call_ids.push(id);
                }
                if !call_ids.is_empty() || has_empty_id {
                    assistants.push(Assistant {
                        index: i,
                        call_ids,
                        has_empty_id,
                    });
                }
            }
            "tool" => {
                let id = str_of(message.get("tool_call_id")).into_owned();
                if id.is_empty() {
                    ambiguous.insert(id);
                    continue;
                }
                let indices = tool_messages_by_call_id.entry(id.clone()).or_default();
                indices.push(i);
                if indices.len() > 1 {
                    ambiguous.insert(id);
                }
            }
            _ => {}
        }
    }

    // Each assistant to align, with its results' indices in order.
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    'assistants: for assistant in &assistants {
        if assistant.has_empty_id {
            continue;
        }
        let mut tool_indices = Vec::with_capacity(assistant.call_ids.len());
        for id in &assistant.call_ids {
            if ambiguous.contains(id) {
                continue 'assistants;
            }
            // Exactly one result must answer the call, after it.
            match tool_messages_by_call_id.get(id).map(Vec::as_slice) {
                Some(&[index]) if index > assistant.index => tool_indices.push(index),
                _ => continue 'assistants,
            }
        }
        tool_indices.sort_unstable();
        let adjacent = tool_indices
            .iter()
            .enumerate()
            .all(|(offset, &index)| index == assistant.index + offset + 1);
        if !adjacent {
            groups.insert(assistant.index, tool_indices);
        }
    }
    if groups.is_empty() {
        return messages;
    }

    let moved: HashSet<usize> = groups.values().flatten().copied().collect();
    let mut slots: Vec<Option<Value>> = messages.into_iter().map(Some).collect();
    let mut aligned = Vec::with_capacity(slots.len());
    for i in 0..slots.len() {
        if moved.contains(&i) {
            continue;
        }
        if let Some(message) = slots[i].take() {
            aligned.push(message);
        }
        if let Some(tool_indices) = groups.get(&i) {
            aligned.extend(tool_indices.iter().filter_map(|&index| slots[index].take()));
        }
    }
    aligned
}

#[cfg(test)]
mod tests;
