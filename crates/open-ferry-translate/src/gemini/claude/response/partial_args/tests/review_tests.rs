use super::*;

#[test]
fn claude_failed_first_partial_chunk_keeps_the_previous_block_stop() {
    let mut stream = GeminiToClaudeStream::new(&json!({}));
    let mut all = events(
        &mut stream,
        &frame(
            json!([{"functionCall":{"name":"weather","args":{"city":"Paris"}}}]),
            false,
        ),
    );
    let failed = events(
        &mut stream,
        &frame(
            json!([{"functionCall":{"name":"different_tool","partialArgs":[{"jsonPath":"city","stringValue":"invalid"}]}}]),
            false,
        ),
    );
    assert!(failed.is_empty());
    assert_eq!(stream.current_tool, "weather");
    let text = events(&mut stream, &frame(json!([{"text":"hi"}]), false));
    assert_eq!(text[0], json!({"type":"content_block_stop","index":0}));
    assert!(
        text.iter()
            .any(|event| event["type"] == "content_block_start"
                && event["index"] == 1
                && event["content_block"]["type"] == "text")
    );
    all.extend(text);
    all.extend(parse(&stream.translate(b"[DONE]")));
    assert_eq!(
        all.iter()
            .filter(|event| event["type"] == "content_block_stop" && event["index"] == 0)
            .count(),
        1
    );
    assert!(!all.iter().any(|event| event["type"] == "message_stop"));
}

#[test]
fn claude_partial_args_preserve_exponent_and_large_number_spelling() {
    let mut stream = GeminiToClaudeStream::new(&json!({}));
    let raw = br#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"numbers","partialArgs":[{"jsonPath":"$.exponent","numberValue":1E5},{"jsonPath":"$.decimal","numberValue":-2.500e-3},{"jsonPath":"$.large","numberValue":123456789012345678901234567890},{"jsonPath":"$.huge","numberValue":1E400}]}}]},"finishReason":"STOP"}],"usageMetadata":{"totalTokenCount":1}}"#;
    assert_eq!(
        arguments(&parse(&stream.translate(raw))),
        r#"{"exponent":1E5,"decimal":-2.500e-3,"large":123456789012345678901234567890,"huge":1E400}"#
    );
}

#[test]
fn claude_full_schema_work_per_probe_exhausts_the_call_without_success() {
    let mut tools: Vec<_> = (0..128)
        .map(|index| {
            json!({
                "name":format!("unused_{index}"),
                "input_schema":{"type":"object","description":"x".repeat(8192)}
            })
        })
        .collect();
    let properties: serde_json::Map<_, _> = (0..64)
        .map(|index| {
            (
                format!("k{index}"),
                json!({"type":"object","properties":{"value":{"type":"string"}}}),
            )
        })
        .collect();
    tools.push(json!({"name":"wide","input_schema":{"type":"object","properties":properties}}));
    let mut stream = GeminiToClaudeStream::new(&json!({"tools":tools}));
    let mut all = Vec::new();
    for index in 0..64 {
        all.extend(events(
            &mut stream,
            &frame(
                json!([{"functionCall":{
                    "name":"wide","willContinue":true,"partialArgs":[{
                        "jsonPath":format!("$.k{index}.value"),"stringValue":"x"
                    }]
                }}]),
                false,
            ),
        ));
        if stream.incomplete_call {
            break;
        }
    }
    assert!(
        stream.incomplete_call,
        "full request/schema scans must count, not just tiny probes"
    );
    assert!(stream.active_call.is_none());
    all.extend(parse(&stream.translate(b"[DONE]")));
    assert!(!all.iter().any(|event| event["type"] == "message_stop"));
    assert!(
        !all.iter()
            .any(|event| event["type"] == "content_block_stop")
    );
}
