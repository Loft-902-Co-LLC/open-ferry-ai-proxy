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
fn round4_two_hundred_nested_probes_complete() {
    for translated in [false, true] {
        let mut tools: Vec<_> = (0..128)
            .map(|index| {
                json!({
                    "name":format!("unused_{index}"),
                    "input_schema":{"type":"object","description":"x".repeat(8192)}
                })
            })
            .collect();
        let properties: serde_json::Map<_, _> = (0..200)
            .map(|index| {
                (
                    format!("k{index}"),
                    json!({"type":"object","properties":{"value":{"type":"string"}}}),
                )
            })
            .collect();
        tools.push(json!({"name":"wide","input_schema":{"type":"object","properties":properties}}));
        let sent = json!({"tools":[{"functionDeclarations":[{"name":"wide","parametersJsonSchema":tools.last().unwrap()["input_schema"]}]}]});
        let mut stream = GeminiToClaudeStream::new(&json!({"tools":tools}));
        if translated {
            stream = stream.with_translated_request(&sent);
        }
        let mut all = Vec::new();
        for index in 0..200 {
            let before = stream
                .active_call
                .as_ref()
                .map_or(0, |call| call.assembler.work_used());
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
            if index > 0 {
                let work = stream.active_call.as_ref().unwrap().assembler.work_used() - before;
                assert!(
                    work < 32 * 1024,
                    "probe {index} re-walked the wide schema: {work} bytes (translated={translated})"
                );
            }
        }
        all.extend(events(
            &mut stream,
            &frame(json!([{"functionCall":{}}]), true),
        ));
        all.extend(parse(&stream.translate(b"[DONE]")));
        assert!(!stream.incomplete_call);
        assert!(all.iter().any(|event| event["type"] == "message_stop"));
        let args: Value = serde_json::from_str(&arguments(&all)).unwrap();
        assert_eq!(args.as_object().unwrap().len(), 200);
        for index in 0..200 {
            assert_eq!(args[format!("k{index}")]["value"], "x");
        }
    }
}

#[test]
fn round4_long_probe_paths_do_not_consume_argument_bytes() {
    let key = "k".repeat(8192);
    let path = format!("${}", format!(".{key}").repeat(40));
    let mut stream = GeminiToClaudeStream::new(&json!({}));
    let mut all = events(
        &mut stream,
        &frame(
            json!([{"functionCall":{"name":"deep","partialArgs":[{"jsonPath":path,"stringValue":"x"}]}}]),
            true,
        ),
    );
    all.extend(parse(&stream.translate(b"[DONE]")));
    assert!(!stream.incomplete_call);
    assert!(all.iter().any(|event| event["type"] == "message_stop"));
    let args: Value = serde_json::from_str(&arguments(&all)).unwrap();
    let mut leaf = &args;
    for _ in 0..40 {
        let object = leaf.as_object().unwrap();
        assert_eq!(object.len(), 1);
        leaf = &object[&key];
    }
    assert_eq!(leaf, "x");
}

#[test]
fn round3_wide_closed_schema_placeholder_probe_completes() {
    let properties: serde_json::Map<_, _> = (0..600)
        .map(|index| {
            (
                format!("property_{index:04}"),
                json!({"type":"string","description":"a string property"}),
            )
        })
        .collect();
    let schema = json!({"type":"object","additionalProperties":false,"properties":properties});
    let request = json!({"tools":[{"name":"wide","input_schema":schema}]});
    let sent =
        json!({"tools":[{"functionDeclarations":[{"name":"wide","parametersJsonSchema":schema}]}]});
    let mut stream = GeminiToClaudeStream::new(&request).with_translated_request(&sent);
    let mut all = events(
        &mut stream,
        &frame(
            json!([{"functionCall":{"name":"wide","willContinue":true,"partialArgs":[{"jsonPath":"$.reason","stringValue":"x"}]}}]),
            false,
        ),
    );
    all.extend(events(
        &mut stream,
        &frame(json!([{"functionCall":{}}]), true),
    ));
    all.extend(parse(&stream.translate(b"[DONE]")));
    assert!(!stream.incomplete_call);
    assert_eq!(arguments(&all), r#"{"reason":"x"}"#);
    assert!(all.iter().any(|event| event["type"] == "message_stop"));
}

#[test]
fn round3_thousand_tools_complete() {
    let tools: Vec<_> = (0..1000).map(|index| json!({"name":format!("tool_{index:04}"),"input_schema":{"type":"object","properties":{"city":{"type":"string"}}}})).collect();
    let declarations: Vec<_> = tools
        .iter()
        .map(|tool| json!({"name":tool["name"],"parametersJsonSchema":tool["input_schema"]}))
        .collect();
    let mut stream = GeminiToClaudeStream::new(&json!({"tools":tools}))
        .with_translated_request(&json!({"tools":[{"functionDeclarations":declarations}]}));
    let mut all = events(
        &mut stream,
        &frame(
            json!([{"functionCall":{"name":"tool_0999","partialArgs":[{"jsonPath":"$.city","stringValue":"Paris"}]}}]),
            true,
        ),
    );
    all.extend(parse(&stream.translate(b"[DONE]")));
    assert!(!stream.incomplete_call);
    assert_eq!(arguments(&all), r#"{"city":"Paris"}"#);
    assert!(all.iter().any(|event| event["type"] == "message_stop"));
}

#[test]
fn round4_missing_empty_and_root_paths_continue_the_existing_claude_call() {
    for path in [None, Some(""), Some("$"), Some("  $  ")] {
        let mut stream = GeminiToClaudeStream::new(&json!({}));
        let mut all = events(
            &mut stream,
            &frame(
                json!([{"functionCall":{"name":"weather","willContinue":true,"partialArgs":[{"jsonPath":"$.city","stringValue":"New ","willContinue":true}]}}]),
                false,
            ),
        );
        let mut part = json!({"stringValue":"x"});
        if let Some(path) = path {
            part["jsonPath"] = json!(path);
        }
        let continuation = json!({"name":"weather","partialArgs":[part]});
        assert!(
            stream
                .active_call
                .as_ref()
                .unwrap()
                .can_continue(&continuation),
            "{path:?} must stay attached to the existing call before failing assembly"
        );
        all.extend(events(
            &mut stream,
            &frame(json!([{"functionCall":continuation}]), true),
        ));
        all.extend(parse(&stream.translate(b"[DONE]")));
        assert_eq!(
            all.iter()
                .filter(|event| event["content_block"]["type"] == "tool_use")
                .count(),
            1,
            "{path:?}"
        );
        assert!(stream.incomplete_call, "{path:?}");
        assert!(
            !all.iter().any(|event| event["type"] == "message_stop"),
            "{path:?}"
        );
    }
}

#[test]
fn round3_claude_fallback_keeps_exact_numbers() {
    let mut stream = GeminiToClaudeStream::new(&json!({}));
    let raw = br#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"numbers","willContinue":false,"args":{"exponent":1E5,"large":123456789012345678901234567890}}}]},"finishReason":"STOP"}]}"#;
    assert_eq!(
        arguments(&parse(&stream.translate(raw))),
        r#"{"exponent":1E5,"large":123456789012345678901234567890}"#
    );
}
