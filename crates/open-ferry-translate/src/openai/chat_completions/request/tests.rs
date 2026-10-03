// Ported from CLIProxyAPI internal/translator/openai/openai/chat-completions/openai_openai_request_test.go
// (v8.0.10, MIT). https://github.com/router-for-me/CLIProxyAPI
//
// reuses_matching_model_payload checks the request comes back unchanged;
// upstream checks it comes back without a copy, which a request passed by
// value can't show.

use serde_json::json;

use super::*;

#[test]
fn reuses_matching_model_payload() {
    let input = json!({"model": "gpt-test", "messages": [{"role": "user", "content": "hello"}]});
    let output = convert_openai_request_to_openai("gpt-test", input.clone());
    assert_eq!(
        serde_json::to_string(&output).unwrap(),
        serde_json::to_string(&input).unwrap()
    );
}

#[test]
fn updates_different_model() {
    let input = json!({"model": "old-model", "messages": []});
    let output = convert_openai_request_to_openai("new-model", input);
    assert_eq!(output["model"], "new-model");
}

#[test]
fn adds_missing_model_last() {
    let output = convert_openai_request_to_openai("m", json!({"messages": [], "stream": true}));
    assert_eq!(
        serde_json::to_string(&output).unwrap(),
        r#"{"messages":[],"stream":true,"model":"m"}"#
    );
}

#[test]
fn leaves_array_unchanged() {
    let output = convert_openai_request_to_openai("m", json!([1]));
    assert_eq!(output, json!([1]));
}
