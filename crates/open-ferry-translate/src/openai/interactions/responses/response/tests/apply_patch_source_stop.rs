// Ported from CLIProxyAPI internal/translator/openai/interactions/responses/apply_patch_source_stop_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Upstream's `step.stop` ends an `apply_patch` call's arguments, even
//! while its name or IDs are still unknown: a fragment after it fails the
//! call, and the fragments before it are replayed as they came once the
//! call is known.
//!
//! Dropped or changed tests: none. Go's checks of the state's
//! `FunctionCalls` read the stream's `calls`.

use std::collections::HashMap;

use serde_json::Map;

use super::*;

// Ports TestInteractionsApplyPatchSourceStopFragments.
#[test]
fn source_stop_fragments() {
    for first in ["item", "call", "neither", "ready"] {
        for late_name in [false, true] {
            for input in ["partial", "complete"] {
                for discovery in ["terminal", "identity", "coincident-snapshot"] {
                    let name = format!("{first}/late-name={late_name}/{input}/{discovery}");
                    let unresolved = first != "ready" || late_name;
                    let mut stream = patch_stream();
                    let mut step = Map::new();
                    step.insert("type".into(), json!("function_call"));
                    if first == "item" || first == "ready" {
                        step.insert("id".into(), json!("item_2"));
                    }
                    if first == "call" || first == "ready" {
                        step.insert("call_id".into(), json!("call_2"));
                    }
                    if !late_name {
                        step.insert("name".into(), json!("functions__apply_patch"));
                    }
                    let (before, after, fin) = if input == "complete" {
                        (r#"{"input":"p"}"#, " \t\n", "p")
                    } else {
                        (r#"{"input":"p"#, r#"q"}"#, "pq")
                    };
                    let mut events =
                        send(&mut stream, patch_step("step.start", Value::Object(step)));
                    events.extend(send(&mut stream, arguments_delta(2, before)));
                    events.extend(send(&mut stream, patch_step("step.stop", Value::Null)));
                    if unresolved {
                        assert!(
                            events.is_empty(),
                            "{name}: unresolved call published: {events:?}"
                        );
                    }
                    let resolved = json!({"index": 2, "type": "function_call", "id": "item_2", "call_id": "call_2", "name": "functions__apply_patch", "arguments": {"input": fin}, "provider_secret": "RAW_SECRET"});
                    let mut post = json!({"event_type": "step.delta", "index": 2, "delta": {"type": "arguments_delta", "arguments": after}});
                    if discovery == "coincident-snapshot" {
                        post["step"] = resolved.clone();
                    }
                    events.extend(send(&mut stream, go_json(post)));
                    if !late_name || discovery == "coincident-snapshot" {
                        let error = error_of(&stream);
                        assert!(
                            error.is_some(),
                            "{name}: post-stop fragment accepted before identity readiness: {events:?}"
                        );
                        if unresolved {
                            assert!(
                                error.is_some_and(|error| error.contains("after source stop")),
                                "{name}: snapshot/replay erased source ordering: {:?}",
                                error_of(&stream)
                            );
                        }
                    } else {
                        let call = stream.calls.get(&2);
                        assert!(
                            error_of(&stream).is_none()
                                && call.is_some_and(|call| call
                                    .pending_error
                                    .as_ref()
                                    .is_some_and(|error| error
                                        .to_string()
                                        .contains("after source stop"))),
                            "{name}: unnamed call lost pending source violation"
                        );
                        assert_eq!(
                            call.map(|call| call.argument_fragments.clone()),
                            Some(vec![before.to_owned()]),
                            "{name}: buffered a fragment after source stop"
                        );
                    }
                    if discovery == "identity" {
                        events.extend(send(
                            &mut stream,
                            go_json(json!({"event_type": "step.delta", "index": 2, "step": resolved, "delta": {"type": "arguments_delta", "arguments": ""}})),
                        ));
                    }
                    events.extend(send(
                        &mut stream,
                        go_json(
                            json!({"event_type": "interaction.completed", "steps": [resolved]}),
                        ),
                    ));
                    let mut failures = 0;
                    for event in &events {
                        if unresolved {
                            assert_eq!(
                                kind(event),
                                "response.failed",
                                "{name}: unresolved stopped call published success: {event}"
                            );
                        }
                        if kind(event) == "response.failed" {
                            failures += 1;
                            assert_eq!(
                                s(event, "response.error.code"),
                                "invalid_tool_arguments",
                                "{name}: unsanitized failure: {event}"
                            );
                        }
                        assert!(
                            kind(event) != "response.completed"
                                && !event.to_string().contains("RAW_SECRET"),
                            "{name}: source violation completed/leaked: {event}"
                        );
                    }
                    assert!(
                        failures == 1 && error_of(&stream).is_some(),
                        "{name}: failure count={failures} events={events:?}"
                    );
                    let more = send(&mut stream, "[DONE]");
                    assert!(more.is_empty(), "{name}: failure reopened: {more:?}");
                }
            }
        }
    }
}

// Ports TestInteractionsApplyPatchSourceStopLateIdentityReplay.
#[test]
fn source_stop_late_identity_replay() {
    for first in ["item", "call", "neither"] {
        for late_name in [false, true] {
            for boundary in ["before-stop", "identity-after-stop", "terminal-snapshot"] {
                let name = format!("{first}/late-name={late_name}/{boundary}");
                let mut stream = patch_stream();
                let mut step = json!({"type": "function_call"});
                if first == "item" {
                    step["id"] = json!("item_2");
                }
                if first == "call" {
                    step["call_id"] = json!("call_2");
                }
                if !late_name {
                    step["name"] = json!("functions__apply_patch");
                }
                let events = send(&mut stream, patch_step("step.start", step));
                assert!(
                    events.is_empty(),
                    "{name}: provisional identity: {events:?}"
                );
                let args = patch_arguments(PATCH_TEXT);
                for fragment in [&args[..15], &args[15..]] {
                    let events = send(&mut stream, arguments_delta(2, fragment));
                    assert!(events.is_empty(), "{name}: unresolved delta: {events:?}");
                }
                if boundary != "before-stop" {
                    let events = send(&mut stream, patch_step("step.stop", Value::Null));
                    assert!(
                        events.is_empty(),
                        "{name}: stop guessed an identity: {events:?}"
                    );
                }
                let mut resolved = json!({"index": 2, "type": "function_call", "id": "item_2", "call_id": "call_2", "name": "functions__apply_patch"});
                let mut events = Vec::new();
                if boundary != "terminal-snapshot" {
                    events = send(
                        &mut stream,
                        go_json(
                            json!({"event_type": "step.delta", "index": 2, "step": resolved, "delta": {"type": "arguments_delta", "arguments": ""}}),
                        ),
                    );
                    let want_count = if boundary == "identity-after-stop" {
                        assert!(
                            stream
                                .calls
                                .get(&2)
                                .is_some_and(|call| call.item_done_emitted),
                            "{name}: identity-only update did not publish pending completion"
                        );
                        5
                    } else {
                        3
                    };
                    assert!(
                        events.len() == want_count && !stream.terminal,
                        "{name}: replay waited for response terminal: {events:?}"
                    );
                    if boundary == "before-stop" {
                        events.extend(send(&mut stream, patch_step("step.stop", Value::Null)));
                    }
                }
                resolved["arguments"] = json!({ "input": PATCH_TEXT });
                events.extend(send(
                    &mut stream,
                    go_json(json!({"event_type": "interaction.completed", "steps": [resolved]})),
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
                    "{name}: snapshot replaced real source fragments"
                );
            }
        }
    }
}

// Ports TestInteractionsApplyPatchSourceStopBeforeStart.
#[test]
fn source_stop_before_start() {
    for before in [r#"{"input":"p"#, r#"{"input":"pq"}"#] {
        let mut stream = patch_stream();
        send(&mut stream, arguments_delta(2, before));
        send(&mut stream, patch_step("step.stop", Value::Null));
        let post = if before.ends_with('}') { " " } else { r#"q"}"# };
        send(&mut stream, arguments_delta(2, post));
        assert!(
            stream
                .calls
                .get(&2)
                .is_some_and(|call| call.pending_error.is_some()),
            "{before}: stop before named start lost source evidence"
        );
        let events = send(
            &mut stream,
            patch_step(
                "step.start",
                json!({"type": "function_call", "id": "item_2", "call_id": "call_2", "name": "functions__apply_patch", "arguments": {"input": "pq"}}),
            ),
        );
        assert!(
            events.len() == 1 && kind(&events[0]) == "response.failed",
            "{before}: late start repaired a stopped call: {events:?}"
        );
    }
}

// Ports TestInteractionsApplyPatchSourceStopOrdinaryFunction.
#[test]
fn source_stop_ordinary_function() {
    let mut stream = patch_stream();
    for raw in [
        r#"{"event_type":"step.start","index":2,"step":{"type":"function_call","id":"ordinary","call_id":"ordinary_call"}}"#,
        r#"{"event_type":"step.delta","index":2,"delta":{"type":"arguments_delta","arguments":"{\"path\":\"a"}}"#,
        r#"{"event_type":"step.stop","index":2}"#,
        r#"{"event_type":"step.delta","index":2,"delta":{"type":"arguments_delta","arguments":".txt\"}"}}"#,
    ] {
        let events = send(&mut stream, raw);
        assert!(
            events.is_empty(),
            "unnamed ordinary call announced: {events:?}"
        );
    }
    let mut events = send(
        &mut stream,
        patch_step(
            "step.start",
            json!({"type": "function_call", "id": "ordinary", "call_id": "ordinary_call", "name": "external_read_file"}),
        ),
    );
    events.extend(send(&mut stream, patch_step("step.stop", Value::Null)));
    events.extend(send(
        &mut stream,
        r#"{"event_type":"interaction.completed"}"#,
    ));
    assert!(
        error_of(&stream).is_none(),
        "patch-only source violation broke ordinary function: {:?}",
        error_of(&stream)
    );
    let mut counts = HashMap::<String, usize>::new();
    for event in &events {
        let event_kind = kind(event);
        assert!(
            !event_kind.contains("custom_tool_call") && event_kind != "response.failed",
            "ordinary function bridged: {event}"
        );
        if event_kind == "response.function_call_arguments.delta" {
            assert_eq!(
                s(event, "delta"),
                r#"{"path":"a.txt"}"#,
                "ordinary buffered fragments changed: {event}"
            );
        }
        *counts.entry(event_kind).or_default() += 1;
    }
    for event_kind in [
        "response.output_item.added",
        "response.function_call_arguments.delta",
        "response.function_call_arguments.done",
        "response.output_item.done",
        "response.completed",
    ] {
        assert_eq!(
            counts.get(event_kind).copied(),
            Some(1),
            "ordinary lifecycle changed: {counts:?}"
        );
    }
}
