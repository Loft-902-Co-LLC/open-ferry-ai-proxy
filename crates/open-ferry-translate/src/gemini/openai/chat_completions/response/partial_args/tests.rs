// Native equivalents of 64316127's Gemini-to-Chat partialArgs cases.
use super::*;

mod review_tests;

fn frame(call: Value, finish: bool) -> Value {
    let mut frame = json!({"candidates":[{"index":0,"content":{"parts":[{"functionCall":call}]}}]});
    if finish {
        frame["candidates"][0]["finishReason"] = "STOP".into();
        frame["usageMetadata"] = json!({"totalTokenCount":1});
    }
    frame
}

fn feed(stream: &mut GeminiToOpenAIStream, frame: &Value) -> Vec<Value> {
    stream.translate(frame.to_string().as_bytes())
}

fn calls(chunks: &[Value]) -> Vec<&Value> {
    chunks
        .iter()
        .flat_map(|chunk| chunk["choices"].as_array().unwrap())
        .flat_map(|choice| {
            choice["delta"]["tool_calls"]
                .as_array()
                .into_iter()
                .flatten()
        })
        .collect()
}

#[test]
fn vertex_partial_args_stream_has_one_initial_identity_and_arguments_only_continuations() {
    let mut stream = GeminiToOpenAIStream::new(&Value::Null);
    let mut chunks = feed(
        &mut stream,
        &frame(
            json!({"name":"weather","id":"call_1","willContinue":true,"partialArgs":[{"jsonPath":"$.city","stringValue":"New ","willContinue":true}]}),
            false,
        ),
    );
    assert_eq!(
        calls(&chunks)[0]["function"]["arguments"],
        r#"{"city":"New "#
    );
    let empty = feed(
        &mut stream,
        &json!({"candidates":[{"index":0,"content":{"parts":[{"text":""},{"thoughtSignature":"ignored"}]}}]}),
    );
    assert!(
        empty
            .iter()
            .all(|chunk| chunk["choices"][0]["delta"]["tool_calls"].is_null())
    );
    chunks.extend(feed(
        &mut stream,
        &frame(
            json!({"name":"weather","partialArgs":[{"jsonPath":"$.city","stringValue":"York"}]}),
            true,
        ),
    ));
    let calls = calls(&chunks);
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["id"], "call_1");
    assert!(calls[1].get("id").is_none());
    assert!(calls[1]["function"].get("name").is_none());
    assert!(calls.iter().all(|call| call["index"] == 0));
    assert_eq!(
        calls
            .iter()
            .filter_map(|call| call["function"]["arguments"].as_str())
            .collect::<String>(),
        r#"{"city":"New York"}"#
    );
    assert_eq!(
        chunks.last().unwrap()["choices"][0]["finish_reason"],
        "tool_calls"
    );
}

#[test]
fn multiple_completed_calls_keep_indices_and_scalar_types() {
    let mut stream = GeminiToOpenAIStream::new(&Value::Null);
    let mut chunks = feed(
        &mut stream,
        &frame(
            json!({"name":"weather","partialArgs":[
        {"jsonPath":"$.city","stringValue":"Tokyo"},{"jsonPath":"$.days","numberValue":0},
        {"jsonPath":"$.metric","boolValue":false},{"jsonPath":"$.extra","nullValue":null}]}),
            false,
        ),
    );
    chunks.extend(feed(
        &mut stream,
        &frame(
            json!({"name":"weather","partialArgs":[{"jsonPath":"$.city","stringValue":"Osaka"}]}),
            true,
        ),
    ));
    let calls = calls(&chunks);
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["index"], 0);
    assert_eq!(calls[1]["index"], 1);
    assert_ne!(calls[0]["id"], calls[1]["id"]);
    assert_eq!(
        calls[0]["function"]["arguments"],
        r#"{"city":"Tokyo","days":0,"metric":false,"extra":null}"#
    );
    assert_eq!(calls[1]["function"]["arguments"], r#"{"city":"Osaka"}"#);
}

#[test]
fn truncated_replaced_interrupted_invalid_calls_cannot_emit_finish_reason() {
    for next in [
        None,
        Some(json!({"content":{"parts":[{"text":"interrupt"}]}})),
        Some(
            json!({"content":{"parts":[{"functionCall":{"name":"weather","id":"call_2","partialArgs":[{"jsonPath":"$.city","stringValue":"Paris"}]}}]}}),
        ),
        Some(
            json!({"content":{"parts":[{"functionCall":{"partialArgs":[{"jsonPath":"$.city.x","numberValue":1}]}}]}}),
        ),
    ] {
        let mut stream = GeminiToOpenAIStream::new(&Value::Null);
        let mut chunks = feed(
            &mut stream,
            &frame(
                json!({"name":"weather","id":"call_1","willContinue":true,"partialArgs":[{"jsonPath":"$.city","stringValue":"New ","willContinue":true}]}),
                false,
            ),
        );
        if let Some(mut candidate) = next {
            candidate["index"] = 0.into();
            candidate["finishReason"] = "STOP".into();
            chunks.extend(feed(&mut stream, &json!({"candidates":[candidate]})));
        }
        chunks.extend(feed(&mut stream, &json!({"candidates":[{"index":0,"finishReason":"STOP"}],"usageMetadata":{"totalTokenCount":1}})));
        chunks.extend(stream.translate(b"[DONE]"));
        assert!(chunks.iter().all(|chunk| {
            chunk["choices"]
                .as_array()
                .unwrap()
                .iter()
                .all(|choice| choice["finish_reason"].is_null())
        }));
    }
}

#[test]
fn candidates_are_independent_and_parameterless_fallbacks_are_complete() {
    let mut stream = GeminiToOpenAIStream::new(&Value::Null);
    let first = feed(
        &mut stream,
        &json!({"candidates":[
        {"index":0,"content":{"parts":[{"functionCall":{"name":"slow","willContinue":true}}]}},
        {"index":3,"content":{"parts":[{"functionCall":{"name":"ping","willContinue":false}}]},"finishReason":"STOP"}],"usageMetadata":{"totalTokenCount":1}}),
    );
    assert_eq!(first[1]["choices"][0]["index"], 3);
    assert_eq!(first[1]["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(calls(&first)[1]["function"]["arguments"], "{}");
    let next = feed(
        &mut stream,
        &frame(
            json!({"name":"fallback","args":{"city":"Paris"},"partialArgs":[]}),
            true,
        ),
    );
    assert_eq!(
        calls(&next)[0]["function"]["arguments"],
        r#"{"city":"Paris"}"#
    );
    assert!(next[0]["choices"][0]["finish_reason"].is_null());
}
