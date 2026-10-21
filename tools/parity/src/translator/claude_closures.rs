//! Compare whole independent closures from Claude's Go map loops. Each input
//! event stays separate, so closures cannot move across provider events.

use super::{sse_chunk, sse_frames};
use serde_json::Value;

pub(super) fn closes_calls(event: &str) -> bool {
    event
        .strip_prefix("data:")
        .and_then(|data| open_ferry_translate::json::first_string_field(data.as_bytes(), "type"))
        .is_some_and(|kind| matches!(kind.as_str(), "message_stop" | "content_block_start"))
}

fn tool_frame(frame: &Value) -> bool {
    match frame["event"].as_str() {
        Some("response.output_item.added" | "response.output_item.done") => matches!(
            frame["data"]["item"]["type"].as_str(),
            Some("function_call" | "custom_tool_call" | "tool_search_call")
        ),
        Some(
            "response.function_call_arguments.delta"
            | "response.function_call_arguments.done"
            | "response.custom_tool_call_input.delta"
            | "response.custom_tool_call_input.done",
        ) => true,
        _ => false,
    }
}

fn identity(frame: &Value) -> Option<(u64, &str)> {
    let data = &frame["data"];
    let id = data
        .get("item_id")
        .or_else(|| data.pointer("/item/id"))?
        .as_str()?;
    Some((data["output_index"].as_u64()?, id))
}

fn groups(frames: &[Value]) -> Option<Vec<usize>> {
    let first_seq = frames.first()?["data"]["sequence_number"].as_u64()?;
    for (i, frame) in frames.iter().enumerate() {
        if frame["data"]["sequence_number"].as_u64()? != first_seq.checked_add(i as u64)?
            || frame["event"] != frame["data"]["type"]
        {
            return None;
        }
    }
    let mut groups = Vec::new();
    let mut start = 0;
    while start < frames.len() {
        let Some(done) =
            (start..frames.len()).find(|&i| frames[i]["event"] == "response.output_item.done")
        else {
            // A block-start may add the next tool after closing previous ones.
            // Leave that unfinished tail exactly where it arrived.
            break;
        };
        let (index, id) = identity(&frames[done])?;
        if groups.iter().any(|(seen, _, _)| *seen == index) {
            return None;
        }
        let kind = frames[done]["data"]["item"]["type"].as_str()?;
        let mut at = start;
        if frames[at]["event"] == "response.output_item.added" {
            if frames[at]["data"]["item"]["type"] != kind {
                return None;
            }
            at += 1;
        }
        if kind != "tool_search_call" {
            let prefix = if kind == "custom_tool_call" {
                "response.custom_tool_call_input"
            } else {
                "response.function_call_arguments"
            };
            let delta = format!("{prefix}.delta");
            while at < done && frames[at]["event"] == delta {
                at += 1;
            }
            if at >= done || frames[at]["event"] != format!("{prefix}.done") {
                return None;
            }
            at += 1;
        }
        if at != done
            || frames[start..=done]
                .iter()
                .any(|frame| identity(frame) != Some((index, id)))
        {
            return None;
        }
        groups.push((index, start, done + 1));
        start = done + 1;
    }
    if groups.len() < 2 {
        return None;
    }
    groups.sort_unstable_by_key(|&(index, _, _)| index);
    Some(
        groups
            .into_iter()
            .flat_map(|(_, start, end)| start..end)
            .chain(start..frames.len())
            .collect(),
    )
}

fn order(frames: &[Value]) -> Vec<usize> {
    let mut order: Vec<_> = (0..frames.len()).collect();
    let mut start = 0;
    while start < frames.len() {
        if !tool_frame(&frames[start]) {
            start += 1;
            continue;
        }
        let end = (start..frames.len())
            .find(|&i| !tool_frame(&frames[i]))
            .unwrap_or(frames.len());
        if let Some(sorted) = groups(&frames[start..end]) {
            for (slot, index) in order[start..end].iter_mut().zip(sorted) {
                *slot = start + index;
            }
        }
        start = end;
    }
    order
}

pub(super) fn normalize(value: &mut Value) {
    let Some(frames) = value.as_array_mut() else {
        return;
    };
    let original = frames.clone();
    for (slot, index) in order(&original).into_iter().enumerate() {
        if slot != index {
            frames[slot] = original[index].clone();
            frames[slot]["data"]["sequence_number"] =
                original[slot]["data"]["sequence_number"].clone();
        }
    }
}

/// Only complete single-frame chunks can move; split frames stay observable.
pub(super) fn chunk_shape(chunks: &[String]) -> Option<Value> {
    let mut frames = Vec::new();
    for chunk in chunks {
        let value = sse_frames(chunk);
        let single = value.as_array()?;
        if single.len() != 1 || single[0].get("event").is_none() {
            return None;
        }
        frames.push(single[0].clone());
    }
    Some(
        order(&frames)
            .into_iter()
            .map(|i| sse_chunk(&chunks[i]))
            .collect(),
    )
}
#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{cases::Case, translator::Translator};

    fn frames(reverse: bool) -> Vec<Value> {
        let call = |index: u64, added: bool| {
            let id = format!("fc_{index}");
            let item =
                json!({"id":id,"type":"function_call","arguments":"{}","status":"completed"});
            let mut group = Vec::new();
            if added {
                group.push(
                    json!({"type":"response.output_item.added","output_index":index,"item":item}),
                );
            }
            group.push(json!({"type":"response.function_call_arguments.done","output_index":index,"item_id":id,"arguments":"{}"}));
            group
                .push(json!({"type":"response.output_item.done","output_index":index,"item":item}));
            group
        };
        let (one, two) = (call(1, true), call(2, false));
        let mut data = if reverse {
            [two, one].concat()
        } else {
            [one, two].concat()
        };
        data.push(json!({"type":"response.completed","response":{"output":[]}}));
        data.into_iter()
            .enumerate()
            .map(|(i, mut data)| {
                data["sequence_number"] = (i + 10).into();
                json!({"event":data["type"],"data":data})
            })
            .collect()
    }

    fn chunks(frames: &[Value]) -> Vec<String> {
        frames
            .iter()
            .map(|frame| {
                format!(
                    "event: {}\ndata: {}\n\n",
                    frame["event"].as_str().unwrap(),
                    frame["data"]
                )
            })
            .collect()
    }

    fn numbered(mut frames: Vec<Value>) -> Value {
        for (i, frame) in frames.iter_mut().enumerate() {
            frame["data"]["sequence_number"] = (i + 10).into();
        }
        frames.into()
    }

    fn closure(index: u64, kind: &str) -> Vec<Value> {
        let id = format!("item_{index}");
        let item = json!({"type":kind,"id":id,"arguments":"{}","input":"patch"});
        let mut data =
            vec![json!({"type":"response.output_item.added","output_index":index,"item":item})];
        if kind != "tool_search_call" {
            let prefix = if kind == "custom_tool_call" {
                "response.custom_tool_call_input"
            } else {
                "response.function_call_arguments"
            };
            data.push(json!({"type":format!("{prefix}.delta"),"output_index":index,"item_id":id,"delta":"{}"}));
            data.push(json!({"type":format!("{prefix}.done"),"output_index":index,"item_id":id,"arguments":"{}"}));
        }
        data.push(json!({"type":"response.output_item.done","output_index":index,"item":item}));
        data.into_iter()
            .map(|data| json!({"event":data["type"],"data":data}))
            .collect()
    }

    #[test]
    fn closing_events_use_the_first_lenient_type_field() {
        for event in [
            r#"data: {"type":"message_stop"} trailing"#,
            r#"data:{"type":"content_block_start","type":"other"}"#,
            r#"data:{"type":"message_stop","unfinished":"#,
        ] {
            assert!(closes_calls(event), "{event}");
        }
        for event in [
            r#"data:{"type":"other","type":"message_stop"}"#,
            r#"data:0 {"type":"message_stop"}"#,
            r#"data:[] {"type":"message_stop"}"#,
        ] {
            assert!(!closes_calls(event), "{event}");
        }
    }

    #[test]
    fn buffered_deltas_custom_and_search_closures_match_every_map_order() {
        let calls = [
            closure(1, "function_call"),
            closure(2, "custom_tool_call"),
            closure(3, "tool_search_call"),
        ];
        let mut expected = numbered(calls.concat());
        normalize(&mut expected);
        for permutation in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let mut value = numbered(
                permutation
                    .into_iter()
                    .flat_map(|i| calls[i].clone())
                    .collect(),
            );
            normalize(&mut value);
            assert_eq!(expected, value, "{permutation:?}");
        }
    }

    #[test]
    fn interleaved_closures_never_equal_contiguous_groups() {
        let unnamed = closure(2, "function_call");
        let named = closure(1, "function_call")[2..].to_vec();
        let terminal =
            vec![json!({"event":"response.completed","data":{"type":"response.completed"}})];
        let mut contiguous = numbered([unnamed.clone(), named.clone(), terminal.clone()].concat());
        let mut interleaved = numbered(
            [
                unnamed[..2].to_vec(),
                named,
                unnamed[2..].to_vec(),
                terminal,
            ]
            .concat(),
        );
        normalize(&mut contiguous);
        normalize(&mut interleaved);
        assert_ne!(contiguous, interleaved);
    }

    #[test]
    fn both_map_loops_accept_data_prefix_without_a_space() {
        let one = closure(1, "function_call");
        let two = closure(2, "custom_tool_call");
        for event in [
            "data:{\"type\":\"message_stop\"}",
            "data:\t {\"type\":\"content_block_start\"}",
        ] {
            let case = Case::response("closures", "{}", vec![event.into()])
                .with_options(json!({"from":"claude","to":"openai-response"}));
            let read = |calls: Vec<Value>| {
                let frames = numbered(calls);
                let chunks = chunks(frames.as_array().unwrap());
                let report = json!({"events":[chunks],"finish":[],"failed":false});
                Translator::RegistryStream
                    .read(&case, report.to_string().as_bytes())
                    .unwrap()
            };
            assert_eq!(
                read([one.clone(), two.clone()].concat()),
                read([two.clone(), one.clone()].concat()),
                "{event}"
            );
        }
    }

    #[test]
    fn native_closures_cannot_move_across_provider_events() {
        let case = Case::response(
            "boundaries",
            "{}",
            vec!["data:{\"type\":\"message_stop\"}".into(); 2],
        );
        let one = closure(1, "function_call");
        let two = closure(2, "function_call");
        let read = |calls: Vec<Value>| {
            let frames = numbered(calls);
            let chunks = chunks(frames.as_array().unwrap());
            let batches = vec![chunks[..4].concat(), chunks[4..].concat(), String::new()];
            Translator::ClaudeResponsesStream
                .read(&case, serde_json::to_string(&batches).unwrap().as_bytes())
                .unwrap()
        };
        assert_ne!(
            read([one.clone(), two.clone()].concat()),
            read([two, one].concat())
        );
    }

    #[test]
    fn repeated_item_ids_keep_distinct_output_index_closures() {
        let one = closure(1, "function_call");
        let mut two = closure(2, "function_call");
        for frame in &mut two {
            if frame["data"].get("item_id").is_some() {
                frame["data"]["item_id"] = "item_1".into();
            } else {
                frame["data"]["item"]["id"] = "item_1".into();
            }
        }
        let mut forward = numbered([one.clone(), two.clone()].concat());
        let mut reverse = numbered([two, one].concat());
        normalize(&mut forward);
        normalize(&mut reverse);
        // Go maps these calls by block index, even when the provider repeats
        // an ID. The output index still separates their complete groups.
        assert_eq!(forward, reverse);
    }

    #[test]
    fn only_independent_terminal_closure_groups_are_reordered() {
        let mut forward = Value::Array(frames(false));
        let mut reverse = Value::Array(frames(true));
        normalize(&mut forward);
        normalize(&mut reverse);
        assert_eq!(forward, reverse);
        for (path, value) in [
            ("/0/data/item_id", json!("wrong")),
            ("/0/data/arguments", json!("{\"changed\":true}")),
            ("/0/data/output_index", json!(99)),
            ("/0/data/sequence_number", json!(999)),
            ("/1/data/item/status", json!("changed")),
            ("/5/event", json!("response.output_text.done")),
        ] {
            let mut changed = Value::Array(frames(true));
            *changed
                .pointer_mut(path)
                .unwrap_or_else(|| panic!("missing {path}")) = value;
            normalize(&mut changed);
            assert_ne!(forward, changed, "must retain {path}");
        }
        let mut reversed_within = frames(true);
        reversed_within.swap(0, 1);
        let mut reversed_within = Value::Array(reversed_within);
        normalize(&mut reversed_within);
        assert_ne!(forward, reversed_within);
    }

    #[test]
    fn registry_closure_shapes_preserve_event_and_chunk_boundaries() {
        let case = Case::response(
            "closures",
            "{}",
            vec!["data: {\"type\":\"message_stop\"}".into()],
        )
        .with_options(json!({"from":"claude","to":"openai-response"}));
        let read = |events: Vec<Vec<String>>| {
            let report = json!({"events":events,"finish":[],"failed":false});
            Translator::RegistryStream
                .read(&case, report.to_string().as_bytes())
                .unwrap()
        };
        let forward = chunks(&frames(false));
        let reverse = chunks(&frames(true));
        assert_eq!(read(vec![forward.clone()]), read(vec![reverse.clone()]));
        let mut joined = reverse.clone();
        let second = joined.remove(1);
        joined[0].push_str(&second);
        assert_ne!(read(vec![forward.clone()]), read(vec![joined]));
        assert_ne!(
            read(vec![forward]),
            read(vec![reverse[..2].to_vec(), reverse[2..].to_vec()])
        );
    }
}
