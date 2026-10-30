use super::*;

#[test]
fn responses_continuation_preserves_assembled_arguments() {
    let first = json!({"name":"weather","willContinue":true,"partialArgs":[{"jsonPath":"$.city","stringValue":"New ","willContinue":true}]});
    let second = json!({"name":"weather","willContinue":true,"partialArgs":[{"jsonPath":"$.city","stringValue":"York","willContinue":true}]});
    let mut expected = StreamingCall::new("weather".into(), String::new(), "fixture".into(), 0);
    expected.assembler.push(&first, None).unwrap();
    assert!(expected.can_continue(&second));
    expected.assembler.push(&second, None).unwrap();
    let mut stream = GeminiToOpenAIResponsesStream::new("gemini-test", &json!({}), &Value::Null);
    feed(&mut stream, &frame(json!([{"functionCall":first}]), false));
    feed(&mut stream, &frame(json!([{"functionCall":second}]), false));
    expected.assembler.push(&json!({}), None).unwrap();
    let actual = &mut stream.active_call.as_mut().unwrap().call.assembler;
    actual.push(&json!({}), None).unwrap();
    assert_eq!(
        actual.full_arguments().unwrap(),
        expected.assembler.full_arguments().unwrap()
    );
}

fn reasoning_done(events: &[Value]) -> usize {
    of_type(events, "response.output_item.done")
        .iter()
        .filter(|event| event["item"]["type"] == "reasoning")
        .count()
}

#[test]
fn responses_text_between_partial_chunks_preserves_the_call() {
    for request in [json!({}), patch_request()] {
        let mut stream = GeminiToOpenAIResponsesStream::new("gemini-test", &request, &Value::Null);
        let mut all = feed(
            &mut stream,
            &frame(
                json!([{"functionCall":{"name":"weather","id":"call_fixture","willContinue":true,"partialArgs":[{"jsonPath":"$.city","stringValue":"New ","willContinue":true}]}}]),
                false,
            ),
        );
        let middle = feed(&mut stream, &frame(json!([{"text":"Checking"}]), false));
        assert_eq!(
            of_type(&middle, "response.output_text.delta")[0]["delta"],
            "Checking"
        );
        assert!(of_type(&middle, "response.function_call_arguments.done").is_empty());
        all.extend(middle);
        all.extend(feed(&mut stream, &frame(json!([{"functionCall":{"partialArgs":[{"jsonPath":"$.city","stringValue":"York"}]}}]), true)));
        let joined: String = of_type(&all, "response.function_call_arguments.delta")
            .iter()
            .filter_map(|event| event["delta"].as_str())
            .collect();
        assert_eq!(joined, r#"{"city":"New York"}"#);
        assert_eq!(
            of_type(&all, "response.function_call_arguments.done")[0]["arguments"],
            joined
        );
        let message_done = all
            .iter()
            .position(|event| {
                event["type"] == "response.output_item.done" && event["item"]["type"] == "message"
            })
            .unwrap();
        let args_done = all
            .iter()
            .position(|event| event["type"] == "response.function_call_arguments.done")
            .unwrap();
        assert!(message_done < args_done);
        assert_eq!(of_type(&all, "response.completed").len(), 1);
        assert!(of_type(&all, "response.failed").is_empty());
        assert!(stream.finalize_tool_input().is_empty());
    }
}

#[test]
fn responses_signature_only_during_partial_call_is_dropped() {
    let mut stream = GeminiToOpenAIResponsesStream::new("gemini-test", &json!({}), &Value::Null);
    let mut all = feed(
        &mut stream,
        &frame(
            json!([{"functionCall":{"name":"weather","willContinue":true,"partialArgs":[{"jsonPath":"$.city","stringValue":"New ","willContinue":true}]}}]),
            false,
        ),
    );
    let middle = feed(
        &mut stream,
        &frame(
            json!([{"thoughtSignature":crate::gemini::openai::responses::test_support::GEMINI_SIGNATURE}]),
            false,
        ),
    );
    assert!(middle.is_empty());
    all.extend(feed(
        &mut stream,
        &frame(
            json!([{"functionCall":{"partialArgs":[{"jsonPath":"$.city","stringValue":"York"}]}}]),
            true,
        ),
    ));
    assert_eq!(reasoning_done(&all), 0);
    assert_eq!(of_type(&all, "response.completed").len(), 1);
}

#[test]
fn responses_empty_text_signature_uses_upstream_timing() {
    for prefix in [
        json!([]),
        json!([{"text":"Before"}]),
        json!([{"functionCall":{"name":"ping","args":{}}}]),
    ] {
        let earlier_item = !prefix.as_array().unwrap().is_empty();
        let mut stream =
            GeminiToOpenAIResponsesStream::new("gemini-test", &json!({}), &Value::Null);
        let mut all = feed(&mut stream, &frame(prefix, false));
        all.extend(feed(&mut stream, &frame(json!([{"functionCall":{"name":"weather","willContinue":true,"partialArgs":[{"jsonPath":"$.city","stringValue":"New ","willContinue":true}]}}]), false)));
        let middle = feed(
            &mut stream,
            &frame(
                json!([{"text":"","thoughtSignature":crate::gemini::openai::responses::test_support::GEMINI_SIGNATURE}]),
                false,
            ),
        );
        assert_eq!(reasoning_done(&middle), usize::from(earlier_item));
        all.extend(middle);
        all.extend(feed(&mut stream, &frame(json!([{"functionCall":{"partialArgs":[{"jsonPath":"$.city","stringValue":"York"}]}}]), true)));
        assert_eq!(reasoning_done(&all), 1);
        assert_eq!(of_type(&all, "response.completed").len(), 1);
    }
}

#[test]
fn responses_different_continuation_signature_flushes_the_earlier_one() {
    let mut stream = GeminiToOpenAIResponsesStream::new("gemini-test", &json!({}), &Value::Null);
    feed(
        &mut stream,
        &frame(
            json!([{"functionCall":{"name":"weather","willContinue":true,"partialArgs":[{"jsonPath":"$.city","stringValue":"New ","willContinue":true}]}}]),
            false,
        ),
    );
    let first = feed(
        &mut stream,
        &frame(
            json!([{"functionCall":{"willContinue":true,"partialArgs":[{"jsonPath":"$.city","stringValue":"Y","willContinue":true}]},"thoughtSignature":crate::gemini::openai::responses::test_support::GEMINI_SIGNATURE}]),
            false,
        ),
    );
    assert_eq!(reasoning_done(&first), 0);
    let second = feed(
        &mut stream,
        &frame(
            json!([{"functionCall":{"willContinue":true,"partialArgs":[{"jsonPath":"$.city","stringValue":"or","willContinue":true}]},"thoughtSignature":crate::gemini::openai::responses::test_support::different_gemini_signature()}]),
            false,
        ),
    );
    assert_eq!(reasoning_done(&second), 1);
    assert!(of_type(&second, "response.function_call_arguments.done").is_empty());
    let last = feed(
        &mut stream,
        &frame(
            json!([{"functionCall":{"partialArgs":[{"jsonPath":"$.city","stringValue":"k"}]}}]),
            true,
        ),
    );
    assert_eq!(reasoning_done(&last), 1);
    assert_eq!(
        of_type(&last, "response.function_call_arguments.done")[0]["arguments"],
        r#"{"city":"New York"}"#
    );
}

#[test]
fn responses_streaming_item_key_order_matches_exact_upstream_bytes() {
    for custom in [false, true] {
        let request = if custom {
            json!({"tools":[{"type":"custom","name":"fixture_tool"}]})
        } else {
            json!({})
        };
        let mut stream = GeminiToOpenAIResponsesStream::new("gemini-test", &request, &Value::Null);
        let raw = stream.translate_line(frame(json!([{"functionCall":{"name":"fixture_tool","id":"call_fixture","partialArgs":[{"jsonPath":"$.input","stringValue":"fixture"}]}}]), true).to_string().as_bytes());
        let (added, done) = if custom {
            (
                r#"{"id":"ctc_call_fixture","type":"custom_tool_call","status":"in_progress","input":"","call_id":"call_fixture","name":"fixture_tool"}"#,
                r#"{"id":"ctc_call_fixture","type":"custom_tool_call","status":"completed","input":"fixture","call_id":"call_fixture","name":"fixture_tool"}"#,
            )
        } else {
            (
                r#"{"id":"fc_call_fixture","type":"function_call","status":"in_progress","arguments":"","call_id":"call_fixture","name":"fixture_tool"}"#,
                r#"{"id":"fc_call_fixture","type":"function_call","status":"completed","arguments":"{\"input\":\"fixture\"}","call_id":"call_fixture","name":"fixture_tool"}"#,
            )
        };
        let items: Vec<_> = raw
            .lines()
            .filter(|line| line.starts_with("data: "))
            .filter_map(|line| {
                line.split_once("\"item\":")
                    .map(|(_, item)| item.strip_suffix('}').unwrap())
            })
            .collect();
        assert_eq!(items, [added, done]);
        assert_eq!(
            of_type(&parse(&raw), "response.completed")[0]["response"]["output"][0].to_string(),
            done
        );
    }
}

#[test]
fn responses_partial_args_preserve_exponent_and_large_number_spelling() {
    let mut stream = GeminiToOpenAIResponsesStream::new("gemini-test", &json!({}), &Value::Null);
    let raw = br#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"numbers","partialArgs":[{"jsonPath":"$.exponent","numberValue":1E5},{"jsonPath":"$.decimal","numberValue":-2.500e-3},{"jsonPath":"$.large","numberValue":123456789012345678901234567890},{"jsonPath":"$.huge","numberValue":1E400}]}}]},"finishReason":"STOP"}],"usageMetadata":{"totalTokenCount":1}}"#;
    let all = parse(&stream.translate_line(raw));
    let expected = r#"{"exponent":1E5,"decimal":-2.500e-3,"large":123456789012345678901234567890,"huge":1E400}"#;
    let joined: String = of_type(&all, "response.function_call_arguments.delta")
        .iter()
        .filter_map(|event| event["delta"].as_str())
        .collect();
    assert_eq!(joined, expected);
    assert_eq!(
        of_type(&all, "response.function_call_arguments.done")[0]["arguments"],
        expected
    );
    assert_eq!(
        of_type(&all, "response.completed")[0]["response"]["output"][0]["arguments"],
        expected
    );
}

#[test]
fn responses_finalize_after_finish_does_not_fail_or_complete_unfinished_arguments() {
    for patch_call in [false, true] {
        for finish in [false, true] {
            let mut stream =
                GeminiToOpenAIResponsesStream::new("gemini-test", &patch_request(), &Value::Null);
            let (name, path) = if patch_call {
                ("functions__apply_patch", "$.input")
            } else {
                ("weather", "$.city")
            };
            let mut all = feed(
                &mut stream,
                &frame(
                    json!([{"functionCall":{"name":name,"willContinue":true,"partialArgs":[{"jsonPath":path,"stringValue":"unfinished","willContinue":true}]}}]),
                    finish,
                ),
            );
            all.extend(parse(&stream.translate_line(b"data: [DONE]")));
            let finalized = parse(&stream.finalize_tool_input());
            assert_eq!(
                of_type(&finalized, "response.failed").len(),
                usize::from(!finish)
            );
            if finish {
                assert!(finalized.is_empty());
            }
            all.extend(finalized);
            assert!(of_type(&all, "response.completed").is_empty());
            assert!(of_type(&all, "response.function_call_arguments.done").is_empty());
            assert!(of_type(&all, "response.custom_tool_call_input.done").is_empty());
        }
    }
}

#[test]
fn round4_missing_empty_and_root_paths_continue_the_existing_responses_call() {
    for path in [None, Some(""), Some("$"), Some("  $  ")] {
        let mut stream =
            GeminiToOpenAIResponsesStream::new("gemini-test", &json!({}), &Value::Null);
        let mut all = feed(
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
                .call
                .can_continue(&continuation),
            "{path:?} must stay attached to the existing Responses call"
        );
        all.extend(feed(
            &mut stream,
            &frame(json!([{"functionCall":continuation}]), true),
        ));
        all.extend(parse(&stream.translate_line(b"data: [DONE]")));
        assert_eq!(
            of_type(&all, "response.output_item.added").len(),
            1,
            "{path:?}"
        );
        assert!(
            of_type(&all, "response.function_call_arguments.done").is_empty(),
            "{path:?}"
        );
        assert!(of_type(&all, "response.completed").is_empty(), "{path:?}");
    }
}
