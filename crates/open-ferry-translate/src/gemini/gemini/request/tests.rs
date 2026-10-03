// Ported from CLIProxyAPI internal/translator/gemini/gemini/gemini_gemini_request_test.go
// (v8.0.10, MIT). https://github.com/router-for-me/CLIProxyAPI
//
// TestConvertGeminiRequestToGeminiReusesLargeNormalizedPayload and
// BenchmarkConvertGeminiRequestToGeminiLargeInlineData are dropped: they check
// that upstream reuses the input buffer and how much it allocates, which a
// request passed by value doesn't show. The tests after the ported ones are
// new; their expected output comes from upstream.

use serde_json::{Value, json};

use super::*;

/// Translates `input` and returns the result without its safety settings,
/// as compact JSON, checking the defaults were added.
fn convert(input: &str) -> String {
    let mut output =
        convert_gemini_request_to_gemini("m", serde_json::from_str(input).unwrap(), false);
    let settings = output
        .as_object_mut()
        .and_then(|fields| fields.shift_remove("safetySettings"));
    assert_eq!(
        settings.as_ref().and_then(Value::as_array).map(Vec::len),
        Some(5),
        "{input}"
    );
    output.to_string()
}

fn backfill(input: Value) -> Value {
    let mut output = input;
    backfill_empty_function_response_names(&mut output);
    output
}

#[test]
fn backfill_single() {
    let out = backfill(json!({"contents": [
        {"role": "model", "parts": [{"functionCall": {"name": "Bash", "args": {"cmd": "ls"}}}]},
        {"role": "user", "parts": [
            {"functionResponse": {"name": "", "response": {"output": "file1.txt"}}}
        ]}
    ]}));
    assert_eq!(
        out["contents"][1]["parts"][0]["functionResponse"]["name"],
        "Bash"
    );
}

#[test]
fn backfill_parallel() {
    let out = backfill(json!({"contents": [
        {"role": "model", "parts": [
            {"functionCall": {"name": "Read", "args": {"path": "/a"}}},
            {"functionCall": {"name": "Grep", "args": {"pattern": "x"}}}
        ]},
        {"role": "user", "parts": [
            {"functionResponse": {"name": "", "response": {"result": "content a"}}},
            {"functionResponse": {"name": "", "response": {"result": "match x"}}}
        ]}
    ]}));
    assert_eq!(
        out["contents"][1]["parts"][0]["functionResponse"]["name"],
        "Read"
    );
    assert_eq!(
        out["contents"][1]["parts"][1]["functionResponse"]["name"],
        "Grep"
    );
}

#[test]
fn backfill_preserves_existing() {
    let out = backfill(json!({"contents": [
        {"role": "model", "parts": [{"functionCall": {"name": "Bash", "args": {}}}]},
        {"role": "user", "parts": [
            {"functionResponse": {"name": "Bash", "response": {"result": "ok"}}}
        ]}
    ]}));
    assert_eq!(
        out["contents"][1]["parts"][0]["functionResponse"]["name"],
        "Bash"
    );
}

#[test]
fn convert_backfills_empty_name() {
    let out = convert_gemini_request_to_gemini(
        "",
        json!({"contents": [
            {"role": "model", "parts": [{"functionCall": {"name": "Bash", "args": {"cmd": "ls"}}}]},
            {"role": "user", "parts": [
                {"functionResponse": {"name": "", "response": {"output": "file1.txt"}}}
            ]}
        ]}),
        false,
    );
    assert_eq!(
        out["contents"][1]["parts"][0]["functionResponse"]["name"],
        "Bash"
    );
}

#[test]
fn backfill_more_responses_than_calls() {
    let out = backfill(json!({"contents": [
        {"role": "model", "parts": [{"functionCall": {"name": "Bash", "args": {}}}]},
        {"role": "user", "parts": [
            {"functionResponse": {"name": "", "response": {"result": "ok"}}},
            {"functionResponse": {"name": "", "response": {"result": "extra"}}}
        ]}
    ]}));
    assert_eq!(
        out["contents"][1]["parts"][0]["functionResponse"]["name"],
        "Bash"
    );
    assert_eq!(
        out["contents"][1]["parts"][1]["functionResponse"]["name"],
        ""
    );
}

#[test]
fn backfill_multiple_groups() {
    let out = backfill(json!({"contents": [
        {"role": "model", "parts": [{"functionCall": {"name": "Read", "args": {}}}]},
        {"role": "user", "parts": [
            {"functionResponse": {"name": "", "response": {"result": "content"}}}
        ]},
        {"role": "model", "parts": [{"functionCall": {"name": "Grep", "args": {}}}]},
        {"role": "user", "parts": [
            {"functionResponse": {"name": "", "response": {"result": "match"}}}
        ]}
    ]}));
    assert_eq!(
        out["contents"][1]["parts"][0]["functionResponse"]["name"],
        "Read"
    );
    assert_eq!(
        out["contents"][3]["parts"][0]["functionResponse"]["name"],
        "Grep"
    );
}

#[test]
fn function_response_with_invalid_role_normalizes_to_user() {
    let out = convert_gemini_request_to_gemini(
        "gemini-3-flash",
        json!({"contents": [
            {"role": "user", "parts": [{"text": "reminder"}]},
            {"role": "invalid", "parts": [{"functionResponse": {"name": "lookup", "response": {}}}]}
        ]}),
        false,
    );
    let contents = out["contents"].as_array().unwrap();
    assert_eq!(contents.len(), 2);
    assert_eq!(contents[1]["role"], "user");
}

#[test]
fn without_contents_only_adds_safety_settings() {
    assert_eq!(
        convert(r#"{"tools":[{"functionDeclarations":[{"parameters":1}]}]}"#),
        r#"{"tools":[{"functionDeclarations":[{"parameters":1}]}]}"#
    );
    assert_eq!(convert(r#""s""#), "{}");
    let output = convert_gemini_request_to_gemini("m", json!([1]), false);
    assert_eq!(output, json!([1]));
}

#[test]
fn keeps_existing_safety_settings() {
    let output = convert_gemini_request_to_gemini(
        "m",
        json!({"contents": [], "safetySettings": null}),
        false,
    );
    assert_eq!(
        output.to_string(),
        r#"{"contents":[],"safetySettings":null}"#
    );
}

#[test]
fn renames_tool_and_schema_fields() {
    assert_eq!(
        convert(concat!(
            r#"{"tools":[{"functionDeclarations":[{"parameters":{"a":1},"name":"f"}],"#,
            r#""function_declarations":1,"x":2},"s",{"function_declarations":[{"parameters":null},3]}],"#,
            r#""contents":[],"generationConfig":{"responseSchema":{"t":1},"x":1}}"#
        )),
        concat!(
            r#"{"tools":[{"function_declarations":[{"name":"f","parametersJsonSchema":{"a":1}}],"x":2},"#,
            r#""s",{"function_declarations":[{"parametersJsonSchema":null},3]}],"#,
            r#""contents":[],"generationConfig":{"x":1,"responseJsonSchema":{"t":1}}}"#
        )
    );
}

#[test]
fn fixes_roles_in_array_contents() {
    assert_eq!(
        convert(r#"{"contents":[{"role":"x"},"s",[1],{"role":"model"},{"role":3}]}"#),
        r#"{"contents":[{"role":"user"},{"role":"model"},[1],{"role":"model"},{"role":"user"}]}"#
    );
}

#[test]
fn fixes_roles_in_other_contents_by_position() {
    assert_eq!(
        convert(
            r#"{"contents":{"a":{"role":"x"},"b":{"parts":[{"functionResponse":{}}]},"c":"s"}}"#
        ),
        concat!(
            r#"{"contents":{"a":{"role":"x"},"b":{"parts":[{"functionResponse":{}}]},"c":"s","#,
            r#""0":{"role":"user"},"1":{"role":"user"},"2":{"role":"model"}}}"#
        )
    );
    assert_eq!(
        convert(r#"{"contents":"x"}"#),
        r#"{"contents":[{"role":"user"}]}"#
    );
    assert_eq!(
        convert(r#"{"contents":null}"#),
        r#"{"contents":[{"role":"user"}]}"#
    );
}

#[test]
fn backfills_through_object_parts_by_key() {
    assert_eq!(
        convert(concat!(
            r#"{"contents":[{"role":"model","parts":[{"functionCall":{"name":"A"}},"#,
            r#"{"functionCall":{"name":"B"}}]},{"role":"user","parts":{"k":{"functionResponse":{"name":"N"}},"#,
            r#""-1":{"functionResponse":{"name":" "}},"zz":{"functionResponse":{}}}}]}"#
        )),
        concat!(
            r#"{"contents":[{"role":"model","parts":[{"functionCall":{"name":"A"},"#,
            r#""thoughtSignature":"skip_thought_signature_validator"},{"functionCall":{"name":"B"}}]},"#,
            r#"{"role":"user","parts":{"k":{"functionResponse":{"name":"N"}},"#,
            r#""-1":{"functionResponse":{"name":"B"}},"zz":{"functionResponse":{}}}}]}"#
        )
    );
}

#[test]
fn backfills_object_contents_at_the_key_read_as_an_index() {
    assert_eq!(
        convert(concat!(
            r#"{"contents":{"abc":{"role":"model","parts":[{"functionCall":{"name":"A"}}]},"#,
            r#""def":{"role":"user","parts":[{"functionResponse":null}]}}}"#
        )),
        concat!(
            r#"{"contents":{"abc":{"role":"model","parts":[{"functionCall":{"name":"A"}}]},"#,
            r#""def":{"role":"user","parts":[{"functionResponse":null}]},"#,
            r#""0":{"parts":[{"functionResponse":{"name":"A"}}]}}}"#
        )
    );
}

#[test]
fn backfill_replaces_scalar_responses_and_skips_arrays() {
    assert_eq!(
        convert(concat!(
            r#"{"contents":[{"role":"model","parts":[{"functionCall":{"name":"A"}},{"functionCall":"s"}]},"#,
            r#"{"role":"user","parts":[{"functionResponse":"s"},{"functionResponse":[1]}]}]}"#
        )),
        concat!(
            r#"{"contents":[{"role":"model","parts":[{"functionCall":{"name":"A"},"#,
            r#""thoughtSignature":"skip_thought_signature_validator"},{"functionCall":"s"}]},"#,
            r#"{"role":"user","parts":[{"functionResponse":{"name":"A"}},{"functionResponse":[1]}]}]}"#
        )
    );
}

#[test]
fn backfill_fills_only_the_turn_after_the_model_turn() {
    let out = backfill(json!({"contents": [
        {"role": "model", "parts": [{"functionCall": {"name": "A"}}]},
        {"role": "user", "parts": [{"text": "hi"}]},
        {"role": "user", "parts": [{"functionResponse": {"name": ""}}]}
    ]}));
    assert_eq!(
        out["contents"][2]["parts"][0]["functionResponse"]["name"],
        ""
    );
}

#[test]
fn backfill_takes_numeric_call_names_as_text() {
    let out = backfill(json!({"contents": [
        {"role": "model", "parts": [{"functionCall": {"name": 12}}, {"functionCall": {}}]},
        {"role": "user", "parts": [
            {"functionResponse": {"name": null}},
            {"functionResponse": {"name": "\u{a0}"}}
        ]}
    ]}));
    assert_eq!(
        out["contents"][1]["parts"][0]["functionResponse"]["name"],
        "12"
    );
    assert_eq!(
        out["contents"][1]["parts"][1]["functionResponse"]["name"],
        ""
    );
}
