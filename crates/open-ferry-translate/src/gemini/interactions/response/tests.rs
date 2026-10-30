// Ported from CLIProxyAPI internal/translator/gemini/interactions/interactions_gemini_common_test.go
// (v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The response translators' tests: upstream's response tests are here, and
//! its request tests in `request/tests.rs`. All of them are ported. The tests
//! after them are new.

use serde_json::{Value, json};

use super::*;

const MODEL: &str = "gemini-3.5-flash";

/// The value at a gjson-like dotted path, where a number indexes an array.
fn get<'v>(value: &'v Value, at: &str) -> Option<&'v Value> {
    at.split('.').try_fold(value, |value, key| match value {
        Value::Array(items) => items.get(key.parse::<usize>().ok()?),
        Value::Object(fields) => fields.get(key),
        _ => None,
    })
}

/// gjson's `String()` of the value at `at`.
fn text(value: &Value, at: &str) -> String {
    str_of(get(value, at)).into_owned()
}

/// gjson's `Int()` of the value at `at`.
fn int(value: &Value, at: &str) -> i64 {
    get(value, at).map_or(0, int_of)
}

/// The array at `at`, or nothing.
fn array<'v>(value: &'v Value, at: &str) -> &'v [Value] {
    get(value, at)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Runs a Gemini stream through one translator, joining what each chunk
/// gives.
fn gemini_stream(chunks: &[&str]) -> Vec<String> {
    let mut stream = GeminiToInteractionsStream::new(MODEL);
    chunks
        .iter()
        .flat_map(|chunk| stream.translate(chunk.as_bytes()))
        .collect()
}

/// `ssePayload`: what follows an event's `data: ` line start, without the
/// line ends after it.
fn sse_data(event: &str) -> &str {
    event.find("\ndata: ").map_or("", |at| {
        event
            .get(at + "\ndata: ".len()..)
            .unwrap_or_default()
            .trim_end_matches(['\r', '\n'])
    })
}

/// The JSON of an event's data, or `Null`.
fn data(event: &str) -> Value {
    serde_json::from_str(sse_data(event)).unwrap_or(Value::Null)
}

/// `eventName`: the data's `event_type`, or else the `event: ` line's name.
fn event_name(event: &str) -> String {
    let kind = text(&data(event), "event_type");
    if !kind.is_empty() {
        return kind;
    }
    match (event.strip_prefix("event: "), event.find('\n')) {
        (Some(_), Some(end)) => event
            .get("event: ".len()..end)
            .unwrap_or_default()
            .to_owned(),
        _ => String::new(),
    }
}

/// `findNthEventPayload`: the data of the `n`th event of `kind`, or `Null`.
fn nth_event(events: &[String], kind: &str, n: usize) -> Value {
    events
        .iter()
        .filter(|event| event_name(event) == kind)
        .nth(n)
        .map_or(Value::Null, |event| data(event))
}

/// `findEventPayload`.
fn find_event(events: &[String], kind: &str) -> Value {
    nth_event(events, kind, 0)
}

/// `findStepDeltaPayloadByType`.
fn find_delta(events: &[String], delta_type: &str) -> Value {
    events
        .iter()
        .filter(|event| event_name(event) == "step.delta")
        .map(|event| data(event))
        .find(|payload| text(payload, "delta.type") == delta_type)
        .unwrap_or(Value::Null)
}

/// `eventTypes`.
fn event_types(events: &[String]) -> String {
    events
        .iter()
        .map(|event| event_name(event))
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>()
        .join(",")
}

/// `countEventType`.
fn count(events: &[String], kind: &str) -> usize {
    events
        .iter()
        .filter(|event| event_name(event) == kind)
        .count()
}

// TestConvertGeminiResponseToInteractionsNonStream.
#[test]
fn non_stream() {
    let out = convert_gemini_response_to_interactions_non_stream(
        MODEL,
        br#"{"responseId":"resp_1","candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":2,"totalTokenCount":3}}"#,
    );
    assert_eq!(text(&out, "steps.0.type"), "model_output");
    assert_eq!(text(&out, "steps.0.content.0.text"), "ok");
    assert_eq!(int(&out, "usage.total_tokens"), 3);
}

// TestConvertGeminiResponseToInteractionsNonStreamSnakeCaseUsage.
#[test]
fn non_stream_snake_case_usage() {
    let out = convert_gemini_response_to_interactions_non_stream(
        MODEL,
        br#"{"responseId":"resp_snake","candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}],"usage_metadata":{"prompt_token_count":11,"candidates_token_count":22,"total_token_count":33,"thoughts_token_count":44,"cached_content_token_count":55}}"#,
    );
    for (at, want) in [
        ("usage.input_tokens", 11),
        ("usage.output_tokens", 22),
        ("usage.reasoning_tokens", 44),
        ("usage.total_tokens", 33),
        ("usage.cached_tokens", 55),
    ] {
        assert_eq!(int(&out, at), want, "{at} in {out}");
    }
}

// TestConvertInteractionsResponseToGeminiStreamFunctionCall.
#[test]
fn interactions_stream_function_call() {
    let mut stream = InteractionsToGeminiStream::new("gemini-3.1-flash-lite");
    let created = stream.translate(
        br#"data: {"interaction":{"id":"i1","model":"gemini-3.1-flash-lite"},"event_type":"interaction.created"}"#,
    );
    assert_eq!(created, None);
    let start = stream.translate(
        br#"data: {"index":0,"step":{"type":"function_call","id":"call_1","signature":"sig_1","name":"get_weather","arguments":{}},"event_type":"step.start"}"#,
    );
    assert_eq!(start, None);
    let delta = stream
        .translate(
            r#"data: {"index":0,"delta":{"type":"arguments_delta","arguments":"{\"location\":\"北京\"}"},"event_type":"step.delta"}"#
                .as_bytes(),
        )
        .expect("a chunk for the arguments");
    assert_eq!(
        text(&delta, "candidates.0.content.parts.0.functionCall.name"),
        "get_weather"
    );
    assert_eq!(
        text(
            &delta,
            "candidates.0.content.parts.0.functionCall.args.location"
        ),
        "北京"
    );
    assert_eq!(
        text(&delta, "candidates.0.content.parts.0.functionCall.id"),
        "call_1"
    );
    assert_eq!(
        text(&delta, "candidates.0.content.parts.0.thoughtSignature"),
        "sig_1"
    );
    let completed = stream
        .translate(
            br#"data: {"interaction":{"id":"i1","status":"requires_action","usage":{"total_input_tokens":2,"total_output_tokens":3,"total_tokens":5,"total_thought_tokens":1,"total_cached_tokens":4},"service_tier":"standard","model":"gemini-3.1-flash-lite"},"event_type":"interaction.completed"}"#,
        )
        .expect("a chunk for the completion");
    assert_eq!(text(&completed, "candidates.0.finishReason"), "STOP");
    assert_eq!(int(&completed, "usageMetadata.promptTokenCount"), 2);
    assert_eq!(int(&completed, "usageMetadata.candidatesTokenCount"), 3);
    assert_eq!(int(&completed, "usageMetadata.totalTokenCount"), 5);
    assert_eq!(
        int(&completed, "usageMetadata.promptTokensDetails.0.tokenCount"),
        2
    );
    assert_eq!(stream.translate(b"event: done\ndata: [DONE]"), None);
}

// TestConvertInteractionsResponseToGeminiStreamFinishMetadataUsage.
#[test]
fn interactions_stream_finish_metadata_usage() {
    let mut stream = InteractionsToGeminiStream::new("gemini-test");
    let out = stream
        .translate(
            br#"data: {"event_type":"finish","metadata":{"total_usage":{"total_input_tokens":2,"total_output_tokens":6,"total_thought_tokens":3,"total_cached_tokens":1,"total_tokens":11}}}"#,
        )
        .expect("a chunk for the finish");
    assert_eq!(text(&out, "candidates.0.finishReason"), "STOP");
    for (at, want) in [
        ("usageMetadata.promptTokenCount", 2),
        ("usageMetadata.candidatesTokenCount", 6),
        ("usageMetadata.thoughtsTokenCount", 3),
        ("usageMetadata.cachedContentTokenCount", 1),
        ("usageMetadata.totalTokenCount", 11),
    ] {
        assert_eq!(int(&out, at), want, "{at} in {out}");
    }
}

// TestConvertInteractionsResponseToGeminiNonStreamFunctionCall.
#[test]
fn interactions_non_stream_function_call() {
    let out = convert_interactions_response_to_gemini_non_stream(
        "gemini-3.1-flash-lite",
        r#"{"id":"i1","model":"gemini-3.1-flash-lite","steps":[{"type":"function_call","call_id":"call_1","signature":"sig_1","name":"get_weather","arguments":{"location":"北京"}}],"usage":{"total_input_tokens":2,"total_output_tokens":3,"total_tokens":5}}"#
            .as_bytes(),
    );
    assert_eq!(
        text(&out, "candidates.0.content.parts.0.functionCall.name"),
        "get_weather"
    );
    assert_eq!(
        text(
            &out,
            "candidates.0.content.parts.0.functionCall.args.location"
        ),
        "北京"
    );
    assert_eq!(
        text(&out, "candidates.0.content.parts.0.thoughtSignature"),
        "sig_1"
    );
    assert_eq!(int(&out, "usageMetadata.totalTokenCount"), 5);
}

// TestConvertGeminiResponseToInteractionsNonStreamFunctionCall.
#[test]
fn non_stream_function_call() {
    let out = convert_gemini_response_to_interactions_non_stream(
        MODEL,
        br#"{"responseId":"resp_1","candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"lookup","args":{"q":"x"}}}]}}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":2,"totalTokenCount":3,"cachedContentTokenCount":4}}"#,
    );
    assert_eq!(text(&out, "steps.0.type"), "function_call");
    assert_eq!(text(&out, "steps.0.name"), "lookup");
    assert_eq!(int(&out, "usage.cached_tokens"), 4);
}

// TestConvertGeminiResponseToInteractionsNonStreamFunctionCallPreservesCallID.
#[test]
fn non_stream_function_call_preserves_call_id() {
    let out = convert_gemini_response_to_interactions_non_stream(
        MODEL,
        br#"{"responseId":"resp_1","candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"lookup","call_id":"call_response_1","args":{"q":"x"}}}]}}]}"#,
    );
    assert_eq!(text(&out, "steps.0.call_id"), "call_response_1");
}

// TestConvertGeminiResponseToInteractionsStreamFunctionCallCallID.
#[test]
fn stream_function_call_call_id() {
    let out = gemini_stream(&[
        r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"lookup","call_id":"call_stream_1","args":{"q":"x"}}}]}}]}"#,
    ]);
    let delta = find_event(&out, "step.delta");
    assert_ne!(delta, Value::Null, "{out:?}");
    assert_eq!(
        text(&find_event(&out, "step.start"), "step.id"),
        "call_stream_1"
    );
    assert_eq!(text(&delta, "delta.arguments"), r#"{"q":"x"}"#);
}

// TestConvertGeminiResponseToInteractionsStreamFunctionCallThoughtSignature.
#[test]
fn stream_function_call_thought_signature() {
    let out = gemini_stream(&[
        r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"thinking","thought":true}]}}]}"#,
        r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"I will call the tool."}]}}]}"#,
        r#"{"candidates":[{"content":{"role":"model","parts":[{"thoughtSignature":"sig-call","functionCall":{"name":"lookup","id":"call_1","args":{"q":"x"}}}]}}]}"#,
    ]);
    let signature = find_delta(&out, "thought_signature");
    assert_eq!(
        text(&signature, "delta.signature"),
        "sig-call",
        "{}",
        event_types(&out)
    );
    assert_eq!(int(&signature, "index"), 2, "{}", event_types(&out));
    let function_start = nth_event(&out, "step.start", 3);
    assert_eq!(text(&function_start, "step.type"), "function_call");
    assert_eq!(text(&function_start, "step.id"), "call_1");
    let arguments = find_delta(&out, "arguments_delta");
    assert_eq!(text(&arguments, "delta.arguments"), r#"{"q":"x"}"#);
}

// TestConvertGeminiResponseToInteractionsStreamStepLifecycle.
#[test]
fn stream_step_lifecycle() {
    let out = gemini_stream(&[
        r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"thinking","thought":true}]}}]}"#,
        r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"answer"}]}}]}"#,
        r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"lookup","id":"call_1","args":{"q":"x"}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":4,"totalTokenCount":7,"thoughtsTokenCount":2}}"#,
    ]);
    assert_eq!(
        event_types(&out),
        "interaction.created,interaction.status_update,step.start,step.delta,step.stop,step.start,step.delta,step.stop,step.start,step.delta,step.stop,interaction.completed,done"
    );
    assert_eq!(
        text(&nth_event(&out, "step.start", 0), "step.type"),
        "thought"
    );
    assert_eq!(
        text(&nth_event(&out, "step.start", 1), "step.type"),
        "model_output"
    );
    assert_eq!(
        text(&nth_event(&out, "step.start", 2), "step.type"),
        "function_call"
    );
    assert_eq!(
        text(&nth_event(&out, "step.delta", 0), "delta.type"),
        "thought_summary"
    );
    assert_eq!(
        text(&nth_event(&out, "step.delta", 2), "delta.type"),
        "arguments_delta"
    );
    let completed = find_event(&out, "interaction.completed");
    assert_eq!(int(&completed, "interaction.usage.total_input_tokens"), 3);
    assert_eq!(int(&completed, "interaction.usage.total_output_tokens"), 4);
    assert_eq!(int(&completed, "interaction.usage.total_thought_tokens"), 2);
}

// TestConvertGeminiResponseToInteractionsStreamSnakeCaseUsage.
#[test]
fn stream_snake_case_usage() {
    let out = gemini_stream(&[
        r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}],"usage_metadata":{"prompt_token_count":11,"candidates_token_count":22,"total_token_count":33,"thoughts_token_count":44,"cached_content_token_count":55}}"#,
    ]);
    assert_eq!(
        count(&out, "interaction.completed"),
        1,
        "{}",
        event_types(&out)
    );
    let completed = find_event(&out, "interaction.completed");
    for (at, want) in [
        ("interaction.usage.total_input_tokens", 11),
        ("interaction.usage.total_output_tokens", 22),
        ("interaction.usage.total_thought_tokens", 44),
        ("interaction.usage.total_tokens", 33),
        ("interaction.usage.total_cached_tokens", 55),
    ] {
        assert_eq!(int(&completed, at), want, "{at} in {completed}");
    }
}

// TestConvertGeminiResponseToInteractionsStreamEmitsTerminalOnce.
#[test]
fn stream_emits_terminal_once() {
    let mut stream = GeminiToInteractionsStream::new(MODEL);
    let finish = stream.translate(br#"{"candidates":[{"finishReason":"STOP"}]}"#);
    let usage = stream.translate(
        br#"{"candidates":[],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":2,"totalTokenCount":3}}"#,
    );
    let done = stream.translate(b"[DONE]");
    assert_eq!(count(&finish, "step.stop"), 0);
    assert_eq!(count(&finish, "interaction.completed"), 0);
    assert_eq!(count(&usage, "step.stop"), 0);
    assert_eq!(count(&usage, "interaction.completed"), 1);
    assert_eq!(count(&done, "interaction.completed"), 0);
    assert!(done.is_empty());
    assert_eq!(count(&usage, "done"), 1);
    let done_event = usage.iter().find(|event| event_name(event) == "done");
    assert_eq!(done_event.map(|event| sse_data(event)), Some("[DONE]"));
    assert_eq!(
        int(
            &find_event(&usage, "interaction.completed"),
            "interaction.usage.total_tokens"
        ),
        3
    );
}

// TestConvertGeminiResponseToInteractionsStreamDoesNotCompleteOnNonTerminalUsage.
#[test]
fn stream_does_not_complete_on_non_terminal_usage() {
    let mut stream = GeminiToInteractionsStream::new("gemini-3.5-flash-low");
    let thought = stream.translate(
        br#"{"candidates":[{"content":{"role":"model","parts":[{"thought":true,"text":"thinking"}]}}],"usageMetadata":{"promptTokenCount":124,"totalTokenCount":124}}"#,
    );
    assert_eq!(
        count(&thought, "interaction.completed"),
        0,
        "{}",
        event_types(&thought)
    );
    assert_eq!(count(&thought, "step.stop"), 0, "{}", event_types(&thought));
    let mut out = thought;
    for chunk in [
        r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"好的，我将为您调用天气查询工具。"}]}}],"usageMetadata":{"promptTokenCount":124,"candidatesTokenCount":17,"totalTokenCount":452,"thoughtsTokenCount":311}}"#,
        r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"get_weather","args":{"location":"北京"},"id":"nriii75p"}}]}}],"usageMetadata":{"promptTokenCount":124,"candidatesTokenCount":33,"totalTokenCount":468,"thoughtsTokenCount":311}}"#,
        r#"{"candidates":[{"content":{"role":"model","parts":[{"text":""}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":124,"candidatesTokenCount":33,"totalTokenCount":468,"thoughtsTokenCount":311}}"#,
    ] {
        out.extend(stream.translate(chunk.as_bytes()));
    }
    assert_eq!(
        count(&out, "interaction.completed"),
        1,
        "{}",
        event_types(&out)
    );
    assert_eq!(
        event_types(&out),
        "interaction.created,interaction.status_update,step.start,step.delta,step.stop,step.start,step.delta,step.stop,step.start,step.delta,step.stop,interaction.completed,done"
    );
    assert_eq!(
        int(
            &find_event(&out, "interaction.completed"),
            "interaction.usage.total_tokens"
        ),
        468
    );
}

// TestConvertGeminiResponseToInteractionsStreamIgnoresTrafficOnlyUsageMetadata.
#[test]
fn stream_ignores_traffic_only_usage_metadata() {
    let out = gemini_stream(&[
        r#"{"candidates":[{"content":{"role":"model","parts":[]}}],"usageMetadata":{"trafficType":"PROVISIONED_THROUGHPUT"}}"#,
    ]);
    assert_eq!(count(&out, "interaction.completed"), 0, "{out:?}");
    assert_eq!(count(&out, "done"), 0, "{out:?}");
}

// TestConvertGeminiResponseToInteractionsStreamCompletesOnDoneWithoutUsage.
#[test]
fn stream_completes_on_done_without_usage() {
    let mut stream = GeminiToInteractionsStream::new(MODEL);
    let finish = stream.translate(br#"{"candidates":[{"finishReason":"STOP"}]}"#);
    let done = stream.translate(b"[DONE]");
    assert_eq!(count(&finish, "interaction.completed"), 0);
    assert_eq!(count(&done, "interaction.completed"), 1);
    assert_eq!(count(&done, "done"), 1);
}

// TestConvertGeminiResponseToInteractionsNonStreamImage.
#[test]
fn non_stream_image() {
    let out = convert_gemini_response_to_interactions_non_stream(
        MODEL,
        br#"{"responseId":"resp_1","candidates":[{"content":{"role":"model","parts":[{"inlineData":{"mimeType":"image/png","data":"aGVsbG8="}}]}}]}"#,
    );
    assert_eq!(text(&out, "steps.0.content.0.type"), "image");
}

// TestConvertInteractionsResponseToGemini_FunctionCallArgsPreservesRef.
#[test]
fn interactions_function_call_args_preserve_ref() {
    let out = convert_interactions_response_to_gemini_non_stream(
        "gemini-3.1-flash-lite",
        br##"{
            "id": "i1",
            "model": "gemini-3.1-flash-lite",
            "steps": [
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "validate_schema",
                    "arguments": {
                        "schema": {
                            "$ref": "#/components/schemas/ErrorModel"
                        }
                    }
                }
            ]
        }"##,
    );
    let args = get(&out, "candidates.0.content.parts.0.functionCall.args");
    assert!(args.is_some_and(Value::is_object), "{out}");
    assert_eq!(
        args.and_then(|args| args.get("schema"))
            .and_then(|schema| schema.get("$ref")),
        Some(&json!("#/components/schemas/ErrorModel"))
    );
}

// TestConvertInteractionsResponseToGemini_FunctionResultWithRef.
#[test]
fn interactions_function_result_with_ref() {
    let out = convert_interactions_response_to_gemini_non_stream(
        "gemini-3.1-flash-lite",
        br##"{
            "id": "i1",
            "model": "gemini-3.1-flash-lite",
            "steps": [
                {
                    "type": "function_result",
                    "call_id": "call_1",
                    "name": "get_schema",
                    "result": {
                        "schema": {
                            "$ref": "#/components/schemas/ErrorModel"
                        }
                    }
                }
            ]
        }"##,
    );
    let result = get(
        &out,
        "candidates.0.content.parts.0.functionResponse.response.result",
    );
    let Some(Value::String(result)) = result else {
        panic!("result is not a string: {out}");
    };
    assert!(result.contains("#/components/schemas/ErrorModel"), "{out}");
}

// TestConvertGeminiResponseToInteractionsNonStream_ThoughtSignature.
#[test]
fn non_stream_thought_signature() {
    let out = convert_gemini_response_to_interactions_non_stream(
        MODEL,
        br#"{
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        {"functionCall": {"name": "lookup", "id": "call_1", "args": {"q": "x"}}, "thoughtSignature": "sig_fc"},
                        {"functionCall": {"name": "search", "id": "call_2", "args": {"q": "y"}}}
                    ]
                }
            }]
        }"#,
    );
    let steps = array(&out, "steps");
    assert_eq!(steps.len(), 3, "{out}");
    assert_eq!(text(&steps[0], "type"), "thought");
    assert_eq!(text(&steps[0], "signature"), "sig_fc");
    assert_eq!(text(&steps[1], "type"), "function_call");
    assert_eq!(text(&steps[1], "name"), "lookup");
    assert_eq!(text(&steps[2], "type"), "function_call");
    assert_eq!(text(&steps[2], "name"), "search");
}

// TestConvertGeminiResponseToInteractionsStream_TrailingThoughtSignature.
#[test]
fn stream_trailing_thought_signature() {
    let out = gemini_stream(&[
        r#"{"candidates":[{"content":{"parts":[{"text":"","thoughtSignature":"sig_trailing"}]}}]}"#,
    ]);
    assert!(
        out.iter()
            .any(|frame| frame.contains("thought_signature") && frame.contains("sig_trailing")),
        "{out:?}"
    );
}

// TestConvertInteractionsResponseToGemini_ResponseFailed.
#[test]
fn interactions_response_failed() {
    for (payload, code, status, message) in [
        (
            r#"data: {"event_type":"response.failed","error":{"message":"devin upstream error: permission_denied","code":"403"}}"#,
            403,
            "PERMISSION_DENIED",
            "permission_denied",
        ),
        (
            r#"data: {"event_type":"interaction.failed","error":{"message":"quota exceeded","code":"resource_exhausted"}}"#,
            429,
            "RESOURCE_EXHAUSTED",
            "quota exceeded",
        ),
        (
            r#"data: {"event_type":"response.failed","interaction":{"error":{"message":"service unavailable","code":"503"}}}"#,
            503,
            "UNAVAILABLE",
            "service unavailable",
        ),
        (
            r#"data: {"event_type":"response.failed"}"#,
            500,
            "INTERNAL",
            "upstream error occurred",
        ),
    ] {
        let mut stream = InteractionsToGeminiStream::new("devin/kimi-k3");
        let event = stream
            .translate(payload.as_bytes())
            .expect("an error chunk");
        assert_eq!(int(&event, "error.code"), code, "{payload}");
        assert_eq!(text(&event, "error.status"), status, "{payload}");
        assert!(text(&event, "error.message").contains(message), "{payload}");
    }
}

/// Masks the IDs and times a Gemini to Interactions stream reads from the
/// clock: `interaction_<n>` and `step_<n>` become `interaction_*` and
/// `step_*`, and `created` and `updated` become `*`.
fn mask_clock(value: &mut Value) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(mask_clock),
        Value::Object(fields) => {
            for (key, field) in fields.iter_mut() {
                if matches!(key.as_str(), "created" | "updated") {
                    *field = json!("*");
                } else {
                    mask_clock(field);
                }
            }
        }
        Value::String(text) => {
            for prefix in ["interaction_", "step_"] {
                let digits = text.strip_prefix(prefix).unwrap_or_default();
                if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
                    *text = format!("{prefix}*");
                }
            }
        }
        _ => {}
    }
}

/// A stream's frames, with the clock readings in their data masked.
fn masked_frames(frames: &[String]) -> String {
    frames
        .iter()
        .map(|frame| {
            let mut value = data(frame);
            if value.is_null() {
                return frame.clone();
            }
            mask_clock(&mut value);
            format!("event: {}\ndata: {value}\n\n", event_name(frame))
        })
        .collect()
}

// Not upstream's: a whole Gemini stream. Unlike upstream, the raw SSE chunk
// delivers its text and signature too (the Vertex bridge deviation).
#[test]
fn whole_gemini_stream() {
    let out = gemini_stream(&[
        r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"plan","thought":true}]}}]}"#,
        r#"data: {"candidates":[{"content":{"role":"model","parts":[{"text":"ok","thoughtSignature":"s1"}]}}]}"#,
        r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"f","args":{"a":1}}},{"functionResponse":{"name":"f","response":{"r":2}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":4,"totalTokenCount":9,"thoughtsTokenCount":2,"cachedContentTokenCount":1}}"#,
        "[DONE]",
    ]);
    let want = [
        r#"event: interaction.created
data: {"interaction":{"id":"interaction_*","status":"in_progress","object":"interaction","model":"gemini-3.5-flash"},"event_type":"interaction.created"}"#,
        r#"event: interaction.status_update
data: {"interaction_id":"interaction_*","status":"in_progress","event_type":"interaction.status_update"}"#,
        r#"event: step.start
data: {"index":0,"step":{"type":"thought"},"event_type":"step.start"}"#,
        r#"event: step.delta
data: {"index":0,"delta":{"content":{"text":"plan","type":"text"},"type":"thought_summary"},"event_type":"step.delta"}"#,
        r#"event: step.stop
data: {"index":0,"event_type":"step.stop"}"#,
        r#"event: step.start
data: {"index":1,"step":{"type":"model_output"},"event_type":"step.start"}"#,
        r#"event: step.delta
data: {"index":1,"delta":{"text":"ok","type":"text"},"event_type":"step.delta"}"#,
        r#"event: step.stop
data: {"index":1,"event_type":"step.stop"}"#,
        r#"event: step.start
data: {"index":2,"step":{"type":"thought"},"event_type":"step.start"}"#,
        r#"event: step.delta
data: {"index":2,"delta":{"signature":"s1","type":"thought_signature"},"event_type":"step.delta"}"#,
        r#"event: step.stop
data: {"index":2,"event_type":"step.stop"}"#,
        r#"event: step.start
data: {"index":3,"step":{"type":"function_call","id":"step_*","name":"f","arguments":{}},"event_type":"step.start"}"#,
        r#"event: step.delta
data: {"index":3,"delta":{"arguments":"{\"a\":1}","type":"arguments_delta"},"event_type":"step.delta"}"#,
        r#"event: step.stop
data: {"index":3,"event_type":"step.stop"}"#,
        r#"event: step.start
data: {"index":4,"step":{"type":"function_result"},"event_type":"step.start"}"#,
        r#"event: step.delta
data: {"index":4,"delta":{"type":"function_result","name":"f","result":{"r":2}},"event_type":"step.delta"}"#,
        r#"event: step.stop
data: {"index":4,"event_type":"step.stop"}"#,
        r#"event: interaction.completed
data: {"interaction":{"id":"interaction_*","status":"completed","usage":{"total_tokens":9,"total_input_tokens":3,"input_tokens_by_modality":[{"modality":"text","tokens":3}],"total_cached_tokens":1,"total_output_tokens":4,"total_tool_use_tokens":0,"total_thought_tokens":2},"created":"*","updated":"*","service_tier":"standard","object":"interaction","model":"gemini-3.5-flash"},"event_type":"interaction.completed"}"#,
        "event: done\ndata: [DONE]",
    ];
    let want: String = want.iter().map(|frame| format!("{frame}\n\n")).collect();
    assert_eq!(masked_frames(&out), want);
}

// Not upstream's: a whole Interactions stream, checked against upstream's
// output. Upstream copies the arguments' text; we write it compact.
#[test]
fn whole_interactions_stream() {
    let mut stream = InteractionsToGeminiStream::new("gemini-x");
    let out: Vec<String> = [
        r#"data: {"interaction":{"id":"i1","model":"gemini-y"},"event_type":"interaction.created"}"#,
        r#"{"index":0,"step":{"type":"thought"},"event_type":"step.start"}"#,
        r#"{"index":0,"delta":{"type":"thought_summary","content":{"type":"text","text":"hmm"}},"event_type":"step.delta"}"#,
        r#"{"index":0,"delta":{"type":"thought_signature","signature":"sig"},"event_type":"step.delta"}"#,
        "event: step.start\ndata: {\"index\":1,\"step\":{\"type\":\"function_call\",\"id\":\"c1\",\"name\":\"f\"},\"event_type\":\"step.start\"}",
        r#"{"index":1,"delta":{"type":"arguments_delta","arguments":"{\"a\": [1, 2]}"},"event_type":"step.delta"}"#,
        r#"{"index":2,"delta":{"type":"text","text":"done"},"event_type":"step.delta"}"#,
        r#"{"index":2,"delta":{"type":"text","text":""},"event_type":"step.delta"}"#,
        r#"{"interaction":{"id":"i1","status":"completed","service_tier":"flex","usage":{"total_input_tokens":5,"total_output_tokens":6}},"event_type":"interaction.completed"}"#,
        "data: [DONE]",
    ]
    .iter()
    .filter_map(|event| stream.translate(event.as_bytes()))
    .map(|chunk| chunk.to_string())
    .collect();
    assert_eq!(
        out,
        [
            r#"{"candidates":[{"content":{"parts":[{"text":"hmm","thought":true}],"role":"model"},"index":0}],"modelVersion":"gemini-x","responseId":"i1"}"#,
            r#"{"candidates":[{"content":{"parts":[{"text":"","thought":true,"thoughtSignature":"sig"}],"role":"model"},"index":0}],"modelVersion":"gemini-x","responseId":"i1"}"#,
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"f","args":{"a":[1,2]},"id":"c1"}}],"role":"model"},"index":0}],"modelVersion":"gemini-x","responseId":"i1"}"#,
            r#"{"candidates":[{"content":{"parts":[{"text":"done"}],"role":"model"},"index":0}],"modelVersion":"gemini-x","responseId":"i1"}"#,
            r#"{"candidates":[{"content":{"parts":[{"text":""}],"role":"model"},"index":0,"finishReason":"STOP"}],"modelVersion":"gemini-x","responseId":"i1","usageMetadata":{"serviceTier":"flex","promptTokenCount":5,"promptTokensDetails":[{"modality":"TEXT","tokenCount":5}],"candidatesTokenCount":6,"totalTokenCount":11}}"#,
        ]
    );
}

// Not upstream's: a whole Interactions response, checked against upstream's
// output.
#[test]
fn whole_interactions_response() {
    let out = convert_interactions_response_to_gemini_non_stream(
        "gemini-x",
        br#"{"id":"i9","model":"gemini-y","steps":[{"type":"thought","content":[{"type":"text","text":"t"}]},{"type":"model_output","content":[{"type":"text","text":"a"},{"type":"image","mime_type":"image/png","data":"AA=="}]},{"type":"function_call","id":"c1","name":"f","arguments":"{\"x\":1}","signature":"s"},{"type":"function_result","call_id":"c1","name":"f","result":"not json"}],"usage":{"input_tokens":1,"output_tokens":2,"reasoning_tokens":3,"cached_tokens":4}}"#,
    );
    assert_eq!(
        out.to_string(),
        r#"{"candidates":[{"content":{"parts":[{"text":"t","thought":true},{"text":"a"},{"inlineData":{"mimeType":"image/png","data":"AA=="}},{"functionCall":{"name":"f","args":{"x":1},"id":"c1"},"thoughtSignature":"s"},{"functionResponse":{"name":"f","response":"not json","id":"c1"}}],"role":"model"},"index":0,"finishReason":"STOP"}],"modelVersion":"gemini-y","responseId":"i9","usageMetadata":{"promptTokenCount":1,"promptTokensDetails":[{"modality":"TEXT","tokenCount":1}],"candidatesTokenCount":2,"totalTokenCount":3,"thoughtsTokenCount":3,"cachedContentTokenCount":4}}"#
    );
}

// Not upstream's: a whole Gemini response, checked against upstream's output.
#[test]
fn whole_gemini_response() {
    let out = convert_gemini_response_to_interactions_non_stream(
        "gemini-x",
        br#"{"responseId":"r1","candidates":[{"content":{"role":"model","parts":[{"text":"t","thought":true},{"text":"a"},{"inlineData":{"mimeType":"image/png","data":"AA=="}},{"functionCall":{"name":"f","args":{"x":1},"id":"c1"},"thoughtSignature":"s"},{"text":"","thoughtSignature":"s2"}]}}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":2,"totalTokenCount":3}}"#,
    );
    assert_eq!(
        out.to_string(),
        r#"{"id":"r1","object":"interaction","status":"completed","model":"gemini-x","steps":[{"type":"thought","content":[{"text":"t"}]},{"type":"model_output","content":[{"text":"a"}]},{"type":"model_output","content":[{"type":"image","mime_type":"image/png","data":"AA=="}]},{"type":"thought","signature":"s"},{"type":"function_call","name":"f","arguments":{"x":1},"call_id":"c1"},{"type":"thought","signature":"s2"}],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}"#
    );
}

// Not upstream's: error codes and statuses map to Gemini's.
#[test]
fn maps_error_codes() {
    for (code, want) in [
        ("400", (400, "INVALID_ARGUMENT")),
        (" UNAUTHENTICATED ", (401, "UNAUTHENTICATED")),
        ("not_found", (404, "NOT_FOUND")),
        ("rate_limit_exceeded", (429, "RESOURCE_EXHAUSTED")),
        ("cancelled", (499, "CANCELLED")),
        ("deadline_exceeded", (504, "DEADLINE_EXCEEDED")),
        ("internal", (500, "INTERNAL")),
        ("502", (502, "INTERNAL")),
        ("418", (418, "INVALID_ARGUMENT")),
        ("600", (500, "INTERNAL")),
        ("", (500, "INTERNAL")),
    ] {
        assert_eq!(map_interactions_error_to_gemini(code), want, "{code:?}");
    }
}

// Not upstream's: an event in SSE lines gives its `data:` payloads joined,
// and an empty event or `[DONE]` gives nothing.
#[test]
fn reads_sse_payloads() {
    assert_eq!(
        sse_payload(b"event: x\ndata: {\"a\":\ndata:1}\n\n").as_deref(),
        Some(&b"{\"a\":\n1}"[..])
    );
    assert_eq!(
        sse_payload(b" {\"a\":1}\n").as_deref(),
        Some(&b"{\"a\":1}"[..])
    );
    assert_eq!(sse_payload(b"data: [DONE]"), None);
    assert_eq!(sse_payload(b" \n"), None);
    assert_eq!(sse_payload(b"event: x\n"), None);
}

// Not upstream's: the first JSON value after white space is read; anything
// else reads as having no fields.
#[test]
fn reads_the_first_json_value() {
    assert_eq!(parse_root(b" \n{\"a\":1} trailing"), json!({ "a": 1 }));
    assert_eq!(parse_root(b"[1]"), json!([1]));
    assert_eq!(parse_root(b"data: {\"a\":1}"), Value::Null);
    assert_eq!(parse_root(b"\"text\""), Value::Null);
    assert_eq!(parse_root(b"{\"a\":"), Value::Null);
}

// Not upstream's: a function call's arguments keep each number as written
// both ways, as upstream copies them, whole or streamed, as JSON or as text,
// and an interaction id sent as the number -0 stays "-0", as gjson's
// String() gives it (checked with Go).
#[test]
fn numbers_keep_their_text() {
    let spelled = r#"{"x":-0,"y":1E20,"z":[1e5,0.10]}"#;
    let text = serde_json::to_string(spelled).unwrap();
    let call = format!(
        r#"{{"candidates":[{{"content":{{"role":"model","parts":[{{"functionCall":{{"name":"f","args":{spelled}}}}}]}},"finishReason":"STOP"}}]}}"#
    );
    let out = convert_gemini_response_to_interactions_non_stream(MODEL, call.as_bytes());
    assert_eq!(get(&out, "steps.0.arguments").unwrap().to_string(), spelled);
    let events = gemini_stream(&[&call]);
    let delta = format!(r#""delta":{{"arguments":{text},"type":"arguments_delta"}}"#);
    assert!(
        events.iter().any(|event| event.contains(&delta)),
        "{events:?}"
    );

    for arguments in [spelled, text.as_str()] {
        let body = format!(
            r#"{{"id":"i1","steps":[{{"type":"function_call","id":"c1","name":"f","arguments":{arguments}}}]}}"#
        );
        let out = convert_interactions_response_to_gemini_non_stream(MODEL, body.as_bytes());
        let args = get(&out, "candidates.0.content.parts.0.functionCall.args").unwrap();
        assert_eq!(args.to_string(), spelled);
    }

    let mut stream = InteractionsToGeminiStream::new(MODEL);
    let chunks: Vec<Value> = [
        r#"data: {"event_type":"interaction.created","interaction":{"id":-0,"model":"m"}}"#.to_owned(),
        r#"data: {"event_type":"step.start","index":0,"step":{"type":"function_call","id":"c1","name":"f"}}"#.to_owned(),
        format!(
            r#"data: {{"event_type":"step.delta","index":0,"delta":{{"type":"arguments_delta","arguments":{text}}}}}"#
        ),
    ]
    .iter()
    .filter_map(|chunk| stream.translate(chunk.as_bytes()))
    .collect();
    assert_eq!(chunks.len(), 1, "{chunks:?}");
    assert_eq!(chunks[0]["responseId"], "-0");
    let args = get(&chunks[0], "candidates.0.content.parts.0.functionCall.args").unwrap();
    assert_eq!(args.to_string(), spelled);
}

// Not upstream's: a function result given as JSON text keeps each number as
// written, as upstream's SetGeminiFunctionResponseRaw copies it.
#[test]
fn function_result_text_keeps_its_numbers() {
    let spelled = r#"{"x":-0,"y":1E20,"z":[1e5,0.10]}"#;
    let text = serde_json::to_string(spelled).unwrap();
    let body = format!(
        r#"{{"id":"i1","steps":[{{"type":"function_result","call_id":"c1","name":"f","result":{text}}}]}}"#
    );
    let out = convert_interactions_response_to_gemini_non_stream(MODEL, body.as_bytes());
    let result = get(
        &out,
        "candidates.0.content.parts.0.functionResponse.response",
    )
    .unwrap();
    assert_eq!(result.to_string(), spelled, "{out}");
}

// Pin the interaction and call IDs. Step IDs aren't exposed for text/thoughts.
// Retry across a clock-second boundary so even completion timestamps compare
// byte for byte, without masking or changing the translator's production clock.
#[test]
fn gemini_stream_bare_json_and_sse_are_byte_identical() {
    let chunks = [
        r#"{"candidates":[{"content":{"parts":[{"text":"hello"}]}}]}"#,
        r#"{"candidates":[{"content":{"parts":[{"thought":true,"text":"thinking"}]}}]}"#,
        r#"{"candidates":[{"content":{"parts":[{"functionCall":{"id":"call_fixed","name":"weather","args":{"city":"Chicago"}}}]},"finishReason":"STOP"}]}"#,
        r#"{"usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":4,"totalTokenCount":15}}"#,
        "[DONE]",
        "[DONE]",
    ];
    for _ in 0..10 {
        let before = unix_nanos().div_euclid(1_000_000_000);
        let mut bare = GeminiToInteractionsStream::new(MODEL);
        let mut sse = GeminiToInteractionsStream::new(MODEL);
        bare.id = "interaction_fixed".into();
        sse.id.clone_from(&bare.id);
        let pairs: Vec<_> = chunks
            .iter()
            .map(|chunk| {
                (
                    bare.translate(chunk.as_bytes()),
                    sse.translate(format!("data: {chunk}\r\n").as_bytes()),
                )
            })
            .collect();
        if before != unix_nanos().div_euclid(1_000_000_000) {
            continue;
        }
        for (index, (bare, sse)) in pairs.iter().enumerate() {
            assert_eq!(bare, sse, "chunk {index}");
        }
        assert!(pairs[3].0.concat().contains("\"total_tokens\":15"));
        assert!(pairs[3].0.concat().contains("data: [DONE]"));
        assert!(pairs[4].0.is_empty());
        assert!(pairs[5].0.is_empty());
        return;
    }
    panic!("clock crossed a second in every comparison");
}

#[test]
fn gemini_stream_ignores_sse_noise_and_accepts_data_done() {
    let mut stream = GeminiToInteractionsStream::new(MODEL);
    for line in ["", " \r\n", "event: message", "data: \t"] {
        assert!(stream.translate(line.as_bytes()).is_empty(), "{line:?}");
        assert!(!stream.started);
    }
    let events = stream.translate(b"data: [DONE]");
    assert_eq!(count(&events, "interaction.completed"), 1);
    assert_eq!(count(&events, "done"), 1);
    assert!(stream.translate(b"data: [DONE]").is_empty());
}

#[test]
fn stream_emits_done_after_terminal_usage() {
    let mut stream = GeminiToInteractionsStream::new(MODEL);
    let out = stream.translate(br#"{"candidates":[{"content":{"parts":[{"text":"ok"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2,"totalTokenCount":5}}"#);
    assert_eq!(
        event_types(&out),
        "interaction.created,interaction.status_update,step.start,step.delta,step.stop,interaction.completed,done"
    );
    assert_eq!(
        int(
            &find_event(&out, "interaction.completed"),
            "interaction.usage.total_tokens"
        ),
        5
    );
    assert!(stream.translate(b"[DONE]").is_empty());
    assert!(
        stream
            .translate(br#"{"candidates":[{"finishReason":"STOP"}]}"#)
            .is_empty()
    );
}

#[test]
fn stream_waits_for_post_finish_usage() {
    for key in ["usageMetadata", "usage_metadata", "cpaUsageMetadata"] {
        let mut stream = GeminiToInteractionsStream::new(MODEL);
        let early = stream.translate(br#"{"candidates":[{"content":{"parts":[{"text":"partial"}]}}],"usageMetadata":{"promptTokenCount":1,"totalTokenCount":1}}"#);
        let finish = stream.translate(br#"{"candidates":[{"finishReason":"STOP"}]}"#);
        for out in [&early, &finish] {
            assert_eq!(count(out, "interaction.completed"), 0);
            assert_eq!(count(out, "done"), 0);
        }
        assert_eq!(count(&finish, "step.stop"), 1);
        assert!(
            stream
                .translate(br#"{"candidates":[{"finishReason":"STOP"}]}"#)
                .is_empty()
        );
        let frame =
            json!({key:{"promptTokenCount":11,"candidatesTokenCount":22,"totalTokenCount":33}});
        let out = stream.translate(frame.to_string().as_bytes());
        assert_eq!(event_types(&out), "interaction.completed,done");
        assert_eq!(
            int(
                &find_event(&out, "interaction.completed"),
                "interaction.usage.total_tokens"
            ),
            33,
            "{key}"
        );
        assert!(stream.translate(frame.to_string().as_bytes()).is_empty());
        assert!(stream.translate(b"[DONE]").is_empty());
    }
}

#[test]
fn stream_split_finish_and_filtered_usage() {
    let mut stream = GeminiToInteractionsStream::new(MODEL);
    let finish = stream.translate(
        br#"{"candidates":[{"content":{"parts":[{"text":"ok"}]},"finishReason":"STOP"}]}"#,
    );
    assert_eq!(count(&finish, "interaction.completed"), 0);
    let out = stream.translate(br#"{"candidates":[],"cpaUsageMetadata":{"promptTokenCount":11,"candidatesTokenCount":22,"totalTokenCount":33}}"#);
    assert_eq!(event_types(&out), "interaction.completed,done");
    assert_eq!(
        int(
            &find_event(&out, "interaction.completed"),
            "interaction.usage.total_tokens"
        ),
        33
    );
}

#[test]
fn stream_done_falls_back_to_latest_usage() {
    for key in ["usageMetadata", "usage_metadata", "cpaUsageMetadata"] {
        for finish in [false, true] {
            let mut stream = GeminiToInteractionsStream::new(MODEL);
            for total in [5, 9] {
                let frame = json!({key:{"prompt_token_count":3,"candidates_token_count":2,"total_token_count":total,"thoughts_token_count":4,"cached_content_token_count":1}});
                let out = stream.translate(frame.to_string().as_bytes());
                assert_eq!(count(&out, "interaction.completed"), 0);
                assert_eq!(count(&out, "done"), 0);
            }
            // Traffic-only metadata must not erase a valid fallback.
            stream.translate(br#"{"usageMetadata":{"trafficType":"PROVISIONED_THROUGHPUT"}}"#);
            if finish {
                let out = stream.translate(br#"{"candidates":[{"finishReason":"STOP"}]}"#);
                assert_eq!(count(&out, "interaction.completed"), 0);
            }
            let out = stream.translate(b"[DONE]");
            assert_eq!(event_types(&out), "interaction.completed,done");
            let usage = find_event(&out, "interaction.completed")["interaction"]["usage"].clone();
            assert_eq!(usage["total_tokens"], 9, "{key}, finish {finish}");
            assert_eq!(usage["total_input_tokens"], 3);
            assert_eq!(usage["total_output_tokens"], 2);
            assert_eq!(usage["total_thought_tokens"], 4);
            assert_eq!(usage["total_cached_tokens"], 1);
            assert!(stream.translate(b"[DONE]").is_empty());
        }
    }
}

#[test]
fn stream_ignores_empty_finish_reason() {
    for reason in [json!(""), json!(" \t"), Value::Null] {
        let mut stream = GeminiToInteractionsStream::new(MODEL);
        let frame = json!({"candidates":[{"content":{"parts":[{"text":"ok"}]},"finishReason":reason}],"usageMetadata":{"totalTokenCount":5}});
        let out = stream.translate(frame.to_string().as_bytes());
        assert!(!stream.finished);
        assert_eq!(count(&out, "step.stop"), 0);
        assert_eq!(count(&out, "interaction.completed"), 0);
        assert_eq!(count(&out, "done"), 0);
    }
}

#[test]
fn stream_done_without_finish_or_usage() {
    let mut stream = GeminiToInteractionsStream::new(MODEL);
    stream.translate(br#"{"candidates":[{"content":{"parts":[{"text":"ok"}]}}]}"#);
    let out = stream.translate(b"[DONE]");
    assert_eq!(event_types(&out), "step.stop,interaction.completed,done");
    assert_eq!(
        find_event(&out, "interaction.completed")["interaction"]["usage"],
        json!({})
    );
    assert!(stream.translate(b"[DONE]").is_empty());
}

#[test]
fn filtered_usage_matches_native_usage_with_spelling_precedence() {
    let usage = json!({"promptTokenCount":3,"candidatesTokenCount":2,"totalTokenCount":5,"thoughtsTokenCount":4,"cachedContentTokenCount":1});
    let mut native = None;
    for key in ["usageMetadata", "usage_metadata", "cpaUsageMetadata"] {
        let frame = json!({"responseId":"fixed","candidates":[{"finishReason":"STOP"}],key:usage});
        let non_stream =
            convert_gemini_response_to_interactions_non_stream(MODEL, frame.to_string().as_bytes());
        assert_eq!(
            non_stream["usage"],
            json!({"input_tokens":3,"output_tokens":2,"reasoning_tokens":4,"total_tokens":5,"cached_tokens":1})
        );
        let out = gemini_stream(&[&frame.to_string()]);
        assert_eq!(
            event_types(&out),
            "interaction.created,interaction.status_update,interaction.completed,done"
        );
        let translated = find_event(&out, "interaction.completed")["interaction"]["usage"].clone();
        if let Some(native) = &native {
            assert_eq!(&translated, native);
        } else {
            native = Some(translated);
        }
    }
    for (frame, expected) in [
        (
            json!({"usageMetadata":{"totalTokenCount":7},"usage_metadata":{"total_token_count":8},"cpaUsageMetadata":{"totalTokenCount":9}}),
            7,
        ),
        (
            json!({"usage_metadata":{"total_token_count":8},"cpaUsageMetadata":{"totalTokenCount":9}}),
            8,
        ),
    ] {
        let out =
            convert_gemini_response_to_interactions_non_stream(MODEL, frame.to_string().as_bytes());
        assert_eq!(out["usage"]["total_tokens"], expected);
        let mut stream = GeminiToInteractionsStream::new(MODEL);
        stream.translate(frame.to_string().as_bytes());
        let out = stream.translate(b"[DONE]");
        assert_eq!(
            find_event(&out, "interaction.completed")["interaction"]["usage"]["total_tokens"],
            expected
        );
    }
}
