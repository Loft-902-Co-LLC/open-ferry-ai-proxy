use super::*;

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
