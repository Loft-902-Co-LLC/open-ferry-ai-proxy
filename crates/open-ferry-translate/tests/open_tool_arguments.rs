// Ported from CLIProxyAPI internal/util/sanitize_test.go (MIT).
// https://github.com/router-for-me/CLIProxyAPI

use open_ferry_translate::gemini::claude::convert_gemini_response_to_claude_non_stream_with_translated;
use serde_json::{Value, json};

#[test]
fn implicit_open_tool_arguments_survive_native_response_translation() {
    for closed in [false, true] {
        let mut schema = json!({"type":"object","properties":{}});
        if closed {
            schema["additionalProperties"] = json!(false);
        }
        let original = json!({"tools":[{"name":"echo","input_schema":schema}]});
        let sent = json!({"tools":[{"functionDeclarations":[{"name":"echo","parametersJsonSchema":schema}]}]});
        let args = json!({"user":"v"});
        let response = json!({"candidates":[{"content":{"parts":[{"functionCall":{
            "name":"echo","args":args}}]},"finishReason":"STOP"}]});
        for translated in [&Value::Null, &sent] {
            let out = convert_gemini_response_to_claude_non_stream_with_translated(
                &original, translated, &response,
            );
            assert_eq!(
                out["content"][0]["input"],
                if closed { json!({}) } else { args.clone() }
            );
        }
    }
}
