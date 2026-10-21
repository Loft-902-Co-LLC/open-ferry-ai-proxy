//! Claude's message_stop closes unfinished functions by iterating a Go map.
//! Compare independent, complete closure groups in output-index order, retaining
//! their contents and internal order. No other stream events are reordered.

use serde_json::Value;

use super::{sse_chunk, sse_frames};

fn function_frame(frame: &Value) -> bool {
    match frame["event"].as_str() {
        Some("response.output_item.added" | "response.output_item.done") => {
            frame["data"]["item"]["type"] == "function_call"
        }
        Some("response.function_call_arguments.done") => true,
        _ => false,
    }
}

/// Returns a permutation only for complete closure groups immediately before
/// a terminal response, with unique indices and consecutive sequence numbers.
fn order(frames: &[Value]) -> Vec<usize> {
    let mut order: Vec<_> = (0..frames.len()).collect();
    for end in 0..frames.len() {
        if !matches!(
            frames[end]["event"].as_str(),
            Some("response.completed" | "response.incomplete")
        ) {
            continue;
        }
        let start = (0..end)
            .rev()
            .find(|&i| !function_frame(&frames[i]))
            .map_or(0, |i| i + 1);
        if let Some(sorted) = groups(&frames[start..end]) {
            for (slot, index) in order[start..end].iter_mut().zip(sorted) {
                *slot = start + index;
            }
        }
    }
    order
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
        let mut done = start;
        if frames[done]["event"] == "response.output_item.added" {
            done += 1;
        }
        let args = frames.get(done)?;
        let item = frames.get(done + 1)?;
        let index = args["data"]["output_index"].as_u64()?;
        let id = args["data"]["item_id"].as_str()?;
        if args["event"] != "response.function_call_arguments.done"
            || item["event"] != "response.output_item.done"
            || item["data"]["item"]["type"] != "function_call"
            || item["data"]["output_index"].as_u64()? != index
            || item["data"]["item"]["id"].as_str()? != id
            || (done != start
                && (frames[start]["data"]["output_index"].as_u64()? != index
                    || frames[start]["data"]["item"]["id"].as_str()? != id))
            || groups.iter().any(|(seen, _, _)| *seen == index)
        {
            return None;
        }
        groups.push((index, start, done + 2));
        start = done + 2;
    }
    if groups.len() < 2 {
        return None;
    }
    groups.sort_unstable_by_key(|&(index, _, _)| index);
    Some(
        groups
            .into_iter()
            .flat_map(|(_, start, end)| start..end)
            .collect(),
    )
}

pub(super) fn normalize(value: &mut Value) {
    let Some(frames) = value.as_array_mut() else {
        return;
    };
    let original = frames.clone();
    for (slot, index) in order(&original).into_iter().enumerate() {
        if slot != index {
            frames[slot] = original[index].clone();
            // Only consecutive sequence slots were admitted above. Keep those
            // slots rather than treating sequence numbers as an ignored field.
            frames[slot]["data"]["sequence_number"] =
                original[slot]["data"]["sequence_number"].clone();
        }
    }
}

/// Keep registry chunk boundaries: reorder only complete, single-frame chunks
/// within the output from one message_stop, never across input events.
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
