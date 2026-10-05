// Ported from CLIProxyAPI internal/translator/gemini/openai/responses/issue6258_terminal_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! When a stream ends: a finish reason waits for a chunk with usage, or for
//! `[DONE]`, so usage sent after it still counts; the first finish reason
//! decides; `MAX_TOKENS` ends the response, and the message open then, as
//! `incomplete`; and usage counts are running totals, each replaced when a
//! chunk has it.
//!
//! Dropped or changed tests: none. Go's subtests become loops whose
//! assertion messages name the case.

use super::*;

/// `issue6258Usage`
const USAGE: &str = r#"{"promptTokenCount":100,"candidatesTokenCount":10,"thoughtsTokenCount":50,"totalTokenCount":160,"cachedContentTokenCount":7}"#;

/// `issue6258Translate`: the data of each event one frame gives.
fn translate_frame(stream: &mut GeminiToOpenAIResponsesStream, frame: &str) -> Vec<Value> {
    translate(stream, frame)
        .into_iter()
        .map(|(_, data)| data)
        .collect()
}

/// A stream from `gemini-3.7-flash` with no request, as Go's tests pass nil.
fn new_stream() -> GeminiToOpenAIResponsesStream {
    GeminiToOpenAIResponsesStream::new("gemini-3.7-flash", &Value::Null, &Value::Null)
}

/// How many events have the `type` `kind`.
fn count_type(events: &[Value], kind: &str) -> usize {
    events
        .iter()
        .filter(|event| text(event, "type") == kind)
        .count()
}

/// `issue6258Terminal`: checks that `events` make one well-formed stream
/// with exactly one final event, and returns that event's `response`.
fn terminal(case: &str, events: &[Value]) -> Value {
    let mut terminal = Value::Null;
    let (mut terminals, mut created, mut in_progress) = (0, 0, 0);
    let mut previous = -1;
    let mut added: HashMap<String, Value> = HashMap::new();
    let mut done: HashMap<String, usize> = HashMap::new();
    let mut response_id = String::new();
    for event in events {
        let seq = int_at(event, "sequence_number");
        assert!(
            seq > previous,
            "{case}: sequence_number={seq} after {previous}"
        );
        previous = seq;
        let id = text(event, "response.id");
        if !id.is_empty() {
            assert!(
                response_id.is_empty() || id == response_id,
                "{case}: response ID changed: {response_id:?} -> {id:?}"
            );
            response_id = id;
        }
        match text(event, "type").as_str() {
            "response.created" => created += 1,
            "response.in_progress" => in_progress += 1,
            "response.output_item.added" => {
                added.insert(text(event, "item.id"), event.clone());
            }
            "response.output_item.done" => {
                let id = text(event, "item.id");
                *done.entry(id.clone()).or_default() += 1;
                assert!(
                    added.get(&id).is_some_and(
                        |start| int_at(start, "output_index") == int_at(event, "output_index")
                    ),
                    "{case}: item identity/index changed: {event}"
                );
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                terminals += 1;
                terminal = event.clone();
            }
            _ => {}
        }
    }
    assert_eq!(terminals, 1, "{case}: terminal count, want exactly 1");
    assert!(
        created == 1 && in_progress == 1,
        "{case}: created/in_progress counts={created}/{in_progress}, want 1/1"
    );
    for id in added.keys() {
        let count = done.get(id).copied().unwrap_or(0);
        assert_eq!(count, 1, "{case}: item {id} done count, want exactly 1");
    }
    for (index, item) in list(&terminal, "response.output").iter().enumerate() {
        assert!(
            added
                .get(&text(item, "id"))
                .is_some_and(|start| int_at(start, "output_index") == index as i64),
            "{case}: terminal output identity/index changed: {item}"
        );
    }
    at(&terminal, "response").cloned().unwrap_or(Value::Null)
}

/// `issue6258AssertUsage`
fn assert_usage(
    case: &str,
    response: &Value,
    input: i64,
    output: i64,
    reasoning: i64,
    total: i64,
    cached: i64,
) {
    for (path, want) in [
        ("input_tokens", input),
        ("output_tokens", output),
        ("output_tokens_details.reasoning_tokens", reasoning),
        ("total_tokens", total),
        ("input_tokens_details.cached_tokens", cached),
    ] {
        let path = format!("usage.{path}");
        assert!(
            exists(response, &path) && int_at(response, &path) == want,
            "{case}: {path}={:?}, want {want}; usage={:?}",
            at(response, &path),
            at(response, "usage")
        );
    }
}

/// `issue6258AssertIncomplete`
fn assert_incomplete(case: &str, response: &Value) {
    assert!(
        text(response, "status") == "incomplete"
            && text(response, "incomplete_details.reason") == "max_output_tokens",
        "{case}: MAX_TOKENS status={:?} details={:?}, want incomplete/max_output_tokens",
        text(response, "status"),
        at(response, "incomplete_details")
    );
    assert!(
        text(response, "output.0.status") == "incomplete"
            && text(response, "output.0.content.0.text") == "partial",
        "{case}: partial message must be preserved and incomplete: {:?}",
        at(response, "output")
    );
}

#[test]
fn issue6258_gemini_responses_split_stop_usage() {
    for wrapped in [false, true] {
        for usage_key in ["usageMetadata", "cpaUsageMetadata"] {
            let case = format!("wrapped={wrapped}/{usage_key}");
            let frame = |body: &str| {
                if wrapped {
                    format!(r#"data: {{"response":{body}}}"#)
                } else {
                    format!("data: {body}")
                }
            };
            let mut stream = new_stream();
            let mut events = translate_frame(
                &mut stream,
                &frame(
                    r#"{"responseId":"split-stop","candidates":[{"content":{"parts":[{"text":"answer"}]}}]}"#,
                ),
            );
            let finish = frame(
                r#"{"candidates":[{"content":{"parts":[{"text":" tail"}]},"finishReason":"STOP"}]}"#,
            );
            let before_usage = translate_frame(&mut stream, &finish);
            assert_eq!(
                count_type(&before_usage, "response.completed"),
                0,
                "{case}: STOP without usage emitted a terminal before the usage tail"
            );
            events.extend(before_usage);
            let repeated_finish = translate_frame(
                &mut stream,
                &frame(r#"{"candidates":[{"finishReason":"STOP"}]}"#),
            );
            assert_eq!(
                count_type(&repeated_finish, "response.completed"),
                0,
                "{case}: repeated pending STOP emitted a terminal before usage"
            );
            events.extend(repeated_finish);
            let after_usage = translate_frame(
                &mut stream,
                &frame(&format!(r#"{{"{usage_key}":{USAGE}}}"#)),
            );
            assert_eq!(
                count_type(&after_usage, "response.completed"),
                1,
                "{case}: usage tail terminals, want 1"
            );
            events.extend(after_usage);
            for repeated in [finish.as_str(), "data: [DONE]", "data: [DONE]"] {
                events.extend(translate_frame(&mut stream, repeated));
            }
            let response = terminal(&case, &events);
            assert!(
                text(&response, "status") == "completed"
                    && text(&response, "output.0.content.0.text") == "answer tail",
                "{case}: lost finish-frame content or wrong status: {response}"
            );
            assert_usage(&case, &response, 100, 60, 50, 160, 7);
        }
    }
}

#[test]
fn issue6258_gemini_responses_max_tokens() {
    for mode in ["same_frame", "split_usage", "no_usage_clean_done"] {
        let case = format!("stream/{mode}");
        let mut stream = new_stream();
        let mut events = translate_frame(
            &mut stream,
            r#"{"responseId":"max","candidates":[{"content":{"parts":[{"text":"partial"}]}}]}"#,
        );
        let mut finish = r#"{"candidates":[{"finishReason":"MAX_TOKENS"}]"#.to_owned();
        if mode == "same_frame" {
            finish.push_str(&format!(r#","usageMetadata":{USAGE}"#));
        }
        finish.push('}');
        events.extend(translate_frame(&mut stream, &finish));
        if mode == "split_usage" {
            events.extend(translate_frame(
                &mut stream,
                &format!(r#"{{"usageMetadata":{USAGE}}}"#),
            ));
        }
        for frame in ["[DONE]", "[DONE]", finish.as_str()] {
            events.extend(translate_frame(&mut stream, frame));
        }
        let response = terminal(&case, &events);
        for event in &events {
            let kind = text(event, "type");
            assert!(
                kind != "response.completed" && kind != "response.failed",
                "{case}: MAX_TOKENS emitted {kind}, want response.incomplete"
            );
        }
        assert_incomplete(&case, &response);
        if mode != "no_usage_clean_done" {
            assert_usage(&case, &response, 100, 60, 50, 160, 7);
        }
    }

    let raw = format!(
        r#"{{"responseId":"max","candidates":[{{"content":{{"parts":[{{"text":"partial"}}]}},"finishReason":"MAX_TOKENS"}}],"usageMetadata":{USAGE}}}"#
    );
    let response = non_stream_output(&raw);
    assert_incomplete("nonstream", &response);
    assert_usage("nonstream", &response, 100, 60, 50, 160, 7);
}

#[test]
fn issue6258_gemini_responses_usage_snapshots() {
    for (name, finish_usage, output, reasoning, total) in [
        (
            "absent_preserves",
            r#"{"candidatesTokenCount":12,"totalTokenCount":182}"#,
            62,
            50,
            182,
        ),
        (
            "explicit_zero_replaces",
            r#"{"candidatesTokenCount":12,"thoughtsTokenCount":0,"totalTokenCount":0}"#,
            12,
            0,
            0,
        ),
    ] {
        let mut stream = new_stream();
        let mut events = Vec::new();
        for frame in [
            format!(
                r#"{{"responseId":"snapshots","candidates":[{{"content":{{"parts":[{{"text":"answer"}}]}}}}],"usageMetadata":{USAGE}}}"#
            ),
            format!(r#"{{"usageMetadata":{USAGE}}}"#),
            r#"{"usageMetadata":{"promptTokenCount":120,"cachedContentTokenCount":0},"cpaUsageMetadata":{"promptTokenCount":999}}"#.to_owned(),
            format!(r#"{{"candidates":[{{"finishReason":"STOP"}}],"usageMetadata":{finish_usage}}}"#),
            "[DONE]".to_owned(),
        ] {
            events.extend(translate_frame(&mut stream, &frame));
        }
        assert_usage(
            name,
            &terminal(name, &events),
            120,
            output,
            reasoning,
            total,
            0,
        );
    }
}

#[test]
fn issue6258_gemini_responses_max_tokens_only_active_message_incomplete() {
    let mut stream = new_stream();
    let mut events = Vec::new();
    for frame in [
        r#"{"responseId":"mixed","candidates":[{"content":{"parts":[{"text":"preface"}]}}]}"#.to_owned(),
        r#"{"candidates":[{"content":{"parts":[{"functionCall":{"id":"call-1","name":"lookup","args":{"key":"value"}}}]}}]}"#.to_owned(),
        format!(
            r#"{{"candidates":[{{"content":{{"parts":[{{"text":"partial"}}]}},"finishReason":"MAX_TOKENS"}}],"usageMetadata":{USAGE}}}"#
        ),
        "[DONE]".to_owned(),
    ] {
        events.extend(translate_frame(&mut stream, &frame));
    }
    let response = terminal("mixed", &events);
    let items = list(&response, "output");
    assert_eq!(
        items.len(),
        3,
        "output count, want preface/tool/active message: {response}"
    );
    for (index, want) in ["completed", "completed", "incomplete"]
        .into_iter()
        .enumerate()
    {
        let got = text(&items[index], "status");
        assert_eq!(got, want, "output[{index}].status");
        for event in &events {
            if text(event, "type") == "response.output_item.done"
                && int_at(event, "output_index") == index as i64
            {
                let got = text(event, "item.status");
                assert_eq!(got, want, "output_item.done[{index}].status");
            }
        }
    }
}

#[test]
fn issue6258_gemini_responses_done_uses_usage_snapshot() {
    for usage_key in ["usageMetadata", "cpaUsageMetadata"] {
        let mut stream = new_stream();
        let mut events = Vec::new();
        for frame in [
            format!(
                r#"{{"responseId":"snapshot-done","candidates":[{{"content":{{"parts":[{{"text":"answer"}}]}}}}],"{usage_key}":{USAGE}}}"#
            ),
            format!(r#"{{"{usage_key}":{USAGE}}}"#),
            r#"{"candidates":[{"finishReason":"STOP"}]}"#.to_owned(),
            "[DONE]".to_owned(),
            "[DONE]".to_owned(),
        ] {
            events.extend(translate_frame(&mut stream, &frame));
        }
        assert_usage(
            usage_key,
            &terminal(usage_key, &events),
            100,
            60,
            50,
            160,
            7,
        );
    }
}

#[test]
fn issue6258_gemini_responses_same_frame_control() {
    let mut stream = new_stream();
    let mut events = translate_frame(
        &mut stream,
        &format!(
            r#"{{"responseId":"control","candidates":[{{"content":{{"parts":[{{"text":"answer"}}]}},"finishReason":"STOP"}}],"usageMetadata":{USAGE}}}"#
        ),
    );
    events.extend(translate_frame(&mut stream, "[DONE]"));
    let response = terminal("control", &events);
    assert_usage("control", &response, 100, 60, 50, 160, 7);
}
