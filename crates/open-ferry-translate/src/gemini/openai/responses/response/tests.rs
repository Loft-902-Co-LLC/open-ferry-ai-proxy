// Ported from CLIProxyAPI internal/translator/gemini/openai/responses/gemini_openai-responses_response_test.go (v8.0.11, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The Gemini to Responses translator, streamed and whole: event order,
//! usage, tool call names, and thought signatures kept in reasoning items or
//! the replay cache so that they come back on the next request.
//!
//! Dropped or changed tests: none.
//!
//! The `parse_rfc3339` tests at the end are not ported from upstream.

mod apply_patch;
mod apply_patch_review;
mod issue6258_terminal;
mod noop_optimization;

use std::collections::HashMap;

use super::super::signature_carrier::{PREFIX, decode};
use super::super::test_support::{GEMINI_SIGNATURE, different_gemini_signature, sse_events};
use super::super::{array_of, convert_openai_responses_request_to_gemini};
use super::*;

/// The model Go's signature tests stream from and replay to.
const MODEL: &str = "gemini-3.6-flash-high";

fn parse(json: &str) -> Value {
    serde_json::from_str(json).expect("test JSON is valid")
}

/// A fixture with `{sig}` replaced by `testResponsesGeminiThoughtSignature`.
fn signed(template: &str) -> String {
    template.replace("{sig}", GEMINI_SIGNATURE)
}

/// [`signed`], with `{sig2}` replaced by `sig2` as well.
fn fill(template: &str, sig2: &str) -> String {
    signed(&template.replace("{sig2}", sig2))
}

/// The events one chunk gives.
fn translate(stream: &mut GeminiToOpenAIResponsesStream, line: &str) -> Vec<(String, Value)> {
    sse_events(&stream.translate_line(line.as_bytes()))
}

/// Runs `lines` through one stream for `original_request`, with no
/// translated request, and returns every event. It doesn't finalize the
/// stream, as Go's tests don't.
fn run_with<S: AsRef<str>>(
    model: &str,
    original_request: &Value,
    lines: &[S],
) -> Vec<(String, Value)> {
    let mut stream = GeminiToOpenAIResponsesStream::new(model, original_request, &Value::Null);
    lines
        .iter()
        .flat_map(|line| translate(&mut stream, line.as_ref()))
        .collect()
}

/// [`run_with`] for a stream Go's tests give no request.
fn run<S: AsRef<str>>(model: &str, lines: &[S]) -> Vec<(String, Value)> {
    run_with(model, &Value::Null, lines)
}

/// The data of the last event named `name`.
fn last_event<'e>(events: &'e [(String, Value)], name: &str) -> Option<&'e Value> {
    events
        .iter()
        .rev()
        .find(|(event, _)| event == name)
        .map(|(_, data)| data)
}

/// The `response.output` of the last `response.completed` event, or `Null`.
fn completed_output(events: &[(String, Value)]) -> Value {
    last_event(events, "response.completed")
        .and_then(|data| at(data, "response.output"))
        .cloned()
        .unwrap_or(Value::Null)
}

/// The `item.type` of each `response.output_item.done` event, joined with
/// commas.
fn done_types(events: &[(String, Value)]) -> String {
    events
        .iter()
        .filter(|(event, _)| event == "response.output_item.done")
        .map(|(_, data)| text(data, "item.type"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Each reasoning item's `encrypted_content` by item ID, as
/// `response.output_item.added` and `response.output_item.done` gave it.
fn reasoning_contents(
    events: &[(String, Value)],
) -> (HashMap<String, String>, HashMap<String, String>) {
    let mut added = HashMap::new();
    let mut done = HashMap::new();
    for (event, data) in events {
        if text(data, "item.type") != "reasoning" {
            continue;
        }
        let entry = (text(data, "item.id"), text(data, "item.encrypted_content"));
        match event.as_str() {
            "response.output_item.added" => {
                added.insert(entry.0, entry.1);
            }
            "response.output_item.done" => {
                done.insert(entry.0, entry.1);
            }
            _ => {}
        }
    }
    (added, done)
}

/// Fails if a reasoning item's `encrypted_content` changed between
/// `response.output_item.added` and `response.output_item.done`. As in Go, an
/// item missing from `done` reads as `""`.
fn assert_signatures_unchanged(added: &HashMap<String, String>, done: &HashMap<String, String>) {
    for (id, signature) in added {
        let done_signature = done.get(id).map_or("", String::as_str);
        assert_eq!(
            done_signature, signature,
            "reasoning item {id} changed signature from {signature:?} to {done_signature:?}"
        );
    }
}

/// A whole response converted with no request, as Go's tests pass nil.
fn non_stream_output(raw: &str) -> Value {
    convert_gemini_response_to_openai_responses_non_stream(
        &Value::Null,
        &Value::Null,
        raw.as_bytes(),
    )
    .expect("the response converts")
}

/// `decodedResponsesCarrierSignature`: the signature a carrier holds, or the
/// value itself if it isn't one.
fn carrier_signature(encrypted_content: &str) -> String {
    let decoded = decode(encrypted_content);
    assert!(
        !decoded.marked || decoded.ok,
        "invalid Responses carrier envelope: {encrypted_content:?}"
    );
    decoded.signature
}

/// The Gemini request for a Responses request that replays `input`, as Go's
/// tests build it with `sjson.SetRawBytes(request, "input", ...)`.
fn replay(input: &Value) -> Value {
    let request = json!({"model": MODEL, "input": input});
    convert_openai_responses_request_to_gemini(MODEL, &request, false)
}

/// [`replay`] with a `function_call_output` for `call_id` after `output`.
fn replay_with_function_output(output: &Value, call_id: &str) -> Value {
    let mut input = array_of(Some(output)).to_vec();
    input.push(json!({"type": "function_call_output", "call_id": call_id, "output": "ok"}));
    replay(&Value::Array(input))
}

/// gjson `Get(path).String()`.
fn text(value: &Value, path: &str) -> String {
    str_of(at(value, path)).into_owned()
}

/// gjson `Get(path).Exists()`.
fn exists(value: &Value, path: &str) -> bool {
    at(value, path).is_some()
}

/// gjson `Get(path).Bool()`.
fn flag(value: &Value, path: &str) -> bool {
    at(value, path).is_some_and(bool_of)
}

/// gjson `Get(path).Int()`.
fn int_at(value: &Value, path: &str) -> i64 {
    at(value, path).map_or(0, int_of)
}

/// gjson `Get(path).Array()`.
fn list<'v>(value: &'v Value, path: &str) -> &'v [Value] {
    array_of(at(value, path))
}

/// gjson `Array()` of the value itself.
fn elements(value: &Value) -> &[Value] {
    array_of(Some(value))
}

/// Whether `value` is valid JSON text, as `gjson.Valid` checks.
fn valid_json(value: &str) -> bool {
    serde_json::from_str::<Value>(value).is_ok()
}

/// The visible text parts of `contents.0`: not thoughts, with text.
fn visible_parts(translated: &Value) -> Vec<&Value> {
    list(translated, "contents.0.parts")
        .iter()
        .filter(|part| !flag(part, "thought") && !text(part, "text").is_empty())
        .collect()
}

/// Each non-empty `thoughtSignature` in the parts of `contents.0`, and the
/// one on the part whose text is `visible`.
fn first_content_signatures(translated: &Value, visible: &str) -> (Vec<String>, String) {
    let mut signatures = Vec::new();
    let mut visible_signature = String::new();
    for part in list(translated, "contents.0.parts") {
        let signature = text(part, "thoughtSignature");
        if signature.is_empty() {
            continue;
        }
        if text(part, "text") == visible {
            visible_signature.clone_from(&signature);
        }
        signatures.push(signature);
    }
    (signatures, visible_signature)
}

#[test]
fn convert_gemini_response_to_openai_responses_output_tokens_include_thoughts() {
    let chunk = r#"data: {"candidates":[{"content":{"role":"model","parts":[{"text":""}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":16,"candidatesTokenCount":5,"thoughtsTokenCount":42,"totalTokenCount":63},"modelVersion":"gemini-3.6-flash","responseId":"resp_usage"}"#;

    let events = run("model", &[chunk]);
    let completed =
        last_event(&events, "response.completed").expect("missing response.completed event");
    let output_tokens = at(completed, "response.usage.output_tokens");
    assert!(
        output_tokens.is_some_and(|tokens| int_of(tokens) == 47),
        "output_tokens = {output_tokens:?}, want present with value 47. Output: {completed}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_non_stream_output_tokens_include_thoughts() {
    let response = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":""}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":16,"thoughtsTokenCount":42,"totalTokenCount":58},"modelVersion":"gemini-3.6-flash","responseId":"resp_usage"}"#;

    let result = non_stream_output(response);
    let output_tokens = at(&result, "usage.output_tokens");
    assert!(
        output_tokens.is_some_and(|tokens| int_of(tokens) == 42),
        "output_tokens = {output_tokens:?}, want present with value 42. Output: {result}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_unwrap_and_aggregate_text() {
    // Vertex-style Gemini stream wraps the actual response payload under "response".
    // This test ensures we unwrap and that output_text.done contains the full text.
    let lines = [
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":""}]}}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2,"cachedContentTokenCount":0},"modelVersion":"test-model","responseId":"req_vrtx_1"},"traceId":"t1"}"#,
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"让"}]}}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2,"cachedContentTokenCount":0},"modelVersion":"test-model","responseId":"req_vrtx_1"},"traceId":"t1"}"#,
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"我先"}]}}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2,"cachedContentTokenCount":0},"modelVersion":"test-model","responseId":"req_vrtx_1"},"traceId":"t1"}"#,
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"了解"}]}}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2,"cachedContentTokenCount":0},"modelVersion":"test-model","responseId":"req_vrtx_1"},"traceId":"t1"}"#,
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"mcp__serena__list_dir","args":{"recursive":false,"relative_path":"internal"},"id":"toolu_1"}}]}}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2,"cachedContentTokenCount":0},"modelVersion":"test-model","responseId":"req_vrtx_1"},"traceId":"t1"}"#,
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":""}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15,"cachedContentTokenCount":2},"modelVersion":"test-model","responseId":"req_vrtx_1"},"traceId":"t1"}"#,
    ];
    let original_request =
        parse(r#"{"instructions":"test instructions","model":"gpt-5","max_output_tokens":123}"#);

    let events = run_with("test-model", &original_request, &lines);

    let mut got_text_done = false;
    let mut got_message_done = false;
    let mut got_response_done = false;
    let mut got_func_done = false;
    let mut text_done = String::new();
    let mut message_text = String::new();
    let mut response_id = String::new();
    let mut created_model = String::new();
    let mut in_progress_model = String::new();
    let mut instructions = String::new();
    let mut cached_tokens = 0;
    let mut func_name = String::new();
    let mut func_args = String::new();
    let mut pos_text_done = None;
    let mut pos_part_done = None;
    let mut pos_message_done = None;
    let mut pos_func_added = None;

    for (i, (event, data)) in events.iter().enumerate() {
        match event.as_str() {
            "response.output_text.done" => {
                got_text_done = true;
                pos_text_done.get_or_insert(i);
                text_done = text(data, "text");
            }
            "response.content_part.done" => {
                pos_part_done.get_or_insert(i);
            }
            "response.output_item.done" => match text(data, "item.type").as_str() {
                "message" => {
                    got_message_done = true;
                    pos_message_done.get_or_insert(i);
                    message_text = text(data, "item.content.0.text");
                }
                "function_call" => {
                    got_func_done = true;
                    func_name = text(data, "item.name");
                    func_args = text(data, "item.arguments");
                }
                _ => {}
            },
            "response.output_item.added" => {
                if text(data, "item.type") == "function_call" {
                    pos_func_added.get_or_insert(i);
                }
            }
            "response.created" => created_model = text(data, "response.model"),
            "response.in_progress" => in_progress_model = text(data, "response.model"),
            "response.completed" => {
                got_response_done = true;
                response_id = text(data, "response.id");
                instructions = text(data, "response.instructions");
                cached_tokens = int_at(data, "response.usage.input_tokens_details.cached_tokens");
            }
            _ => {}
        }
    }

    assert!(got_text_done, "missing response.output_text.done event");
    let (Some(pos_text_done), Some(pos_part_done), Some(pos_message_done), Some(pos_func_added)) = (
        pos_text_done,
        pos_part_done,
        pos_message_done,
        pos_func_added,
    ) else {
        panic!(
            "missing ordering events: textDone={pos_text_done:?} partDone={pos_part_done:?} messageDone={pos_message_done:?} funcAdded={pos_func_added:?}"
        );
    };
    assert!(
        pos_text_done < pos_part_done
            && pos_part_done < pos_message_done
            && pos_message_done < pos_func_added,
        "unexpected message/function ordering: textDone={pos_text_done} partDone={pos_part_done} messageDone={pos_message_done} funcAdded={pos_func_added}"
    );
    assert!(
        got_message_done,
        "missing message response.output_item.done event"
    );
    assert!(
        got_func_done,
        "missing function_call response.output_item.done event"
    );
    assert!(got_response_done, "missing response.completed event");

    assert_eq!(text_done, "让我先了解", "unexpected output_text.done text");
    assert_eq!(message_text, "让我先了解", "unexpected message done text");

    assert_eq!(response_id, "resp_req_vrtx_1", "unexpected response id");
    assert_eq!(created_model, "gpt-5", "response.created models");
    assert_eq!(in_progress_model, "gpt-5", "response.in_progress models");
    assert_eq!(
        instructions, "test instructions",
        "unexpected instructions echo"
    );
    assert_eq!(cached_tokens, 2, "unexpected cached token count");

    assert_eq!(
        func_name, "mcp__serena__list_dir",
        "unexpected function name"
    );
    let args: Value = serde_json::from_str(&func_args)
        .unwrap_or_else(|_| panic!("invalid function arguments JSON: {func_args:?}"));
    assert!(
        !flag(&args, "recursive"),
        "unexpected recursive arg: {:?}",
        args.get("recursive")
    );
    assert_eq!(
        text(&args, "relative_path"),
        "internal",
        "unexpected relative_path arg"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_consecutive_signed_visible_text_preserves_every_signature()
 {
    let signature2 = different_gemini_signature();
    let lines = [
        r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"a"}]}}],"modelVersion":"gemini-3.6-flash","responseId":"signed-text"}}"#.to_owned(),
        signed(r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"b","thoughtSignature":"{sig}"}]}}],"modelVersion":"gemini-3.6-flash","responseId":"signed-text"}}"#),
        fill(r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"c","thoughtSignature":"{sig2}"}]},"finishReason":"STOP"}],"modelVersion":"gemini-3.6-flash","responseId":"signed-text"}}"#, &signature2),
        "data: [DONE]".to_owned(),
    ];
    let events = run(MODEL, &lines);
    let (added, done) = reasoning_contents(&events);
    assert!(
        added.is_empty() && done.is_empty(),
        "text signatures emitted reasoning items: added={} done={}",
        added.len(),
        done.len()
    );

    let translated = replay(&completed_output(&events));
    let visible = visible_parts(&translated);
    assert!(
        visible.len() == 2
            && text(visible[0], "text") == "ab"
            && text(visible[0], "thoughtSignature") == GEMINI_SIGNATURE
            && text(visible[1], "text") == "c"
            && text(visible[1], "thoughtSignature") == signature2,
        "signed visible text did not round-trip by segment: {translated}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_non_stream_consecutive_signed_visible_text_preserves_every_signature()
 {
    let signature2 = different_gemini_signature();
    let raw = fill(
        r#"{"candidates":[{"content":{"parts":[{"text":"a"},{"text":"b","thoughtSignature":"{sig}"},{"text":"c","thoughtSignature":"{sig2}"}]},"finishReason":"STOP"}],"responseId":"signed-text-nonstream"}"#,
        &signature2,
    );
    let out = non_stream_output(&raw);
    let output = at(&out, "output").cloned().unwrap_or(Value::Null);
    assert!(
        carrier_signature(&text(&output, "0.encrypted_content")) == GEMINI_SIGNATURE
            && text(&output, "1.content.0.text") == "ab"
            && carrier_signature(&text(&output, "2.encrypted_content")) == signature2
            && text(&output, "3.content.0.text") == "c",
        "non-stream signed visible text was not segmented: {out}"
    );

    let mut output_without_ids = output.clone();
    for index in [0, 2] {
        if let Some(item) = output_without_ids
            .get_mut(index)
            .and_then(Value::as_object_mut)
        {
            item.shift_remove("id");
        }
    }
    let translated = replay(&output_without_ids);
    let visible = visible_parts(&translated);
    assert!(
        visible.len() == 2
            && text(visible[0], "thoughtSignature") == GEMINI_SIGNATURE
            && text(visible[1], "thoughtSignature") == signature2,
        "non-stream signatures did not round-trip after client stripped reasoning IDs: {translated}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_signed_visible_then_unsigned_preserves_boundary() {
    let lines = [
        signed(r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"signed","thoughtSignature":"{sig}"}]}}],"responseId":"signed-then-unsigned"}}"#),
        r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"unsigned"}]},"finishReason":"STOP"}],"responseId":"signed-then-unsigned"}}"#.to_owned(),
        "data: [DONE]".to_owned(),
    ];
    let completed = completed_output(&run(MODEL, &lines));
    let translated = replay(&completed);
    let parts = list(&translated, "contents.0.parts");
    assert!(
        parts.len() == 2
            && text(&parts[0], "text") == "signed"
            && text(&parts[0], "thoughtSignature") == GEMINI_SIGNATURE
            && text(&parts[1], "text") == "unsigned"
            && text(&parts[1], "thoughtSignature").is_empty(),
        "signed/unsigned visible boundary changed: output={completed} translated={translated}"
    );
    assert!(
        !completed.to_string().contains(PREFIX) && !translated.to_string().contains(PREFIX),
        "Cached text carrier must not appear on either wire: output={completed} translated={translated}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_leading_carrier_does_not_cross_signed_thought() {
    let signature2 = different_gemini_signature();
    let lines = [
        signed(r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"","thoughtSignature":"{sig}"}]}}],"responseId":"leading-before-signed-thought"}}"#),
        fill(r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"reason","thought":true,"thoughtSignature":"{sig2}"}]}}],"responseId":"leading-before-signed-thought"}}"#, &signature2),
        r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"answer"}]},"finishReason":"STOP"}],"responseId":"leading-before-signed-thought"}}"#.to_owned(),
        "data: [DONE]".to_owned(),
    ];
    let stream_output = completed_output(&run(MODEL, &lines));
    let raw = fill(
        r#"{"candidates":[{"content":{"parts":[{"text":"","thoughtSignature":"{sig}"},{"text":"reason","thought":true,"thoughtSignature":"{sig2}"},{"text":"answer"}]},"finishReason":"STOP"}],"responseId":"leading-before-signed-thought-nonstream"}"#,
        &signature2,
    );
    let non_stream = non_stream_output(&raw);
    let non_stream_output = at(&non_stream, "output").cloned().unwrap_or(Value::Null);

    for (name, output) in [
        ("stream", &stream_output),
        ("non-stream", &non_stream_output),
    ] {
        let translated = replay(output);
        let parts = list(&translated, "contents.0.parts");
        assert!(
            parts.len() == 3
                && exists(&parts[0], "text")
                && text(&parts[0], "text").is_empty()
                && text(&parts[0], "thoughtSignature") == GEMINI_SIGNATURE
                && text(&parts[1], "text") == "reason"
                && flag(&parts[1], "thought")
                && text(&parts[1], "thoughtSignature") == signature2
                && text(&parts[2], "text") == "answer"
                && text(&parts[2], "thoughtSignature").is_empty(),
            "{name} leading carrier crossed signed thought: output={output} translated={translated}"
        );
    }
}

#[test]
fn convert_gemini_response_to_openai_responses_non_stream_signed_visible_then_unsigned_preserves_boundary()
 {
    let raw = signed(
        r#"{"candidates":[{"content":{"parts":[{"text":"signed","thoughtSignature":"{sig}"},{"text":"unsigned"}]},"finishReason":"STOP"}],"responseId":"signed-then-unsigned-nonstream"}"#,
    );
    let out = non_stream_output(&raw);
    let output = at(&out, "output").cloned().unwrap_or(Value::Null);
    let translated = replay(&output);
    let parts = list(&translated, "contents.0.parts");
    assert!(
        parts.len() == 2
            && text(&parts[0], "text") == "signed"
            && text(&parts[0], "thoughtSignature") == GEMINI_SIGNATURE
            && text(&parts[1], "text") == "unsigned"
            && text(&parts[1], "thoughtSignature").is_empty(),
        "non-stream signed/unsigned visible boundary changed: output={output} translated={translated}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_non_stream_trailing_carrier_direction_does_not_depend_on_id()
 {
    let signature2 = different_gemini_signature();
    let raw = fill(
        r#"{"candidates":[{"content":{"parts":[{"text":"answer","thoughtSignature":"{sig}"},{"text":"","thoughtSignature":"{sig2}"}]},"finishReason":"STOP"}],"responseId":"trailing-direction-nonstream"}"#,
        &signature2,
    );
    let out = non_stream_output(&raw);
    let output = at(&out, "output").cloned().unwrap_or(Value::Null);
    let translated = replay(&output);
    let parts = list(&translated, "contents.0.parts");
    assert!(
        parts.len() == 2
            && text(&parts[0], "text") == "answer"
            && text(&parts[0], "thoughtSignature") == GEMINI_SIGNATURE
            && exists(&parts[1], "text")
            && text(&parts[1], "text").is_empty()
            && text(&parts[1], "thoughtSignature") == signature2,
        "non-stream trailing carrier changed direction: output={output} translated={translated}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_cached_trailing_carrier_preserves_direction() {
    let signature2 = different_gemini_signature();
    let lines = [
        signed(
            r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"answer","thoughtSignature":"{sig}"}]}}],"responseId":"trailing-direction-stream"}}"#,
        ),
        fill(
            r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"","thoughtSignature":"{sig2}"}]},"finishReason":"STOP"}],"responseId":"trailing-direction-stream"}}"#,
            &signature2,
        ),
        "data: [DONE]".to_owned(),
    ];
    let completed = completed_output(&run(MODEL, &lines));
    let translated = replay(&completed);
    let parts = list(&translated, "contents.0.parts");
    assert!(
        parts.len() == 2
            && text(&parts[0], "text") == "answer"
            && text(&parts[0], "thoughtSignature") == GEMINI_SIGNATURE
            && exists(&parts[1], "text")
            && text(&parts[1], "text").is_empty()
            && text(&parts[1], "thoughtSignature") == signature2,
        "Cached trailing carrier changed direction: output={completed} translated={translated}"
    );
    assert!(
        !completed.to_string().contains(PREFIX) && !translated.to_string().contains(PREFIX),
        "Cached Responses carrier leaked across protocol boundary: output={completed} translated={translated}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_visible_signature_does_not_overwrite_signed_thought()
{
    let signature2 = different_gemini_signature();
    let lines = [
        signed(
            r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"one","thought":true,"thoughtSignature":"{sig}"}]}}],"responseId":"signed-thought-visible"}}"#,
        ),
        fill(
            r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"answer","thoughtSignature":"{sig2}"}]},"finishReason":"STOP"}],"responseId":"signed-thought-visible"}}"#,
            &signature2,
        ),
        "data: [DONE]".to_owned(),
    ];
    let events = run(MODEL, &lines);
    let (added, done) = reasoning_contents(&events);
    assert_signatures_unchanged(&added, &done);
    let completed = completed_output(&events);
    assert!(
        elements(&completed).len() == 2
            && carrier_signature(&text(&completed, "0.encrypted_content")) == GEMINI_SIGNATURE
            && text(&completed, "1.type") == "message",
        "Visible signature changed the reasoning timeline: {completed}"
    );
    let translated = replay(&completed);
    let (signatures, visible_signature) = first_content_signatures(&translated, "answer");
    assert!(
        signatures.len() == 2 && visible_signature == signature2,
        "thought/visible signatures did not round-trip: signatures={signatures:?} visible={visible_signature:?} translated={translated}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_flushes_visible_signature_before_later_thought() {
    let signature2 = different_gemini_signature();
    const SIGNATURE3: &str = "third-distinct-gemini-signature-123456";
    let lines = [
        signed(r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"thought-a","thought":true,"thoughtSignature":"{sig}"}]}}],"responseId":"visible-before-thought"}}"#),
        fill(r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"answer","thoughtSignature":"{sig2}"}]}}],"responseId":"visible-before-thought"}}"#, &signature2),
        r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"thought-c","thought":true,"thoughtSignature":"{sig3}"}]},"finishReason":"STOP"}],"responseId":"visible-before-thought"}}"#.replace("{sig3}", SIGNATURE3),
        "data: [DONE]".to_owned(),
    ];
    let completed = completed_output(&run(MODEL, &lines));
    assert!(
        elements(&completed).len() == 3
            && carrier_signature(&text(&completed, "0.encrypted_content")) == GEMINI_SIGNATURE
            && text(&completed, "1.type") == "message"
            && carrier_signature(&text(&completed, "2.encrypted_content")) == SIGNATURE3,
        "visible signature crossed later thought: {completed}"
    );
    let translated =
        convert_openai_responses_request_to_gemini(MODEL, &json!({"input": completed}), false);
    assert!(
        text(&translated, "contents.0.parts.1.text") == "answer"
            && text(&translated, "contents.0.parts.1.thoughtSignature") == signature2
            && text(&translated, "contents.0.parts.2.text") == "thought-c",
        "cached visible signature crossed later thought on replay: {translated}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_function_and_trailing_signatures_round_trip() {
    let signature2 = different_gemini_signature();
    let lines = [
        signed(
            r#"data: {"response":{"candidates":[{"content":{"parts":[{"thoughtSignature":"{sig}","functionCall":{"name":"run_command","args":{"command":"true"}}}]}}],"responseId":"function-trailing"}}"#,
        ),
        fill(
            r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"","thoughtSignature":"{sig2}"}]},"finishReason":"STOP"}],"responseId":"function-trailing"}}"#,
            &signature2,
        ),
        "data: [DONE]".to_owned(),
    ];
    let completed = completed_output(&run(MODEL, &lines));
    let translated = replay(&completed);
    let signatures: Vec<String> = list(&translated, "contents")
        .iter()
        .flat_map(|content| list(content, "parts"))
        .map(|part| text(part, "thoughtSignature"))
        .filter(|signature| !signature.is_empty())
        .collect();
    assert!(
        signatures.len() == 2 && signatures[0] == GEMINI_SIGNATURE && signatures[1] == signature2,
        "function/trailing signatures = {signatures:?}; completed={completed} translated={translated}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_non_stream_function_and_trailing_signatures_preserve_order()
 {
    let signature2 = different_gemini_signature();
    let raw = fill(
        r#"{"candidates":[{"content":{"parts":[{"thoughtSignature":"{sig}","functionCall":{"name":"run_command","args":{"command":"true"}}},{"text":"","thoughtSignature":"{sig2}"}]},"finishReason":"STOP"}],"responseId":"function-trailing-nonstream"}"#,
        &signature2,
    );
    let out = non_stream_output(&raw);
    assert!(
        carrier_signature(&text(&out, "output.0.encrypted_content")) == GEMINI_SIGNATURE
            && text(&out, "output.1.type") == "function_call"
            && carrier_signature(&text(&out, "output.2.encrypted_content")) == signature2,
        "non-stream function/trailing order malformed: {out}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_function_then_trailing_signature_has_stream_parity()
{
    let raw = signed(
        r#"{"candidates":[{"content":{"parts":[{"text":"preamble"},{"functionCall":{"name":"run_command","args":{"command":"true"}}},{"text":"","thoughtSignature":"{sig}"}]},"finishReason":"STOP"}],"responseId":"function-trailing-parity"}"#,
    );

    let stream_output =
        completed_output(&run(MODEL, &[format!("data: {raw}"), "[DONE]".to_owned()]));
    let non_stream = non_stream_output(&raw);
    let non_stream_output = at(&non_stream, "output").cloned().unwrap_or(Value::Null);
    for (name, output) in [
        ("stream", &stream_output),
        ("non-stream", &non_stream_output),
    ] {
        let items = elements(output);
        assert!(
            items.len() == 3
                && text(&items[0], "type") == "message"
                && text(&items[1], "type") == "function_call"
                && text(&items[2], "type") == "reasoning",
            "{name} function/trailing order malformed: {output}"
        );
        let carrier = decode(&text(&items[2], "encrypted_content"));
        assert!(
            carrier.marked
                && carrier.ok
                && carrier.signature == GEMINI_SIGNATURE
                && carrier.direction == PREVIOUS
                && carrier.target == FUNCTION,
            "{name} function/trailing carrier malformed: {output}"
        );
        let translated = replay(output);
        let parts = list(&translated, "contents.0.parts");
        assert!(
            parts.len() == 2
                && text(&parts[0], "text") == "preamble"
                && text(&parts[1], "functionCall.name") == "run_command"
                && text(&parts[1], "thoughtSignature") == GEMINI_SIGNATURE,
            "{name} trailing function signature did not replay: {translated}"
        );
    }
}

#[test]
fn convert_gemini_response_to_openai_responses_non_stream_trailing_signature_follows_pending_reasoning()
 {
    let signature2 = different_gemini_signature();
    let raw = fill(
        r#"{"candidates":[{"content":{"parts":[{"text":"thought","thought":true,"thoughtSignature":"{sig}"},{"text":"","thoughtSignature":"{sig2}"}]},"finishReason":"STOP"}],"responseId":"reasoning-trailing-nonstream"}"#,
        &signature2,
    );
    let out = non_stream_output(&raw);
    assert!(
        carrier_signature(&text(&out, "output.0.encrypted_content")) == GEMINI_SIGNATURE
            && carrier_signature(&text(&out, "output.1.encrypted_content")) == signature2,
        "non-stream reasoning/trailing order malformed: {out}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_non_stream_unsigned_thought_does_not_steal_function_signature()
 {
    let raw = signed(
        r#"{"candidates":[{"content":{"parts":[{"thoughtSignature":"{sig}","functionCall":{"name":"run_command","args":{"command":"true"}}},{"text":"later thought","thought":true}]},"finishReason":"STOP"}],"responseId":"function-unsigned-thought"}"#,
    );
    let out = non_stream_output(&raw);
    assert!(
        carrier_signature(&text(&out, "output.0.encrypted_content")) == GEMINI_SIGNATURE
            && text(&out, "output.1.type") == "function_call"
            && text(&out, "output.2.summary.0.text") == "later thought"
            && text(&out, "output.2.encrypted_content").is_empty(),
        "unsigned thought stole function signature: {out}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_interleaved_thought_and_text_preserves_order() {
    let signature2 = different_gemini_signature();
    let line = fill(
        r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"thought-a","thought":true,"thoughtSignature":"{sig}"},{"text":"answer-a"},{"text":"thought-b","thought":true,"thoughtSignature":"{sig2}"},{"text":"answer-b"}]},"finishReason":"STOP"}],"responseId":"interleaved"}}"#,
        &signature2,
    );
    let events = run(MODEL, &[line, "[DONE]".to_owned()]);
    let got = done_types(&events);
    assert_eq!(
        got, "reasoning,message,reasoning,message",
        "interleaved done order"
    );
    let completed = completed_output(&events);
    assert!(
        text(&completed, "0.summary.0.text") == "thought-a"
            && text(&completed, "1.content.0.text") == "answer-a"
            && text(&completed, "2.summary.0.text") == "thought-b"
            && text(&completed, "3.content.0.text") == "answer-b",
        "interleaved completed output malformed: {completed}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_non_stream_interleaved_thought_and_text_preserves_order()
 {
    let signature2 = different_gemini_signature();
    let raw = fill(
        r#"{"candidates":[{"content":{"parts":[{"text":"thought-a","thought":true,"thoughtSignature":"{sig}"},{"text":"answer-a"},{"text":"thought-b","thought":true,"thoughtSignature":"{sig2}"},{"text":"answer-b"}]},"finishReason":"STOP"}],"responseId":"interleaved-nonstream"}"#,
        &signature2,
    );
    let out = non_stream_output(&raw);
    let got = list(&out, "output").len();
    assert_eq!(got, 4, "interleaved non-stream output count; output={out}");
    assert!(
        text(&out, "output.0.type") == "reasoning"
            && text(&out, "output.1.type") == "message"
            && text(&out, "output.2.type") == "reasoning"
            && text(&out, "output.3.type") == "message",
        "interleaved non-stream order malformed: {out}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_leading_empty_and_signed_text_round_trip_in_order() {
    let signature2 = different_gemini_signature();
    let lines = [
        signed(
            r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"","thoughtSignature":"{sig}"}]}}],"responseId":"leading-empty-signed-text"}}"#,
        ),
        fill(
            r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"answer","thoughtSignature":"{sig2}"}]},"finishReason":"STOP"}],"responseId":"leading-empty-signed-text"}}"#,
            &signature2,
        ),
        "data: [DONE]".to_owned(),
    ];
    let translated = replay(&completed_output(&run(MODEL, &lines)));
    let (signatures, visible_signature) = first_content_signatures(&translated, "answer");
    assert!(
        signatures.len() == 2
            && signatures[0] == GEMINI_SIGNATURE
            && visible_signature == signature2,
        "leading empty/signed text signatures={signatures:?} visible={visible_signature:?} translated={translated}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_signed_text_and_trailing_signature_round_trip_in_order()
 {
    let signature2 = different_gemini_signature();
    let lines = [
        signed(
            r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"answer","thoughtSignature":"{sig}"}]}}],"responseId":"signed-text-trailing"}}"#,
        ),
        fill(
            r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"","thoughtSignature":"{sig2}"}]},"finishReason":"STOP"}],"responseId":"signed-text-trailing"}}"#,
            &signature2,
        ),
        "data: [DONE]".to_owned(),
    ];
    let completed = completed_output(&run(MODEL, &lines));
    assert!(
        elements(&completed).len() == 1 && text(&completed, "0.type") == "message",
        "signed text/trailing completed order malformed: {completed}"
    );
    let translated = replay(&completed);
    let (signatures, _) = first_content_signatures(&translated, "");
    assert!(
        signatures.len() == 2 && signatures[0] == GEMINI_SIGNATURE && signatures[1] == signature2,
        "signed text/trailing signatures = {signatures:?}; translated={translated}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_preserves_multiple_leading_empty_signatures() {
    let signature2 = different_gemini_signature();
    let line = fill(
        r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"","thoughtSignature":"{sig}"},{"text":"","thoughtSignature":"{sig2}"}]},"finishReason":"STOP"}],"responseId":"leading-empty-signatures"}}"#,
        &signature2,
    );
    let completed = completed_output(&run(MODEL, &[line, "[DONE]".to_owned()]));
    assert!(
        carrier_signature(&text(&completed, "0.encrypted_content")) == GEMINI_SIGNATURE
            && carrier_signature(&text(&completed, "1.encrypted_content")) == signature2,
        "leading empty signatures were not preserved: {completed}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_non_stream_signed_text_and_trailing_signature_round_trip_in_order()
 {
    let signature2 = different_gemini_signature();
    let raw = fill(
        r#"{"candidates":[{"content":{"parts":[{"text":"answer","thoughtSignature":"{sig}"},{"text":"","thoughtSignature":"{sig2}"}]},"finishReason":"STOP"}],"responseId":"signed-text-trailing-nonstream"}"#,
        &signature2,
    );
    let out = non_stream_output(&raw);
    assert!(
        carrier_signature(&text(&out, "output.0.encrypted_content")) == GEMINI_SIGNATURE
            && text(&out, "output.1.type") == "message"
            && carrier_signature(&text(&out, "output.2.encrypted_content")) == signature2,
        "non-stream signed text/trailing order malformed: {out}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_distinct_signed_thoughts_use_distinct_items() {
    let signature2 = different_gemini_signature();
    let lines = [
        signed(
            r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"one","thought":true,"thoughtSignature":"{sig}"}]}}],"modelVersion":"gemini-3.6-flash","responseId":"signed-thoughts"}}"#,
        ),
        fill(
            r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"two","thought":true,"thoughtSignature":"{sig2}"}]},"finishReason":"STOP"}],"modelVersion":"gemini-3.6-flash","responseId":"signed-thoughts"}}"#,
            &signature2,
        ),
        "data: [DONE]".to_owned(),
    ];
    let events = run(MODEL, &lines);
    let (added, done) = reasoning_contents(&events);
    assert!(
        added.len() == 2 && done.len() == 2,
        "reasoning items added/done = {}/{}, want 2/2",
        added.len(),
        done.len()
    );
    assert_signatures_unchanged(&added, &done);
    let completed = completed_output(&events);
    let got = carrier_signature(&text(&completed, "0.encrypted_content"));
    assert_eq!(got, GEMINI_SIGNATURE, "first completed signature");
    let got = carrier_signature(&text(&completed, "1.encrypted_content"));
    assert_eq!(got, signature2, "second completed signature");
}

#[test]
fn convert_gemini_response_to_openai_responses_non_stream_distinct_signed_thoughts_use_distinct_items()
 {
    let signature2 = different_gemini_signature();
    let raw = fill(
        r#"{"candidates":[{"content":{"parts":[{"text":"one","thought":true,"thoughtSignature":"{sig}"},{"text":"two","thought":true,"thoughtSignature":"{sig2}"}]},"finishReason":"STOP"}],"responseId":"signed-thoughts-nonstream"}"#,
        &signature2,
    );
    let out = non_stream_output(&raw);
    let got = list(&out, "output").len();
    assert_eq!(got, 2, "reasoning output count; output={out}");
    let got = carrier_signature(&text(&out, "output.0.encrypted_content"));
    assert_eq!(got, GEMINI_SIGNATURE, "first signature; output={out}");
    let got = carrier_signature(&text(&out, "output.1.encrypted_content"));
    assert_eq!(got, signature2, "second signature; output={out}");
}

#[test]
fn convert_gemini_response_to_openai_responses_visible_signature_completes_active_reasoning() {
    let lines = [
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"hidden thought","thought":true}]}}],"modelVersion":"gemini-3.6-flash","responseId":"resp_active_reasoning"}}"#.to_owned(),
        signed(r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"visible answer","thoughtSignature":"{sig}"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":2,"thoughtsTokenCount":3,"totalTokenCount":15},"modelVersion":"gemini-3.6-flash","responseId":"resp_active_reasoning"}}"#),
    ];
    let events = run(MODEL, &lines);
    let mut added_id = String::new();
    let mut added_signature = String::new();
    let mut done_id = String::new();
    let mut done_signature = String::new();
    for (event, data) in &events {
        if event == "response.output_item.added" && text(data, "item.type") == "reasoning" {
            added_id = text(data, "item.id");
            added_signature = text(data, "item.encrypted_content");
        }
        if event == "response.output_item.done" && text(data, "item.type") == "reasoning" {
            done_id = text(data, "item.id");
            done_signature = text(data, "item.encrypted_content");
        }
    }
    let got = done_types(&events);
    assert_eq!(got, "reasoning,message", "done item order");
    assert!(
        !added_id.is_empty()
            && added_id == done_id
            && carrier_signature(&added_signature) == GEMINI_SIGNATURE
            && done_signature == added_signature,
        "reasoning item changed between added and done: added=({added_id:?},{added_signature:?}) done=({done_id:?},{done_signature:?})"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_late_thought_signature_is_immutable() {
    let signature = different_gemini_signature();
    let lines = [
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"one","thought":true}]}}],"responseId":"late-thought-signature"}}"#.to_owned(),
        fill(r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"two","thought":true,"thoughtSignature":"{sig2}"}]},"finishReason":"STOP"}],"responseId":"late-thought-signature"}}"#, &signature),
        "data: [DONE]".to_owned(),
    ];
    let mut added_id = String::new();
    let mut added_signature = String::new();
    let mut done_id = String::new();
    let mut done_signature = String::new();
    let mut done_text = String::new();
    for (event, data) in run(MODEL, &lines) {
        if text(&data, "item.type") != "reasoning" {
            continue;
        }
        match event.as_str() {
            "response.output_item.added" => {
                added_id = text(&data, "item.id");
                added_signature = text(&data, "item.encrypted_content");
            }
            "response.output_item.done" => {
                done_id = text(&data, "item.id");
                done_signature = text(&data, "item.encrypted_content");
                done_text = text(&data, "item.summary.0.text");
            }
            _ => {}
        }
    }
    assert!(
        !added_id.is_empty()
            && added_id == done_id
            && carrier_signature(&added_signature) == signature
            && done_signature == added_signature
            && done_text == "onetwo",
        "late thought signature replay malformed: added=({added_id:?},{added_signature:?}) done=({done_id:?},{done_signature:?},{done_text:?})"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_done_finalizes_started_stream_exactly_once() {
    let mut stream = GeminiToOpenAIResponsesStream::new(MODEL, &Value::Null, &Value::Null);
    translate(
        &mut stream,
        r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"unsigned thought","thought":true}]}}],"responseId":"done-finalize"}}"#,
    );
    let out = translate(&mut stream, "[DONE]");

    let mut deltas = Vec::new();
    let mut output_done_count = 0;
    let mut completed_count = 0;
    for (event, data) in &out {
        match event.as_str() {
            "response.reasoning_summary_text.delta" => deltas.push(text(data, "delta")),
            "response.output_item.done" => output_done_count += 1,
            "response.completed" => completed_count += 1,
            _ => {}
        }
    }
    assert!(
        deltas.concat() == "unsigned thought" && output_done_count == 1 && completed_count == 1,
        "DONE finalization malformed: deltas={deltas:?} output_done={output_done_count} completed={completed_count}"
    );
    let duplicate = translate(&mut stream, "[DONE]");
    assert!(
        duplicate.is_empty(),
        "duplicate DONE emitted {} events",
        duplicate.len()
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_finish_reason_then_done_does_not_duplicate_completion()
 {
    let mut stream = GeminiToOpenAIResponsesStream::new(MODEL, &Value::Null, &Value::Null);
    let mut out = translate(
        &mut stream,
        r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"answer"}]},"finishReason":"STOP"}],"responseId":"finish-then-done"}}"#,
    );
    out.extend(translate(&mut stream, "data: [DONE]"));

    let completed_count = out
        .iter()
        .filter(|(event, _)| event == "response.completed")
        .count();
    assert_eq!(
        completed_count, 1,
        "finish reason followed by DONE emitted {completed_count} completion events"
    );
    let duplicate = translate(&mut stream, "data: [DONE]");
    assert!(
        duplicate.is_empty(),
        "DONE after finish reason emitted {} events",
        duplicate.len()
    );
    let late = translate(
        &mut stream,
        r#"{"candidates":[{"content":{"parts":[{"text":"late"}]}}]}"#,
    );
    assert!(
        late.is_empty(),
        "input after completion emitted {} events",
        late.len()
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_bare_done_before_start_emits_nothing() {
    let mut stream = GeminiToOpenAIResponsesStream::new(MODEL, &Value::Null, &Value::Null);
    let out = translate(&mut stream, "data: [DONE]");
    assert!(out.is_empty(), "bare DONE emitted {} events", out.len());
    assert!(
        !stream.started && !stream.completed,
        "bare DONE changed stream state: started={} completed={}",
        stream.started,
        stream.completed
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_non_stream_visible_signature_completes_reasoning() {
    let raw = signed(
        r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"hidden thought","thought":true},{"text":"visible answer","thoughtSignature":"{sig}"}]},"finishReason":"STOP"}],"modelVersion":"gemini-3.6-flash","responseId":"resp_nonstream_active"}"#,
    );
    let out = non_stream_output(&raw);
    let got = text(&out, "output.0.type");
    assert_eq!(got, "reasoning", "output.0.type; output={out}");
    let got = carrier_signature(&text(&out, "output.0.encrypted_content"));
    assert_eq!(got, GEMINI_SIGNATURE, "reasoning signature; output={out}");
    let got = text(&out, "output.1.type");
    assert_eq!(got, "message", "output.1.type; output={out}");
}

#[test]
fn convert_gemini_response_to_openai_responses_preserves_text_around_function() {
    let lines = [
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"preface"}]}}],"modelVersion":"gemini-3.6-flash","responseId":"resp_mixed_stream"}}"#,
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"run_command","args":{"command":"true"}}}]}}],"modelVersion":"gemini-3.6-flash","responseId":"resp_mixed_stream"}}"#,
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"after"}]},"finishReason":"STOP"}],"modelVersion":"gemini-3.6-flash","responseId":"resp_mixed_stream"}}"#,
        "data: [DONE]",
    ];
    let events = run(MODEL, &lines);
    let got = done_types(&events);
    assert_eq!(got, "message,function_call,message", "done item order");
    let completed = completed_output(&events);
    let got = text(&completed, "0.content.0.text");
    assert_eq!(got, "preface", "completed first message");
    let got = text(&completed, "2.content.0.text");
    assert_eq!(got, "after", "completed trailing message");

    let translated = replay_with_function_output(&completed, &text(&completed, "1.call_id"));
    let contents = list(&translated, "contents");
    assert!(
        contents.len() == 2
            && text(&contents[0], "role") == "model"
            && text(&contents[1], "role") == "user",
        "mixed turn round-trip roles malformed: {translated}"
    );
    let parts = list(&contents[0], "parts");
    assert!(
        parts.len() == 3
            && text(&parts[0], "text") == "preface"
            && exists(&parts[1], "functionCall")
            && text(&parts[2], "text") == "after",
        "mixed turn model parts malformed: {translated}"
    );
    assert!(
        exists(&contents[1], "parts.0.functionResponse"),
        "function response must immediately follow the combined model turn: {translated}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_pending_signature_before_function_round_trips() {
    let lines = [
        signed(r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"","thoughtSignature":"{sig}"}]}}],"modelVersion":"gemini-3.6-flash","responseId":"pending-function-signature"}}"#),
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"id":"native-pending-call","name":"run_command","args":{"command":"true"}}}]},"finishReason":"STOP"}],"modelVersion":"gemini-3.6-flash","responseId":"pending-function-signature"}}"#.to_owned(),
        "data: [DONE]".to_owned(),
    ];
    let completed = completed_output(&run(MODEL, &lines));
    assert!(
        completed.is_array(),
        "stream did not emit response.completed output"
    );
    let mut call_id = String::new();
    for item in elements(&completed) {
        if text(item, "type") == "function_call" {
            call_id = text(item, "call_id");
        }
    }
    assert!(
        !call_id.is_empty(),
        "completed output has no function call: {completed}"
    );

    let translated = replay_with_function_output(&completed, &call_id);

    let mut function_signature = String::new();
    let mut detached_signatures = 0;
    for part in list(&translated, "contents.0.parts") {
        if exists(part, "functionCall") {
            function_signature = text(part, "thoughtSignature");
        }
        if exists(part, "text")
            && text(part, "text").is_empty()
            && !text(part, "thoughtSignature").is_empty()
        {
            detached_signatures += 1;
        }
    }
    assert!(
        function_signature == GEMINI_SIGNATURE && detached_signatures == 0,
        "pending signature was not rebound to function call: function signature={function_signature:?} detached={detached_signatures} translated={translated}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_signed_text_before_signed_function_round_trips() {
    // Go flips the last bit of the decoded signature here itself, as
    // `differentResponsesGeminiThoughtSignature` does.
    let tool_signature = different_gemini_signature();
    let lines = [
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"before "}]}}],"modelVersion":"gemini-3.6-flash","responseId":"resp_signed_mixed"}}"#.to_owned(),
        signed(r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"tool","thoughtSignature":"{sig}"}]}}],"modelVersion":"gemini-3.6-flash","responseId":"resp_signed_mixed"}}"#),
        fill(r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"thoughtSignature":"{sig2}","functionCall":{"name":"run_command","args":{"command":"true"}}}]},"finishReason":"STOP"}],"modelVersion":"gemini-3.6-flash","responseId":"resp_signed_mixed"}}"#, &tool_signature),
        "data: [DONE]".to_owned(),
    ];
    let completed = completed_output(&run(MODEL, &lines));
    let call_id = text(&completed, "3.call_id");
    let translated = replay_with_function_output(&completed, &call_id);

    let mut text_signature = String::new();
    let mut function_signature = String::new();
    for content in list(&translated, "contents") {
        for part in list(content, "parts") {
            if exists(part, "functionCall") {
                function_signature = text(part, "thoughtSignature");
            } else if text(part, "text") == "before tool" {
                text_signature = text(part, "thoughtSignature");
            }
        }
    }
    assert_eq!(
        text_signature, GEMINI_SIGNATURE,
        "text signature; translated={translated}"
    );
    assert_eq!(
        function_signature, tool_signature,
        "function signature; completed={completed} translated={translated}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_non_stream_preserves_text_around_signed_function() {
    let raw = signed(
        r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"preface"},{"thoughtSignature":"{sig}","functionCall":{"name":"run_command","args":{"command":"true"}}},{"text":"after"}]},"finishReason":"STOP"}],"modelVersion":"gemini-3.6-flash","responseId":"resp_nonstream_order"}"#,
    );
    let out = non_stream_output(&raw);
    let got = text(&out, "output.0.type");
    assert_eq!(got, "message", "output.0.type; output={out}");
    let got = text(&out, "output.1.type");
    assert_eq!(got, "reasoning", "output.1.type; output={out}");
    let got = text(&out, "output.2.type");
    assert_eq!(got, "function_call", "output.2.type; output={out}");
    let got = text(&out, "output.3.type");
    assert_eq!(
        got, "message",
        "output.3.type, want trailing message; output={out}"
    );
    let got = text(&out, "output.3.content.0.text");
    assert_eq!(got, "after", "trailing message; output={out}");
}

#[test]
fn convert_gemini_response_to_openai_responses_caches_signature_after_visible_text() {
    let lines = [
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"visible answer"}]}}],"modelVersion":"gemini-3.6-flash","responseId":"resp_detached"}}"#.to_owned(),
        signed(r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"","thoughtSignature":"{sig}"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":2,"thoughtsTokenCount":3,"totalTokenCount":15},"modelVersion":"gemini-3.6-flash","responseId":"resp_detached"}}"#),
    ];
    let events = run(MODEL, &lines);
    let got = done_types(&events);
    let completed_output = completed_output(&events);
    assert!(
        got == "message" && elements(&completed_output).len() == 1,
        "unexpected client timeline: done={got} output={completed_output}"
    );
    let translated = convert_openai_responses_request_to_gemini(
        MODEL,
        &json!({"input": completed_output}),
        false,
    );
    assert_eq!(
        text(&translated, "contents.0.parts.0.thoughtSignature"),
        GEMINI_SIGNATURE,
        "cached signature was not replayed: {translated}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_gemini_tool_signature() {
    let line = signed(
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"thoughtSignature":"{sig}","functionCall":{"id":"native-id","name":"run_command","args":{"command":"true"}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":2,"thoughtsTokenCount":3,"totalTokenCount":15},"modelVersion":"gemini-3.6-flash","responseId":"resp_tool_sig"}}"#,
    );
    let events = run(MODEL, &[line]);
    let mut signature = String::new();
    for (event, data) in &events {
        if event == "response.output_item.done" && text(data, "item.type") == "reasoning" {
            signature = text(data, "item.encrypted_content");
        }
    }
    let got = done_types(&events);
    assert_eq!(got, "reasoning,function_call", "tool signature item order");
    assert_eq!(
        carrier_signature(&signature),
        GEMINI_SIGNATURE,
        "tool signature = {signature:?}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_non_stream_detached_signature() {
    let raw = signed(
        r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"visible answer"},{"text":"","thoughtSignature":"{sig}"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":2,"thoughtsTokenCount":3,"totalTokenCount":15},"modelVersion":"gemini-3.6-flash","responseId":"resp_nonstream_detached"}"#,
    );
    let out = non_stream_output(&raw);
    let got = text(&out, "output.0.type");
    assert_eq!(got, "reasoning", "output.0.type; output={out}");
    let got = carrier_signature(&text(&out, "output.0.encrypted_content"));
    assert_eq!(got, GEMINI_SIGNATURE, "detached signature; output={out}");
    let got = text(&out, "output.1.type");
    assert_eq!(got, "message", "output.1.type; output={out}");
}

#[test]
fn convert_gemini_response_to_openai_responses_reasoning_encrypted_content() {
    let signature = "RXE0RENrZ0lDeEFDR0FJcVFOZDdjUzlleGFuRktRdFcvSzNyZ2MvWDNCcDQ4RmxSbGxOWUlOVU5kR1l1UHMrMGdkMVp0Vkg3ekdKU0g4YVljc2JjN3lNK0FrdGpTNUdqamI4T3Z0VVNETzdQd3pmcFhUOGl3U3hXUEJvTVFRQ09mWTFyMEtTWGZxUUlJakFqdmFGWk83RW1XRlBKckJVOVpkYzdDKw==";
    let lines = [
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"thought":true,"thoughtSignature":"{sig}","text":""}]}}],"modelVersion":"test-model","responseId":"req_vrtx_sig"},"traceId":"t1"}"#.replace("{sig}", signature),
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"thought":true,"text":"a"}]}}],"modelVersion":"test-model","responseId":"req_vrtx_sig"},"traceId":"t1"}"#.to_owned(),
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"hello"}]}}],"modelVersion":"test-model","responseId":"req_vrtx_sig"},"traceId":"t1"}"#.to_owned(),
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":""}]},"finishReason":"STOP"}],"modelVersion":"test-model","responseId":"req_vrtx_sig"},"traceId":"t1"}"#.to_owned(),
        "data: [DONE]".to_owned(),
    ];

    let mut added_encrypted = String::new();
    let mut done_encrypted = String::new();
    for (event, data) in run("test-model", &lines) {
        if text(&data, "item.type") != "reasoning" {
            continue;
        }
        match event.as_str() {
            "response.output_item.added" => added_encrypted = text(&data, "item.encrypted_content"),
            "response.output_item.done" => done_encrypted = text(&data, "item.encrypted_content"),
            _ => {}
        }
    }

    assert_eq!(
        carrier_signature(&added_encrypted),
        signature,
        "unexpected encrypted_content in response.output_item.added: got {added_encrypted:?}"
    );
    assert!(
        done_encrypted == added_encrypted && carrier_signature(&done_encrypted) == signature,
        "unexpected encrypted_content in response.output_item.done: got {done_encrypted:?}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_function_call_event_order() {
    let lines = [
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"tool0"}}]}}],"modelVersion":"test-model","responseId":"req_vrtx_1"},"traceId":"t1"}"#,
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"tool1"}}]}}],"modelVersion":"test-model","responseId":"req_vrtx_1"},"traceId":"t1"}"#,
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"tool2","args":{"a":1}}}]}}],"modelVersion":"test-model","responseId":"req_vrtx_1"},"traceId":"t1"}"#,
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":""}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15,"cachedContentTokenCount":0},"modelVersion":"test-model","responseId":"req_vrtx_1"},"traceId":"t1"}"#,
    ];

    let events = run("test-model", &lines);

    let mut pos_added = [None; 3];
    let mut pos_args_delta = [None; 3];
    let mut pos_args_done = [None; 3];
    let mut pos_item_done = [None; 3];
    let mut pos_completed = None;
    let mut delta_by_index: HashMap<usize, String> = HashMap::new();

    for (i, (event, data)) in events.iter().enumerate() {
        let index = usize::try_from(int_at(data, "output_index"))
            .ok()
            .filter(|&index| index < 3);
        match event.as_str() {
            "response.output_item.added" => {
                if text(data, "item.type") != "function_call" {
                    continue;
                }
                if let Some(index) = index {
                    pos_added[index] = Some(i);
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(index) = index {
                    pos_args_delta[index] = Some(i);
                    delta_by_index.insert(index, text(data, "delta"));
                }
            }
            "response.function_call_arguments.done" => {
                if let Some(index) = index {
                    pos_args_done[index] = Some(i);
                }
            }
            "response.output_item.done" => {
                if text(data, "item.type") != "function_call" {
                    continue;
                }
                if let Some(index) = index {
                    pos_item_done[index] = Some(i);
                }
            }
            "response.completed" => {
                pos_completed = Some(i);

                let output = at(data, "response.output");
                assert!(
                    output.is_some_and(Value::is_array),
                    "missing response.output in response.completed"
                );
                let got = list(data, "response.output").len();
                assert_eq!(got, 3, "unexpected response.output length");
                assert!(
                    text(data, "response.output.0.name") == "tool0"
                        && text(data, "response.output.0.arguments") == "{}",
                    "unexpected output[0]: {:?}",
                    at(data, "response.output.0")
                );
                assert!(
                    text(data, "response.output.1.name") == "tool1"
                        && text(data, "response.output.1.arguments") == "{}",
                    "unexpected output[1]: {:?}",
                    at(data, "response.output.1")
                );
                assert_eq!(
                    text(data, "response.output.2.name"),
                    "tool2",
                    "unexpected output[2] name: {:?}",
                    at(data, "response.output.2")
                );
                let arguments = text(data, "response.output.2.arguments");
                assert!(
                    valid_json(&arguments),
                    "unexpected output[2] arguments: {arguments:?}"
                );
            }
            _ => {}
        }
    }

    let pos_completed = pos_completed.expect("missing response.completed event");
    let mut item_done = Vec::new();
    for index in 0..3 {
        let (Some(added), Some(args_delta), Some(args_done), Some(done)) = (
            pos_added[index],
            pos_args_delta[index],
            pos_args_done[index],
            pos_item_done[index],
        ) else {
            panic!(
                "missing function call events for output_index {index}: added={:?} argsDelta={:?} argsDone={:?} itemDone={:?}",
                pos_added[index], pos_args_delta[index], pos_args_done[index], pos_item_done[index]
            );
        };
        assert!(
            added < args_delta && args_delta < args_done && args_done < done,
            "unexpected ordering for output_index {index}: added={added} argsDelta={args_delta} argsDone={args_done} itemDone={done}"
        );
        if index > 0 {
            let previous_done = item_done[index - 1];
            assert!(
                previous_done < added,
                "function call events overlap between {} and {index}: prevDone={previous_done} nextAdded={added}",
                index - 1
            );
        }
        item_done.push(done);
    }

    assert_eq!(
        delta_by_index.get(&0).map(String::as_str),
        Some("{}"),
        "unexpected delta for output_index 0"
    );
    assert_eq!(
        delta_by_index.get(&1).map(String::as_str),
        Some("{}"),
        "unexpected delta for output_index 1"
    );
    let delta = delta_by_index.get(&2).cloned().unwrap_or_default();
    assert!(
        !delta.is_empty()
            && serde_json::from_str::<Value>(&delta).is_ok_and(|args| int_at(&args, "a") == 1),
        "unexpected delta for output_index 2: got {delta:?}"
    );
    assert!(
        item_done[2] < pos_completed,
        "response.completed should be after last output_item.done: last={} completed={pos_completed}",
        item_done[2]
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_response_output_ordering() {
    let lines = [
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"tool0","args":{"x":"y"}}}]}}],"modelVersion":"test-model","responseId":"req_vrtx_2"},"traceId":"t2"}"#,
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"hi"}]}}],"modelVersion":"test-model","responseId":"req_vrtx_2"},"traceId":"t2"}"#,
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":""}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2,"cachedContentTokenCount":0},"modelVersion":"test-model","responseId":"req_vrtx_2"},"traceId":"t2"}"#,
    ];

    let events = run("test-model", &lines);

    let mut pos_func_done = None;
    let mut pos_msg_added = None;
    let mut pos_completed = None;

    for (i, (event, data)) in events.iter().enumerate() {
        match event.as_str() {
            "response.output_item.done" => {
                if text(data, "item.type") == "function_call" && int_at(data, "output_index") == 0 {
                    pos_func_done = Some(i);
                }
            }
            "response.output_item.added" => {
                if text(data, "item.type") == "message" && int_at(data, "output_index") == 1 {
                    pos_msg_added = Some(i);
                }
            }
            "response.completed" => {
                pos_completed = Some(i);
                assert_eq!(
                    text(data, "response.output.0.type"),
                    "function_call",
                    "expected response.output[0] to be function_call: {:?}",
                    at(data, "response.output.0")
                );
                assert_eq!(
                    text(data, "response.output.1.type"),
                    "message",
                    "expected response.output[1] to be message: {:?}",
                    at(data, "response.output.1")
                );
                assert_eq!(
                    text(data, "response.output.1.content.0.text"),
                    "hi",
                    "unexpected message text in response.output[1]: {:?}",
                    at(data, "response.output.1")
                );
            }
            _ => {}
        }
    }

    let (Some(pos_func_done), Some(pos_msg_added), Some(pos_completed)) =
        (pos_func_done, pos_msg_added, pos_completed)
    else {
        panic!(
            "missing required events: funcDone={pos_func_done:?} msgAdded={pos_msg_added:?} completed={pos_completed:?}"
        );
    };
    assert!(
        pos_func_done < pos_msg_added,
        "expected function_call to complete before message is added: funcDone={pos_func_done} msgAdded={pos_msg_added}"
    );
    assert!(
        pos_msg_added < pos_completed,
        "expected response.completed after message added: msgAdded={pos_msg_added} completed={pos_completed}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_restores_additional_namespace_custom_tool_call() {
    let original_request = parse(
        r#"{
		"model":"gemini-2.5-flash",
		"input":[{"type":"additional_tools","role":"developer","tools":[
			{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"exec"}]}
		]}]
	}"#,
    );
    let chunks = [
        r#"data: {"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"functions__exec","args":{"input":"pwd"}}}]},"finishReason":"STOP"}],"modelVersion":"gemini-2.5-flash","responseId":"resp_custom_stream"}"#,
        "data: [DONE]",
    ];

    let mut added = None;
    let mut input_done = None;
    let mut done = None;
    let mut completed = None;
    let mut function_events = 0;
    for (event, data) in run_with("gemini-2.5-flash", &original_request, &chunks) {
        match event.as_str() {
            "response.output_item.added" => {
                if text(&data, "item.type") == "custom_tool_call" {
                    added = Some(data);
                }
            }
            "response.custom_tool_call_input.done" => input_done = Some(data),
            "response.output_item.done" => {
                if text(&data, "item.type") == "custom_tool_call" {
                    done = Some(data);
                }
            }
            "response.function_call_arguments.delta" | "response.function_call_arguments.done" => {
                function_events += 1;
            }
            "response.completed" => completed = Some(data),
            _ => {}
        }
    }

    let (Some(added), Some(input_done), Some(done), Some(completed)) =
        (&added, &input_done, &done, &completed)
    else {
        panic!(
            "missing custom tool lifecycle events: added={} input_done={} done={} completed={}",
            added.is_some(),
            input_done.is_some(),
            done.is_some(),
            completed.is_some()
        );
    };
    assert_eq!(function_events, 0, "function call events");
    for (label, item) in [
        ("added", at(added, "item")),
        ("done", at(done, "item")),
        ("completed", at(completed, "response.output.0")),
    ] {
        let item = item.unwrap_or(&Value::Null);
        assert_eq!(text(item, "name"), "exec", "{label} name");
        assert_eq!(text(item, "namespace"), "functions", "{label} namespace");
    }
    assert_eq!(text(input_done, "input"), "pwd", "custom input.done input");
    assert_eq!(text(done, "item.input"), "pwd", "done input");
    assert_eq!(
        text(completed, "response.output.0.type"),
        "custom_tool_call",
        "completed output type"
    );
    assert_eq!(
        text(completed, "response.output.0.input"),
        "pwd",
        "completed input"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_non_stream_restores_additional_namespace_custom_tool_call()
 {
    let original_request = parse(
        r#"{
		"model":"gemini-2.5-flash",
		"input":[{"type":"additional_tools","role":"developer","tools":[
			{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"exec"}]}
		]}]
	}"#,
    );
    let raw = r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"functions__exec","args":{"input":"pwd"}}}]}}],"modelVersion":"gemini-2.5-flash","responseId":"resp_custom_nonstream"}"#;

    let root = convert_gemini_response_to_openai_responses_non_stream(
        &original_request,
        &Value::Null,
        raw.as_bytes(),
    )
    .unwrap_or(Value::Null);

    assert_eq!(
        text(&root, "output.0.type"),
        "custom_tool_call",
        "non-stream output type; raw: {root}"
    );
    assert_eq!(
        text(&root, "output.0.name"),
        "exec",
        "non-stream output name"
    );
    assert_eq!(
        text(&root, "output.0.namespace"),
        "functions",
        "non-stream output namespace"
    );
    assert_eq!(
        text(&root, "output.0.input"),
        "pwd",
        "non-stream output input"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_restores_additional_namespace_function_call() {
    let original_request = parse(
        r#"{
		"model":"gemini-2.5-flash",
		"input":[{"type":"additional_tools","role":"developer","tools":[
			{"type":"namespace","name":"functions","tools":[{"type":"function","name":"continuity_probe","parameters":{"type":"object","properties":{"value":{"type":"string"}}}}]}]
		}]
	}"#,
    );
    let chunks = [
        r#"data: {"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"functions__continuity_probe","args":{"value":"PROBE"}}}]},"finishReason":"STOP"}],"modelVersion":"gemini-2.5-flash","responseId":"resp_func_stream"}"#,
        "data: [DONE]",
    ];

    let mut added = None;
    let mut arg_done = None;
    let mut done = None;
    let mut completed = None;
    for (event, data) in run_with("gemini-2.5-flash", &original_request, &chunks) {
        match event.as_str() {
            "response.output_item.added" => {
                if text(&data, "item.type") == "function_call" {
                    added = Some(data);
                }
            }
            "response.function_call_arguments.done" => arg_done = Some(data),
            "response.output_item.done" => {
                if text(&data, "item.type") == "function_call" {
                    done = Some(data);
                }
            }
            "response.completed" => completed = Some(data),
            _ => {}
        }
    }

    let (Some(added), Some(_), Some(done), Some(completed)) =
        (&added, &arg_done, &done, &completed)
    else {
        panic!(
            "missing function tool lifecycle events: added={} arg_done={} done={} completed={}",
            added.is_some(),
            arg_done.is_some(),
            done.is_some(),
            completed.is_some()
        );
    };
    for (label, item) in [
        ("added", at(added, "item")),
        ("done", at(done, "item")),
        ("completed", at(completed, "response.output.0")),
    ] {
        let item = item.unwrap_or(&Value::Null);
        assert_eq!(text(item, "name"), "continuity_probe", "{label} name");
        assert_eq!(text(item, "namespace"), "functions", "{label} namespace");
    }
    assert_eq!(
        text(completed, "response.output.0.type"),
        "function_call",
        "completed output type"
    );
    let arguments = text(completed, "response.output.0.arguments");
    let value = serde_json::from_str::<Value>(&arguments)
        .map(|arguments| text(&arguments, "value"))
        .unwrap_or_default();
    assert_eq!(value, "PROBE", "completed value");
}

#[test]
fn convert_gemini_response_to_openai_responses_non_stream_restores_additional_namespace_function_call()
 {
    let original_request = parse(
        r#"{
		"model":"gemini-2.5-flash",
		"input":[{"type":"additional_tools","role":"developer","tools":[
			{"type":"namespace","name":"functions","tools":[{"type":"function","name":"continuity_probe","parameters":{"type":"object","properties":{"value":{"type":"string"}}}}]}]
		}]
	}"#,
    );
    let raw = r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"functions__continuity_probe","args":{"value":"PROBE"}}}]}}],"modelVersion":"gemini-2.5-flash","responseId":"resp_func_nonstream"}"#;

    let root = convert_gemini_response_to_openai_responses_non_stream(
        &original_request,
        &Value::Null,
        raw.as_bytes(),
    )
    .unwrap_or(Value::Null);

    assert_eq!(
        text(&root, "output.0.type"),
        "function_call",
        "non-stream output type; raw: {root}"
    );
    assert_eq!(
        text(&root, "output.0.name"),
        "continuity_probe",
        "non-stream output name"
    );
    assert_eq!(
        text(&root, "output.0.namespace"),
        "functions",
        "non-stream output namespace"
    );
    let arguments = text(&root, "output.0.arguments");
    let value = serde_json::from_str::<Value>(&arguments)
        .map(|arguments| text(&arguments, "value"))
        .unwrap_or_default();
    assert_eq!(value, "PROBE", "non-stream output value");
}

#[test]
fn convert_gemini_response_to_openai_responses_message_output_item_done_fields() {
    let chunks = [
        r#"data: {"candidates":[{"content":{"role":"model","parts":[{"text":"hello"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"totalTokenCount":7},"modelVersion":"gemini-2.5-flash","responseId":"resp_item_done_test"}"#,
    ];
    let original_request =
        parse(r#"{"model":"gemini-2.5-flash","input":"Reply with exactly: hello"}"#);

    let mut got_item_done = false;
    for (event, data) in run_with("gemini-2.5-flash", &original_request, &chunks) {
        if event == "response.output_item.done" && text(&data, "item.type") == "message" {
            got_item_done = true;
            assert!(
                exists(&data, "item.content.0.annotations"),
                "missing item.content.0.annotations in response.output_item.done: {data}"
            );
            assert!(
                at(&data, "item.content.0.annotations").is_some_and(Value::is_array),
                "item.content.0.annotations should be an array: {data}"
            );
            assert!(
                exists(&data, "item.content.0.logprobs"),
                "missing item.content.0.logprobs in response.output_item.done: {data}"
            );
            assert!(
                at(&data, "item.content.0.logprobs").is_some_and(Value::is_array),
                "item.content.0.logprobs should be an array: {data}"
            );
        }
    }

    assert!(
        got_item_done,
        "missing message response.output_item.done event"
    );
}

// Not ported from upstream: `parse_rfc3339` stands in for Go's
// `time.Parse(time.RFC3339Nano, ...)`, and these check that it accepts and
// rejects what Go's parser does.

/// 2024-01-02T15:04:05Z in Unix seconds.
const JAN_2_2024_150405: i64 = 1_704_207_845;

#[test]
fn parse_rfc3339_reads_utc_and_any_fraction() {
    assert_eq!(
        parse_rfc3339("2024-01-02T15:04:05Z"),
        Some(JAN_2_2024_150405)
    );
    assert_eq!(
        parse_rfc3339("2024-01-02T15:04:05.1Z"),
        Some(JAN_2_2024_150405)
    );
    assert_eq!(
        parse_rfc3339("2024-01-02T15:04:05.123456789123Z"),
        Some(JAN_2_2024_150405)
    );
    assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(parse_rfc3339("1969-12-31T23:59:59Z"), Some(-1));
    // A period with no digits after it isn't a fraction.
    assert_eq!(parse_rfc3339("2024-01-02T15:04:05.Z"), None);
}

#[test]
fn parse_rfc3339_accepts_one_digit_hour() {
    assert_eq!(
        parse_rfc3339("2024-01-02T5:04:05Z"),
        Some(JAN_2_2024_150405 - 10 * 3_600)
    );
    assert_eq!(
        parse_rfc3339("2024-01-02T05:04:05Z"),
        parse_rfc3339("2024-01-02T5:04:05Z")
    );
    // Minutes and seconds still take two digits.
    assert_eq!(parse_rfc3339("2024-01-02T15:4:05Z"), None);
    assert_eq!(parse_rfc3339("2024-01-02T15:04:5Z"), None);
}

#[test]
fn parse_rfc3339_accepts_comma_fraction() {
    assert_eq!(
        parse_rfc3339("2024-01-02T15:04:05,5Z"),
        Some(JAN_2_2024_150405)
    );
    assert_eq!(
        parse_rfc3339("2024-01-02T15:04:05,123+01:00"),
        Some(JAN_2_2024_150405 - 3_600)
    );
}

#[test]
fn parse_rfc3339_accepts_offsets_up_to_24_60() {
    assert_eq!(
        parse_rfc3339("2024-01-02T15:04:05+01:00"),
        Some(JAN_2_2024_150405 - 3_600)
    );
    assert_eq!(
        parse_rfc3339("2024-01-02T15:04:05-01:30"),
        Some(JAN_2_2024_150405 + 5_400)
    );
    assert_eq!(
        parse_rfc3339("2024-01-02T15:04:05+24:60"),
        Some(JAN_2_2024_150405 - (24 * 60 + 60) * 60)
    );
    assert_eq!(
        parse_rfc3339("2024-01-02T15:04:05-24:00"),
        Some(JAN_2_2024_150405 + 24 * 3_600)
    );
    assert_eq!(parse_rfc3339("2024-01-02T15:04:05+25:00"), None);
    assert_eq!(parse_rfc3339("2024-01-02T15:04:05+00:61"), None);
    // The offset takes a colon and two digits on each side.
    assert_eq!(parse_rfc3339("2024-01-02T15:04:05+0100"), None);
    assert_eq!(parse_rfc3339("2024-01-02T15:04:05+1:00"), None);
}

#[test]
fn parse_rfc3339_validates_day_against_month_and_leap_year() {
    assert_eq!(parse_rfc3339("2024-02-29T00:00:00Z"), Some(1_709_164_800));
    assert_eq!(parse_rfc3339("2000-02-29T00:00:00Z"), Some(951_782_400));
    assert_eq!(parse_rfc3339("2023-02-29T00:00:00Z"), None);
    assert_eq!(parse_rfc3339("1900-02-29T00:00:00Z"), None);
    assert_eq!(parse_rfc3339("2024-04-31T00:00:00Z"), None);
    assert_eq!(parse_rfc3339("2024-01-00T00:00:00Z"), None);
    assert_eq!(parse_rfc3339("2024-13-01T00:00:00Z"), None);
    assert_eq!(parse_rfc3339("2024-01-02T24:00:00Z"), None);
    assert_eq!(parse_rfc3339("2024-01-02T23:60:00Z"), None);
    assert_eq!(parse_rfc3339("2024-01-02T23:59:60Z"), None);
}

#[test]
fn parse_rfc3339_rejects_trailing_text() {
    assert_eq!(parse_rfc3339("2024-01-02T15:04:05Zjunk"), None);
    assert_eq!(parse_rfc3339("2024-01-02T15:04:05Z "), None);
    assert_eq!(parse_rfc3339("2024-01-02T15:04:05+01:00 "), None);
    assert_eq!(parse_rfc3339("2024-01-02T15:04:05+01:000"), None);
    // A zone is required.
    assert_eq!(parse_rfc3339("2024-01-02T15:04:05"), None);
    assert_eq!(parse_rfc3339(""), None);
}
