// Ported from CLIProxyAPI internal/translator/openai/interactions/responses/apply_patch_review_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Snapshots of a call after it started or completed: ordinary functions
//! keep their arguments as they came, and an `apply_patch` snapshot whose
//! index, IDs, type or input disagree with the call fails the response
//! without showing the arguments.
//!
//! Dropped or changed tests: none. Go's checks of the state's
//! `FunctionCalls` read the stream's `calls`.

use std::collections::HashMap;

use super::super::read::read;
use super::*;

// Ports TestInteractionsApplyPatchOrdinaryFunctionStopSnapshotDeltas.
#[test]
fn ordinary_function_stop_snapshot_deltas() {
    for (request_name, request, upstream) in [
        (
            "ordinary function",
            r#"{"tools":[{"type":"function","name":"lookup"}]}"#,
            "lookup",
        ),
        (
            "ordinary function with patch bridge",
            r#"{"tools":[{"type":"custom","name":"apply_patch"},{"type":"function","name":"lookup"}]}"#,
            "lookup",
        ),
        (
            "function winner named apply_patch",
            r#"{"tools":[{"type":"function","name":"apply_patch"}],"input":[{"type":"additional_tools","tools":[{"type":"custom","name":"apply_patch"}]}]}"#,
            "apply_patch",
        ),
    ] {
        for identity_at in ["start", "after delta", "stop", "delta before start"] {
            let name = format!("{request_name}/{identity_at}");
            let mut stream = stream_for(request);
            let mut events = Vec::new();
            let mut step = json!({"type": "function_call", "arguments": {}});
            let identity = |step: &mut Value| {
                step["id"] = json!("item_2");
                step["call_id"] = json!("call_2");
                step["name"] = json!(upstream);
            };
            if identity_at == "start" {
                identity(&mut step);
            }
            if identity_at != "delta before start" {
                events.extend(send(&mut stream, patch_step("step.start", step.clone())));
            }
            let args = r#"{"x":1}"#;
            let deltas = send(&mut stream, arguments_delta(2, args));
            if identity_at != "start" {
                assert!(
                    deltas.is_empty(),
                    "{name}: arguments emitted before announcement: {deltas:?}"
                );
            }
            events.extend(deltas);
            identity(&mut step);
            if identity_at == "after delta" || identity_at == "delta before start" {
                events.extend(send(&mut stream, patch_step("step.start", step.clone())));
            }
            step["arguments"] = json!({"x": 1});
            events.extend(send(&mut stream, patch_step("step.stop", step.clone())));
            let more = send(&mut stream, patch_step("step.stop", step.clone()));
            assert!(
                more.is_empty(),
                "{name}: repeated stop replayed arguments: {more:?}"
            );
            events.extend(send(
                &mut stream,
                r#"{"event_type":"interaction.completed"}"#,
            ));
            let mut delta = String::new();
            let mut counts = HashMap::<String, usize>::new();
            for event in &events {
                let event_kind = kind(event);
                *counts.entry(event_kind.clone()).or_default() += 1;
                match event_kind.as_str() {
                    "response.function_call_arguments.delta" => {
                        assert!(
                            counts.get("response.output_item.added") == Some(&1)
                                && s(event, "item_id") == "item_2",
                            "{name}: delta precedes real announcement: {event}"
                        );
                        delta.push_str(&s(event, "delta"));
                    }
                    "response.function_call_arguments.done" => {
                        assert_eq!(
                            s(event, "arguments"),
                            args,
                            "{name}: done arguments changed"
                        );
                    }
                    "response.output_item.done" => assert!(
                        s(event, "item.arguments") == args
                            && s(event, "item.type") == "function_call"
                            && s(event, "item.name") == upstream,
                        "{name}: ordinary function changed: {event}"
                    ),
                    "response.completed" => assert_eq!(
                        s(event, "response.output.0.arguments"),
                        args,
                        "{name}: final arguments changed: {event}"
                    ),
                    _ => {}
                }
            }
            let counted = |event_type: &str| counts.get(event_type).copied().unwrap_or(0);
            assert!(
                delta == args
                    && counted("response.function_call_arguments.delta") == 1
                    && counted("response.function_call_arguments.done") == 1
                    && counted("response.output_item.done") == 1
                    && counted("response.completed") == 1
                    && counted("response.failed") == 0,
                "{name}: arguments replayed or lost: delta={delta:?} counts={counts:?}"
            );
        }
    }
}

/// A patch stream whose `apply_patch` call at index 2 has completed with
/// input `p`.
fn completed_patch_stream() -> InteractionsToOpenAIResponsesStream {
    let mut stream = patch_stream();
    send(
        &mut stream,
        patch_step(
            "step.start",
            json!({"type": "function_call", "id": "item_2", "call_id": "call_2", "name": "functions__apply_patch", "arguments": {"input": "p"}}),
        ),
    );
    send(&mut stream, patch_step("step.stop", Value::Null));
    stream
}

// Ports TestInteractionsApplyPatchCompletedItemStopAndFinalTypeConflicts.
#[test]
fn completed_item_stop_and_final_type_conflicts() {
    for entry in ["step.start", "step.stop", "interaction.completed", "finish"] {
        for key in ["index", "id", "call_id"] {
            for item_type in ["model_output", "thought", "custom_tool_call"] {
                let mut stream = patch_stream();
                send(
                    &mut stream,
                    patch_step(
                        "step.start",
                        json!({"type": "function_call", "id": "item_2", "call_id": "call_2", "name": "functions__apply_patch", "arguments": {"input": "p"}}),
                    ),
                );
                let completed = send(&mut stream, patch_step("step.stop", Value::Null));
                assert!(
                    completed.len() == 3
                        && completed.last().map(kind).as_deref()
                            == Some("response.output_item.done"),
                    "fixture did not complete patch item: {completed:?}"
                );
                let mut step = json!({ "type": item_type });
                let mut event = json!({ "event_type": entry });
                match key {
                    "index" => {
                        step["index"] = json!(2);
                        event["index"] = json!(2);
                    }
                    "id" => step["id"] = json!("item_2"),
                    _ => step["call_id"] = json!("call_2"),
                }
                match entry {
                    "interaction.completed" => event["interaction"] = json!({ "steps": [step] }),
                    "finish" => event["steps"] = json!([step]),
                    _ => event["step"] = step,
                }
                let events = send(&mut stream, go_json(event));
                assert_review_failure(&mut stream, &events);
            }
        }
    }
}

// Ports TestInteractionsApplyPatchFinalOnlyIdentityAndEnvelope.
#[test]
fn final_only_identity_and_envelope() {
    for entry in ["interaction.completed", "finish"] {
        for location in ["interaction", "root"] {
            for evidence in ["unnamed", "malformed named", "valid named"] {
                let name = format!("{entry}/{location}/{evidence}");
                let mut step = json!({"index": 2, "type": "function_call", "id": "item_2", "call_id": "call_2", "arguments": {"input": "secret"}});
                if evidence != "unnamed" {
                    step["name"] = json!("functions__apply_patch");
                }
                let mut event = json!({ "event_type": entry });
                let path = if location == "interaction" {
                    event["interaction"] = json!({"id": "r", "steps": [step]});
                    "interaction.steps"
                } else {
                    event["steps"] = json!([step]);
                    "steps"
                };
                let mut raw = go_json(event);
                if evidence == "malformed named" {
                    raw.pop();
                    let (value, valid) = read(&raw);
                    let value = value.unwrap_or(Value::Null);
                    assert!(
                        !valid && s(&value, &format!("{path}.0.name")) == "functions__apply_patch",
                        "{name}: fixture must retain a recoverable patch step in incomplete outer JSON"
                    );
                }
                let mut stream = patch_stream();
                let events = send(&mut stream, raw);
                if evidence == "valid named" {
                    assert_patch_lifecycle(&events, "secret");
                } else {
                    assert_review_failure(&mut stream, &events);
                }
            }
        }
    }
}

// Ports TestInteractionsApplyPatchFinalOnlyOtherToolsKeepLegacyBehavior.
#[test]
fn final_only_other_tools_keep_legacy_behavior() {
    for name in ["lookup", "exec"] {
        for malformed in [false, true] {
            let mut stream = stream_for(
                r#"{"tools":[{"type":"custom","name":"apply_patch"},{"type":"function","name":"lookup"},{"type":"custom","name":"exec"}]}"#,
            );
            let mut raw = go_json(json!({
                "event_type": "interaction.completed",
                "steps": [{"type": "function_call", "name": name, "arguments": {"x": 1}}],
            }));
            if malformed {
                raw.pop();
            }
            let events = send(&mut stream, raw);
            assert!(
                events.len() == 1
                    && kind(&events[0]) == "response.completed"
                    && get(&events[0], "response.output")
                        .and_then(Value::as_array)
                        .is_some_and(Vec::is_empty),
                "{name}/{malformed}: legacy final-only ordinary/custom behavior changed: {events:?}"
            );
            assert!(
                error_of(&stream).is_none(),
                "{name}/{malformed}: ordinary/custom arguments received patch validation: {:?}",
                error_of(&stream)
            );
        }
    }
}

// Ports TestInteractionsApplyPatchFinalSnapshotsPreserveOtherOutputsAndCalls.
#[test]
fn final_snapshots_preserve_other_outputs_and_calls() {
    let mut stream = stream_for(
        r#"{"tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"apply_patch"}]},{"type":"function","name":"lookup"},{"type":"custom","name":"exec"}]}"#,
    );
    let snapshots = [
        json!({"type": "function_call", "id": "item_2", "call_id": "call_2", "name": "functions__apply_patch", "arguments": {"input": "p"}}),
        json!({"type": "function_call", "id": "ordinary", "name": "lookup", "arguments": {"x": 1}}),
        json!({"type": "function_call", "id": "custom", "name": "exec", "arguments": {"command": "ls"}}),
        json!({"type": "model_output", "id": "message", "content": "hello"}),
    ];
    for (index, step) in snapshots.iter().enumerate() {
        let event = |entry: &str, key: &str, value: Value| {
            go_json(json!({ "event_type": entry, "index": index, key: value }))
        };
        send(&mut stream, event("step.start", "step", step.clone()));
        if index == 3 {
            send(
                &mut stream,
                event(
                    "step.delta",
                    "delta",
                    json!({"type": "text", "text": "hello"}),
                ),
            );
        }
        send(&mut stream, event("step.stop", "step", step.clone()));
    }
    // Array positions are not explicit call indexes when the supplied IDs
    // identify other steps.
    let final_steps = json!([snapshots[3], snapshots[2], snapshots[1], snapshots[0]]);
    let events = send(
        &mut stream,
        go_json(
            json!({"event_type": "interaction.completed", "interaction": {"steps": final_steps}}),
        ),
    );
    assert!(
        events.len() == 1 && kind(&events[0]) == "response.completed",
        "legitimate reordered snapshots failed: {events:?}"
    );
    let output = get(&events[0], "response.output")
        .cloned()
        .unwrap_or(Value::Null);
    assert!(
        output.as_array().map(Vec::len) == Some(4)
            && s(&output, "0.input") == "p"
            && s(&output, "1.arguments") == r#"{"x":1}"#
            && s(&output, "2.input") == r#"{"command":"ls"}"#
            && s(&output, "3.content.0.text") == "hello",
        "other output/call lost or rebound: {output}"
    );
}

// Ports TestInteractionsApplyPatchFinalOnlyCallDoesNotRebindUnrelatedItem.
#[test]
fn final_only_call_does_not_rebind_unrelated_item() {
    let mut stream = stream_for(
        r#"{"tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"apply_patch"}]},{"type":"function","name":"lookup"}]}"#,
    );
    send(
        &mut stream,
        r#"{"event_type":"step.start","index":0,"step":{"type":"function_call","id":"ordinary","name":"lookup","arguments":{"x":1}}}"#,
    );
    send(&mut stream, r#"{"event_type":"step.stop","index":0}"#);
    let events = send(
        &mut stream,
        r#"{"event_type":"interaction.completed","steps":[{"type":"function_call","id":"patch","call_id":"patch_call","name":"functions__apply_patch","arguments":{"input":"p"}}]}"#,
    );
    let fin = events.last().cloned().unwrap_or(Value::Null);
    assert!(
        kind(&fin) == "response.completed"
            && s(&fin, "response.output.0.id") == "ordinary"
            && s(&fin, "response.output.0.arguments") == r#"{"x":1}"#
            && s(&fin, "response.output.1.id") == "patch"
            && s(&fin, "response.output.1.input") == "p",
        "array position rebound unrelated call: {events:?}"
    );
}

// Ports TestInteractionsApplyPatchExistingCallAcceptsOmittedSnapshotType.
#[test]
fn existing_call_accepts_omitted_snapshot_type() {
    for entry in ["step.start", "step.stop", "interaction.completed"] {
        let mut stream = completed_patch_stream();
        let step = json!({"id": "item_2", "arguments": {"input": "p"}});
        let raw = if entry == "interaction.completed" {
            go_json(json!({ "event_type": entry, "steps": [step] }))
        } else {
            patch_step(entry, step)
        };
        let mut events = send(&mut stream, raw);
        if entry != "interaction.completed" {
            assert!(
                events.is_empty(),
                "{entry}: equivalent snapshot replayed lifecycle: {events:?}"
            );
            events = send(&mut stream, r#"{"event_type":"interaction.completed"}"#);
        }
        assert!(
            events.len() == 1
                && kind(&events[0]) == "response.completed"
                && s(&events[0], "response.output.0.input") == "p",
            "{entry}: omitted type erased existing patch identity: {events:?}"
        );
    }
}

// Ports TestInteractionsApplyPatchMalformedPreNameEnvelopeRetainsSuppliedID.
#[test]
fn malformed_pre_name_envelope_retains_supplied_id() {
    let mut stream = patch_stream();
    let events = send(
        &mut stream,
        r#"{"event_type":"step.start","step":{"type":"function_call","id":"item_2","call_id":"call_2","arguments":{"input":"secret"}}"#,
    );
    assert!(
        events.is_empty(),
        "unnamed malformed evidence leaked: {events:?}"
    );
    let events = send(
        &mut stream,
        r#"{"event_type":"interaction.completed","steps":[{"type":"function_call","id":"item_2","call_id":"call_2","name":"functions__apply_patch","arguments":{"input":"secret"}}]}"#,
    );
    assert_review_failure(&mut stream, &events);
}

// Ports TestInteractionsApplyPatchFinalOnlyUnnamedUnkeyedFunctionFails.
#[test]
fn final_only_unnamed_unkeyed_function_fails() {
    let mut stream = patch_stream();
    let events = send(
        &mut stream,
        r#"{"event_type":"interaction.completed","steps":[{"type":"function_call","arguments":{"input":"secret"}}]}"#,
    );
    assert_review_failure(&mut stream, &events);
}

// Ports TestInteractionsApplyPatchOrdinaryLateIdentityReplaysOnlyBufferedPrefix.
#[test]
fn ordinary_late_identity_replays_only_buffered_prefix() {
    let mut stream = stream_for(
        r#"{"tools":[{"type":"custom","name":"apply_patch"},{"type":"function","name":"lookup"}]}"#,
    );
    let mut events = Vec::new();
    for raw in [
        r#"{"event_type":"step.start","index":2,"step":{"type":"function_call","arguments":{}}}"#,
        r#"{"event_type":"step.delta","index":2,"delta":{"type":"arguments_delta","arguments":"{\"x\":"}}"#,
        r#"{"event_type":"step.start","index":2,"step":{"type":"function_call","id":"item_2","name":"lookup"}}"#,
        r#"{"event_type":"step.delta","index":2,"delta":{"type":"arguments_delta","arguments":"1}"}}"#,
        r#"{"event_type":"step.stop","index":2,"step":{"type":"function_call","id":"item_2","name":"lookup","arguments":{"x":1}}}"#,
        r#"{"event_type":"interaction.completed"}"#,
    ] {
        events.extend(send(&mut stream, raw));
    }
    let mut delta = String::new();
    for event in &events {
        if kind(event) == "response.function_call_arguments.delta" {
            delta.push_str(&s(event, "delta"));
        }
        if kind(event) == "response.function_call_arguments.done" {
            assert_eq!(s(event, "arguments"), r#"{"x":1}"#, "{event}");
        }
    }
    let fin = events.last().cloned().unwrap_or(Value::Null);
    assert!(
        delta == r#"{"x":1}"# && s(&fin, "response.output.0.arguments") == delta,
        "late-identity prefix replayed or lost: delta={delta:?} events={events:?}"
    );
}

/// The output index of the `apply_patch` call at `index`, if it was
/// announced.
fn patch_output_index(stream: &InteractionsToOpenAIResponsesStream, index: i64) -> Option<i64> {
    stream
        .calls
        .get(&index)
        .and_then(|call| call.patch.as_ref())
        .map(|patch| patch.output_index)
}

// Ports TestInteractionsApplyPatchConflictingIndexAndIDs.
#[test]
fn conflicting_index_and_ids() {
    let identities = [
        ("both IDs", "item_2", "call_2", 3),
        ("item ID only", "item_2", "", 3),
        ("call ID only", "", "call_2", 3),
        ("matching item wrong call", "item_2", "wrong_call", 3),
        ("wrong item matching call", "wrong_item", "call_2", 3),
        ("item and other patch call", "item_2", "call_4", 3),
        ("other patch item and call", "item_4", "call_2", 3),
        ("index of other patch", "item_2", "call_2", 4),
        ("split IDs without index", "item_2", "call_4", -1),
        ("split IDs at real index", "item_2", "call_4", 2),
    ];
    for entry in ["step.start", "step.stop", "interaction.completed", "finish"] {
        for phase in ["active item", "completed item"] {
            for (identity, item_id, call_id, index) in identities {
                for snapshot in [
                    "model_output",
                    "thought",
                    "custom_tool_call",
                    "function same input",
                    "function changed input",
                    "omitted type",
                ] {
                    let name = format!("{entry}/{phase}/{identity}/{snapshot}");
                    let mut stream = patch_stream();
                    send(
                        &mut stream,
                        patch_step(
                            "step.start",
                            json!({"type": "function_call", "id": "item_2", "call_id": "call_2", "name": "functions__apply_patch"}),
                        ),
                    );
                    send(&mut stream, arguments_delta(2, &patch_arguments("p")));
                    send(
                        &mut stream,
                        patch_review_snapshot(
                            "step.start",
                            4,
                            json!({"type": "function_call", "id": "item_4", "call_id": "call_4", "name": "functions__apply_patch", "arguments": {"input": "q"}}),
                        ),
                    );
                    if phase == "completed item" {
                        let completed = send(&mut stream, patch_step("step.stop", Value::Null));
                        assert!(
                            completed.len() == 2
                                && kind(&completed[1]) == "response.output_item.done",
                            "{name}: fixture did not complete real source input: {completed:?}"
                        );
                    }
                    let mut step = json!({});
                    if !item_id.is_empty() {
                        step["id"] = json!(item_id);
                    }
                    if !call_id.is_empty() {
                        step["call_id"] = json!(call_id);
                    }
                    match snapshot {
                        "function same input" | "function changed input" | "omitted type" => {
                            step["name"] = json!("functions__apply_patch");
                            let input = if snapshot == "function changed input" {
                                "secret"
                            } else {
                                "p"
                            };
                            step["arguments"] = json!({ "input": input });
                            if snapshot != "omitted type" {
                                step["type"] = json!("function_call");
                            }
                        }
                        _ => step["type"] = json!(snapshot),
                    }
                    let events = send(&mut stream, patch_review_snapshot(entry, index, step));
                    assert_review_failure(&mut stream, &events);
                    assert!(
                        stream.calls.len() == 2 && patch_output_index(&stream, 2) == Some(2),
                        "{name}: conflicting identity created or rebound a patch call"
                    );
                }
            }
        }
    }
}

// Ports TestInteractionsApplyPatchRootAndStepIndexesMustAgree.
#[test]
fn root_and_step_indexes_must_agree() {
    for entry in ["step.start", "step.stop"] {
        for (root, nested) in [(2, 3), (3, 2)] {
            let mut stream = completed_patch_stream();
            let step = json!({"index": nested, "type": "function_call", "id": "item_2", "call_id": "call_2", "name": "functions__apply_patch", "arguments": {"input": "p"}});
            let events = send(&mut stream, patch_review_snapshot(entry, root, step));
            assert_review_failure(&mut stream, &events);
        }
    }
}

// Ports TestInteractionsApplyPatchPreNameIndexConflictRetained.
#[test]
fn pre_name_index_conflict_retained() {
    for entry in ["step.start", "step.stop", "interaction.completed", "finish"] {
        for key in ["id", "call_id", "both"] {
            for snapshot_type in ["model_output", "function_call", "omitted"] {
                for resolved_at in [
                    "original index",
                    "changed index",
                    "IDs only",
                    "original index without IDs",
                    "changed index without IDs",
                ] {
                    let name = format!("{entry}/{key}/{snapshot_type}/{resolved_at}");
                    let mut stream = patch_stream();
                    send(
                        &mut stream,
                        patch_step(
                            "step.start",
                            json!({"type": "function_call", "id": "item_2", "call_id": "call_2", "arguments": {"input": "p"}}),
                        ),
                    );
                    let mut conflict = json!({"index": 3});
                    if key != "call_id" {
                        conflict["id"] = json!("item_2");
                    }
                    if key != "id" {
                        conflict["call_id"] = json!("call_2");
                    }
                    if snapshot_type != "omitted" {
                        conflict["type"] = json!(snapshot_type);
                    }
                    let mut resolved = json!({"type": "function_call", "id": "item_2", "call_id": "call_2", "name": "functions__apply_patch", "arguments": {"input": "p"}});
                    let index = if resolved_at.starts_with("original index") {
                        2
                    } else if resolved_at.starts_with("changed index") {
                        3
                    } else {
                        -1
                    };
                    if resolved_at.ends_with("without IDs")
                        && let Some(resolved) = resolved.as_object_mut()
                    {
                        resolved.remove("id");
                        resolved.remove("call_id");
                    }
                    let events = if entry == "step.start" || entry == "step.stop" {
                        // Unknown names must not make identity contradictions
                        // disappear later.
                        send(&mut stream, patch_review_snapshot(entry, 3, conflict));
                        send(
                            &mut stream,
                            patch_review_snapshot("interaction.completed", index, resolved),
                        )
                    } else {
                        if index >= 0 {
                            resolved["index"] = json!(index);
                        }
                        send(
                            &mut stream,
                            go_json(json!({ "event_type": entry, "steps": [conflict, resolved] })),
                        )
                    };
                    assert_review_failure(&mut stream, &events);
                    assert!(
                        stream
                            .calls
                            .values()
                            .all(|call| call.patch.is_none() && !call.added),
                        "{name}: late identity announced a patch after contradictory evidence"
                    );
                }
            }
        }
    }
}

// Ports TestInteractionsApplyPatchConsistentSnapshotIdentityMatrix.
#[test]
fn consistent_snapshot_identity_matrix() {
    for entry in ["step.start", "step.stop", "interaction.completed", "finish"] {
        for key in ["index and IDs", "IDs only", "item only", "call only"] {
            for snapshot_type in ["function_call", "omitted"] {
                let name = format!("{entry}/{key}/{snapshot_type}");
                let mut stream = patch_stream();
                let mut events = send(
                    &mut stream,
                    patch_step(
                        "step.start",
                        json!({"type": "function_call", "id": "item_2", "call_id": "call_2", "name": "functions__apply_patch", "arguments": {"input": "p"}}),
                    ),
                );
                events.extend(send(&mut stream, patch_step("step.stop", Value::Null)));
                let mut step = json!({"arguments": {"input": "p"}});
                let index = if key == "index and IDs" { 2 } else { -1 };
                if key != "call only" {
                    step["id"] = json!("item_2");
                }
                if key != "item only" {
                    step["call_id"] = json!("call_2");
                }
                if snapshot_type != "omitted" {
                    step["type"] = json!(snapshot_type);
                }
                events.extend(send(&mut stream, patch_review_snapshot(entry, index, step)));
                if entry == "step.start" || entry == "step.stop" {
                    events.extend(send(
                        &mut stream,
                        r#"{"event_type":"interaction.completed"}"#,
                    ));
                }
                assert_patch_lifecycle(&events, "p");
                assert!(
                    stream.calls.len() == 1 && error_of(&stream).is_none(),
                    "{name}: consistent snapshot changed identity: {:?}",
                    error_of(&stream)
                );
            }
        }
    }
}

// Ports TestInteractionsApplyPatchUnmatchedNewIDsRemainIndependent.
#[test]
fn unmatched_new_ids_remain_independent() {
    for entry in ["step.start", "interaction.completed", "finish"] {
        for index in [-1, 3] {
            let name = format!("{entry}/{index}");
            let mut stream = completed_patch_stream();
            let step = json!({"type": "function_call", "id": "new_item", "call_id": "new_call", "name": "functions__apply_patch", "arguments": {"input": "q"}});
            let mut events = send(&mut stream, patch_review_snapshot(entry, index, step));
            if entry == "step.start" {
                events.extend(send(
                    &mut stream,
                    r#"{"event_type":"interaction.completed"}"#,
                ));
            }
            let fin = events.last().cloned().unwrap_or(Value::Null);
            let output = get(&fin, "response.output")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            assert!(
                kind(&fin) == "response.completed" && output.len() == 2,
                "{name}: unmatched new IDs were rejected: {events:?}"
            );
            let inputs: HashMap<String, String> = output
                .iter()
                .map(|item| (s(item, "call_id"), s(item, "input")))
                .collect();
            assert!(
                inputs.get("call_2").map(String::as_str) == Some("p")
                    && inputs.get("new_call").map(String::as_str) == Some("q")
                    && error_of(&stream).is_none(),
                "{name}: new call rebound existing patch: {events:?}"
            );
        }
    }
}

// Ports TestInteractionsApplyPatchOrdinaryExplicitIndexKeepsLegacyBehavior.
#[test]
fn ordinary_explicit_index_keeps_legacy_behavior() {
    let mut stream = stream_for(
        r#"{"tools":[{"type":"custom","name":"apply_patch"},{"type":"function","name":"lookup"}]}"#,
    );
    for index in [2, 3] {
        let step = json!({"type": "function_call", "id": "ordinary", "call_id": "ordinary_call", "name": "lookup", "arguments": {"x": index}});
        send(
            &mut stream,
            patch_review_snapshot("step.start", index, step.clone()),
        );
        send(&mut stream, patch_review_snapshot("step.stop", index, step));
    }
    let events = send(&mut stream, r#"{"event_type":"interaction.completed"}"#);
    assert!(
        events.len() == 1
            && kind(&events[0]) == "response.completed"
            && s(&events[0], "response.output.0.arguments") == r#"{"x":2}"#
            && s(&events[0], "response.output.1.arguments") == r#"{"x":3}"#
            && error_of(&stream).is_none(),
        "ordinary explicit-index behavior received patch validation: {events:?}"
    );
}

// Ports TestInteractionsApplyPatchThreeEventChangedIndexReproduction.
#[test]
fn three_event_changed_index_reproduction() {
    for snapshot in [
        r#"{"index":3,"type":"model_output","id":"item_2","call_id":"call_2"}"#,
        r#"{"index":3,"type":"function_call","id":"item_2","call_id":"call_2","name":"functions__apply_patch","arguments":{"input":"secret"}}"#,
    ] {
        let mut stream = patch_stream();
        send(
            &mut stream,
            r#"{"event_type":"step.start","index":2,"step":{"type":"function_call","id":"item_2","call_id":"call_2","name":"functions__apply_patch","arguments":{"input":"p"}}}"#,
        );
        send(&mut stream, r#"{"event_type":"step.stop","index":2}"#);
        let events = send(
            &mut stream,
            format!(r#"{{"event_type":"interaction.completed","steps":[{snapshot}]}}"#),
        );
        assert_review_failure(&mut stream, &events);
        assert_eq!(
            stream.calls.len(),
            1,
            "{snapshot}: terminal snapshot created a second call with the same IDs"
        );
    }
}

// Ports TestInteractionsApplyPatchIDsCannotSelectUnrelatedOrdinaryIndex.
#[test]
fn ids_cannot_select_unrelated_ordinary_index() {
    for entry in ["step.start", "step.stop", "interaction.completed", "finish"] {
        for snapshot_type in ["model_output", "function_call"] {
            let name = format!("{entry}/{snapshot_type}");
            let mut stream = stream_for(
                r#"{"tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"apply_patch"}]},{"type":"function","name":"lookup"}]}"#,
            );
            send(
                &mut stream,
                patch_step(
                    "step.start",
                    json!({"type": "function_call", "id": "item_2", "call_id": "call_2", "name": "functions__apply_patch", "arguments": {"input": "p"}}),
                ),
            );
            send(&mut stream, patch_step("step.stop", Value::Null));
            send(
                &mut stream,
                patch_review_snapshot(
                    "step.start",
                    3,
                    json!({"type": "function_call", "id": "ordinary", "call_id": "ordinary_call", "name": "lookup", "arguments": {"x": 1}}),
                ),
            );
            send(
                &mut stream,
                patch_review_snapshot("step.stop", 3, Value::Null),
            );
            let step = json!({"type": snapshot_type, "id": "item_2", "call_id": "call_2", "name": "lookup", "arguments": {"x": 2}});
            let events = send(&mut stream, patch_review_snapshot(entry, 3, step));
            assert_review_failure(&mut stream, &events);
            let call = stream.calls.get(&3);
            assert!(
                call.is_some_and(|call| call.id == "ordinary"
                    && call.arguments == r#"{"x":1}"#
                    && call.patch.is_none()),
                "{name}: conflicting patch IDs mutated an unrelated ordinary call"
            );
        }
    }
}

// Ports TestInteractionsApplyPatchNestedChangedIndexAndCompletedSource.
#[test]
fn nested_changed_index_and_completed_source() {
    for entry in ["step.start", "step.stop", "interaction.completed", "finish"] {
        for index in [-1, 2, 3] {
            let name = format!("{entry}/{index}");
            let mut stream = patch_stream();
            send(
                &mut stream,
                patch_step(
                    "step.start",
                    json!({"type": "function_call", "id": "item_2", "call_id": "call_2", "name": "functions__apply_patch"}),
                ),
            );
            send(&mut stream, arguments_delta(2, &patch_arguments("p")));
            send(&mut stream, patch_step("step.stop", Value::Null));
            let mut step = json!({"type": "function_call", "id": "item_2", "call_id": "call_2", "name": "functions__apply_patch", "arguments": {"input": "secret"}});
            if index >= 0 {
                step["index"] = json!(index);
            }
            let events = send(&mut stream, patch_review_snapshot(entry, -1, step));
            assert_review_failure(&mut stream, &events);
            assert_eq!(
                stream.calls.len(),
                1,
                "{name}: function snapshot bypassed completed real source comparison"
            );
        }
    }
}
