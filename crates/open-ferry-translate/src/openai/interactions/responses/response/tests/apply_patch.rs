// Ported from CLIProxyAPI internal/translator/openai/interactions/responses/apply_patch_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The client's `apply_patch` custom tool called through an Interactions
//! function call: its input decoded as the arguments stream, at every split,
//! and the response failed when the arguments, repeated snapshots or the
//! call's identity are wrong.
//!
//! Dropped or changed tests:
//! - TestInteractionsApplyPatchDeclarationAndHistory tests the request
//!   translator, and is with its tests (`request/tests/patch_declaration.rs`).
//! - TestInteractionsApplyPatchWinningDeclarationAndNegativeCompatibility
//!   keeps only its response half; the check of the declaration the request
//!   translator writes is with the request translator's tests.
//! - Upstream reads the whole-response failure from the stream state it
//!   passes in; these read it from the converter's `Err`.

use super::*;

// Ports TestInteractionsApplyPatchEverySplitPreviewAndCompletion.
#[test]
fn every_split_previews_and_completes() {
    let args = patch_arguments(PATCH_TEXT)
        .replace('中', "\x5cu4e2d")
        .replace('😀', "\x5cud83d\x5cude00");
    assert!(args.is_ascii());
    for split in 0..=args.len() {
        let mut stream = patch_stream();
        let mut events = send(
            &mut stream,
            patch_step(
                "step.start",
                json!({"type": "function_call", "id": "item_2", "call_id": "call_2", "name": "functions__apply_patch", "arguments": {}}),
            ),
        );
        for fragment in [&args[..split], &args[split..]] {
            events.extend(send(&mut stream, arguments_delta(2, fragment)));
        }
        let preview: String = events
            .iter()
            .filter(|event| kind(event) == "response.custom_tool_call_input.delta")
            .map(|event| s(event, "delta"))
            .collect();
        assert_eq!(
            preview, PATCH_TEXT,
            "split {split}: no decoded preview before stop"
        );
        events.extend(send(&mut stream, patch_step("step.stop", Value::Null)));
        events.extend(send(
            &mut stream,
            r#"{"event_type":"interaction.completed","interaction":{"id":"r"}}"#,
        ));
        assert_patch_lifecycle(&events, PATCH_TEXT);
    }
}

// Ports TestInteractionsApplyPatchInvalidNonStream.
#[test]
fn invalid_whole_response() {
    for args in [
        "{}",
        r#"{"input":1}"#,
        r#"{"input":"a","input":"b"}"#,
        r#"{"input":"secret""#,
        r#"{"input":"x","extra":1}"#,
    ] {
        let raw = go_json(json!({
            "id": "r",
            "steps": [{"type": "function_call", "name": "functions__apply_patch", "arguments": args}],
        }));
        let request = parse(PATCH_REQUEST);
        assert!(
            convert_interactions_response_to_openai_responses_non_stream(
                MODEL,
                &request,
                &Value::Null,
                &raw
            )
            .is_none(),
            "invalid arguments returned output: {args}"
        );
        assert!(
            non_stream(MODEL, &request, &Value::Null, &raw).is_err(),
            "missing error: {args}"
        );
    }
}

// Ports TestInteractionsApplyPatchSnapshotMatrix.
#[test]
fn snapshot_matrix() {
    let start = |name: &str, id: &str, args: &str| {
        let mut step = json!({"type": "function_call", "id": id, "call_id": "call_2"});
        if !name.is_empty() {
            step["name"] = json!(name);
        }
        if !args.is_empty() {
            step["arguments"] = json!(args);
        }
        patch_step("step.start", step)
    };
    let delta = |args: &str| arguments_delta(2, args);
    let stop = |args: &str| {
        patch_step(
            "step.stop",
            json!({"name": "functions__apply_patch", "arguments": args}),
        )
    };
    let fin = |args: &str| {
        go_json(json!({
            "event_type": "interaction.completed",
            "interaction": {"id": "r", "steps": [{"index": 2, "type": "function_call", "id": "item_2", "call_id": "call_2", "name": "functions__apply_patch", "arguments": args}]},
        }))
    };
    let patch = "functions__apply_patch";
    let cases: Vec<(&str, Vec<Vec<u8>>, bool, &str)> = vec![
        (
            "partial prefix completed by stop",
            vec![
                start(patch, "item_2", ""),
                delta(r#"{"input":"a"#),
                stop(r#"{"input":"ab"}"#),
                fin(r#"{"input":"ab"}"#),
            ],
            false,
            "ab",
        ),
        (
            "equivalent full snapshot",
            vec![
                start(patch, "item_2", r#"{"input":"a"}"#),
                stop("{ \"input\" : \"\x5cu0061\" }"),
                fin(r#"{"input":"a"}"#),
            ],
            false,
            "a",
        ),
        (
            "full source cannot extend",
            vec![
                start(patch, "item_2", ""),
                delta(r#"{"input":"a"}"#),
                stop(r#"{"input":"ab"}"#),
            ],
            true,
            "",
        ),
        (
            "complete snapshot cannot extend",
            vec![
                start(patch, "item_2", r#"{"input":"a"}"#),
                stop(r#"{"input":"ab"}"#),
            ],
            true,
            "",
        ),
        (
            "invalid snapshot before name retained",
            vec![
                start("", "item_2", r#"{"input":1}"#),
                start(patch, "item_2", r#"{"input":"a"}"#),
            ],
            true,
            "",
        ),
        (
            "truncated snapshot before name retained",
            vec![
                start("", "item_2", r#"{"input":"a"#),
                start(patch, "item_2", r#"{"input":"a"}"#),
            ],
            true,
            "",
        ),
        (
            "conflicting ID before name retained",
            vec![
                start("", "item_2", ""),
                start("", "wrong", ""),
                start(patch, "item_2", r#"{"input":"a"}"#),
            ],
            true,
            "",
        ),
        (
            "unknown-name fragments replayed",
            vec![
                start("", "item_2", ""),
                delta(r#"{"input":"a"#),
                start(patch, "item_2", ""),
                delta(r#"b"}"#),
                stop(r#"{"input":"ab"}"#),
                fin(r#"{"input":"ab"}"#),
            ],
            false,
            "ab",
        ),
        (
            "completed item conflicts at final",
            vec![
                start(patch, "item_2", r#"{"input":"a"}"#),
                stop(r#"{"input":"a"}"#),
                fin(r#"{"input":"ab"}"#),
            ],
            true,
            "",
        ),
        (
            "completed item conflicts at repeated stop",
            vec![
                start(patch, "item_2", r#"{"input":"a"}"#),
                stop(r#"{"input":"a"}"#),
                stop(r#"{"input":"ab"}"#),
            ],
            true,
            "",
        ),
        (
            "completed item rejects later delta",
            vec![
                start(patch, "item_2", r#"{"input":"a"}"#),
                stop(r#"{"input":"a"}"#),
                delta(r#"{"input":"ab"}"#),
            ],
            true,
            "",
        ),
        (
            "truncated delta cannot complete without snapshot",
            vec![
                start(patch, "item_2", ""),
                delta(r#"{"input":"a"#),
                patch_step("step.stop", Value::Null),
            ],
            true,
            "",
        ),
        (
            "illegal delta remains failed",
            vec![
                start(patch, "item_2", ""),
                delta(r#"{"input":1}"#),
                stop(r#"{"input":"a"}"#),
            ],
            true,
            "",
        ),
    ];
    for (name, inputs, fail, want) in cases {
        let mut stream = patch_stream();
        let mut events = Vec::new();
        for input in inputs {
            events.extend(send(&mut stream, input));
        }
        let failures: Vec<&Value> = events
            .iter()
            .filter(|event| kind(event) == "response.failed")
            .collect();
        for failure in &failures {
            assert_eq!(
                s(failure, "response.error.code"),
                "invalid_tool_arguments",
                "{name}: {failure}"
            );
        }
        if fail {
            assert_eq!(failures.len(), 1, "{name}: {events:?}");
            assert!(error_of(&stream).is_some(), "{name}: missing error");
        } else {
            assert!(
                failures.is_empty(),
                "{name}: unexpected failure: {events:?}"
            );
            assert_patch_lifecycle(&events, want);
        }
        for input in [
            br#"{"event_type":"interaction.completed"}"#.to_vec(),
            br#"{"event_type":"interaction.failed"}"#.to_vec(),
            b"[DONE]".to_vec(),
            patch_step("step.start", json!({"type": "model_output"})),
            delta("late"),
        ] {
            let more = send(&mut stream, input);
            assert!(more.is_empty(), "{name}: terminal reopened: {more:?}");
        }
    }
}

// Ports the response half of
// TestInteractionsApplyPatchWinningDeclarationAndNegativeCompatibility.
#[test]
fn winning_declaration_and_negative_compatibility() {
    for (name, request, upstream, args, want_type, want_input) in [
        (
            "top function beats additional custom",
            r#"{"tools":[{"type":"function","name":"apply_patch","parameters":{"type":"object","properties":{"n":{"type":"number"}}}}],"input":[{"type":"additional_tools","tools":[{"type":"custom","name":"apply_patch"}]}]}"#,
            "apply_patch",
            r#"{"n":1}"#,
            "function_call",
            "",
        ),
        (
            "direct function beats namespace custom",
            r#"{"tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"apply_patch"}]},{"type":"function","name":"functions__apply_patch"}]}"#,
            "functions__apply_patch",
            r#"{"n":1}"#,
            "function_call",
            "",
        ),
        (
            "other custom remains lenient",
            r#"{"tools":[{"type":"custom","name":"exec"}]}"#,
            "exec",
            r#"{"command":"ls"}"#,
            "custom_tool_call",
            r#"{"command":"ls"}"#,
        ),
        (
            "sanitization collision keeps qualified Interactions name",
            r#"{"tools":[{"type":"namespace","name":"a.b","tools":[{"type":"custom","name":"apply_patch"}]},{"type":"function","name":"a_b__apply_patch"}]}"#,
            "a.b__apply_patch",
            r#"{"input":"p"}"#,
            "custom_tool_call",
            "p",
        ),
    ] {
        let mut stream = stream_for(request);
        send(
            &mut stream,
            patch_step(
                "step.start",
                json!({"type": "function_call", "id": "c", "name": upstream}),
            ),
        );
        send(&mut stream, arguments_delta(2, args));
        send(&mut stream, patch_step("step.stop", Value::Null));
        let events = send(&mut stream, r#"{"event_type":"interaction.completed"}"#);
        let item = events
            .last()
            .and_then(|event| get(event, "response.output.0"))
            .cloned()
            .unwrap_or(Value::Null);
        assert!(
            s(&item, "type") == want_type && s(&item, "input") == want_input,
            "{name}: compatibility changed: {item}"
        );
        if want_type == "function_call" {
            assert_eq!(s(&item, "arguments"), args, "{name}: {item}");
        }
    }
}

// Ports TestInteractionsApplyPatchMalformedEnvelopeAndTerminalFailure.
#[test]
fn malformed_envelope_and_terminal_failure() {
    let mut stream = patch_stream();
    send(
        &mut stream,
        patch_step(
            "step.start",
            json!({"type": "function_call", "id": "item_2", "call_id": "call_2", "name": "functions__apply_patch"}),
        ),
    );
    let events = send(
        &mut stream,
        r#"{"event_type":"step.stop","index":2,"step":{"arguments":{"input":"secret"}}"#,
    );
    assert!(
        events.len() == 1 && kind(&events[0]) == "response.failed",
        "malformed complete snapshot accepted: {events:?}"
    );
    let more = send(&mut stream, r#"{"event_type":"interaction.completed"}"#);
    assert!(more.is_empty(), "failure reopened");
    let out = convert_interactions_response_to_openai_responses_non_stream(
        "devin",
        &parse(PATCH_REQUEST),
        &Value::Null,
        br#"{"id":"r","steps":[{"type":"function_call","name":"functions__apply_patch","arguments":{"input":"secret"}}]"#,
    );
    assert!(out.is_none(), "malformed whole response accepted: {out:?}");
}

// Ports TestInteractionsApplyPatchDeltaBeforeStartDoesNotLeakArguments.
#[test]
fn delta_before_start_does_not_leak_arguments() {
    let mut stream = patch_stream();
    let early = send(&mut stream, arguments_delta(2, r#"{"input":"a"#));
    assert!(
        early.is_empty(),
        "raw arguments emitted before tool identity: {early:?}"
    );
    let mut events = send(
        &mut stream,
        patch_step(
            "step.start",
            json!({"type": "function_call", "id": "item_2", "call_id": "call_2", "name": "functions__apply_patch"}),
        ),
    );
    events.extend(send(&mut stream, arguments_delta(2, r#"b"}"#)));
    events.extend(send(&mut stream, patch_step("step.stop", Value::Null)));
    events.extend(send(
        &mut stream,
        r#"{"event_type":"interaction.completed"}"#,
    ));
    assert_patch_lifecycle(&events, "ab");
}

// Ports TestInteractionsApplyPatchInitialSnapshotDoesNotFakePreview.
#[test]
fn initial_snapshot_does_not_fake_preview() {
    let mut stream = patch_stream();
    let events = send(
        &mut stream,
        patch_step(
            "step.start",
            json!({"type": "function_call", "id": "item_2", "call_id": "call_2", "name": "functions__apply_patch", "arguments": {"input": "p"}}),
        ),
    );
    assert_eq!(
        count(&events, "response.custom_tool_call_input.delta"),
        0,
        "initial complete snapshot presented as early generation"
    );
}

// Ports TestInteractionsApplyPatchEarlyPreviewBeforeJSONCompletion.
#[test]
fn early_preview_before_json_completion() {
    let mut stream = patch_stream();
    send(
        &mut stream,
        patch_step(
            "step.start",
            json!({"type": "function_call", "id": "item_2", "call_id": "call_2", "name": "functions__apply_patch"}),
        ),
    );
    let events = send(
        &mut stream,
        arguments_delta(2, r#"{"input":"*** Begin Patch\n"#),
    );
    assert!(
        events.len() == 1
            && s(&events[0], "delta") == "*** Begin Patch\n"
            && kind(&events[0]) == "response.custom_tool_call_input.delta",
        "no early preview: {events:?}"
    );
}

// Ports TestInteractionsApplyPatchLateIdentityAtStopAndCompletedItemTypeConflict.
#[test]
fn late_identity_at_stop_and_completed_item_type_conflict() {
    let mut stream = patch_stream();
    send(
        &mut stream,
        patch_step(
            "step.start",
            json!({"type": "function_call", "arguments": {"input": "p"}}),
        ),
    );
    let events = send(&mut stream, patch_step("step.stop", Value::Null));
    assert!(
        events.is_empty(),
        "unknown tool completed with raw arguments: {events:?}"
    );
    let events = send(
        &mut stream,
        patch_step(
            "step.stop",
            json!({"type": "function_call", "id": "item_2", "call_id": "call_2", "name": "functions__apply_patch", "arguments": {"input": "p"}}),
        ),
    );
    assert_eq!(
        events.len(),
        4,
        "late identity did not complete: {events:?}"
    );
    let events = send(
        &mut stream,
        patch_step(
            "step.start",
            json!({"type": "model_output", "id": "item_2"}),
        ),
    );
    assert!(
        events.len() == 1 && kind(&events[0]) == "response.failed",
        "completed item type conflict ignored: {events:?}"
    );
}

// Ports TestInteractionsApplyPatchDoesNotChangeOtherFunctionNames.
#[test]
fn does_not_change_other_function_names() {
    let mut stream = stream_for(
        r#"{"tools":[{"type":"custom","name":"apply_patch"},{"type":"function","name":"read_file"}]}"#,
    );
    let events = send(
        &mut stream,
        patch_step(
            "step.start",
            json!({"type": "function_call", "id": "c", "name": "external_read_file"}),
        ),
    );
    assert!(
        events.len() == 1 && s(&events[0], "item.name") == "external_read_file",
        "borrowed a tool name alias: {events:?}"
    );
}

// Ports TestInteractionsApplyPatchInterleavedCallsRemainIndependent.
#[test]
fn interleaved_calls_remain_independent() {
    let mut stream = patch_stream();
    let mut events = Vec::new();
    let mut step = |stream: &mut InteractionsToOpenAIResponsesStream,
                    index: i64,
                    event: &str,
                    value: Value| {
        let key = if event == "step.delta" {
            "delta"
        } else {
            "step"
        };
        events.extend(send(
            stream,
            go_json(json!({ "event_type": event, "index": index, key: value })),
        ));
    };
    for (index, name) in [(0, "c1"), (1, "c2")] {
        step(
            &mut stream,
            index,
            "step.start",
            json!({"type": "function_call", "id": name, "call_id": format!("call_{name}"), "name": "functions__apply_patch"}),
        );
    }
    let fragment = |arguments: &str| json!({"type": "arguments_delta", "arguments": arguments});
    step(&mut stream, 0, "step.delta", fragment(r#"{"input":"a"#));
    step(&mut stream, 1, "step.delta", fragment(r#"{"input":"b"#));
    step(&mut stream, 0, "step.delta", fragment(r#"1"}"#));
    step(&mut stream, 1, "step.delta", fragment(r#"2"}"#));
    step(&mut stream, 1, "step.stop", Value::Null);
    step(&mut stream, 0, "step.stop", Value::Null);
    events.extend(send(
        &mut stream,
        r#"{"event_type":"interaction.completed"}"#,
    ));
    let mut inputs = std::collections::HashMap::<String, String>::new();
    for event in &events {
        if kind(event) == "response.custom_tool_call_input.delta" {
            let id = s(event, "item_id");
            assert_eq!(s(event, "call_id"), format!("call_{id}"), "{event}");
            inputs.entry(id).or_default().push_str(&s(event, "delta"));
        }
        if kind(event) == "response.custom_tool_call_input.done" {
            let input = inputs
                .get(&s(event, "item_id"))
                .cloned()
                .unwrap_or_default();
            assert_eq!(s(event, "input"), input, "{event}");
        }
    }
    let fin = events.last().cloned().unwrap_or(Value::Null);
    assert!(
        inputs.get("c1").map(String::as_str) == Some("a1")
            && inputs.get("c2").map(String::as_str) == Some("b2")
            && s(&fin, "response.output.0.input") == "a1"
            && s(&fin, "response.output.1.input") == "b2",
        "cross-call contamination: inputs={inputs:?} final={fin}"
    );
}

// Ports TestInteractionsApplyPatchUpstreamFailureClosesEveryEvent.
#[test]
fn upstream_failure_closes_every_event() {
    let mut stream = patch_stream();
    send(
        &mut stream,
        patch_step(
            "step.start",
            json!({"type": "function_call", "id": "item_2", "call_id": "call_2", "name": "functions__apply_patch"}),
        ),
    );
    let events = send(
        &mut stream,
        r#"{"event_type":"interaction.failed","error":{"message":"upstream failed"}}"#,
    );
    assert!(
        events.len() == 1 && kind(&events[0]) == "response.failed",
        "{events:?}"
    );
    for raw in [
        br#"{"event_type":"interaction.completed"}"#.to_vec(),
        br#"{"event_type":"interaction.failed"}"#.to_vec(),
        b"[DONE]".to_vec(),
        patch_step("step.stop", Value::Null),
    ] {
        let more = send(&mut stream, raw);
        assert!(more.is_empty(), "failed response reopened: {more:?}");
    }
}

// Ports TestInteractionsApplyPatchUnresolvedIdentityCannotComplete.
#[test]
fn unresolved_identity_cannot_complete() {
    let mut stream = patch_stream();
    send(
        &mut stream,
        patch_step(
            "step.start",
            json!({"type": "function_call", "id": "c1", "arguments": {"input": "secret"}}),
        ),
    );
    let events = send(&mut stream, r#"{"event_type":"interaction.completed"}"#);
    assert!(
        events.len() == 1 && kind(&events[0]) == "response.failed",
        "unresolved identity accepted: {events:?}"
    );
}

// Ports TestInteractionsApplyPatchCompletedItemRejectsEmptySnapshot.
#[test]
fn completed_item_rejects_empty_snapshot() {
    let mut stream = patch_stream();
    send(
        &mut stream,
        patch_step(
            "step.start",
            json!({"type": "function_call", "id": "item_2", "call_id": "call_2", "name": "functions__apply_patch", "arguments": {"input": "a"}}),
        ),
    );
    send(&mut stream, patch_step("step.stop", Value::Null));
    let events = send(
        &mut stream,
        patch_step(
            "step.start",
            json!({"type": "function_call", "id": "item_2", "call_id": "call_2", "name": "functions__apply_patch", "arguments": {}}),
        ),
    );
    assert!(
        events.len() == 1 && kind(&events[0]) == "response.failed",
        "complete empty snapshot was treated as initial placeholder: {events:?}"
    );
}

// Ports TestInteractionsApplyPatchNonStreamRejectsUnresolvedIdentity.
#[test]
fn whole_response_rejects_unresolved_identity() {
    let raw = br#"{"steps":[{"type":"function_call","id":"c1","arguments":{"input":"secret"}}]}"#;
    let request = parse(PATCH_REQUEST);
    let out = convert_interactions_response_to_openai_responses_non_stream(
        "devin",
        &request,
        &Value::Null,
        raw,
    );
    assert!(out.is_none(), "unresolved identity accepted: {out:?}");
    assert!(
        non_stream("devin", &request, &Value::Null, raw).is_err(),
        "missing error state"
    );
}
