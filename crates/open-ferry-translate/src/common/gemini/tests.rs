//! Ports internal/translator/common/gemini_test.go.
//!
//! Dropped: `TestSplitGeminiFunctionResponseTurns`, as only the Antigravity
//! translators use the function it tests.
//!
//! Changed: the zero-byte turn in "skips empty contents or contents with empty
//! parts" is `null` here, as turns are parsed JSON.

use serde_json::{Value, json};

use super::*;
use crate::json::path;

fn values(texts: &[&str]) -> Vec<Value> {
    texts
        .iter()
        .map(|text| serde_json::from_str(text).unwrap())
        .collect()
}

fn text<'v>(value: &'v Value, at: &str) -> &'v str {
    path(value, at).and_then(Value::as_str).unwrap_or_default()
}

fn parts(content: &Value) -> &[Value] {
    content["parts"].as_array().unwrap()
}

#[test]
fn merge_adjacent_gemini_contents_empty_and_single_item() {
    assert!(merge_adjacent_gemini_contents(Vec::new()).is_empty());
    let single = values(&[r#"{"role":"user","parts":[{"text":"hello"}]}"#]);
    assert_eq!(merge_adjacent_gemini_contents(single).len(), 1);
    // A lone turn is returned even without parts.
    let empty = values(&[r#"{"role":"user","parts":[]}"#]);
    assert_eq!(merge_adjacent_gemini_contents(empty).len(), 1);
}

#[test]
fn merge_adjacent_gemini_contents_merges_consecutive_user_turns() {
    let contents = values(&[
        r#"{"role":"user","parts":[{"text":"first prompt"}]}"#,
        r#"{"role":"user","parts":[{"text":"<system-reminder>rule 1</system-reminder>"}]}"#,
        r#"{"role":"user","parts":[{"text":"<system-reminder>rule 2</system-reminder>"}]}"#,
        r#"{"role":"model","parts":[{"text":"assistant answer"}]}"#,
        r#"{"role":"user","parts":[{"text":"follow-up"}]}"#,
    ]);
    let merged = merge_adjacent_gemini_contents(contents);
    assert_eq!(merged.len(), 3, "{merged:?}");

    assert_eq!(text(&merged[0], "role"), "user");
    let parts0 = parts(&merged[0]);
    assert_eq!(parts0.len(), 3);
    assert_eq!(text(&parts0[0], "text"), "first prompt");
    assert_eq!(
        text(&parts0[1], "text"),
        "<system-reminder>rule 1</system-reminder>"
    );
    assert_eq!(
        text(&parts0[2], "text"),
        "<system-reminder>rule 2</system-reminder>"
    );
    assert_eq!(text(&merged[1], "role"), "model");
    assert_eq!(text(&merged[2], "role"), "user");
}

#[test]
fn merge_adjacent_gemini_contents_keeps_model_turns_apart() {
    let contents = values(&[
        r#"{"role":"user","parts":[{"text":"question"}]}"#,
        r#"{"role":"model","parts":[{"text":"thought","thought":true}]}"#,
        r#"{"role":"model","parts":[{"text":"answer"}]}"#,
    ]);
    assert_eq!(merge_adjacent_gemini_contents(contents).len(), 3);
}

#[test]
fn merge_adjacent_gemini_contents_skips_turns_without_parts() {
    let mut contents = vec![Value::Null];
    contents.extend(values(&[
        r#"{"role":"user","parts":[]}"#,
        r#"{"role":"user","parts":[{"text":"hello"}]}"#,
    ]));
    assert_eq!(merge_adjacent_gemini_contents(contents).len(), 1);
}

#[test]
fn merge_adjacent_gemini_contents_moves_trailing_text_before_function_response() {
    let contents = values(&[
        r#"{"role":"user","parts":[{"functionResponse":{"name":"read","response":{"result":"ok"}}}]}"#,
        r#"{"role":"user","parts":[{"text":"<system-reminder>reminder</system-reminder>"}]}"#,
    ]);
    let merged = merge_adjacent_gemini_contents(contents);
    assert_eq!(merged.len(), 1);
    let parts = parts(&merged[0]);
    assert_eq!(parts.len(), 2);
    assert_eq!(
        text(&parts[0], "text"),
        "<system-reminder>reminder</system-reminder>"
    );
    assert_eq!(text(&parts[1], "functionResponse.name"), "read");
}

#[test]
fn merge_adjacent_gemini_contents_keeps_other_fields_and_their_order() {
    let contents = values(&[
        r#"{"parts":[{"text":"a"}],"role":"user","extra":1}"#,
        r#"{"role":"user","parts":[{"text":"b"}],"other":2}"#,
    ]);
    assert_eq!(
        serde_json::to_string(&merge_adjacent_gemini_contents(contents)).unwrap(),
        r#"[{"parts":[{"text":"a"},{"text":"b"}],"role":"user","extra":1}]"#
    );
}

#[test]
fn merge_adjacent_gemini_user_contents_merges_pure_text_turns() {
    let contents = values(&[
        r#"{"role":"user","parts":[{"text":"prompt 1"}]}"#,
        r#"{"role":"user","parts":[{"text":"prompt 2"}]}"#,
    ]);
    let merged = merge_adjacent_gemini_user_contents(contents);
    assert_eq!(merged.len(), 1);
    assert_eq!(parts(&merged[0]).len(), 2);
}

#[test]
fn merge_adjacent_gemini_user_contents_keeps_function_responses_apart() {
    let contents = values(&[
        r#"{"role":"user","parts":[{"functionResponse":{"name":"test","response":{"result":"ok"}}}]}"#,
        r#"{"role":"user","parts":[{"text":"user note"}]}"#,
        r#"{"role":"user","parts":[{"function_response":{"name":"test2","response":{"result":"ok2"}}}]}"#,
    ]);
    assert_eq!(merge_adjacent_gemini_user_contents(contents).len(), 3);
}

#[test]
fn merge_adjacent_gemini_user_contents_does_not_reorder() {
    let contents = values(&[
        r#"{"role":"user","parts":[{"inline_data":{"data":"x"}}]}"#,
        r#"{"role":"user","parts":[{"text":"after"}]}"#,
    ]);
    let merged = merge_adjacent_gemini_user_contents(contents);
    assert_eq!(
        merged[0]["parts"],
        json!([{"inline_data":{"data":"x"}},{"text":"after"}])
    );
}

#[test]
fn content_has_gemini_function_response_reads_arrays_and_objects() {
    let has =
        |text: &str| content_has_gemini_function_response(&serde_json::from_str(text).unwrap());
    assert!(has(r#"{"parts":[{"text":"a"},{"functionResponse":{}}]}"#));
    assert!(has(r#"{"parts":[{"function_response":null}]}"#));
    // gjson's ForEach visits an object's values.
    assert!(has(r#"{"parts":{"x":{"functionResponse":{}}}}"#));
    assert!(!has(r#"{"parts":[{"functionCall":{}}]}"#));
    assert!(!has(r#"{"parts":"functionResponse"}"#));
    assert!(!has(r#"{"role":"user"}"#));
}

#[test]
fn reorder_gemini_user_parts_unchanged_without_function_response() {
    let parts = values(&[
        r#"{"text":"hello"}"#,
        r#"{"inline_data":{"mime_type":"image/png","data":"abc"}}"#,
    ]);
    let reordered = reorder_gemini_user_parts(parts);
    assert_eq!(reordered.len(), 2);
    assert_eq!(text(&reordered[0], "text"), "hello");
}

#[test]
fn reorder_gemini_user_parts_unchanged_when_text_already_leads() {
    let parts = values(&[
        r#"{"text":"context"}"#,
        r#"{"functionResponse":{"name":"read","response":{"result":"ok"}}}"#,
    ]);
    let reordered = reorder_gemini_user_parts(parts);
    assert_eq!(reordered.len(), 2);
    assert_eq!(text(&reordered[0], "text"), "context");
}

#[test]
fn reorder_gemini_user_parts_moves_trailing_text_first() {
    let parts = values(&[
        r#"{"functionResponse":{"name":"read","response":{"result":"ok"}}}"#,
        r#"{"text":"<system-reminder>reminder</system-reminder>"}"#,
    ]);
    let reordered = reorder_gemini_user_parts(parts);
    assert_eq!(reordered.len(), 2);
    assert_eq!(
        text(&reordered[0], "text"),
        "<system-reminder>reminder</system-reminder>"
    );
    assert_eq!(text(&reordered[1], "functionResponse.name"), "read");
}

#[test]
fn reorder_gemini_user_parts_keeps_relative_order() {
    let parts = values(&[
        r#"{"text":"leading"}"#,
        r#"{"functionResponse":{"name":"tool1","response":{"result":"1"}}}"#,
        r#"{"functionResponse":{"name":"tool2","response":{"result":"2"}}}"#,
        r#"{"text":"trailing"}"#,
    ]);
    let reordered = reorder_gemini_user_parts(parts);
    assert_eq!(reordered.len(), 4);
    assert_eq!(text(&reordered[0], "text"), "leading");
    assert_eq!(text(&reordered[1], "text"), "trailing");
    assert_eq!(text(&reordered[2], "functionResponse.name"), "tool1");
    assert_eq!(text(&reordered[3], "functionResponse.name"), "tool2");
}

#[test]
fn reorder_gemini_user_parts_moves_only_text_parts() {
    let parts = values(&[
        r#"{"inline_data":{}}"#,
        r#"{"functionResponse":{}}"#,
        r#"{"text":null}"#,
        r#"{"inline_data":{"n":2}}"#,
    ]);
    assert_eq!(
        Value::Array(reorder_gemini_user_parts(parts)),
        json!([{"text":null},{"inline_data":{}},{"functionResponse":{}},{"inline_data":{"n":2}}])
    );
}

#[test]
fn contains_json_ref_cases() {
    let cases = [
        (
            "top-level $ref object",
            r##"{"$ref": "#/components/schemas/ErrorModel"}"##,
            true,
        ),
        (
            "nested $ref in object",
            r##"{"responses":{"400":{"content":{"application/json":{"schema":{"$ref":"#/components/schemas/ErrorModel"}}}}}}"##,
            true,
        ),
        (
            "nested $ref in array",
            r##"[{"schema":{"$ref":"#/components/schemas/ErrorModel"}}]"##,
            true,
        ),
        (
            "plain object without $ref",
            r#"{"temperature": 72, "city": "Seattle"}"#,
            false,
        ),
        (
            "plain array without $ref",
            r#"[1, 2, {"name": "test"}]"#,
            false,
        ),
        ("$ref with non-string value", r#"{"$ref": 123}"#, false),
        (
            "primitive string containing $ref text",
            r#""this is just a string containing $ref""#,
            false,
        ),
    ];
    for (name, text, want) in cases {
        let value: Value = serde_json::from_str(text).unwrap();
        assert_eq!(contains_json_ref(&value), want, "{name}");
    }
}

#[test]
fn set_gemini_function_response_result_keeps_ref_as_string() {
    let mut part = json!({"functionResponse":{"name":"test"}});
    let result = json!({"schema":{"$ref":"#/components/schemas/ErrorModel"}});
    set_gemini_function_response_result(
        &mut part,
        "functionResponse.response.result",
        Some(result),
    );
    assert_eq!(
        part["functionResponse"]["response"]["result"],
        json!(r##"{"schema":{"$ref":"#/components/schemas/ErrorModel"}}"##)
    );
}

#[test]
fn set_gemini_function_response_result_keeps_other_objects_as_json() {
    let mut part = json!({"functionResponse":{"name":"test"}});
    let result = json!({"ok":true,"code":200});
    set_gemini_function_response_result(
        &mut part,
        "functionResponse.response.result",
        Some(result),
    );
    let value = &part["functionResponse"]["response"]["result"];
    assert!(value.is_object(), "{value}");
    assert_eq!(value["ok"], json!(true));
}

#[test]
fn set_gemini_function_response_result_puts_ref_under_result_of_response() {
    let mut part = json!({"functionResponse":{"name":"test"}});
    let result = json!({"schema":{"$ref":"#/components/schemas/ErrorModel"}});
    set_gemini_function_response_result(&mut part, "functionResponse.response", Some(result));
    assert!(part["functionResponse"]["response"]["result"].is_string());
}

#[test]
fn set_gemini_function_response_result_without_result_stores_empty_string() {
    let mut part = json!({"functionResponse":{"name":"test","response":{"result":1}}});
    set_gemini_function_response_result(&mut part, "functionResponse.response.result", None);
    assert_eq!(
        part,
        json!({"functionResponse":{"name":"test","response":{"result":""}}})
    );
}

#[test]
fn set_gemini_function_response_raw_cases() {
    let set = |raw: &str| {
        let mut part = json!({"functionResponse":{"name":"t"}});
        set_gemini_function_response_raw(&mut part, "functionResponse.response.result", raw);
        part["functionResponse"]["response"]["result"].clone()
    };
    assert_eq!(set("  "), json!(""));
    assert_eq!(set(r#" {"a": [1, 2]} "#), json!({"a":[1,2]}));
    assert_eq!(set("42"), json!(42));
    assert_eq!(set(r#""text""#), json!("text"));
    // The text keeps its spacing, as upstream copies it.
    assert_eq!(set(" {\"$ref\": \"#/x\"}\n"), json!("{\"$ref\": \"#/x\"}"));
    assert_eq!(set("not json"), json!(""));
}

#[test]
fn is_gemini_thought_part_reads_thought_as_gjson_bool() {
    let thought = |text: &str| is_gemini_thought_part(&serde_json::from_str(text).unwrap());
    assert!(thought(r#"{"thought":true}"#));
    assert!(thought(r#"{"thought":"true"}"#));
    assert!(thought(r#"{"thought":1}"#));
    assert!(!thought(r#"{"thought":false}"#));
    assert!(!thought(r#"{"text":"x"}"#));
    assert!(!thought(r#"[{"thought":true}]"#));
}
