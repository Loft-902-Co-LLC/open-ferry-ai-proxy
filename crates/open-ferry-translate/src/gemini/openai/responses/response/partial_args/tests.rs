// Native equivalents of 64316127's Gemini-to-Responses partialArgs cases.
use super::*;

mod review_tests;

fn frame(parts: Value, finish: bool) -> Value {
    let mut frame = json!({"responseId":"resp-fixture","candidates":[{"content":{"parts":parts}}]});
    if finish {
        frame["candidates"][0]["finishReason"] = "STOP".into();
        frame["usageMetadata"] = json!({"totalTokenCount":1});
    }
    frame
}

fn parse(raw: &str) -> Vec<Value> {
    raw.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn feed(stream: &mut GeminiToOpenAIResponsesStream, frame: &Value) -> Vec<Value> {
    parse(&stream.translate_line(format!("data: {frame}").as_bytes()))
}

fn of_type<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|event| event["type"] == kind)
        .collect()
}

#[test]
fn vertex_partial_args_stream_done_matches_deltas_and_item_identity() {
    let mut stream = GeminiToOpenAIResponsesStream::new("gemini-test", &json!({}), &Value::Null);
    let first = feed(
        &mut stream,
        &frame(
            json!([{"functionCall":{"name":"weather","id":"call_1","willContinue":true,"partialArgs":[{"jsonPath":"$.city","stringValue":"New ","willContinue":true}]}}]),
            false,
        ),
    );
    assert!(of_type(&first, "response.function_call_arguments.done").is_empty());
    assert_eq!(
        of_type(&first, "response.function_call_arguments.delta")[0]["delta"],
        r#"{"city":"New "#
    );
    let mut all = first;
    all.extend(feed(&mut stream, &frame(json!([{"functionCall":{"name":"weather","partialArgs":[{"jsonPath":"$.city","stringValue":"York"}]}}]), true)));
    let delta: String = of_type(&all, "response.function_call_arguments.delta")
        .iter()
        .filter_map(|event| event["delta"].as_str())
        .collect();
    assert_eq!(delta, r#"{"city":"New York"}"#);
    assert_eq!(
        of_type(&all, "response.function_call_arguments.done")[0]["arguments"],
        delta
    );
    let added = of_type(&all, "response.output_item.added")[0];
    let done = of_type(&all, "response.output_item.done")[0];
    assert_eq!(added["item"]["id"], "fc_call_1");
    assert_eq!(done["item"]["id"], added["item"]["id"]);
    assert_eq!(done["item"]["arguments"], delta);
    assert_eq!(
        of_type(&all, "response.completed")[0]["response"]["output"][0]["id"],
        "fc_call_1"
    );
}

#[test]
fn repeated_calls_nested_arrays_scalars_and_deferred_signatures() {
    let signature = crate::gemini::openai::responses::test_support::GEMINI_SIGNATURE;
    let mut stream = GeminiToOpenAIResponsesStream::new("gemini-test", &json!({}), &Value::Null);
    let mut all = feed(
        &mut stream,
        &frame(
            json!([
        {"text":"","thoughtSignature":signature},
        {"functionCall":{"name":"query_db","willContinue":true,"partialArgs":[
            {"jsonPath":"$.filter.tags[0]","stringValue":"golang"},
            {"jsonPath":"$.filter.tags[1]","stringValue":"ai"},
            {"jsonPath":"$.limit","numberValue":0},{"jsonPath":"$.strict","boolValue":false},{"jsonPath":"$.cursor","nullValue":null}]}},
        {"text":""},{"thoughtSignature":crate::gemini::openai::responses::test_support::different_gemini_signature()}]),
            false,
        ),
    );
    assert!(
        of_type(&all, "response.output_item.done")
            .iter()
            .all(|event| event["item"]["type"] != "function_call")
    );
    all.extend(feed(&mut stream, &frame(json!([
        {"functionCall":{}},
        {"functionCall":{"name":"query_db","partialArgs":[{"jsonPath":"$.filter.tags[0]","stringValue":"rust"}]}}
    ]), true)));
    let done = of_type(&all, "response.function_call_arguments.done");
    assert_eq!(done.len(), 2);
    assert_eq!(
        done[0]["arguments"],
        r#"{"filter":{"tags":["golang","ai"]},"limit":0,"strict":false,"cursor":null}"#
    );
    assert_eq!(done[1]["arguments"], r#"{"filter":{"tags":["rust"]}}"#);
    assert_ne!(done[0]["item_id"], done[1]["item_id"]);
    assert_eq!(
        of_type(&all, "response.output_item.done")
            .iter()
            .filter(|event| event["item"]["type"] == "reasoning")
            .count(),
        1
    );
    assert_eq!(of_type(&all, "response.completed").len(), 1);
}

#[test]
fn truncated_replaced_invalid_calls_have_no_successful_completion() {
    for next in [
        None,
        Some(
            json!([{"functionCall":{"name":"weather","id":"call_b","partialArgs":[{"jsonPath":"$.city","stringValue":"Paris"}]}}]),
        ),
        Some(json!([{"functionCall":{"partialArgs":[{"jsonPath":"$.city.x","numberValue":1}]}}])),
    ] {
        let mut stream =
            GeminiToOpenAIResponsesStream::new("gemini-test", &json!({}), &Value::Null);
        let mut all = feed(
            &mut stream,
            &frame(
                json!([{"functionCall":{"name":"weather","id":"call_a","willContinue":true,"partialArgs":[{"jsonPath":"$.city","stringValue":"New ","willContinue":true}]}}]),
                false,
            ),
        );
        if let Some(next) = next {
            all.extend(feed(&mut stream, &frame(next, true)));
        }
        all.extend(parse(&stream.translate_line(b"data: [DONE]")));
        all.extend(parse(&stream.finalize_tool_input()));
        assert!(of_type(&all, "response.completed").is_empty());
        assert!(
            of_type(&all, "response.function_call_arguments.done")
                .iter()
                .all(|event| event["item_id"] != "fc_call_a")
        );
        assert!(
            of_type(&all, "response.output_item.done")
                .iter()
                .all(|event| event["item"]["id"] != "fc_call_a")
        );
    }
}

#[test]
fn parameterless_and_empty_partial_args_fallback() {
    let mut stream = GeminiToOpenAIResponsesStream::new("gemini-test", &json!({}), &Value::Null);
    let all = feed(
        &mut stream,
        &frame(
            json!([
                {"functionCall":{"name":"ping","willContinue":false}},
                {"functionCall":{"name":"weather","args":{"city":"Paris"},"partialArgs":[]}}
            ]),
            true,
        ),
    );
    let done = of_type(&all, "response.function_call_arguments.done");
    assert_eq!(done[0]["arguments"], "{}");
    assert_eq!(done[1]["arguments"], r#"{"city":"Paris"}"#);
}

fn patch_request() -> Value {
    json!({"tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"apply_patch","format":{"type":"grammar","definition":"start: patch"}}]}]})
}

#[test]
fn apply_patch_partial_args_reuse_strict_decoder_and_namespace_identity() {
    let mut stream =
        GeminiToOpenAIResponsesStream::new("gemini-test", &patch_request(), &Value::Null);
    let mut all = feed(
        &mut stream,
        &frame(
            json!([{"functionCall":{"name":"functions__apply_patch","id":"patch_fixture","willContinue":true,"partialArgs":[{"jsonPath":"$.input","stringValue":"*** Begin Patch\n","willContinue":true}]}}]),
            false,
        ),
    );
    all.extend(feed(&mut stream, &frame(json!([{"functionCall":{"partialArgs":[{"jsonPath":"$.input","stringValue":"*** End Patch\n"}]}}]), true)));
    let input: String = of_type(&all, "response.custom_tool_call_input.delta")
        .iter()
        .filter_map(|event| event["delta"].as_str())
        .collect();
    assert_eq!(input, "*** Begin Patch\n*** End Patch\n");
    let done = of_type(&all, "response.custom_tool_call_input.done")[0];
    assert_eq!(done["input"], input);
    assert_eq!(done["item_id"], "ctc_patch_fixture");
    assert_eq!(done["call_id"], "patch_fixture");
    let item = &of_type(&all, "response.output_item.done")[0]["item"];
    assert_eq!(item["namespace"], "functions");
    assert_eq!(item["name"], "apply_patch");
    assert_eq!(
        of_type(&all, "response.completed")[0]["response"]["output"][0]["input"],
        input
    );
}

#[test]
fn partial_patch_and_snapshot_evidence_cannot_disagree_or_duplicate_output() {
    for same in [true, false] {
        let mut stream =
            GeminiToOpenAIResponsesStream::new("gemini-test", &patch_request(), &Value::Null);
        let mut all = feed(
            &mut stream,
            &frame(
                json!([{"functionCall":{"name":"functions__apply_patch","id":"patch_fixture","partialArgs":[{"jsonPath":"$.input","stringValue":"first"}]}}]),
                false,
            ),
        );
        all.extend(feed(&mut stream, &frame(json!([{"functionCall":{"name":"functions__apply_patch","id":"patch_fixture","args":{"input":if same {"first"} else {"different"}}}}]), true)));
        assert_eq!(
            of_type(&all, "response.custom_tool_call_input.done").len(),
            1
        );
        assert_eq!(of_type(&all, "response.failed").len(), usize::from(!same));
        assert_eq!(of_type(&all, "response.completed").len(), usize::from(same));
    }
    let mut stream =
        GeminiToOpenAIResponsesStream::new("gemini-test", &patch_request(), &Value::Null);
    let mut all = feed(
        &mut stream,
        &frame(
            json!([{"functionCall":{"name":"functions__apply_patch","id":"patch_fixture","args":{"input":"first"}}}]),
            false,
        ),
    );
    all.extend(feed(&mut stream, &frame(json!([{"functionCall":{"name":"functions__apply_patch","id":"patch_fixture","partialArgs":[{"jsonPath":"$.input","stringValue":"different"}]}}]), true)));
    assert_eq!(of_type(&all, "response.failed").len(), 1);
    assert_eq!(
        of_type(&all, "response.custom_tool_call_input.done").len(),
        1
    );
    assert!(of_type(&all, "response.completed").is_empty());
}

#[test]
fn partial_patch_rejects_conflicting_explicit_part_indexes() {
    let mut stream =
        GeminiToOpenAIResponsesStream::new("gemini-test", &patch_request(), &Value::Null);
    let mut all = feed(
        &mut stream,
        &frame(
            json!([{"index":1,"functionCall":{"name":"functions__apply_patch","id":"patch_fixture","willContinue":true,"partialArgs":[{"jsonPath":"$.input","stringValue":"a","willContinue":true}]}}]),
            false,
        ),
    );
    all.extend(feed(&mut stream, &frame(json!([{"index":2,"functionCall":{"partialArgs":[{"jsonPath":"$.input","stringValue":"b"}]}}]), true)));
    assert_eq!(of_type(&all, "response.failed").len(), 1);
    assert!(of_type(&all, "response.custom_tool_call_input.done").is_empty());
}

#[test]
fn invalid_and_unterminated_partial_patch_calls_fail_without_success_or_input_leak() {
    for final_call in [
        None,
        Some(json!({"partialArgs":[{"jsonPath":"$.extra","numberValue":1}]})),
        Some(json!({"partialArgs":[{"jsonPath":"$.input.x","numberValue":1}]})),
    ] {
        let mut stream =
            GeminiToOpenAIResponsesStream::new("gemini-test", &patch_request(), &Value::Null);
        let mut all = feed(
            &mut stream,
            &frame(
                json!([{"functionCall":{"name":"functions__apply_patch","willContinue":true,"partialArgs":[{"jsonPath":"$.input","stringValue":"fixture-private","willContinue":true}]}}]),
                false,
            ),
        );
        if let Some(call) = final_call {
            all.extend(feed(
                &mut stream,
                &frame(json!([{"functionCall":call}]), true),
            ));
        }
        all.extend(parse(&stream.finalize_tool_input()));
        let failed = of_type(&all, "response.failed");
        assert_eq!(failed.len(), 1);
        assert_eq!(
            failed[0]["response"]["error"]["code"],
            "invalid_tool_arguments"
        );
        assert!(!failed[0].to_string().contains("fixture-private"));
        assert!(of_type(&all, "response.custom_tool_call_input.done").is_empty());
        assert!(of_type(&all, "response.completed").is_empty());
    }
}
