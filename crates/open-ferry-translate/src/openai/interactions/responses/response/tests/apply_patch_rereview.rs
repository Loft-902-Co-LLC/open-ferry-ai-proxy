// Ported from CLIProxyAPI internal/translator/openai/interactions/responses/apply_patch_rereview_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A call whose name comes late: an item type that contradicts it, seen
//! before the name, fails the response once the name says it is
//! `apply_patch`, and does nothing to an ordinary function or to a stream
//! with no `apply_patch` bridge.
//!
//! Dropped or changed tests: none. Go's checks of the state's
//! `FunctionCalls` read the stream's `calls`.

use super::*;

/// `patchTypeReviewIdentity`: the index and IDs `key` names.
fn type_review_identity(key: &str) -> (i64, Value) {
    let index = if key == "index only" || key == "index and IDs" {
        2
    } else {
        -1
    };
    let mut step = json!({});
    if key == "IDs only" || key == "index and IDs" || key == "item only" {
        step["id"] = json!("item_2");
    }
    if key == "IDs only" || key == "index and IDs" || key == "call only" {
        step["call_id"] = json!("call_2");
    }
    (index, step)
}

/// `patchTypeReviewEntries`: the events that can resolve a call whose
/// evidence came in `entry`.
fn type_review_entries(entry: &str) -> &[&str] {
    if entry == "step.start" || entry == "step.stop" {
        &["step.start", "step.stop", "interaction.completed", "finish"]
    } else if entry == "interaction.completed" {
        &["interaction.completed"]
    } else {
        &["finish"]
    }
}

/// `patchTypeReviewFinal`
fn type_review_final(entry: &str, steps: &[&Value]) -> Vec<u8> {
    go_json(json!({ "event_type": entry, "steps": steps }))
}

/// The pre-name start of the `apply_patch` call at index 2.
const UNNAMED_START: &str = r#"{"event_type":"step.start","index":2,"step":{"type":"function_call","id":"item_2","call_id":"call_2","arguments":{"input":"p"}}}"#;

// Ports TestInteractionsApplyPatchPreNameConsistentTypeConflict.
#[test]
fn pre_name_consistent_type_conflict() {
    for entry in ["step.start", "step.stop", "interaction.completed", "finish"] {
        for item_type in ["model_output", "thought", "custom_tool_call"] {
            for evidence_key in ["index and IDs", "index only", "IDs only"] {
                for resolved_key in [
                    "index and IDs",
                    "index only",
                    "IDs only",
                    "item only",
                    "call only",
                ] {
                    for &resolved_entry in type_review_entries(entry) {
                        let name = [entry, item_type, evidence_key, resolved_entry, resolved_key]
                            .join("/");
                        let mut stream = patch_stream();
                        let events = send(&mut stream, UNNAMED_START);
                        assert!(
                            events.is_empty(),
                            "{name}: unnamed call announced: {events:?}"
                        );
                        let (index, mut conflict) = type_review_identity(evidence_key);
                        conflict["type"] = json!(item_type);
                        let (resolved_index, mut resolved) = type_review_identity(resolved_key);
                        resolved["type"] = json!("function_call");
                        resolved["name"] = json!("functions__apply_patch");
                        resolved["arguments"] = json!({"input": "p"});
                        let events = if entry == "step.start" || entry == "step.stop" {
                            for event in
                                send(&mut stream, patch_review_snapshot(entry, index, conflict))
                            {
                                assert!(
                                    s(&event, "item.type") != "custom_tool_call"
                                        && !kind(&event)
                                            .starts_with("response.custom_tool_call_input.")
                                        && kind(&event) != "response.failed",
                                    "{name}: unresolved type evidence prematurely treated as patch: {event}"
                                );
                            }
                            send(
                                &mut stream,
                                patch_review_snapshot(resolved_entry, resolved_index, resolved),
                            )
                        } else {
                            if index >= 0 {
                                conflict["index"] = json!(index);
                            }
                            if resolved_index >= 0 {
                                resolved["index"] = json!(resolved_index);
                            }
                            // Both snapshots belong to the same call in this
                            // single terminal event.
                            send(
                                &mut stream,
                                type_review_final(entry, &[&conflict, &resolved]),
                            )
                        };
                        assert_review_failure(&mut stream, &events);
                        let call = stream.calls.get(&2);
                        assert!(
                            stream.calls.len() == 1
                                && stream.pending_identity_errors.is_empty()
                                && call.is_some_and(|call| call
                                    .pending_error
                                    .as_ref()
                                    .is_some_and(|error| error.to_string().contains("type"))
                                    && call.patch.is_none()
                                    && !call.added),
                            "{name}: type conflict lost or hidden by identity failure"
                        );
                    }
                }
            }
        }
    }
}

// Ports TestInteractionsApplyPatchPreNameOmittedType.
#[test]
fn pre_name_omitted_type() {
    for entry in ["step.start", "step.stop", "interaction.completed", "finish"] {
        for resolved_key in [
            "index and IDs",
            "index only",
            "IDs only",
            "item only",
            "call only",
        ] {
            for &resolved_entry in type_review_entries(entry) {
                let name = format!("{entry}/{resolved_entry}/{resolved_key}");
                let mut stream = patch_stream();
                let mut events = send(&mut stream, UNNAMED_START);
                let (_, mut snapshot) = type_review_identity("index and IDs");
                let (index, mut resolved) = type_review_identity(resolved_key);
                resolved["type"] = json!("function_call");
                resolved["name"] = json!("functions__apply_patch");
                resolved["arguments"] = json!({"input": "p"});
                if entry == "step.start" || entry == "step.stop" {
                    events.extend(send(&mut stream, patch_review_snapshot(entry, 2, snapshot)));
                    events.extend(send(
                        &mut stream,
                        patch_review_snapshot(resolved_entry, index, resolved),
                    ));
                    if resolved_entry == "step.start" || resolved_entry == "step.stop" {
                        events.extend(send(
                            &mut stream,
                            r#"{"event_type":"interaction.completed"}"#,
                        ));
                    }
                } else {
                    snapshot["index"] = json!(2);
                    if index >= 0 {
                        resolved["index"] = json!(index);
                    }
                    events.extend(send(
                        &mut stream,
                        type_review_final(entry, &[&snapshot, &resolved]),
                    ));
                }
                assert_patch_lifecycle(&events, "p");
                assert!(
                    error_of(&stream).is_none()
                        && stream
                            .calls
                            .get(&2)
                            .is_some_and(|call| call.pending_error.is_none())
                        && stream.calls.len() == 1,
                    "{name}: omitted type introduced a conflict: {:?}",
                    error_of(&stream)
                );
            }
        }
    }
}

// Ports TestInteractionsApplyPatchPreNameTypeOrdinaryCompatibility.
#[test]
fn pre_name_type_ordinary_compatibility() {
    for (request_name, request, upstream) in [
        (
            "ordinary",
            r#"{"tools":[{"type":"custom","name":"apply_patch"},{"type":"function","name":"lookup"}]}"#,
            "lookup",
        ),
        (
            "function winner",
            r#"{"tools":[{"type":"function","name":"apply_patch"},{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"apply_patch"}]}],"input":[{"type":"additional_tools","tools":[{"type":"custom","name":"apply_patch"}]}]}"#,
            "apply_patch",
        ),
    ] {
        for entry in ["step.start", "step.stop", "interaction.completed", "finish"] {
            for item_type in ["model_output", "thought", "custom_tool_call", "omitted"] {
                let name = format!("{request_name}/{entry}/{item_type}");
                let mut stream = stream_for(request);
                send(
                    &mut stream,
                    patch_step(
                        "step.start",
                        json!({"type": "function_call", "id": "item_2", "call_id": "call_2", "arguments": {"x": 1}}),
                    ),
                );
                let mut conflict = json!({"index": 2, "id": "item_2", "call_id": "call_2"});
                if item_type != "omitted" {
                    conflict["type"] = json!(item_type);
                }
                let resolved = json!({"index": 2, "type": "function_call", "id": "item_2", "call_id": "call_2", "name": upstream, "arguments": {"x": 1}});
                let events = if entry == "step.start" || entry == "step.stop" {
                    let mut events = send(&mut stream, patch_review_snapshot(entry, 2, conflict));
                    events.extend(send(
                        &mut stream,
                        patch_review_snapshot("step.start", 2, resolved),
                    ));
                    events.extend(send(&mut stream, patch_step("step.stop", Value::Null)));
                    events.extend(send(
                        &mut stream,
                        r#"{"event_type":"interaction.completed"}"#,
                    ));
                    events
                } else {
                    send(
                        &mut stream,
                        type_review_final(entry, &[&conflict, &resolved]),
                    )
                };
                for event in &events {
                    assert!(
                        kind(event) != "response.failed"
                            && !kind(event).starts_with("response.custom_tool_call_input.")
                            && s(event, "item.type") != "custom_tool_call",
                        "{name}: ordinary winner received patch behavior: {event}"
                    );
                }
                let fin = events.last().cloned().unwrap_or(Value::Null);
                assert!(
                    kind(&fin) == "response.completed"
                        && s(&fin, "response.output.0.type") == "function_call"
                        && s(&fin, "response.output.0.name") == upstream
                        && s(&fin, "response.output.0.arguments") == r#"{"x":1}"#
                        && error_of(&stream).is_none(),
                    "{name}: ordinary late identity changed: {events:?}"
                );
            }
        }
    }
}

// Ports TestInteractionsApplyPatchPreNameUnrelatedModelOutput.
#[test]
fn pre_name_unrelated_model_output() {
    for entry in ["interaction.completed", "finish"] {
        let mut stream = patch_stream();
        send(
            &mut stream,
            patch_review_snapshot(
                "step.start",
                0,
                json!({"type": "model_output", "id": "message"}),
            ),
        );
        send(
            &mut stream,
            go_json(
                json!({"event_type": "step.delta", "index": 0, "delta": {"type": "text", "text": "hello"}}),
            ),
        );
        send(
            &mut stream,
            patch_review_snapshot("step.stop", 0, Value::Null),
        );
        send(
            &mut stream,
            patch_step(
                "step.start",
                json!({"type": "function_call", "id": "item_2", "call_id": "call_2", "arguments": {"input": "p"}}),
            ),
        );
        let message = json!({"type": "model_output", "id": "message", "content": "hello"});
        let call = json!({"type": "function_call", "id": "item_2", "call_id": "call_2", "name": "functions__apply_patch", "arguments": {"input": "p"}});
        let events = send(&mut stream, type_review_final(entry, &[&message, &call]));
        let fin = events.last().cloned().unwrap_or(Value::Null);
        assert!(
            kind(&fin) == "response.completed"
                && get(&fin, "response.output")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    == Some(2)
                && s(&fin, "response.output.0.content.0.text") == "hello"
                && s(&fin, "response.output.1.input") == "p"
                && error_of(&stream).is_none(),
            "{entry}: unrelated message became a type conflict: {events:?}"
        );
    }
}

// Ports TestInteractionsApplyPatchPreNameNoBridgeLegacy.
#[test]
fn pre_name_no_bridge_legacy() {
    for (request_name, request, upstream) in [
        (
            "ordinary only",
            r#"{"tools":[{"type":"function","name":"lookup"}]}"#,
            "lookup",
        ),
        (
            "function winner without bridge",
            r#"{"tools":[{"type":"function","name":"apply_patch"}],"input":[{"type":"additional_tools","tools":[{"type":"custom","name":"apply_patch"}]}]}"#,
            "apply_patch",
        ),
    ] {
        for entry in ["step.start", "interaction.completed", "finish"] {
            let name = format!("{request_name}/{entry}");
            let mut stream = stream_for(request);
            send(
                &mut stream,
                patch_step(
                    "step.start",
                    json!({"type": "function_call", "id": "item_2", "call_id": "call_2", "arguments": {"x": 1}}),
                ),
            );
            let message =
                json!({"index": 2, "type": "model_output", "id": "item_2", "call_id": "call_2"});
            let resolved = json!({"index": 2, "type": "function_call", "id": "item_2", "call_id": "call_2", "name": upstream, "arguments": {"x": 1}});
            if entry == "step.start" {
                let events = send(&mut stream, patch_review_snapshot(entry, 2, message));
                assert!(
                    events.len() == 2
                        && s(&events[0], "item.type") == "message"
                        && kind(&events[1]) == "response.content_part.added",
                    "{name}: ordinary-only repeated start changed: {events:?}"
                );
                send(
                    &mut stream,
                    patch_review_snapshot("step.start", 2, resolved),
                );
                send(&mut stream, patch_step("step.stop", Value::Null));
                let events = send(&mut stream, r#"{"event_type":"interaction.completed"}"#);
                assert!(
                    events.len() == 1
                        && s(&events[0], "response.output.0.name") == upstream
                        && s(&events[0], "response.output.0.arguments") == r#"{"x":1}"#,
                    "{name}: ordinary-only function changed: {events:?}"
                );
            } else {
                let events = send(
                    &mut stream,
                    type_review_final(entry, &[&message, &resolved]),
                );
                let call = stream.calls.get(&2);
                assert!(
                    events.len() == 1
                        && kind(&events[0]) == "response.completed"
                        && call.is_some_and(|call| call.raw_name.is_empty() && !call.added),
                    "{name}: non-patch final filtering changed: {events:?}"
                );
            }
            assert!(
                error_of(&stream).is_none() && !stream.patch_bridge,
                "{name}: ordinary winner became a patch"
            );
        }
    }
}
