// Ported from CLIProxyAPI internal/translator/openai/interactions/responses/interactions_openai_responses_response_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Both directions, streamed and whole: event order, text and arguments
//! sent once, usage, tool names and namespaces as the client declared them,
//! thought signatures, and failures. The `apply_patch` bridge's tests are
//! in the submodules.
//!
//! Dropped or changed tests:
//! - TestConvertInteractionsResponseToOpenAIResponsesNonStreamRestoresAntigravityToolName,
//!   TestConvertInteractionsResponseToOpenAIResponsesStreamRestoresAntigravityToolName
//!   and
//!   TestConvertInteractionsResponseToOpenAIResponses_AntigravityCustomToolRestoresNameAndType
//!   are dropped: they test the Antigravity tool name mapping, which isn't
//!   ported.
//! - TestConvertInteractionsResponseToOpenAIResponsesPreservesNonCollidingAndNonAntigravityNames
//!   keeps only its second case; the first is about an Antigravity model.
//! - The two PreservesEnvironmentID tests stream from `devin/swe-2`, not
//!   upstream's Antigravity model.
//! - Go's tests read each frame upstream returns apart; these read the
//!   frames from the joined text [`translate`] returns.
//!
//! [`translate`]: InteractionsToOpenAIResponsesStream::translate

mod apply_patch;
mod apply_patch_identity;
mod apply_patch_rereview;
mod apply_patch_review;
mod apply_patch_source_stop;

use serde_json::{Value, json};

use super::to_responses::non_stream;
use super::*;

/// The model upstream's `apply_patch` tests stream from.
const MODEL: &str = "devin/swe-2";

/// `patchRequest`: `apply_patch` declared in the `functions` namespace.
const PATCH_REQUEST: &str = r#"{"tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"apply_patch","format":{"type":"grammar","definition":"start: patch"}}]}]}"#;

/// `patchText`
const PATCH_TEXT: &str = "  *** Begin Patch\n*** Add File: 中.txt\n+😀\n*** End Patch\n ";

fn parse(json: &str) -> Value {
    serde_json::from_str(json).expect("test JSON is valid")
}

/// `value` with its keys sorted, as Go's `json.Marshal` writes a map.
fn sorted(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(entries.into_iter().map(|(k, v)| (k, sorted(v))).collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

/// `patchJSON`
fn go_json(value: Value) -> Vec<u8> {
    serde_json::to_vec(&sorted(value)).expect("JSON writes")
}

/// `patchArguments`
fn patch_arguments(input: &str) -> String {
    String::from_utf8(go_json(json!({ "input": input }))).expect("JSON is UTF-8")
}

/// `patchStep`
fn patch_step(event: &str, step: Value) -> Vec<u8> {
    go_json(json!({ "event_type": event, "index": 2, "step": step }))
}

/// A `step.delta` adding `arguments` to the step at `index`.
fn arguments_delta(index: i64, arguments: &str) -> Vec<u8> {
    go_json(json!({
        "event_type": "step.delta",
        "index": index,
        "delta": { "type": "arguments_delta", "arguments": arguments },
    }))
}

/// `patchEvents`: the data of each `data:` line that is JSON.
fn events(out: &str) -> Vec<Value> {
    out.split('\n')
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str(data).ok())
        .collect()
}

/// A stream from [`MODEL`] for the client's `request`.
fn stream_for(request: &str) -> InteractionsToOpenAIResponsesStream {
    InteractionsToOpenAIResponsesStream::new(MODEL, &parse(request), &Value::Null)
}

/// A stream for [`PATCH_REQUEST`].
fn patch_stream() -> InteractionsToOpenAIResponsesStream {
    stream_for(PATCH_REQUEST)
}

/// `patchSend`: the events one chunk gives.
fn send(stream: &mut InteractionsToOpenAIResponsesStream, raw: impl AsRef<[u8]>) -> Vec<Value> {
    events(&stream.translate(raw.as_ref()))
}

/// gjson's `Get` for a path of keys and array indexes.
fn get<'v>(value: &'v Value, path: &str) -> Option<&'v Value> {
    value.pointer(&format!("/{}", path.replace('.', "/")))
}

/// gjson's `Get(path).String()`.
fn s(value: &Value, path: &str) -> String {
    match get(value, path) {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

/// gjson's `Get(path).Int()`.
fn int(value: &Value, path: &str) -> i64 {
    get(value, path).and_then(Value::as_i64).unwrap_or(0)
}

/// An event's `type`.
fn kind(event: &Value) -> String {
    s(event, "type")
}

/// How many of `events` are of type `kind`.
fn count(events: &[Value], event_type: &str) -> usize {
    events
        .iter()
        .filter(|event| kind(event) == event_type)
        .count()
}

/// The stream's tool input error, as text.
fn error_of(stream: &InteractionsToOpenAIResponsesStream) -> Option<String> {
    stream.tool_input_error().map(ToString::to_string)
}

/// `assertPatchLifecycle`
#[track_caller]
fn assert_patch_lifecycle(events: &[Value], want: &str) {
    let mut delta = String::new();
    let mut counts = std::collections::HashMap::<String, usize>::new();
    let mut last_seq = 0;
    for event in events {
        let event_kind = kind(event);
        *counts.entry(event_kind.clone()).or_default() += 1;
        let seq = int(event, "sequence_number");
        assert!(seq > last_seq, "sequence: {event}");
        last_seq = seq;
        match event_kind.as_str() {
            "response.custom_tool_call_input.delta" | "response.custom_tool_call_input.done" => {
                assert!(
                    s(event, "item_id") == "item_2"
                        && s(event, "call_id") == "call_2"
                        && int(event, "output_index") == 2,
                    "event identity: {event}"
                );
                if event_kind == "response.custom_tool_call_input.delta" {
                    delta.push_str(&s(event, "delta"));
                } else {
                    assert_eq!(s(event, "input"), want, "done input: {event}");
                }
            }
            "response.output_item.added" | "response.output_item.done" => {
                assert!(
                    s(event, "item.id") == "item_2"
                        && s(event, "item.call_id") == "call_2"
                        && s(event, "item.namespace") == "functions"
                        && s(event, "item.name") == "apply_patch",
                    "item identity: {event}"
                );
                if event_kind == "response.output_item.done" {
                    assert_eq!(s(event, "item.input"), want, "item input: {event}");
                }
            }
            "response.completed" => assert!(
                s(event, "response.output.0.input") == want
                    && s(event, "response.output.0.id") == "item_2"
                    && s(event, "response.output.0.call_id") == "call_2",
                "final input: {event}"
            ),
            _ => {}
        }
    }
    let counted = |event_type: &str| counts.get(event_type).copied().unwrap_or(0);
    assert!(
        delta == want
            && counted("response.output_item.added") == 1
            && counted("response.custom_tool_call_input.done") == 1
            && counted("response.output_item.done") == 1
            && counted("response.completed") == 1
            && counted("response.function_call_arguments.delta") == 0,
        "lifecycle counts={counts:?} delta={delta:?}"
    );
}

/// `assertInteractionsPatchReviewFailure`
#[track_caller]
fn assert_review_failure(stream: &mut InteractionsToOpenAIResponsesStream, events: &[Value]) {
    assert!(
        events.len() == 1
            && kind(&events[0]) == "response.failed"
            && s(&events[0], "response.error.code") == "invalid_tool_arguments",
        "expected only sanitized failure: {events:?}"
    );
    let raw = events[0].to_string();
    assert!(
        !raw.contains("secret") && !raw.contains(r#"\"input\""#),
        "raw arguments leaked: {raw}"
    );
    assert!(
        stream.tool_input_error().is_some(),
        "missing tool input error"
    );
    for raw in [
        br#"{"event_type":"interaction.completed"}"#.to_vec(),
        br#"{"event_type":"interaction.failed"}"#.to_vec(),
        b"[DONE]".to_vec(),
        patch_step("step.stop", Value::Null),
    ] {
        let more = send(stream, raw);
        assert!(more.is_empty(), "failed response reopened: {more:?}");
    }
}

/// `patchReviewSnapshot`: `step` as `entry` carries it, at `index` unless
/// that is negative. A step's index goes in the event for `step.start` and
/// `step.stop`, and in the step for the terminal events.
fn patch_review_snapshot(entry: &str, index: i64, mut step: Value) -> Vec<u8> {
    let mut event = json!({ "event_type": entry });
    if entry == "step.start" || entry == "step.stop" {
        event["step"] = step;
        if index >= 0 {
            event["index"] = json!(index);
        }
    } else {
        if index >= 0 {
            step["index"] = json!(index);
        }
        if entry == "finish" {
            event["steps"] = json!([step]);
        } else {
            event["interaction"] = json!({ "steps": [step] });
        }
    }
    go_json(event)
}

/// An SSE frame.
fn frame(event: &str, data: &str) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// Runs `chunks` through one stream from `model` for the client's
/// `original_request`, and returns the joined output.
fn run_with(model: &str, original_request: &Value, chunks: &[String]) -> String {
    let mut stream =
        InteractionsToOpenAIResponsesStream::new(model, original_request, &Value::Null);
    chunks
        .iter()
        .map(|chunk| stream.translate(chunk.as_bytes()))
        .collect()
}

/// `findResponsesEventPayload`: the first event of type `kind`, or `Null`.
fn find(events: &[Value], event_type: &str) -> Value {
    events
        .iter()
        .find(|event| kind(event) == event_type)
        .cloned()
        .unwrap_or(Value::Null)
}

/// `responsesEventNames`
fn names(events: &[Value]) -> String {
    events
        .iter()
        .map(kind)
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>()
        .join(",")
}

/// One Interactions frame: its name, from `event_type` or else its `event:`
/// line, and its data.
fn interactions_frames(out: &str) -> Vec<(String, String)> {
    out.split("\n\n")
        .filter(|frame| !frame.is_empty())
        .map(|frame| {
            let (event, data) = frame.split_once("\ndata: ").expect("a data line");
            let event = event.strip_prefix("event: ").unwrap_or_default();
            let named = serde_json::from_str::<Value>(data)
                .ok()
                .map(|data| s(&data, "event_type"))
                .filter(|name| !name.is_empty());
            (named.unwrap_or_else(|| event.to_owned()), data.to_owned())
        })
        .collect()
}

/// `findInteractionsEventPayload`: the data of the first frame named
/// `name`, as JSON, or `Null`.
fn find_interactions(out: &str, name: &str) -> Value {
    interactions_frames(out)
        .into_iter()
        .find(|(event, _)| event == name)
        .and_then(|(_, data)| serde_json::from_str(&data).ok())
        .unwrap_or(Value::Null)
}

/// `countInteractionsEventType`
fn count_interactions(out: &str, name: &str) -> usize {
    interactions_frames(out)
        .iter()
        .filter(|(event, _)| event == name)
        .count()
}

/// `interactionsEventNames`
fn interactions_names(out: &str) -> String {
    interactions_frames(out)
        .into_iter()
        .map(|(event, _)| event)
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>()
        .join(",")
}

/// Runs `chunks` through one Responses → Interactions stream from
/// `gpt-test`, and returns the joined output.
fn run_back(chunks: &[&str]) -> String {
    let mut stream = OpenAIResponsesToInteractionsStream::new("gpt-test");
    chunks
        .iter()
        .map(|chunk| stream.translate(chunk.as_bytes()))
        .collect()
}

/// `testGPTResponsesReasoningSignature`
fn gpt_reasoning_signature() -> String {
    use base64::Engine;
    let mut payload = vec![0u8; 1 + 8 + 16 + 16 + 32];
    payload[0] = 0x80;
    payload[8] = 1;
    for (i, byte) in payload.iter_mut().enumerate().skip(9) {
        *byte = i as u8;
    }
    base64::engine::general_purpose::URL_SAFE.encode(payload)
}

fn gpt_test_request() -> Value {
    json!({ "model": "gpt-test" })
}

// Ports TestConvertInteractionsResponseToOpenAIResponsesNonStream.
#[test]
fn interactions_whole_response_to_responses() {
    let raw = br#"{"id":"interaction_1","object":"interaction","status":"completed","steps":[{"type":"model_output","content":[{"text":"ok"}]}],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}"#;
    let out = convert_interactions_response_to_openai_responses_non_stream(
        "gpt-test",
        &gpt_test_request(),
        &Value::Null,
        raw,
    )
    .expect("a response");
    assert_eq!(s(&out, "output.0.content.0.text"), "ok", "{out}");
    assert_eq!(int(&out, "usage.total_tokens"), 3, "{out}");
}

// Ports TestConvertInteractionsResponseToOpenAIResponsesStream.
#[test]
fn interactions_stream_to_responses() {
    let out = run_with(
        "gpt-test",
        &gpt_test_request(),
        &[
            frame(
                "interaction.created",
                r#"{"interaction":{"id":"interaction_1","model":"source-model"},"event_type":"interaction.created"}"#,
            ),
            frame(
                "step.delta",
                r#"{"index":0,"delta":{"content":{"text":"thinking","type":"text"},"type":"thought_summary"},"event_type":"step.delta"}"#,
            ),
            frame(
                "step.delta",
                r#"{"index":1,"delta":{"text":"I will call a tool.","type":"text"},"event_type":"step.delta"}"#,
            ),
            frame(
                "step.start",
                r#"{"index":2,"step":{"id":"call_1","type":"function_call","name":"get_weather","arguments":{}},"event_type":"step.start"}"#,
            ),
            frame(
                "step.delta",
                r#"{"index":2,"delta":{"arguments":"{\"location\":\"北京\"}","type":"arguments_delta"},"event_type":"step.delta"}"#,
            ),
            frame("step.stop", r#"{"index":2,"event_type":"step.stop"}"#),
            frame(
                "interaction.completed",
                r#"{"interaction":{"id":"interaction_1","status":"completed","usage":{"total_tokens":399,"total_input_tokens":123,"total_cached_tokens":5,"total_output_tokens":36,"total_thought_tokens":240},"created":"2026-07-06T06:01:35Z","object":"interaction","model":"gpt-test"},"event_type":"interaction.completed"}"#,
            ),
            frame("done", "[DONE]"),
        ],
    );
    let out = events(&out);
    let text = find(&out, "response.output_text.delta");
    assert_eq!(s(&text, "delta"), "I will call a tool.", "{text}");
    let arguments = find(&out, "response.function_call_arguments.delta");
    assert_eq!(
        s(&arguments, "delta"),
        r#"{"location":"北京"}"#,
        "{arguments}"
    );
    let done = find(&out, "response.function_call_arguments.done");
    assert_eq!(s(&done, "item_id"), "call_1", "{done}");
    assert_eq!(s(&done, "arguments"), r#"{"location":"北京"}"#, "{done}");
    let created = find(&out, "response.created");
    assert_eq!(s(&created, "response.model"), "gpt-test");
    let completed = find(&out, "response.completed");
    assert_eq!(
        int(&completed, "response.usage.total_tokens"),
        399,
        "{completed}"
    );
    assert_eq!(
        int(
            &completed,
            "response.usage.output_tokens_details.reasoning_tokens"
        ),
        240,
        "{completed}"
    );
    assert!(names(&out).contains("response.completed"));
}

// Ports TestConvertInteractionsResponseToOpenAIResponsesStreamFunctionCallStartArguments.
#[test]
fn function_call_start_arguments() {
    let out = events(&run_with(
        "gpt-test",
        &Value::Null,
        &[
            frame(
                "step.start",
                r#"{"index":0,"step":{"id":"call_1","type":"function_call","name":"lookup","arguments":{"q":"x"}},"event_type":"step.start"}"#,
            ),
            frame("step.stop", r#"{"index":0,"event_type":"step.stop"}"#),
        ],
    ));
    assert_eq!(
        names(&out),
        "response.output_item.added,response.function_call_arguments.delta,response.function_call_arguments.done,response.output_item.done"
    );
    let delta = find(&out, "response.function_call_arguments.delta");
    assert_eq!(s(&delta, "delta"), r#"{"q":"x"}"#, "{delta}");
    let done = find(&out, "response.function_call_arguments.done");
    assert_eq!(s(&done, "arguments"), r#"{"q":"x"}"#, "{done}");
    let item = find(&out, "response.output_item.done");
    assert_eq!(s(&item, "item.arguments"), r#"{"q":"x"}"#, "{item}");
}

// Ports TestConvertInteractionsResponseToOpenAIResponsesStreamFunctionCallEmptyArguments.
#[test]
fn function_call_empty_arguments() {
    let out = events(&run_with(
        "gpt-test",
        &Value::Null,
        &[
            frame(
                "step.start",
                r#"{"index":0,"step":{"id":"call_1","type":"function_call","name":"lookup","arguments":{}},"event_type":"step.start"}"#,
            ),
            frame("step.stop", r#"{"index":0,"event_type":"step.stop"}"#),
            frame(
                "interaction.completed",
                r#"{"interaction":{"id":"interaction_1","status":"completed","model":"gpt-test"},"event_type":"interaction.completed"}"#,
            ),
        ],
    ));
    assert_eq!(
        names(&out),
        "response.output_item.added,response.function_call_arguments.done,response.output_item.done,response.completed"
    );
    let done = find(&out, "response.function_call_arguments.done");
    assert_eq!(s(&done, "arguments"), "{}", "{done}");
    let item = find(&out, "response.output_item.done");
    assert_eq!(s(&item, "item.arguments"), "{}", "{item}");
    let completed = find(&out, "response.completed");
    assert_eq!(
        s(&completed, "response.output.0.arguments"),
        "{}",
        "{completed}"
    );
}

// Ports TestConvertInteractionsResponseToOpenAIResponsesStreamFunctionCallEventsAreIdempotent.
#[test]
fn function_call_events_are_idempotent() {
    let start = frame(
        "step.start",
        r#"{"index":0,"step":{"id":"call_1","type":"function_call","name":"lookup","arguments":{"q":"x"}},"event_type":"step.start"}"#,
    );
    let stop = frame("step.stop", r#"{"index":0,"event_type":"step.stop"}"#);
    let out = events(&run_with(
        "gpt-test",
        &Value::Null,
        &[start.clone(), start, stop.clone(), stop],
    ));
    assert_eq!(
        names(&out),
        "response.output_item.added,response.function_call_arguments.delta,response.function_call_arguments.done,response.output_item.done"
    );
}

// Ports TestConvertInteractionsResponseToOpenAIResponsesStreamModelOutputDoneIncludesText.
#[test]
fn model_output_done_includes_text() {
    let out = events(&run_with(
        "gpt-test",
        &gpt_test_request(),
        &[
            frame(
                "step.start",
                r#"{"index":0,"step":{"id":"msg_1","type":"model_output"},"event_type":"step.start"}"#,
            ),
            frame(
                "step.delta",
                r#"{"index":0,"delta":{"text":"hello","type":"text"},"event_type":"step.delta"}"#,
            ),
            frame(
                "step.delta",
                r#"{"index":0,"delta":{"text":" world","type":"text"},"event_type":"step.delta"}"#,
            ),
            frame("step.stop", r#"{"index":0,"event_type":"step.stop"}"#),
        ],
    ));
    let text = find(&out, "response.output_text.done");
    assert_eq!(s(&text, "text"), "hello world", "{text}");
    let part = find(&out, "response.content_part.done");
    assert_eq!(s(&part, "part.text"), "hello world", "{part}");
    let item = find(&out, "response.output_item.done");
    assert_eq!(s(&item, "item.content.0.text"), "hello world", "{item}");
}

// Ports TestConvertInteractionsResponseToOpenAIResponsesStreamReasoningSummaryLifecycle.
#[test]
fn stream_reasoning_summary_lifecycle() {
    let cases: [(&str, &[&str], bool); 4] = [
        ("single_frame", &["thinking"], false),
        (
            "multiple_frames_late_signature",
            &["think", " carefully"],
            true,
        ),
        ("signature_only", &[], true),
        ("empty_summary", &[""], false),
    ];
    for (name, chunks, signed) in cases {
        let mut stream = stream_for("{}");
        let mut out = Vec::new();
        let mut push = |raw: String| {
            let frames = send(&mut stream, format!("data: {raw}\n\n"));
            out.extend(frames.iter().cloned());
            frames
        };
        push(
            r#"{"index":3,"step":{"id":"reasoning_3","type":"thought"},"event_type":"step.start"}"#
                .into(),
        );
        for (i, chunk) in chunks.iter().enumerate() {
            let chunk_json = Value::from(*chunk);
            let field = if i % 2 == 0 {
                format!(r#""content":{{"type":"text","text":{chunk_json}}}"#)
            } else {
                format!(r#""text":{chunk_json}"#)
            };
            let frames = push(format!(
                r#"{{"index":3,"delta":{{"type":"thought_summary",{field}}},"event_type":"step.delta"}}"#
            ));
            assert_eq!(
                names(&frames),
                "response.reasoning_summary_text.delta",
                "{name}: frame {i}"
            );
            assert_eq!(frames.len(), 1, "{name}: frame {i}");
            let payload = find(&frames, "response.reasoning_summary_text.delta");
            assert_eq!(
                get(&payload, "delta"),
                Some(&chunk_json),
                "{name}: {payload}"
            );
        }
        let signature = if signed {
            gpt_reasoning_signature()
        } else {
            String::new()
        };
        if signed {
            for value in [signature.as_str(), ""] {
                let frames = push(format!(
                    r#"{{"index":3,"delta":{{"type":"thought_signature","signature":"{value}"}},"event_type":"step.delta"}}"#
                ));
                assert!(frames.is_empty(), "{name}: signature emitted {frames:?}");
            }
        }
        push(r#"{"index":3,"event_type":"step.stop"}"#.into());
        push(r#"{"interaction":{"id":"interaction_1","status":"completed"},"event_type":"interaction.completed"}"#.into());
        let mut want_names = vec![
            "response.output_item.added",
            "response.reasoning_summary_part.added",
        ];
        want_names.extend(
            chunks
                .iter()
                .map(|_| "response.reasoning_summary_text.delta"),
        );
        want_names.extend([
            "response.reasoning_summary_text.done",
            "response.reasoning_summary_part.done",
            "response.output_item.done",
            "response.completed",
        ]);
        assert_eq!(names(&out), want_names.join(","), "{name}");
        assert_eq!(out.len(), want_names.len(), "{name}");
        let mut previous_sequence = 0;
        for (i, (payload, want)) in out.iter().zip(&want_names).enumerate() {
            let sequence = get(payload, "sequence_number").and_then(Value::as_i64);
            assert!(
                sequence.is_some() && (i == 0 || sequence == Some(previous_sequence + 1)),
                "{name}: invalid sequence: {payload}"
            );
            previous_sequence = sequence.unwrap_or_default();
            if *want == "response.completed" {
                continue;
            }
            assert_eq!(
                get(payload, "output_index").and_then(Value::as_i64),
                Some(3),
                "{name}: invalid output_index: {payload}"
            );
            let mut id_path = "item.id";
            if want.starts_with("response.reasoning_summary_") {
                id_path = "item_id";
                assert_eq!(
                    get(payload, "summary_index").and_then(Value::as_i64),
                    Some(0),
                    "{name}: invalid summary_index: {payload}"
                );
            }
            assert_eq!(
                get(payload, id_path).and_then(Value::as_str),
                Some("reasoning_3"),
                "{name}: invalid item identity: {payload}"
            );
        }
        let text = chunks.concat();
        for (event, path, want) in [
            ("response.output_item.added", "item.status", "in_progress"),
            (
                "response.reasoning_summary_part.added",
                "part.type",
                "summary_text",
            ),
            ("response.reasoning_summary_part.added", "part.text", ""),
            ("response.reasoning_summary_text.done", "text", &text),
            (
                "response.reasoning_summary_part.done",
                "part.type",
                "summary_text",
            ),
            ("response.reasoning_summary_part.done", "part.text", &text),
            ("response.output_item.done", "item.type", "reasoning"),
            ("response.output_item.done", "item.status", "completed"),
            (
                "response.output_item.done",
                "item.summary.0.type",
                "summary_text",
            ),
            ("response.output_item.done", "item.summary.0.text", &text),
            (
                "response.output_item.done",
                "item.encrypted_content",
                &signature,
            ),
        ] {
            let payload = find(&out, event);
            assert_eq!(
                get(&payload, path).and_then(Value::as_str),
                Some(want),
                "{name}: {event} {path} = {payload}"
            );
        }
        let done = find(&out, "response.output_item.done");
        let done = get(&done, "item").cloned().unwrap_or_default();
        let completed = find(&out, "response.completed");
        let completed = get(&completed, "response.output")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            get(&done, "summary")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1),
            "{name}: {done}"
        );
        assert_eq!(completed.len(), 1, "{name}: {completed:?}");
        assert_eq!(
            completed.first().map(Value::to_string),
            Some(done.to_string()),
            "{name}: terminal reasoning items differ"
        );
    }
}

/// A thought stream signed with `signature`.
fn signed_thought_stream(signatures: &[&str]) -> Vec<Value> {
    let mut chunks = vec![
        frame(
            "step.start",
            r#"{"index":0,"step":{"type":"thought"},"event_type":"step.start"}"#,
        ),
        frame(
            "step.delta",
            r#"{"index":0,"delta":{"content":{"text":"thinking","type":"text"},"type":"thought_summary"},"event_type":"step.delta"}"#,
        ),
    ];
    for signature in signatures {
        chunks.push(frame(
            "step.delta",
            &format!(
                r#"{{"index":0,"delta":{{"signature":"{signature}","type":"thought_signature"}},"event_type":"step.delta"}}"#
            ),
        ));
    }
    chunks.push(frame(
        "step.stop",
        r#"{"index":0,"event_type":"step.stop"}"#,
    ));
    chunks.push(frame(
        "interaction.completed",
        r#"{"interaction":{"id":"interaction_1","status":"completed","object":"interaction","model":"gpt-test"},"event_type":"interaction.completed"}"#,
    ));
    events(&run_with("gpt-test", &gpt_test_request(), &chunks))
}

// Ports TestConvertInteractionsResponseToOpenAIResponsesStreamPreservesThoughtSignature.
#[test]
fn stream_preserves_thought_signature() {
    let signature = gpt_reasoning_signature();
    let out = signed_thought_stream(&["", &signature]);
    assert!(!names(&out).contains("response.output_text.delta"));
    let done = find(&out, "response.output_item.done");
    assert_eq!(s(&done, "item.encrypted_content"), signature, "{done}");
    assert_eq!(s(&done, "item.summary.0.text"), "thinking", "{done}");
    let completed = find(&out, "response.completed");
    assert_eq!(
        s(&completed, "response.output.0.encrypted_content"),
        signature,
        "{completed}"
    );
}

// Ports TestConvertInteractionsResponseToOpenAIResponsesStreamDropsForeignThoughtSignature.
#[test]
fn stream_drops_foreign_thought_signature() {
    let out = signed_thought_stream(&["foreign-gemini-signature"]);
    let done = find(&out, "response.output_item.done");
    assert_eq!(s(&done, "item.encrypted_content"), "", "{done}");
    assert_eq!(s(&done, "item.summary.0.text"), "thinking", "{done}");
    let completed = find(&out, "response.completed");
    assert_eq!(
        s(&completed, "response.output.0.encrypted_content"),
        "",
        "{completed}"
    );
}

// Ports TestConvertInteractionsResponseToOpenAIResponsesNonStreamThoughtSignature.
#[test]
fn whole_response_thought_signature() {
    let signature = gpt_reasoning_signature();
    let raw = format!(
        r#"{{"id":"interaction_1","object":"interaction","status":"completed","steps":[{{"type":"thought","signature":"{signature}","content":[{{"type":"text","text":"thinking"}}]}}],"usage":{{"total_tokens":1}}}}"#
    );
    let out = convert_interactions_response_to_openai_responses_non_stream(
        "gpt-test",
        &gpt_test_request(),
        &Value::Null,
        raw.as_bytes(),
    )
    .expect("a response");
    assert_eq!(s(&out, "output.0.encrypted_content"), signature, "{out}");
    assert_eq!(s(&out, "output.0.summary.0.text"), "thinking", "{out}");

    let raw = br#"{"id":"interaction_1","object":"interaction","status":"completed","steps":[{"type":"thought","thought_signature":"foreign-gemini-signature","content":[{"type":"text","text":"thinking"}]}],"usage":{"total_tokens":1}}"#;
    let out = convert_interactions_response_to_openai_responses_non_stream(
        "gpt-test",
        &gpt_test_request(),
        &Value::Null,
        raw,
    )
    .expect("a response");
    assert_eq!(s(&out, "output.0.encrypted_content"), "", "{out}");
    assert_eq!(s(&out, "output.0.summary.0.text"), "thinking", "{out}");
}

// Ports TestConvertOpenAIResponsesResponseToInteractionsNonStreamFunctionCall.
#[test]
fn responses_whole_function_call() {
    let raw = br#"{"id":"resp_1","output":[{"type":"function_call","name":"lookup","call_id":"call_1","arguments":{"q":"x"}}],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}"#;
    let out = convert_openai_responses_response_to_interactions_non_stream("gpt-test", raw);
    assert_eq!(s(&out, "steps.0.type"), "function_call");
    assert_eq!(s(&out, "steps.0.name"), "lookup");
    assert_eq!(s(&out, "steps.0.call_id"), "call_1");
}

// Ports TestConvertOpenAIResponsesResponseToInteractionsNonStreamFunctionCallStringArgs.
#[test]
fn responses_whole_function_call_string_arguments() {
    let raw = br#"{"id":"resp_1","output":[{"type":"function_call","name":"lookup","call_id":"call_1","arguments":"{\"q\":\"x\"}"}],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}"#;
    let out = convert_openai_responses_response_to_interactions_non_stream("gpt-test", raw);
    assert_eq!(s(&out, "steps.0.type"), "function_call");
    assert_eq!(s(&out, "steps.0.arguments.q"), "x");
}

// Ports TestConvertOpenAIResponsesResponseToInteractionsNonStreamUsageDetails.
#[test]
fn responses_whole_usage_details() {
    let raw = br#"{"id":"resp_1","output":[{"type":"message","content":[{"type":"output_text","text":"ok"}]}],"usage":{"input_tokens":11,"output_tokens":13,"total_tokens":24,"input_tokens_details":{"cached_tokens":5},"output_tokens_details":{"reasoning_tokens":7}}}"#;
    let out = convert_openai_responses_response_to_interactions_non_stream("gpt-test", raw);
    assert_eq!(s(&out, "id"), "resp_1", "{out}");
    assert_eq!(int(&out, "usage.input_tokens"), 11, "{out}");
    assert_eq!(int(&out, "usage.output_tokens"), 13, "{out}");
    assert_eq!(int(&out, "usage.reasoning_tokens"), 7, "{out}");
    assert_eq!(int(&out, "usage.cached_tokens"), 5, "{out}");
}

// Ports TestConvertOpenAIResponsesResponseToInteractionsStreamFunctionCallCallID.
#[test]
fn responses_stream_function_call_call_id() {
    let out = run_back(&[
        r#"{"type":"response.output_item.done","item":{"type":"function_call","id":"fc_1","call_id":"call_stream_1","name":"lookup","arguments":"{\"q\":\"x\"}"}}"#,
    ]);
    let delta = find_interactions(&out, "step.delta");
    assert!(!delta.is_null(), "step.delta payload not found");
    let start = find_interactions(&out, "step.start");
    assert_eq!(s(&start, "step.id"), "call_stream_1");
    assert_eq!(s(&delta, "delta.arguments"), r#"{"q":"x"}"#);
}

// Ports TestConvertOpenAIResponsesResponseToInteractionsStreamSkipsDoneArgumentsAfterDelta.
#[test]
fn responses_stream_skips_done_arguments_after_delta() {
    let mut stream = OpenAIResponsesToInteractionsStream::new("gpt-test");
    let out = stream.translate(br#"{"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_1","call_id":"call_1","delta":"{\"q\":\"x\"}"}"#);
    let delta = find_interactions(&out, "step.delta");
    assert_eq!(s(&delta, "delta.arguments"), r#"{"q":"x"}"#, "{delta}");
    let out = stream.translate(br#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":"{\"q\":\"x\"}"}}"#);
    assert_eq!(count_interactions(&out, "step.delta"), 0);
    assert_eq!(count_interactions(&out, "step.stop"), 1);
}

/// Upstream's tests of text a message's `output_item.done` repeats.
fn skips_done_text_after(delta: &[u8]) {
    let mut stream = OpenAIResponsesToInteractionsStream::new("gpt-test");
    let out = stream.translate(delta);
    let payload = find_interactions(&out, "step.delta");
    assert_eq!(s(&payload, "delta.text"), "hi", "{payload}");
    let out = stream.translate(br#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_1","content":[{"type":"output_text","text":"hi"}]}}"#);
    assert_eq!(count_interactions(&out, "step.delta"), 0);
}

// Ports TestConvertOpenAIResponsesResponseToInteractionsStreamSkipsDoneTextAfterDelta.
#[test]
fn responses_stream_skips_done_text_after_delta() {
    skips_done_text_after(
        br#"{"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"hi"}"#,
    );
}

// Ports TestConvertOpenAIResponsesResponseToInteractionsStreamSkipsDoneTextAfterUnkeyedDelta.
#[test]
fn responses_stream_skips_done_text_after_unkeyed_delta() {
    skips_done_text_after(
        br#"{"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"delta":"hi"}"#,
    );
}

// Ports TestConvertOpenAIResponsesResponseToInteractionsStreamCompletedOutputFallback.
#[test]
fn responses_stream_completed_output_fallback() {
    let out = run_back(&[
        r#"{"type":"response.completed","response":{"output":[{"type":"message","id":"msg_1","content":[{"type":"output_text","text":"final"}]}],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}"#,
    ]);
    let delta = find_interactions(&out, "step.delta");
    assert_eq!(s(&delta, "delta.text"), "final", "{delta}");
    assert_eq!(count_interactions(&out, "interaction.completed"), 1);
}

// Ports TestConvertOpenAIResponsesResponseToInteractionsStreamEmitsDone.
#[test]
fn responses_stream_emits_done() {
    let mut stream = OpenAIResponsesToInteractionsStream::new("gpt-test");
    let completed = stream.translate(br#"{"type":"response.completed","response":{"output":[],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}"#);
    let done = stream.translate(b"data: [DONE]");
    assert_eq!(count_interactions(&completed, "interaction.completed"), 1);
    assert_eq!(count_interactions(&completed, "done"), 1);
    assert_eq!(count_interactions(&done, "interaction.completed"), 0);
    assert_eq!(count_interactions(&done, "done"), 0);
    let payload = interactions_frames(&completed)
        .into_iter()
        .find(|(event, _)| event == "done")
        .map(|(_, data)| data);
    assert_eq!(payload.as_deref(), Some("[DONE]"));
}

// Ports TestConvertInteractionsResponseToOpenAIResponsesStreamFinishMetadataUsage.
#[test]
fn finish_metadata_usage() {
    let out = events(&run_with(
        "gpt-test",
        &Value::Null,
        &[r#"data: {"event_type":"finish","metadata":{"total_usage":{"total_input_tokens":2,"total_output_tokens":6,"total_thought_tokens":3,"total_cached_tokens":1,"total_tokens":11}}}"#.to_owned()],
    ));
    let payload = find(&out, "response.completed");
    assert!(!payload.is_null(), "response.completed payload not found");
    assert_eq!(int(&payload, "response.usage.input_tokens"), 2, "{payload}");
    assert_eq!(
        int(&payload, "response.usage.output_tokens"),
        6,
        "{payload}"
    );
    assert_eq!(
        int(
            &payload,
            "response.usage.output_tokens_details.reasoning_tokens"
        ),
        3,
        "{payload}"
    );
    assert_eq!(
        int(
            &payload,
            "response.usage.input_tokens_details.cached_tokens"
        ),
        1,
        "{payload}"
    );
    assert_eq!(
        int(&payload, "response.usage.total_tokens"),
        11,
        "{payload}"
    );
}

// Ports TestConvertOpenAIResponsesResponseToInteractionsStreamCreatedThenDelta.
#[test]
fn responses_stream_created_then_delta() {
    let out = run_back(&[
        r#"{"type":"response.created","response":{"id":"resp_1","model":"gpt-test"}}"#,
        r#"{"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"hi"}"#,
    ]);
    assert_eq!(
        interactions_names(&out),
        "interaction.created,interaction.status_update,step.start,step.delta"
    );
    let payload = find_interactions(&out, "interaction.status_update");
    assert_eq!(s(&payload, "interaction_id"), "resp_1", "{payload}");
}

// Ports TestConvertOpenAIResponsesResponseToInteractionsStreamCompletesAfterSteps.
#[test]
fn responses_stream_completes_after_steps() {
    let out = run_back(&[
        r#"{"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"我将调用工具。"}"#,
        r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":"{\"q\":\"weather\"}"}}"#,
        r#"{"type":"response.completed","response":{"output":[],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}"#,
    ]);
    assert_eq!(
        interactions_names(&out),
        "interaction.created,interaction.status_update,step.start,step.delta,step.stop,step.start,step.delta,step.stop,interaction.completed,done"
    );
    let completed = find_interactions(&out, "interaction.completed");
    assert_eq!(
        int(&completed, "interaction.usage.total_tokens"),
        3,
        "{completed}"
    );
}

// Ports TestConvertOpenAIResponsesResponseToInteractionsStreamSkipsCompletedTextAfterUnkeyedDelta.
#[test]
fn responses_stream_skips_completed_text_after_unkeyed_delta() {
    let mut stream = OpenAIResponsesToInteractionsStream::new("gpt-test");
    let out = stream.translate(
        br#"{"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"delta":"final"}"#,
    );
    let delta = find_interactions(&out, "step.delta");
    assert_eq!(s(&delta, "delta.text"), "final", "{delta}");
    let out = stream.translate(br#"{"type":"response.completed","response":{"output":[{"type":"message","id":"msg_1","content":[{"type":"output_text","text":"final"}]}],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}"#);
    assert_eq!(count_interactions(&out, "step.delta"), 0);
    assert_eq!(count_interactions(&out, "interaction.completed"), 1);
}

// Ports TestConvertOpenAIResponsesResponseToInteractionsIncompleteTerminal.
#[test]
fn responses_incomplete_terminal() {
    // NonStream
    let raw = br#"{"id":"resp_1","status":"incomplete","output":[{"type":"message","content":[{"type":"output_text","text":"partial"}]}],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}"#;
    let out = convert_openai_responses_response_to_interactions_non_stream("gpt-test", raw);
    assert_eq!(s(&out, "status"), "incomplete", "{out}");
    assert_eq!(s(&out, "steps.0.content.0.text"), "partial", "{out}");
    assert_eq!(int(&out, "usage.total_tokens"), 3, "{out}");

    // Stream
    let mut stream = OpenAIResponsesToInteractionsStream::new("gpt-test");
    let out = stream.translate(br#"{"type":"response.incomplete","response":{"id":"resp_1","status":"incomplete","output":[{"type":"message","id":"msg_1","content":[{"type":"output_text","text":"partial"}]}],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}"#);
    assert_eq!(count_interactions(&out, "interaction.completed"), 1);
    assert_eq!(count_interactions(&out, "done"), 1);
    let delta = find_interactions(&out, "step.delta");
    assert_eq!(s(&delta, "delta.text"), "partial", "{delta}");
    let completed = find_interactions(&out, "interaction.completed");
    assert_eq!(
        s(&completed, "interaction.status"),
        "incomplete",
        "{completed}"
    );
    assert_eq!(
        int(&completed, "interaction.usage.total_tokens"),
        3,
        "{completed}"
    );
    let done = stream.translate(b"data: [DONE]");
    assert_eq!(count_interactions(&done, "interaction.completed"), 0);
    assert_eq!(count_interactions(&done, "done"), 0);

    // CompletedControl
    let raw = br#"{"id":"resp_1","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}"#;
    let out = convert_openai_responses_response_to_interactions_non_stream("gpt-test", raw);
    assert_eq!(s(&out, "status"), "completed", "{out}");
}

// Ports TestConvertInteractionsResponseToOpenAIResponsesNonStream_PreservesEnvironmentID,
// from devin/swe-2 rather than upstream's Antigravity model.
#[test]
fn whole_response_preserves_environment_id() {
    let raw = br#"{"id":"interaction_1","object":"interaction","environment_id":"env_abc123","status":"completed","steps":[{"type":"model_output","content":[{"text":"ok"}]}],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}"#;
    let out = convert_interactions_response_to_openai_responses_non_stream(
        MODEL,
        &json!({ "model": MODEL }),
        &Value::Null,
        raw,
    )
    .expect("a response");
    assert_eq!(s(&out, "environment_id"), "env_abc123", "{out}");
}

// Ports TestConvertInteractionsResponseToOpenAIResponsesStream_PreservesEnvironmentID,
// from devin/swe-2 rather than upstream's Antigravity model.
#[test]
fn stream_preserves_environment_id() {
    let out = events(&run_with(
        MODEL,
        &json!({ "model": MODEL }),
        &[
            "event: interaction.created\ndata: {\"interaction\":{\"id\":\"interaction_1\",\"environment_id\":\"env_stream123\",\"model\":\"devin/swe-2\"},\"event_type\":\"interaction.created\"}\n\n".to_owned(),
            "event: interaction.completed\ndata: {\"interaction\":{\"id\":\"interaction_1\",\"environment_id\":\"env_stream123\",\"status\":\"completed\"},\"event_type\":\"interaction.completed\"}\n\n".to_owned(),
            "event: done\ndata: [DONE]\n\n".to_owned(),
        ],
    ));
    let created = find(&out, "response.created");
    assert_eq!(
        s(&created, "response.environment_id"),
        "env_stream123",
        "{created}"
    );
    let completed = find(&out, "response.completed");
    assert_eq!(
        s(&completed, "response.environment_id"),
        "env_stream123",
        "{completed}"
    );
}

// Ports the second case of
// TestConvertInteractionsResponseToOpenAIResponsesPreservesNonCollidingAndNonAntigravityNames.
#[test]
fn preserves_external_names() {
    let raw = br#"{
		"id":"interaction_2",
		"model":"gemini-3.1-flash-lite",
		"steps":[
			{"type":"function_call","id":"call_2","name":"external_read_file","arguments":{"path":"/etc/hosts"}}
		]
	}"#;
    let out = convert_interactions_response_to_openai_responses_non_stream(
        "gemini-3.1-flash-lite",
        &json!({ "model": "gemini-3.1-flash-lite" }),
        &Value::Null,
        raw,
    )
    .expect("a response");
    assert_eq!(s(&out, "output.0.name"), "external_read_file", "{out}");
}

/// Upstream's `gh issue view` command, with `>` and `&` in it.
const COMMAND: &str =
    "gh issue view 5802 --json number,title,body,url,state,labels,assignees 2>&1 | head -100";

/// Whether `text` holds `>` or `&` escaped as Go's JSON escapes them.
fn html_escaped(text: &str) -> bool {
    text.contains(r"\u003e") || text.contains(r"\u0026")
}

// Ports TestConvertInteractionsResponseToOpenAIResponses_PreservesHTMLCharactersInToolCallArguments.
#[test]
fn preserves_html_characters_in_tool_call_arguments() {
    let raw = format!(
        r#"{{
		"id":"interaction_test",
		"model":"devin/swe-2",
		"steps":[
			{{"type":"function_call","id":"bash_1","name":"bash","arguments":{{"command":{},"timeout":60}}}}
		]
	}}"#,
        Value::from(COMMAND)
    );
    let out = convert_interactions_response_to_openai_responses_non_stream(
        MODEL,
        &Value::Null,
        &Value::Null,
        raw.as_bytes(),
    )
    .expect("a response")
    .to_string();
    assert!(!html_escaped(&out), "{out}");
    assert!(out.contains("2>&1"), "{out}");

    let out = run_with(
        MODEL,
        &Value::Null,
        &[
            "event: interaction.created\ndata: {\"interaction\":{\"id\":\"interaction_test\",\"model\":\"devin/swe-2\"},\"event_type\":\"interaction.created\"}\n\n".to_owned(),
            "event: step.start\ndata: {\"index\":1,\"step\":{\"type\":\"function_call\",\"id\":\"bash_1\",\"name\":\"bash\"},\"event_type\":\"step.start\"}\n\n".to_owned(),
            format!("event: step.delta\ndata: {{\"index\":1,\"delta\":{{\"type\":\"arguments_delta\",\"arguments\":\"{{\\\"command\\\":\\\"{COMMAND}\\\",\\\"timeout\\\":60}}\"}},\"event_type\":\"step.delta\"}}\n\n"),
            "event: step.stop\ndata: {\"index\":1,\"event_type\":\"step.stop\"}\n\n".to_owned(),
            "event: interaction.completed\ndata: {\"interaction\":{\"id\":\"interaction_test\",\"status\":\"completed\"},\"event_type\":\"interaction.completed\"}\n\n".to_owned(),
            "event: done\ndata: [DONE]\n\n".to_owned(),
        ],
    );
    assert!(!html_escaped(&out), "{out}");
    let done = find(&events(&out), "response.function_call_arguments.done");
    assert!(
        !done.is_null(),
        "missing response.function_call_arguments.done"
    );
    assert!(done.to_string().contains("2>&1"), "{done}");
}

// Ports TestConvertInteractionsResponseToOpenAIResponses_LogReplayTwoToolCalls.
#[test]
fn log_replay_two_tool_calls() {
    let out = run_with(
        MODEL,
        &Value::Null,
        &[
            "event: interaction.created\ndata: {\"interaction\":{\"id\":\"interaction_69c3126a-ab3\",\"model\":\"devin/swe-2\"},\"event_type\":\"interaction.created\"}\n\n".to_owned(),
            "event: step.start\ndata: {\"index\":0,\"step\":{\"type\":\"thought\"},\"event_type\":\"step.start\"}\n\n".to_owned(),
            "event: step.delta\ndata: {\"index\":0,\"delta\":{\"type\":\"thought_summary\",\"text\":\"I need to triage GitHub issue 5802.\"},\"event_type\":\"step.delta\"}\n\n".to_owned(),
            "event: step.stop\ndata: {\"index\":0,\"event_type\":\"step.stop\"}\n\n".to_owned(),
            "event: step.start\ndata: {\"index\":1,\"step\":{\"type\":\"function_call\",\"id\":\"title_0\",\"call_id\":\"title_0\",\"name\":\"title\"},\"event_type\":\"step.start\"}\n\n".to_owned(),
            "event: step.delta\ndata: {\"index\":1,\"delta\":{\"type\":\"arguments_delta\",\"arguments\":\"{\\\"title\\\": \\\"Triage issue 5802\\\"}\"},\"event_type\":\"step.delta\"}\n\n".to_owned(),
            "event: step.stop\ndata: {\"index\":1,\"event_type\":\"step.stop\"}\n\n".to_owned(),
            "event: step.start\ndata: {\"index\":2,\"step\":{\"type\":\"function_call\",\"id\":\"bash_1\",\"call_id\":\"bash_1\",\"name\":\"bash\"},\"event_type\":\"step.start\"}\n\n".to_owned(),
            format!("event: step.delta\ndata: {{\"index\":2,\"delta\":{{\"type\":\"arguments_delta\",\"arguments\":\"{{\\\"command\\\": \\\"{COMMAND}\\\", \\\"timeout\\\": 60}}\"}},\"event_type\":\"step.delta\"}}\n\n"),
            "event: step.stop\ndata: {\"index\":2,\"event_type\":\"step.stop\"}\n\n".to_owned(),
            "event: interaction.completed\ndata: {\"interaction\":{\"id\":\"interaction_69c3126a-ab3\",\"status\":\"completed\"},\"event_type\":\"interaction.completed\"}\n\n".to_owned(),
            "event: done\ndata: [DONE]\n\n".to_owned(),
        ],
    );
    assert!(!html_escaped(&out), "{out}");
    let completed = find(&events(&out), "response.completed");
    assert!(!completed.is_null(), "missing response.completed");
    let output = completed["response"]["output"]
        .as_array()
        .expect("an output array");
    assert_eq!(output.len(), 3, "{completed}");
    let title = &output[1];
    assert!(
        s(title, "name") == "title" && s(title, "call_id") == "title_0",
        "{title}"
    );
    assert_eq!(s(title, "arguments"), r#"{"title": "Triage issue 5802"}"#);
    let bash = &output[2];
    assert!(
        s(bash, "name") == "bash" && s(bash, "call_id") == "bash_1",
        "{bash}"
    );
    assert!(s(bash, "arguments").contains("2>&1"), "{bash}");
}

// Ports TestConvertInteractionsResponseToOpenAIResponses_FunctionCallHasStatus.
#[test]
fn function_call_has_status() {
    let out = events(&run_with(
        MODEL,
        &Value::Null,
        &[
            "event: interaction.created\ndata: {\"interaction\":{\"id\":\"i1\",\"model\":\"devin/swe-2\"},\"event_type\":\"interaction.created\"}\n\n".to_owned(),
            "event: step.start\ndata: {\"index\":0,\"step\":{\"type\":\"function_call\",\"id\":\"call_1\",\"call_id\":\"call_1\",\"name\":\"write_file\"},\"event_type\":\"step.start\"}\n\n".to_owned(),
            "event: step.delta\ndata: {\"index\":0,\"delta\":{\"type\":\"arguments_delta\",\"arguments\":\"{\\\"path\\\":\\\"/tmp/test\\\"}\"},\"event_type\":\"step.delta\"}\n\n".to_owned(),
            "event: step.stop\ndata: {\"index\":0,\"event_type\":\"step.stop\"}\n\n".to_owned(),
            "event: interaction.completed\ndata: {\"interaction\":{\"id\":\"i1\",\"status\":\"completed\"},\"event_type\":\"interaction.completed\"}\n\n".to_owned(),
            "event: done\ndata: [DONE]\n\n".to_owned(),
        ],
    ));
    let added = find(&out, "response.output_item.added");
    assert_eq!(s(&added, "item.status"), "in_progress", "{added}");
    let done = find(&out, "response.output_item.done");
    assert_eq!(s(&done, "item.status"), "completed", "{done}");
    let completed = find(&out, "response.completed");
    assert_eq!(
        s(&completed, "response.output.0.status"),
        "completed",
        "{completed}"
    );

    let raw = br#"{
		"id":"i1",
		"model":"devin/swe-2",
		"steps":[
			{"type":"function_call","id":"call_1","name":"write_file","arguments":{"path":"/tmp/test"}}
		]
	}"#;
    let out = convert_interactions_response_to_openai_responses_non_stream(
        MODEL,
        &Value::Null,
        &Value::Null,
        raw,
    )
    .expect("a response");
    assert_eq!(s(&out, "output.0.status"), "completed", "{out}");

    let out = events(&run_with(
        MODEL,
        &Value::Null,
        &[
            "event: interaction.created\ndata: {\"interaction\":{\"id\":\"i2\",\"model\":\"devin/swe-2\"},\"event_type\":\"interaction.created\"}\n\n".to_owned(),
            "event: step.start\ndata: {\"index\":0,\"step\":{\"type\":\"function_call\",\"id\":\"call_2\",\"name\":\"write_file\"},\"event_type\":\"step.start\"}\n\n".to_owned(),
            "event: step.delta\ndata: {\"index\":0,\"delta\":{\"type\":\"arguments_delta\",\"arguments\":\"{\\\"path\\\":\\\"/tmp/t\"},\"event_type\":\"step.delta\"}\n\n".to_owned(),
            "event: step.stop\ndata: {\"index\":0,\"event_type\":\"step.stop\"}\n\n".to_owned(),
            "event: interaction.completed\ndata: {\"interaction\":{\"id\":\"i2\",\"status\":\"incomplete\",\"finish_reason\":\"length\"},\"event_type\":\"interaction.completed\"}\n\n".to_owned(),
            "event: done\ndata: [DONE]\n\n".to_owned(),
        ],
    ));
    let incomplete = find(&out, "response.incomplete");
    assert!(!incomplete.is_null(), "expected response.incomplete");
    assert_eq!(s(&incomplete, "response.status"), "incomplete");
    assert_eq!(
        s(&incomplete, "response.incomplete_details.reason"),
        "max_output_tokens"
    );
}

// Ports TestConvertInteractionsResponseToOpenAIResponses_ContentFilterIncomplete.
#[test]
fn content_filter_incomplete() {
    let out = events(&run_with(
        MODEL,
        &Value::Null,
        &[
            "event: interaction.created\ndata: {\"interaction\":{\"id\":\"i_cf\",\"model\":\"devin/swe-2\"},\"event_type\":\"interaction.created\"}\n\n".to_owned(),
            "event: step.start\ndata: {\"index\":0,\"step\":{\"type\":\"model_output\"},\"event_type\":\"step.start\"}\n\n".to_owned(),
            "event: step.delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text\",\"text\":\"blocked\"},\"event_type\":\"step.delta\"}\n\n".to_owned(),
            "event: step.stop\ndata: {\"index\":0,\"event_type\":\"step.stop\"}\n\n".to_owned(),
            "event: interaction.completed\ndata: {\"interaction\":{\"id\":\"i_cf\",\"status\":\"incomplete\",\"finish_reason\":\"content_filter\"},\"event_type\":\"interaction.completed\"}\n\n".to_owned(),
            "event: done\ndata: [DONE]\n\n".to_owned(),
        ],
    ));
    let incomplete = find(&out, "response.incomplete");
    assert!(!incomplete.is_null(), "expected response.incomplete");
    assert_eq!(
        s(&incomplete, "response.incomplete_details.reason"),
        "content_filter",
        "{incomplete}"
    );

    let raw = br#"{
		"id":"i_cf",
		"model":"devin/swe-2",
		"status":"incomplete",
		"finish_reason":"content_filter",
		"steps":[{"type":"model_output","content":[{"type":"text","text":"blocked"}]}]
	}"#;
    let out = convert_interactions_response_to_openai_responses_non_stream(
        MODEL,
        &Value::Null,
        &Value::Null,
        raw,
    )
    .expect("a response");
    assert_eq!(s(&out, "status"), "incomplete");
    assert_eq!(s(&out, "incomplete_details.reason"), "content_filter");
}

// Ports TestConvertInteractionsResponseToOpenAIResponses_MissingUsageDefaultsToZeros.
#[test]
fn missing_usage_defaults_to_zeros() {
    let out = events(&run_with(
        MODEL,
        &Value::Null,
        &[
            "event: interaction.created\ndata: {\"interaction\":{\"id\":\"i_nousage\",\"model\":\"devin/swe-2\"},\"event_type\":\"interaction.created\"}\n\n".to_owned(),
            "event: step.start\ndata: {\"index\":0,\"step\":{\"type\":\"model_output\"},\"event_type\":\"step.start\"}\n\n".to_owned(),
            "event: step.delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text\",\"text\":\"hello\"},\"event_type\":\"step.delta\"}\n\n".to_owned(),
            "event: step.stop\ndata: {\"index\":0,\"event_type\":\"step.stop\"}\n\n".to_owned(),
            "event: interaction.completed\ndata: {\"interaction\":{\"id\":\"i_nousage\",\"status\":\"completed\"},\"event_type\":\"interaction.completed\"}\n\n".to_owned(),
            "event: done\ndata: [DONE]\n\n".to_owned(),
        ],
    ));
    let completed = find(&out, "response.completed");
    assert!(!completed.is_null(), "missing response.completed");
    assert!(
        get(&completed, "response.usage.input_tokens").is_some(),
        "{completed}"
    );
    assert_eq!(int(&completed, "response.usage.input_tokens"), 0);
    assert!(get(&completed, "response.usage.output_tokens").is_some());
    assert!(get(&completed, "response.usage.total_tokens").is_some());

    let raw = br#"{
		"id":"i_nousage",
		"model":"devin/swe-2",
		"status":"completed",
		"steps":[{"type":"model_output","content":[{"type":"text","text":"hello"}]}]
	}"#;
    let out = convert_interactions_response_to_openai_responses_non_stream(
        MODEL,
        &Value::Null,
        &Value::Null,
        raw,
    )
    .expect("a response");
    assert!(get(&out, "usage.input_tokens").is_some(), "{out}");
    assert!(get(&out, "usage.output_tokens").is_some());
    assert!(get(&out, "usage.total_tokens").is_some());
}

// Ports TestConvertInteractionsResponseToOpenAIResponses_RestoresNamespaceAndCustomTool.
#[test]
fn restores_namespace_and_custom_tool() {
    let request = parse(
        r#"{
		"model": "devin/gemini-3-7-flash",
		"tools": [
			{
				"type": "namespace",
				"name": "multi_agent_v1",
				"tools": [
					{"type": "function", "name": "close_agent", "description": "Close an agent"}
				]
			},
			{
				"type": "namespace",
				"name": "functions",
				"tools": [
					{"type": "custom", "name": "exec", "description": "Run custom command"}
				]
			}
		]
	}"#,
    );
    let raw = br#"{
		"id": "resp_1",
		"steps": [
			{
				"type": "function_call",
				"id": "call_1",
				"name": "multi_agent_v1__close_agent",
				"arguments": {"target": "agent_1"}
			},
			{
				"type": "function_call",
				"id": "call_2",
				"name": "functions__exec",
				"arguments": {"input": "echo hi"}
			}
		]
	}"#;
    let model = "devin/gemini-3-7-flash";
    let out = convert_interactions_response_to_openai_responses_non_stream(
        model,
        &request,
        &Value::Null,
        raw,
    )
    .expect("a response");
    assert_eq!(s(&out, "output.0.name"), "close_agent");
    assert_eq!(s(&out, "output.0.namespace"), "multi_agent_v1");
    assert_eq!(s(&out, "output.0.type"), "function_call");
    assert_eq!(s(&out, "output.1.name"), "exec");
    assert_eq!(s(&out, "output.1.namespace"), "functions");
    assert_eq!(s(&out, "output.1.type"), "custom_tool_call");
    assert_eq!(s(&out, "output.1.input"), "echo hi");
    assert!(get(&out, "output.1.arguments").is_none());

    let mut stream = InteractionsToOpenAIResponsesStream::new(model, &request, &Value::Null);
    let start = send(
        &mut stream,
        br#"{"event_type":"step.start","index":0,"step":{"type":"function_call","call_id":"call_1","name":"multi_agent_v1__close_agent","arguments":"{\"target\":\"agent_1\"}"}}"#,
    );
    let added = find(&start, "response.output_item.added");
    assert!(!added.is_null(), "expected response.output_item.added");
    assert_eq!(s(&added, "item.name"), "close_agent");
    assert_eq!(s(&added, "item.namespace"), "multi_agent_v1");
    assert_eq!(s(&added, "item.type"), "function_call");
    let stop = send(&mut stream, br#"{"event_type":"step.stop","index":0}"#);
    let done = find(&stop, "response.output_item.done");
    assert!(!done.is_null(), "expected response.output_item.done");
    assert_eq!(s(&done, "item.name"), "close_agent");
    assert_eq!(s(&done, "item.namespace"), "multi_agent_v1");

    let mut stream = InteractionsToOpenAIResponsesStream::new(model, &request, &Value::Null);
    let start = send(
        &mut stream,
        br#"{"event_type":"step.start","index":0,"step":{"type":"function_call","call_id":"call_2","name":"functions__exec"}}"#,
    );
    let added = find(&start, "response.output_item.added");
    assert!(!added.is_null(), "expected response.output_item.added");
    assert_eq!(s(&added, "item.type"), "custom_tool_call");
    assert_eq!(s(&added, "item.name"), "exec");
    assert_eq!(s(&added, "item.namespace"), "functions");
    assert_eq!(count(&start, "response.custom_tool_call_input.done"), 0);
    let delta = send(
        &mut stream,
        br#"{"event_type":"step.delta","index":0,"delta":{"type":"arguments_delta","arguments":"{\"input\":\"pwd\"}"}}"#,
    );
    assert!(
        delta
            .iter()
            .all(|event| !kind(event).starts_with("response.function_call_arguments")),
        "{delta:?}"
    );
    let stop = send(&mut stream, br#"{"event_type":"step.stop","index":0}"#);
    assert_eq!(count(&stop, "response.custom_tool_call_input.done"), 1);
    assert_eq!(
        s(
            &find(&stop, "response.custom_tool_call_input.done"),
            "input"
        ),
        "pwd"
    );
    assert_eq!(count(&stop, "response.output_item.done"), 1);
    let done = find(&stop, "response.output_item.done");
    assert_eq!(s(&done, "item.input"), "pwd");
    assert_eq!(s(&done, "item.type"), "custom_tool_call");
}

// Ports TestConvertInteractionsResponseToOpenAIResponses_ResponseFailed.
#[test]
fn response_failed() {
    for (name, payload, want_message, want_code) in [
        (
            "response_failed_top_level",
            r#"data: {"event_type":"response.failed","error":{"message":"devin upstream error (permission_denied): Unable to process request due to an MCP configuration issue.","code":"403"}}"#,
            "permission_denied",
            "403",
        ),
        (
            "interaction_failed_nested",
            r#"data: {"event_type":"interaction.failed","interaction":{"error":{"message":"service unavailable","code":"503"}}}"#,
            "service unavailable",
            "503",
        ),
        (
            "fallback_defaults",
            r#"data: {"event_type":"response.failed"}"#,
            "upstream execution failed",
            "",
        ),
    ] {
        let mut stream =
            InteractionsToOpenAIResponsesStream::new("devin/kimi-k3", &Value::Null, &Value::Null);
        let out = send(&mut stream, payload);
        assert!(!out.is_empty(), "{name}: no events");
        let failed = find(&out, "response.failed");
        assert!(!failed.is_null(), "{name}: {out:?}");
        assert_eq!(s(&failed, "response.status"), "failed", "{name}");
        assert!(
            s(&failed, "response.error.message").contains(want_message),
            "{name}: {failed}"
        );
        if !want_code.is_empty() {
            assert_eq!(s(&failed, "response.error.code"), want_code, "{name}");
        }
    }
}

// Ports TestConvertInteractionsResponseToOpenAIResponses_PreservesIDAndSeqOnFailure.
#[test]
fn preserves_id_and_sequence_on_failure() {
    let mut stream =
        InteractionsToOpenAIResponsesStream::new("devin/kimi-k3", &Value::Null, &Value::Null);
    let created = send(
        &mut stream,
        r#"data: {"event_type":"interaction.created","interaction":{"id":"interaction_test_id","model":"devin/kimi-k3"}}"#,
    );
    let created = find(&created, "response.created");
    assert_eq!(s(&created, "response.id"), "interaction_test_id");
    assert_eq!(int(&created, "sequence_number"), 1);
    let failed = send(
        &mut stream,
        r#"data: {"event_type":"response.failed","error":{"message":"permission denied","code":"403"}}"#,
    );
    let failed = find(&failed, "response.failed");
    assert_eq!(s(&failed, "response.id"), "interaction_test_id");
    assert_eq!(int(&failed, "sequence_number"), 2);
}

// Not upstream's: a whole Interactions response with an apply_patch call
// whose arguments aren't one input string fails, with the reason.
#[test]
fn whole_response_patch_failure_says_why() {
    let raw = br#"{"id":"r","steps":[{"type":"function_call","name":"functions__apply_patch","arguments":{}}]}"#;
    let error = non_stream(MODEL, &parse(PATCH_REQUEST), &Value::Null, raw)
        .expect_err("invalid arguments fail");
    assert!(!error.to_string().is_empty());
    assert!(
        convert_interactions_response_to_openai_responses_non_stream(
            MODEL,
            &parse(PATCH_REQUEST),
            &Value::Null,
            raw
        )
        .is_none()
    );
}

// Not upstream's: FinalizeToolInput fails a stream for an `apply_patch`
// client that never completed, and only that one; with no chunk given there
// is nothing to finalize, as upstream has no state then.
#[test]
fn finalize_tool_input_fails_only_an_unfinished_patch_stream() {
    let created = r#"data: {"event_type":"interaction.created","interaction":{"id":"i1"}}"#;
    let completed = r#"data: {"event_type":"interaction.completed","interaction":{"id":"i1","status":"completed"}}"#;

    let mut stream = patch_stream();
    assert_eq!(stream.finalize_tool_input(), "");
    assert!(stream.tool_input_error().is_none());

    let mut stream = patch_stream();
    send(&mut stream, created);
    let failed = events(&stream.finalize_tool_input());
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(kind(&failed[0]), "response.failed");
    assert_eq!(s(&failed[0], "response.id"), "i1");
    assert!(stream.tool_input_error().is_some());
    assert_eq!(stream.finalize_tool_input(), "");

    let mut stream = patch_stream();
    send(&mut stream, created);
    send(&mut stream, completed);
    assert_eq!(stream.finalize_tool_input(), "");
    assert!(stream.tool_input_error().is_none());

    let mut stream = stream_for("{}");
    send(&mut stream, created);
    assert_eq!(stream.finalize_tool_input(), "");
    assert!(stream.tool_input_error().is_none());
}

// Not upstream's: a function call's arguments keep each number as written
// both ways, as upstream copies them, from a whole response, one with text
// after it, or a stream, and an interaction id sent as the number -0 stays
// "-0", as gjson's String() gives it (checked with Go).
#[test]
fn numbers_keep_their_text() {
    let spelled = r#"{"x":-0,"y":1E20,"z":[1e5,0.10]}"#;
    let text = serde_json::to_string(spelled).unwrap();
    for tail in ["", " x"] {
        let body = format!(
            r#"{{"id":"i1","steps":[{{"type":"function_call","id":"c1","name":"f","arguments":{spelled}}}]}}{tail}"#
        );
        let out = convert_interactions_response_to_openai_responses_non_stream(
            "m",
            &Value::Null,
            &Value::Null,
            body.as_bytes(),
        )
        .expect("a response");
        assert_eq!(out["output"][0]["arguments"], spelled, "{out}");

        for arguments in [spelled, text.as_str()] {
            let body = format!(
                r#"{{"id":"r1","output":[{{"type":"function_call","call_id":"c1","name":"f","arguments":{arguments}}}]}}{tail}"#
            );
            let out =
                convert_openai_responses_response_to_interactions_non_stream("m", body.as_bytes());
            assert_eq!(out["steps"][0]["arguments"].to_string(), spelled, "{out}");
        }
    }

    let mut stream = InteractionsToOpenAIResponsesStream::new("m", &Value::Null, &Value::Null);
    let mut out = stream.translate(
        br#"data: {"event_type":"interaction.created","interaction":{"id":-0,"model":"m"}}"#,
    );
    out += &stream.translate(
        format!(
            r#"data: {{"event_type":"step.start","index":0,"step":{{"type":"function_call","id":"c1","name":"f","arguments":{spelled}}}}}"#
        )
        .as_bytes(),
    );
    out += &stream.translate(br#"data: {"event_type":"step.stop","index":0}"#);
    assert!(out.contains(r#""response":{"id":"-0","#), "{out}");
    assert!(out.contains(&format!(r#""delta":{text}"#)), "{out}");
    assert!(out.contains(&format!(r#""arguments":{text}"#)), "{out}");

    let mut stream = OpenAIResponsesToInteractionsStream::new("m");
    let out =
        stream.translate(br#"data: {"type":"response.created","response":{"id":-0,"model":"m"}}"#);
    assert!(out.contains(r#"{"interaction":{"id":"-0","#), "{out}");
}
