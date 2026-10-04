// Ported from CLIProxyAPI internal/translator/openai/interactions/responses/apply_patch_identity_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! An `apply_patch` call whose IDs come late: nothing is published until
//! both are known, the buffered fragments are then replayed as they came,
//! and evidence that contradicts the call fails the response.
//!
//! Dropped or changed tests: none.

use std::collections::HashMap;

use serde_json::Map;

use super::*;

// Ports TestInteractionsApplyPatchNamedLateIdentity.
#[test]
fn named_late_identity() {
    for first in [
        "item",
        "call",
        "neither",
        "neither-item-first",
        "neither-call-first",
    ] {
        for boundary in ["stop", "terminal"] {
            let name = format!("{first}-{boundary}");
            let mut stream = patch_stream();
            let mut step = Map::new();
            step.insert("type".into(), json!("function_call"));
            step.insert("name".into(), json!("functions__apply_patch"));
            if first == "item" {
                step.insert("id".into(), json!("item_2"));
            }
            if first == "call" {
                step.insert("call_id".into(), json!("call_2"));
            }
            let events = send(
                &mut stream,
                patch_step("step.start", Value::Object(step.clone())),
            );
            assert!(
                events.is_empty(),
                "{name}: published provisional IDs: {events:?}"
            );
            let args = patch_arguments(PATCH_TEXT);
            for fragment in [&args[..15], &args[15..]] {
                let events = send(&mut stream, arguments_delta(2, fragment));
                assert!(
                    events.is_empty(),
                    "{name}: published unresolved delta: {events:?}"
                );
            }
            if first.starts_with("neither-") {
                if first == "neither-item-first" {
                    step.insert("id".into(), json!("item_2"));
                } else {
                    step.insert("call_id".into(), json!("call_2"));
                }
                let events = send(
                    &mut stream,
                    patch_step("step.start", Value::Object(step.clone())),
                );
                assert!(
                    events.is_empty(),
                    "{name}: one late ID is still unresolved: {events:?}"
                );
            }
            step.insert("id".into(), json!("item_2"));
            step.insert("call_id".into(), json!("call_2"));
            step.insert("arguments".into(), json!({ "input": PATCH_TEXT }));
            let mut events = Vec::new();
            if boundary == "stop" {
                events.extend(send(
                    &mut stream,
                    patch_step("step.stop", Value::Object(step.clone())),
                ));
            }
            step.insert("index".into(), json!(2));
            events.extend(send(
                &mut stream,
                go_json(json!({
                    "event_type": "interaction.completed",
                    "interaction": {"steps": [Value::Object(step)]},
                })),
            ));
            assert_patch_lifecycle(&events, PATCH_TEXT);
            let fragments: Vec<String> = events
                .iter()
                .filter(|event| kind(event) == "response.custom_tool_call_input.delta")
                .map(|event| s(event, "delta"))
                .collect();
            assert_eq!(
                fragments,
                [&PATCH_TEXT[..5], &PATCH_TEXT[5..]],
                "{name}: not the real buffered fragments"
            );
        }
    }
}

// Ports TestInteractionsApplyPatchFirstLateIDAccepted.
#[test]
fn first_late_id_accepted() {
    let mut stream = patch_stream();
    send(
        &mut stream,
        patch_step(
            "step.start",
            json!({"type": "function_call", "id": "item_2", "name": "functions__apply_patch"}),
        ),
    );
    send(&mut stream, arguments_delta(2, r#"{"input":"p"#));
    let events = send(
        &mut stream,
        patch_step(
            "step.stop",
            json!({"type": "function_call", "id": "item_2", "call_id": "call_2", "name": "functions__apply_patch", "arguments": {"input": "p"}}),
        ),
    );
    assert!(
        error_of(&stream).is_none(),
        "first real ID rejected: {:?} events={events:?}",
        error_of(&stream)
    );
}

// Ports TestInteractionsApplyPatchNamedLateIdentityEvidence.
#[test]
fn named_late_identity_evidence() {
    for (name, start, after) in [
        (
            "item-change",
            r#"{"event_type":"step.start","index":2,"step":{"type":"function_call","id":"item_2","name":"functions__apply_patch"}}"#,
            r#"{"event_type":"step.stop","index":2,"step":{"id":"changed","call_id":"call_2"}}"#,
        ),
        (
            "call-change",
            r#"{"event_type":"step.start","index":2,"step":{"type":"function_call","call_id":"call_2","name":"functions__apply_patch"}}"#,
            r#"{"event_type":"step.stop","index":2,"step":{"id":"item_2","call_id":"changed"}}"#,
        ),
        (
            "repeated-start-type",
            r#"{"event_type":"step.start","index":2,"step":{"type":"function_call","id":"item_2","name":"functions__apply_patch"}}"#,
            r#"{"event_type":"step.start","index":2,"step":{"type":"model_output","id":"item_2"}}"#,
        ),
        (
            "terminal-type",
            r#"{"event_type":"step.start","index":2,"step":{"type":"function_call","id":"item_2","name":"functions__apply_patch"}}"#,
            r#"{"event_type":"interaction.completed","steps":[{"index":2,"type":"model_output","id":"item_2"}]}"#,
        ),
        (
            "partial-snapshot",
            r#"{"event_type":"step.start","index":2,"step":{"type":"function_call","id":"item_2","name":"functions__apply_patch"}}"#,
            r#"{"event_type":"step.stop","index":2,"step":{"arguments":"{\"input\":\"p"}}"#,
        ),
        (
            "invalid-snapshot",
            r#"{"event_type":"step.start","index":2,"step":{"type":"function_call","call_id":"call_2","name":"functions__apply_patch"}}"#,
            r#"{"event_type":"step.stop","index":2,"step":{"arguments":{"input":"p","extra":1}}}"#,
        ),
    ] {
        let mut stream = patch_stream();
        let events = send(&mut stream, start);
        assert!(events.is_empty(), "{name}: early output: {events:?}");
        let events = send(&mut stream, after);
        assert!(
            events.len() == 1
                && kind(&events[0]) == "response.failed"
                && error_of(&stream).is_some(),
            "{name}: lost evidence: {events:?}"
        );
        let events = send(&mut stream, r#"{"event_type":"interaction.completed"}"#);
        assert!(events.is_empty(), "{name}: failure reopened: {events:?}");
    }
}

// Ports TestInteractionsApplyPatchUnresolvedAllKeys.
#[test]
fn unresolved_all_keys() {
    for partial in [false, true] {
        for discover in ["index", "step-index", "item", "call"] {
            let name = format!(
                "{discover}-{}",
                if partial { "partial" } else { "snapshot" }
            );
            let mut stream = patch_stream();
            send(
                &mut stream,
                r#"{"event_type":"step.start","index":2,"step":{"type":"function_call","id":"a"}}"#,
            );
            let raw = r#"{"event_type":"step.start","index":3,"step":{"index":4,"type":"model_output","id":"a","call_id":"late"}}"#;
            let raw = if partial { &raw[..raw.len() - 1] } else { raw };
            send(&mut stream, raw);
            let mut root = json!({
                "event_type": "step.start",
                "step": {"type": "function_call", "name": "functions__apply_patch"},
            });
            match discover {
                "index" => root["index"] = json!(3),
                "step-index" => root["step"]["index"] = json!(4),
                "item" => root["step"]["id"] = json!("a"),
                _ => root["step"]["call_id"] = json!("late"),
            }
            let events = send(&mut stream, go_json(root));
            assert!(
                events.len() == 1 && kind(&events[0]) == "response.failed",
                "{name}: alias erased evidence: {events:?}"
            );
        }
    }
}

// Ports TestInteractionsApplyPatchTerminalIdentityFallback.
#[test]
fn terminal_identity_fallback() {
    for first in ["item", "call", "neither"] {
        for terminal in ["interaction.completed", "finish", "done"] {
            let name = format!("{first}-{terminal}");
            let mut stream = patch_stream();
            let mut step = json!({"type": "function_call", "name": "functions__apply_patch"});
            let id = if first == "call" { "call_2" } else { "item_2" };
            if first == "item" {
                step["id"] = json!(id);
            }
            if first == "call" {
                step["call_id"] = json!(id);
            }
            let events = send(&mut stream, patch_step("step.start", step));
            assert!(events.is_empty(), "{name}: early fallback: {events:?}");
            let events = send(
                &mut stream,
                r#"{"event_type":"step.delta","index":2,"delta":{"type":"arguments_delta","arguments":"{\"input\":\"pq\"}"}}"#,
            );
            assert!(events.is_empty(), "{name}: early delta: {events:?}");
            let events = send(&mut stream, patch_step("step.stop", Value::Null));
            assert!(
                events.is_empty(),
                "{name}: step stop is not a response terminal: {events:?}"
            );
            let events = send(&mut stream, go_json(json!({ "event_type": terminal })));
            let mut counts = HashMap::<String, usize>::new();
            for event in &events {
                let event_kind = kind(event);
                let (identity, key) = match event_kind.as_str() {
                    "response.output_item.added" | "response.output_item.done" => {
                        (get(event, "item").cloned().unwrap_or(Value::Null), "id")
                    }
                    "response.completed" => (
                        get(event, "response.output.0")
                            .cloned()
                            .unwrap_or(Value::Null),
                        "id",
                    ),
                    _ => (event.clone(), "item_id"),
                };
                *counts.entry(event_kind).or_default() += 1;
                assert!(
                    s(&identity, key) == id && s(&identity, "call_id") == id,
                    "{name}: fallback identity changed: {event}"
                );
            }
            for event_kind in [
                "response.output_item.added",
                "response.custom_tool_call_input.delta",
                "response.custom_tool_call_input.done",
                "response.output_item.done",
            ] {
                assert_eq!(
                    counts.get(event_kind).copied(),
                    Some(1),
                    "{name}: fallback lifecycle: {counts:?}"
                );
            }
            let events = send(
                &mut stream,
                patch_step(
                    "step.start",
                    json!({"type": "function_call", "id": "changed", "call_id": "changed", "name": "functions__apply_patch"}),
                ),
            );
            assert!(
                events.is_empty(),
                "{name}: terminal identity reopened: {events:?}"
            );
        }
    }
}

/// What the interleaved test wants at output index `index`: `two` at 2,
/// `three` at 3, and nothing elsewhere, as a Go map gives.
fn at<'a>(index: i64, two: &'a str, three: &'a str) -> &'a str {
    match index {
        2 => two,
        3 => three,
        _ => "",
    }
}

// Ports TestInteractionsApplyPatchNamedLateIdentityInterleaved.
#[test]
fn named_late_identity_interleaved() {
    let mut stream = patch_stream();
    for raw in [
        r#"{"event_type":"step.start","index":2,"step":{"type":"function_call","id":"a2","name":"functions__apply_patch"}}"#,
        r#"{"event_type":"step.start","index":3,"step":{"type":"function_call","call_id":"c3","name":"functions__apply_patch"}}"#,
        r#"{"event_type":"step.delta","index":2,"delta":{"type":"arguments_delta","arguments":"{\"input\":\"two\"}"}}"#,
        r#"{"event_type":"step.delta","index":3,"delta":{"type":"arguments_delta","arguments":"{\"input\":\"three\"}"}}"#,
        r#"{"event_type":"step.stop","index":2}"#,
        r#"{"event_type":"step.stop","index":3}"#,
    ] {
        let events = send(&mut stream, raw);
        assert!(events.is_empty(), "early interleaved event: {events:?}");
    }
    let events = send(
        &mut stream,
        r#"{"event_type":"interaction.completed","steps":[{"index":3,"type":"function_call","id":"a3","call_id":"c3","name":"functions__apply_patch","arguments":{"input":"three"}},{"index":2,"type":"function_call","id":"a2","call_id":"c2","name":"functions__apply_patch","arguments":{"input":"two"}}]}"#,
    );
    let mut counts = HashMap::<i64, HashMap<String, usize>>::new();
    for event in &events {
        let event_kind = kind(event);
        if event_kind == "response.completed" {
            let output = get(event, "response.output")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for (position, item) in output.iter().enumerate() {
                let index = position as i64 + 2;
                assert!(
                    s(item, "id") == at(index, "a2", "a3")
                        && s(item, "call_id") == at(index, "c2", "c3")
                        && s(item, "input") == at(index, "two", "three"),
                    "final crossed calls: {event}"
                );
            }
            continue;
        }
        let index = int(event, "output_index");
        *counts
            .entry(index)
            .or_default()
            .entry(event_kind.clone())
            .or_default() += 1;
        let (identity, key) = match get(event, "item") {
            Some(item) => (item.clone(), "id"),
            None => (event.clone(), "item_id"),
        };
        assert!(
            s(&identity, key) == at(index, "a2", "a3")
                && s(&identity, "call_id") == at(index, "c2", "c3"),
            "crossed identities: {event}"
        );
        if event_kind == "response.custom_tool_call_input.delta" {
            assert_eq!(
                s(event, "delta"),
                at(index, "two", "three"),
                "crossed fragments: {event}"
            );
        }
    }
    for index in [2, 3] {
        for event_kind in [
            "response.output_item.added",
            "response.custom_tool_call_input.delta",
            "response.custom_tool_call_input.done",
            "response.output_item.done",
        ] {
            assert_eq!(
                counts
                    .get(&index)
                    .and_then(|kinds| kinds.get(event_kind))
                    .copied(),
                Some(1),
                "interleaved lifecycle: {counts:?}"
            );
        }
    }
}
