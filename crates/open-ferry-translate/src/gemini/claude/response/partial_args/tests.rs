// Native equivalents of 64316127's Gemini-to-Claude partialArgs cases.
use super::*;

mod review_tests;

fn frame(parts: Value, finish: bool) -> Value {
    let mut frame = json!({"candidates":[{"content":{"parts":parts}}]});
    if finish {
        frame["candidates"][0]["finishReason"] = "STOP".into();
    }
    frame
}

fn events(stream: &mut GeminiToClaudeStream, frame: &Value) -> Vec<Value> {
    parse(&stream.translate(frame.to_string().as_bytes()))
}

fn parse(raw: &str) -> Vec<Value> {
    raw.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn arguments(events: &[Value]) -> String {
    events
        .iter()
        .filter_map(|event| event["delta"]["partial_json"].as_str())
        .collect()
}

#[test]
fn vertex_partial_args_stream_concatenates_and_preserves_identity() {
    let mut stream = GeminiToClaudeStream::new(&json!({}));
    let first = events(
        &mut stream,
        &frame(
            json!([{"functionCall":{"name":"weather","willContinue":true,"partialArgs":[{"jsonPath":"$.city","stringValue":"New ","willContinue":true}]}}]),
            false,
        ),
    );
    assert_eq!(arguments(&first), r#"{"city":"New "#);
    let mut all = first;
    all.extend(events(&mut stream, &frame(json!([{"functionCall":{"name":"weather","willContinue":true,"partialArgs":[{"jsonPath":"$.city","stringValue":"York"}]}}]), false)));
    assert!(
        !all.iter()
            .any(|event| event["type"] == "content_block_stop")
    );
    all.extend(events(
        &mut stream,
        &frame(json!([{"functionCall":{}}]), true),
    ));
    all.extend(parse(&stream.translate(b"[DONE]")));
    assert_eq!(arguments(&all), r#"{"city":"New York"}"#);
    assert_eq!(
        all.iter()
            .filter(|event| event["content_block"]["type"] == "tool_use")
            .count(),
        1
    );
    assert_eq!(
        all.iter()
            .filter(|event| event["type"] == "content_block_stop")
            .count(),
        1
    );
    assert!(all.iter().any(|event| event["type"] == "message_stop"));
}

#[test]
fn signatures_and_empty_text_do_not_split_an_open_call() {
    let mut stream = GeminiToClaudeStream::new(&json!({}));
    let mut all = events(
        &mut stream,
        &frame(
            json!([{"functionCall":{"name":"weather","willContinue":true,"partialArgs":[{"jsonPath":"$.city","stringValue":"Ber","willContinue":true}]}}]),
            false,
        ),
    );
    let middle = events(
        &mut stream,
        &frame(
            json!([{"text":""},{"thoughtSignature":"signature-one"},{"thought_signature":"signature-two"}]),
            false,
        ),
    );
    assert!(middle.is_empty());
    all.extend(events(
        &mut stream,
        &frame(
            json!([{"functionCall":{"partialArgs":[{"jsonPath":"$.city","stringValue":"lin"}]}}]),
            true,
        ),
    ));
    assert_eq!(arguments(&all), r#"{"city":"Berlin"}"#);
    let stop = all
        .iter()
        .position(|event| event["type"] == "content_block_stop")
        .unwrap();
    let signatures: Vec<_> = all
        .iter()
        .enumerate()
        .filter(|(_, event)| event["delta"]["type"] == "signature_delta")
        .collect();
    assert_eq!(signatures.len(), 2);
    assert!(signatures.iter().all(|(at, _)| *at > stop));
}

#[test]
fn truncated_replaced_interrupted_and_invalid_calls_never_finish_successfully() {
    for next in [
        None,
        Some(json!([{"text":"interruption"}])),
        Some(
            json!([{"functionCall":{"name":"weather","id":"call_b","partialArgs":[{"jsonPath":"$.city","stringValue":"Paris"}]}}]),
        ),
        Some(json!([{"functionCall":{"partialArgs":[{"jsonPath":"$.city.x","numberValue":1}]}}])),
    ] {
        let mut stream = GeminiToClaudeStream::new(&json!({}));
        let mut all = events(
            &mut stream,
            &frame(
                json!([{"functionCall":{"name":"weather","id":"call_a","willContinue":true,"partialArgs":[{"jsonPath":"$.city","stringValue":"New ","willContinue":true}]}}]),
                false,
            ),
        );
        if let Some(next) = next {
            all.extend(events(&mut stream, &frame(next, true)));
        }
        all.extend(parse(&stream.translate(b"[DONE]")));
        assert!(!all.iter().any(|event| event["type"] == "message_stop"));
        assert!(
            !all.iter()
                .any(|event| event["type"] == "content_block_stop" && event["index"] == 0)
        );
    }
}

#[test]
fn distinct_completed_calls_parameterless_and_empty_partial_fallback() {
    let mut stream = GeminiToClaudeStream::new(&json!({}));
    let all = events(
        &mut stream,
        &frame(
            json!([
                {"functionCall":{"name":"weather","partialArgs":[{"jsonPath":"$.city","stringValue":"Paris"}]}},
                {"functionCall":{"name":"weather","partialArgs":[{"jsonPath":"$.city","stringValue":"Lyon"}]}},
                {"functionCall":{"name":"ping","willContinue":false}},
                {"functionCall":{"name":"weather","args":{"city":"Rome"},"partialArgs":[]}}
            ]),
            true,
        ),
    );
    let ids: Vec<_> = all
        .iter()
        .filter_map(|event| event["content_block"]["id"].as_str())
        .collect();
    assert_eq!(ids.len(), 4);
    assert_ne!(ids[0], ids[1]);
    assert_eq!(
        arguments(&all),
        r#"{"city":"Paris"}{"city":"Lyon"}{}{"city":"Rome"}"#
    );
}

#[test]
fn nested_placeholder_cleaning_preserves_declared_reason_and_dotted_keys() {
    let request = json!({"tools":[{"name":"lookup","input_schema":{"type":"object","properties":{
        "options":{"type":"object","properties":{},"additionalProperties":false},
        "a":{"type":"object","properties":{"b":{"type":"object","properties":{"keep":{"type":"number"}}}}},
        "a.b":{"type":"object","properties":{},"additionalProperties":false},
        "items":{"type":"array","items":{"type":"object","properties":{"id":{"type":"number"}}}},
        "real":{"type":"object","properties":{"reason":{"type":"string"}}}
    }}}]});
    let translated = json!({"tools":[{"functionDeclarations":[{"name":"lookup","parameters":{"type":"object","properties":{
        "options":{"type":"object","properties":{"reason":{"type":"string"}},"additionalProperties":false},
        "a.b":{"type":"object","properties":{"reason":{"type":"string"}},"additionalProperties":false},
        "items":{"type":"array","items":{"type":"object","properties":{"_":{"type":"boolean"},"id":{"type":"number"}}}}
    }}}]}]});
    let mut stream = GeminiToClaudeStream::new(&request).with_translated_request(&translated);
    let mut all = events(
        &mut stream,
        &frame(
            json!([{"functionCall":{"name":"lookup","willContinue":true,"partialArgs":[
        {"jsonPath":"$.real.reason","stringValue":"ke","willContinue":true},
        {"jsonPath":"$.a.b.keep","numberValue":1},
        {"jsonPath":"$['a.b'].junk","stringValue":"drop"},
        {"jsonPath":"$.options.reason","stringValue":"injected","willContinue":true},
        {"jsonPath":"$.items[0]._","boolValue":true},
        {"jsonPath":"$.items[0].id","numberValue":7}]}}]),
            false,
        ),
    );
    all.extend(events(&mut stream, &frame(json!([{"functionCall":{"partialArgs":[{"jsonPath":"$.real.reason","stringValue":"pt"},{"jsonPath":"$.options.reason","stringValue":"-more"}]}}]), true)));
    assert_eq!(
        arguments(&all),
        r#"{"real":{"reason":"kept"},"a":{"b":{"keep":1}},"a.b":{},"options":{},"items":[{"id":7}]}"#
    );
}
