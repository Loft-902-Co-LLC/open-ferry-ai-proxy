// Ported from CLIProxyAPI internal/translator/gemini/openai/responses/noop_optimization_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The generation config the request translator builds from sampling, stop
//! and JSON schema settings. The test's name refers to how Go builds the
//! object; like upstream, it checks only the result.
//!
//! Dropped or changed tests: none.

use super::*;

/// gjson `Get(path).Float()`.
fn float_at(value: &Value, path: &str) -> f64 {
    at(value, path).and_then(Value::as_f64).unwrap_or(0.0)
}

#[test]
fn convert_openai_responses_request_to_gemini_builds_generation_config_without_intermediate_object()
{
    let input = parse(
        r#"{"input":"hello","temperature":0.5,"top_p":0.9,"stop_sequences":["done"],"text":{"format":{"type":"json_schema","schema":{"type":"object"}}}}"#,
    );

    let output = convert_openai_responses_request_to_gemini("gemini-test", &input, false);

    let got = float_at(&output, "generationConfig.temperature");
    assert_eq!(got, 0.5, "temperature");
    let got = float_at(&output, "generationConfig.topP");
    assert_eq!(got, 0.9, "topP");
    assert_eq!(
        text(&output, "generationConfig.stopSequences.0"),
        "done",
        "stop sequence"
    );
    assert_eq!(
        text(&output, "generationConfig.responseMimeType"),
        "application/json",
        "responseMimeType"
    );
    assert!(
        exists(&output, "generationConfig.responseJsonSchema"),
        "responseJsonSchema should be present"
    );
    assert!(
        !exists(&output, "generationConfig.responseSchema"),
        "responseSchema should not be present"
    );
}
