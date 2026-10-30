use super::*;

#[test]
fn round4_missing_empty_and_root_paths_keep_one_failed_chat_call() {
    for path in [None, Some(""), Some("$"), Some("  $  ")] {
        let mut stream = GeminiToOpenAIStream::new(&Value::Null);
        let mut chunks = feed(
            &mut stream,
            &frame(
                json!({"name":"weather","willContinue":true,"partialArgs":[{"jsonPath":"$.city","stringValue":"New ","willContinue":true}]}),
                false,
            ),
        );
        let mut part = json!({"stringValue":"x"});
        if let Some(path) = path {
            part["jsonPath"] = json!(path);
        }
        let continuation = json!({"name":"weather","partialArgs":[part]});
        assert!(
            stream.active_calls[&0].can_continue(&continuation),
            "{path:?} must stay attached to the existing Chat call"
        );
        chunks.extend(feed(&mut stream, &frame(continuation, true)));
        let calls = calls(&chunks);
        assert_eq!(
            calls.iter().filter(|call| call.get("id").is_some()).count(),
            1,
            "{path:?}"
        );
        assert!(calls.iter().all(|call| call["index"] == 0), "{path:?}");
        assert_eq!(
            calls
                .iter()
                .filter_map(|call| call["function"]["arguments"].as_str())
                .collect::<String>(),
            r#"{"city":"New "#,
            "{path:?}"
        );
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk["choices"][0]["finish_reason"].is_null()),
            "{path:?}"
        );
    }
}

#[test]
fn chat_partial_args_preserve_exponent_and_large_number_spelling() {
    let mut stream = GeminiToOpenAIStream::new(&Value::Null);
    let raw = br#"{"candidates":[{"index":0,"content":{"parts":[{"functionCall":{"name":"numbers","partialArgs":[{"jsonPath":"$.exponent","numberValue":1E5},{"jsonPath":"$.decimal","numberValue":-2.500e-3},{"jsonPath":"$.large","numberValue":123456789012345678901234567890},{"jsonPath":"$.huge","numberValue":1E400}]}}]},"finishReason":"STOP"}],"usageMetadata":{"totalTokenCount":1}}"#;
    let chunks = stream.translate(raw);
    let arguments: String = calls(&chunks)
        .iter()
        .filter_map(|call| call["function"]["arguments"].as_str())
        .collect();
    assert_eq!(
        arguments,
        r#"{"exponent":1E5,"decimal":-2.500e-3,"large":123456789012345678901234567890,"huge":1E400}"#
    );
}
