// Ported from CLIProxyAPI internal/translator/gemini/openai/responses/apply_patch_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The client's `apply_patch` custom tool called through Gemini function
//! calls: its input taken from complete arguments, and the response failed
//! when the arguments, repeated snapshots or the call's identity are wrong.
//!
//! Dropped or changed tests: none. Where the arguments leave the whole chunk
//! invalid JSON (`{"input":"secret"` in
//! gemini_apply_patch_reject_invalid_complete_arguments, and "truncated
//! snapshot before name" in gemini_apply_patch_complete_snapshot_matrix), the
//! stream and the whole response fail as upstream's do, though because the
//! chunk can't be read at all rather than from the call found in it. The
//! stream's chunk then gives `response.failed` alone, without the
//! `response.created` and `response.in_progress` upstream sends first; the
//! tests don't check those.

use super::*;

const PATCH_REQUEST: &str = r#"{"tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"apply_patch","format":{"type":"grammar","definition":"start: patch"}}]}]}"#;

fn patch_request() -> Value {
    parse(PATCH_REQUEST)
}

/// The data of each event, as Go's tests keep it.
fn event_data(stream: &mut GeminiToOpenAIResponsesStream, raw: &str) -> Vec<Value> {
    translate(stream, raw)
        .into_iter()
        .map(|(_, data)| data)
        .collect()
}

#[test]
fn gemini_apply_patch_complete_arguments_lifecycle() {
    let request = patch_request();
    let mut stream = GeminiToOpenAIResponsesStream::new("gemini-3.1-pro", &request, &Value::Null);
    let raw = r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"functions__apply_patch","args":{"input":"  *** Begin Patch\n*** End Patch\n "}}}]}}]}"#;
    let initial = translate(&mut stream, r#"{"responseId":"r"}"#);
    assert_eq!(initial.len(), 2, "initial events");
    let mut out = translate(&mut stream, raw);
    out.extend(translate(&mut stream, "[DONE]"));

    let mut delta = String::new();
    let mut done = Value::Null;
    let mut item = Value::Null;
    let mut last = Value::Null;
    for (kind, event) in out {
        match kind.as_str() {
            "response.custom_tool_call_input.delta" => delta.push_str(&text(&event, "delta")),
            "response.custom_tool_call_input.done" => done = event,
            "response.output_item.done" => item = at(&event, "item").cloned().unwrap_or_default(),
            "response.completed" => {
                last = at(&event, "response.output.0").cloned().unwrap_or_default();
            }
            _ => {}
        }
    }
    let want = "  *** Begin Patch\n*** End Patch\n ";
    assert!(
        delta == want
            && text(&done, "input") == want
            && text(&item, "input") == want
            && text(&last, "input") == want,
        "inconsistent patch input: delta={delta:?} done={done} item={item} final={last}"
    );
    assert!(
        text(&done, "call_id") == text(&item, "call_id")
            && text(&done, "item_id") == text(&item, "id")
            && text(&item, "namespace") == "functions"
            && text(&last, "id") == text(&item, "id"),
        "patch identity changed"
    );
}

#[test]
fn gemini_apply_patch_reject_invalid_complete_arguments() {
    let request = patch_request();
    let lone_surrogate = concat!(r#"{"input":""#, "\\", r#"ud800"}"#);
    for args in [
        "{}",
        r#"{"input":1}"#,
        r#"{"input":"a","input":"b"}"#,
        r#"{"input":"secret""#,
        r#"{"input":"x","extra":1}"#,
        lone_surrogate,
    ] {
        let raw = [
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"functions__apply_patch","args":"#,
            args,
            r#"}}]},"finishReason":"STOP"}]}"#,
        ]
        .concat();
        let mut stream = GeminiToOpenAIResponsesStream::new("gemini", &request, &Value::Null);

        let out = convert_gemini_response_to_openai_responses_non_stream(
            &request,
            &Value::Null,
            raw.as_bytes(),
        );
        assert!(out.is_none(), "{args}: invalid input returned: {out:?}");
        assert!(
            non_stream(&request, &Value::Null, raw.as_bytes()).is_err(),
            "{args}: missing non-stream error"
        );

        let mut failures = 0;
        for (kind, event) in translate(&mut stream, &raw) {
            if kind == "response.failed" {
                failures += 1;
                assert_eq!(
                    text(&event, "response.error.code"),
                    "invalid_tool_arguments",
                    "{args}: wrong error"
                );
            }
            assert!(
                kind != "response.completed" && kind != "response.custom_tool_call_input.done",
                "{args}: invalid success: {kind} {event}"
            );
        }
        assert_eq!(failures, 1, "{args}: failures");
        assert!(
            stream.translate_line(b"[DONE]").is_empty(),
            "{args}: failure reopened"
        );
    }
}

#[test]
fn gemini_apply_patch_complete_snapshot_matrix() {
    fn frame(name: &str, id: &str, args: &str) -> String {
        [
            r#"{"candidates":[{"content":{"parts":[{"partIndex":2,"functionCall":{"id":""#,
            id,
            r#"","name":""#,
            name,
            r#"","args":"#,
            args,
            "}}]}}]}",
        ]
        .concat()
    }
    const PATCH: &str = "functions__apply_patch";
    let escaped_a = concat!(r#"{ "input" : ""#, "\\", r#"u0061" }"#);
    let cases = [
        (
            "equivalent encoding after item done",
            vec![
                frame(PATCH, "c1", r#"{"input":"a"}"#),
                frame(PATCH, "c1", escaped_a),
            ],
            false,
        ),
        (
            "complete input cannot extend",
            vec![
                frame(PATCH, "c1", r#"{"input":"a"}"#),
                frame(PATCH, "c1", r#"{"input":"ab"}"#),
            ],
            true,
        ),
        (
            "ID conflict after item done",
            vec![
                frame(PATCH, "c1", r#"{"input":"a"}"#),
                frame(PATCH, "c2", r#"{"input":"a"}"#),
            ],
            true,
        ),
        (
            "illegal snapshot before name",
            vec![
                frame("", "c1", r#"{"input":1}"#),
                frame(PATCH, "c1", r#"{"input":"a"}"#),
            ],
            true,
        ),
        (
            "truncated snapshot before name",
            vec![
                frame("", "c1", r#"{"input":"a"#),
                frame(PATCH, "c1", r#"{"input":"a"}"#),
            ],
            true,
        ),
        (
            "ID conflict before name",
            vec![
                frame("", "c1", r#"{"input":"a"}"#),
                frame("", "c2", r#"{"input":"a"}"#),
                frame(PATCH, "c1", r#"{"input":"a"}"#),
            ],
            true,
        ),
        (
            "full snapshots before name cannot extend",
            vec![
                frame("", "c1", r#"{"input":"a"}"#),
                frame(PATCH, "c1", r#"{"input":"ab"}"#),
            ],
            true,
        ),
        (
            "valid name arrives later",
            vec![
                frame("", "c1", r#"{"input":"a"}"#),
                frame(PATCH, "c1", r#"{"input":"a"}"#),
            ],
            false,
        ),
    ];
    let request = patch_request();
    for (name, inputs, fail) in cases {
        let mut stream = GeminiToOpenAIResponsesStream::new("gemini", &request, &Value::Null);
        let events: Vec<Value> = inputs
            .iter()
            .flat_map(|input| event_data(&mut stream, input))
            .collect();
        let failures = events
            .iter()
            .filter(|event| text(event, "type") == "response.failed")
            .count();
        let done = events
            .iter()
            .filter(|event| text(event, "type") == "response.custom_tool_call_input.done")
            .count();
        if fail {
            assert_eq!(failures, 1, "{name}: failures; events={events:?}");
        } else {
            assert!(
                failures == 0 && done == 1,
                "{name}: failures={failures} done={done}"
            );
            let chunks = event_data(&mut stream, "[DONE]");
            let event = chunks.last().unwrap_or(&Value::Null);
            assert!(
                list(event, "response.output").len() == 1
                    && text(event, "response.output.0.input") == "a",
                "{name}: final: {event}"
            );
        }
        for input in [inputs[0].as_str(), "[DONE]"] {
            assert!(
                stream.translate_line(input.as_bytes()).is_empty(),
                "{name}: terminal reopened"
            );
        }
    }
}

#[test]
fn gemini_apply_patch_winning_declaration_and_negative_compatibility() {
    let cases = [
        (
            "function wins",
            r#"{"tools":[{"type":"function","name":"apply_patch"}],"input":[{"type":"additional_tools","tools":[{"type":"custom","name":"apply_patch"}]}]}"#,
            "apply_patch",
            r#"{"n":1}"#,
            "function_call",
            "",
        ),
        (
            "direct function wins namespace",
            r#"{"tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"apply_patch"}]},{"type":"function","name":"functions__apply_patch"}]}"#,
            "functions__apply_patch",
            r#"{"n":1}"#,
            "function_call",
            "",
        ),
        (
            "other custom",
            r#"{"tools":[{"type":"custom","name":"exec"}]}"#,
            "exec",
            r#"{"command":"ls"}"#,
            "custom_tool_call",
            r#"{"command":"ls"}"#,
        ),
    ];
    for (name, request, upstream, args, want_type, want_input) in cases {
        let request = parse(request);
        let raw = [
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":""#,
            upstream,
            r#"","args":"#,
            args,
            r#"}}]},"finishReason":"STOP"}]}"#,
        ]
        .concat();
        let out = convert_gemini_response_to_openai_responses_non_stream(
            &request,
            &Value::Null,
            raw.as_bytes(),
        )
        .unwrap_or(Value::Null);
        let item = at(&out, "output.0").unwrap_or(&Value::Null);
        assert!(
            text(item, "type") == want_type && text(item, "input") == want_input,
            "{name}: compatibility changed: {out}"
        );
        let mut stream = GeminiToOpenAIResponsesStream::new("gemini", &request, &Value::Null);
        let mut stream_events = translate(&mut stream, &raw);
        stream_events.extend(translate(&mut stream, "[DONE]"));
        for (kind, event) in stream_events {
            assert!(
                kind != "response.custom_tool_call_input.delta" && kind != "response.failed",
                "{name}: patch behavior applied to non-patch: {kind} {event}"
            );
            if kind == "response.completed" {
                assert_eq!(
                    text(&event, "response.output.0.type"),
                    want_type,
                    "{name}: {event}"
                );
            }
        }
    }
}

#[test]
fn gemini_apply_patch_conflicting_name_and_part_identity() {
    let request = patch_request();
    for second in [
        r#"{"partIndex":3,"functionCall":{"id":"c1","name":"functions__apply_patch","args":{"input":"a"}}}"#,
        r#"{"partIndex":2,"functionCall":{"id":"c1","name":"other","args":{"input":"a"}}}"#,
    ] {
        let mut stream = GeminiToOpenAIResponsesStream::new("gemini", &request, &Value::Null);
        let first = r#"{"candidates":[{"content":{"parts":[{"partIndex":2,"functionCall":{"id":"c1","name":"functions__apply_patch","args":{"input":"a"}}}]}}]}"#;
        translate(&mut stream, first);
        let chunks = translate(
            &mut stream,
            &[r#"{"candidates":[{"content":{"parts":["#, second, "]}}]}"].concat(),
        );
        assert_eq!(chunks.len(), 1, "identity conflict ignored: {chunks:?}");
        assert_eq!(chunks[0].0, "response.failed", "missing identity failure");
    }
}

#[test]
fn gemini_apply_patch_non_stream_conflicting_complete_calls() {
    let request = patch_request();
    let raw = r#"{"candidates":[{"content":{"parts":[{"functionCall":{"id":"c1","name":"functions__apply_patch","args":{"input":"a"}}},{"functionCall":{"id":"c1","name":"functions__apply_patch","args":{"input":"ab"}}}]}}]}"#;
    let out = convert_gemini_response_to_openai_responses_non_stream(
        &request,
        &Value::Null,
        raw.as_bytes(),
    );
    assert!(
        out.is_none(),
        "conflicting source call IDs accepted: {out:?}"
    );
    assert!(
        non_stream(&request, &Value::Null, raw.as_bytes()).is_err(),
        "missing error state"
    );
}

#[test]
fn gemini_apply_patch_history_preserves_whitespace_and_tool_pair() {
    let request = parse(
        r#"{"tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"apply_patch"}]}],"input":[{"type":"custom_tool_call","call_id":"c1","namespace":"functions","name":"apply_patch","input":"  *** Begin Patch\n*** Add File: 中.txt\n+😀\n*** End Patch\n "},{"type":"custom_tool_call_output","call_id":"c1","output":"ok"},{"role":"user","type":"message","content":"continue"}]}"#,
    );
    let out = convert_openai_responses_request_to_gemini("gemini-3.1-pro-preview", &request, false);
    let want = "  *** Begin Patch\n*** Add File: 中.txt\n+😀\n*** End Patch\n ";
    let mut call = &Value::Null;
    let mut result = &Value::Null;
    for content in list(&out, "contents") {
        for part in list(content, "parts") {
            if let Some(function_call) = part.get("functionCall") {
                call = function_call;
            }
            if let Some(function_response) = part.get("functionResponse") {
                result = function_response;
            }
        }
    }
    assert!(
        text(call, "args.input") == want
            && text(call, "name") == "functions__apply_patch"
            && text(result, "name") == "functions__apply_patch",
        "history changed: {out}"
    );
}

#[test]
fn gemini_apply_patch_does_not_rebind_ordinary_function_calls() {
    let request = parse(
        r#"{"tools":[{"type":"custom","name":"apply_patch"},{"type":"function","name":"first"},{"type":"function","name":"second"}]}"#,
    );
    let mut stream = GeminiToOpenAIResponsesStream::new("gemini", &request, &Value::Null);
    for name in ["first", "second"] {
        let chunk = [
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"id":"reused","name":""#,
            name,
            r#"","args":{"n":1}}}]}}]}"#,
        ]
        .concat();
        let mut item = Value::Null;
        for (kind, event) in translate(&mut stream, &chunk) {
            if kind == "response.output_item.done" {
                item = at(&event, "item").cloned().unwrap_or_default();
            }
        }
        assert!(
            text(&item, "name") == name && text(&item, "type") == "function_call",
            "ordinary function changed: {item}"
        );
    }
}

#[test]
fn gemini_apply_patch_distinct_unkeyed_complete_calls() {
    let request = patch_request();
    let mut stream = GeminiToOpenAIResponsesStream::new("gemini", &request, &Value::Null);
    let raw = r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"functions__apply_patch","args":{"input":"a"}}},{"functionCall":{"name":"functions__apply_patch","args":{"input":"b"}}}]},"finishReason":"STOP"}]}"#;
    let mut inputs: HashMap<String, String> = HashMap::new();
    let mut last = Value::Null;
    let mut stream_events = translate(&mut stream, raw);
    stream_events.extend(translate(&mut stream, "[DONE]"));
    for (kind, event) in stream_events {
        match kind.as_str() {
            "response.custom_tool_call_input.delta" => {
                inputs
                    .entry(text(&event, "item_id"))
                    .or_default()
                    .push_str(&text(&event, "delta"));
            }
            "response.custom_tool_call_input.done" => {
                let streamed = inputs
                    .get(&text(&event, "item_id"))
                    .map_or("", String::as_str);
                assert_eq!(streamed, text(&event, "input"), "{event}");
            }
            "response.completed" => last = at(&event, "response").cloned().unwrap_or_default(),
            _ => {}
        }
    }
    assert!(
        list(&last, "output").len() == 2
            && text(&last, "output.0.input") == "a"
            && text(&last, "output.1.input") == "b"
            && text(&last, "output.0.id") != text(&last, "output.1.id"),
        "distinct calls merged: {last}"
    );
}

#[test]
fn gemini_apply_patch_unresolvable_call_identity_fails_closed() {
    let request = patch_request();
    for part in [
        r#"{"functionCall":{"args":{"input":"secret"}}}"#,
        r#"{"partIndex":2,"functionCall":{"id":"c1","args":{"input":"secret"}}}"#,
    ] {
        let mut stream = GeminiToOpenAIResponsesStream::new("gemini", &request, &Value::Null);
        let raw = [
            r#"{"candidates":[{"content":{"parts":["#,
            part,
            r#"]},"finishReason":"STOP"}]}"#,
        ]
        .concat();
        let mut failures = 0;
        for (kind, event) in translate(&mut stream, &raw) {
            if kind == "response.failed" {
                failures += 1;
            }
            assert!(
                kind != "response.completed" && kind != "response.function_call_arguments.delta",
                "unresolvable identity accepted: {kind} {event}"
            );
        }
        assert_eq!(failures, 1, "failures");
        let out = convert_gemini_response_to_openai_responses_non_stream(
            &request,
            &Value::Null,
            raw.as_bytes(),
        );
        assert!(out.is_none(), "unresolvable non-stream accepted: {out:?}");
    }
}
