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

/// The merge as upstream does it: each joined turn written out and, for
/// [`Merge::Reordering`], reordered again.
fn merge_one_join_at_a_time(contents: Vec<Value>, merge: Merge) -> Vec<Value> {
    if contents.len() <= 1 {
        return contents;
    }
    let mut merged: Vec<Value> = Vec::new();
    for content in contents {
        if !matches!(content.get("parts"), Some(Value::Array(parts)) if !parts.is_empty()) {
            continue;
        }
        if let Some(last) = merged.last_mut()
            && str_of(last.get("role")) == "user"
            && str_of(content.get("role")) == "user"
            && (merge == Merge::Reordering
                || !(content_has_gemini_function_response(last)
                    || content_has_gemini_function_response(&content)))
        {
            let mut parts = last["parts"].as_array().unwrap().clone();
            parts.extend(content["parts"].as_array().unwrap().iter().cloned());
            if merge == Merge::Reordering {
                parts = reorder_gemini_user_parts(parts);
            }
            last["parts"] = Value::Array(parts);
            continue;
        }
        merged.push(content);
    }
    merged
}

#[test]
fn merges_as_upstream_joins_one_turn_at_a_time() {
    let kinds = [
        json!({"text": "t"}),
        json!({"functionResponse": {"name": "f"}}),
        json!({"function_response": {"name": "g"}}),
        json!({"functionResponse": {"name": "h"}, "text": "both"}),
        json!({"inlineData": {"data": "x"}}),
        json!({"functionCall": {"name": "c"}}),
        json!("not a part"),
    ];
    let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n as u64) as usize
    };
    for case in 0..3000 {
        let contents: Vec<Value> = (0..next(12))
            .map(|turn| {
                let parts: Vec<Value> = (0..next(5))
                    .map(|part| {
                        let mut kind = kinds[next(kinds.len())].clone();
                        if let Some(fields) = kind.as_object_mut() {
                            fields.insert("n".into(), json!(format!("{turn}.{part}")));
                        }
                        kind
                    })
                    .collect();
                let role = ["user", "user", "user", "model"][next(4)];
                match next(10) {
                    0 => json!({"role": role}),
                    1 => json!({"parts": parts, "role": role}),
                    _ => json!({"role": role, "parts": parts}),
                }
            })
            .collect();
        for merge in [Merge::Reordering, Merge::InOrder] {
            assert_eq!(
                merge_user_turns(contents.clone(), merge),
                merge_one_join_at_a_time(contents.clone(), merge),
                "case {case}, {}",
                Value::Array(contents.clone())
            );
        }
    }
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

// The tests below port TestSanitizeFunctionName, TestSanitizedToolNameMap and
// TestRestoreSanitizedToolName from internal/util/sanitize_test.go. The other
// tests there cover functions not ported here.

#[test]
fn sanitize_gemini_function_name_cases() {
    let long = "this_is_a_very_long_name_that_exactly_reaches_sixty_four_charact";
    for (input, expected) in [
        ("valid_name", "valid_name"),
        ("name.with.dots", "name.with.dots"),
        ("name:with:colons", "name:with:colons"),
        ("name-with-dashes", "name-with-dashes"),
        (
            "name.with_dots:colons-dashes",
            "name.with_dots:colons-dashes",
        ),
        ("name!with@invalid#chars", "name_with_invalid_chars"),
        ("name with spaces", "name_with_spaces"),
        ("name_with_\u{4f60}\u{597d}_chars", "name_with____chars"),
        ("123name", "_123name"),
        (".name", "_.name"),
        (":name", "_:name"),
        ("-name", "_-name"),
        ("!name", "_name"),
        (long, long),
        (&format!("{long}X"), long),
        (
            "this_is_a_very_long_name_that_exceeds_the_sixty_four_character_limit_for_function_names",
            "this_is_a_very_long_name_that_exceeds_the_sixty_four_character_l",
        ),
        (
            "1234567890123456789012345678901234567890123456789012345678901234",
            "_123456789012345678901234567890123456789012345678901234567890123",
        ),
        (
            "!234567890123456789012345678901234567890123456789012345678901234",
            "_234567890123456789012345678901234567890123456789012345678901234",
        ),
        ("", ""),
        ("@", "_"),
        ("a", "a"),
        ("1", "_1"),
        ("_", "_"),
    ] {
        let got = sanitize_gemini_function_name(input);
        assert_eq!(got, expected, "{input}");
        assert!(got.len() <= 64, "{input}");
        assert!(
            got.is_empty() || matches!(got.as_bytes()[0], b'a'..=b'z' | b'A'..=b'Z' | b'_'),
            "{input}"
        );
    }
}

#[test]
fn sanitized_tool_name_map_cases() {
    let names = sanitized_tool_name_map(&json!({"tools": [
        {"name": "valid_tool", "input_schema": {}},
        {"name": "mcp/server/read", "input_schema": {}},
        {"name": "tool@v2", "input_schema": {}}
    ]}))
    .unwrap();
    assert_eq!(names["mcp_server_read"], "mcp/server/read");
    assert_eq!(names["tool_v2"], "tool@v2");
    assert!(!names.contains_key("valid_tool"));

    assert!(
        sanitized_tool_name_map(&json!({"tools": [
            {"name": "Read", "input_schema": {}},
            {"name": "Write", "input_schema": {}}
        ]}))
        .is_none()
    );
    assert!(sanitized_tool_name_map(&json!({})).is_none());
    assert!(sanitized_tool_name_map(&Value::Null).is_none());
    assert!(
        sanitized_tool_name_map(&json!({"tools": [
            {"type": "function", "function": {"name": "web/search"}},
            {"type": "web_search", "name": "web_search"}
        ]}))
        .is_none()
    );

    let names = sanitized_tool_name_map(&json!({"tools": [
        {"name": "read/file", "input_schema": {}},
        {"name": "read@file", "input_schema": {}}
    ]}))
    .unwrap();
    assert_eq!(names["read_file"], "read/file");
}

#[test]
fn sanitized_tool_name_map_trims_and_reads_names_as_text() {
    let names = sanitized_tool_name_map(&json!({"tools": [
        {"name": "  a/b  "},
        {"name": 12},
        {"name": " "},
        "x"
    ]}))
    .unwrap();
    assert_eq!(names.len(), 2);
    assert_eq!(names["a_b"], "a/b");
    assert_eq!(names["_12"], "12");
}

#[test]
fn restore_sanitized_tool_name_cases() {
    let names = SanitizedToolNames::from([
        ("mcp_server_read".to_owned(), "mcp/server/read".to_owned()),
        ("tool_v2".to_owned(), "tool@v2".to_owned()),
    ]);
    assert_eq!(
        restore_sanitized_tool_name(Some(&names), "mcp_server_read"),
        "mcp/server/read"
    );
    assert_eq!(
        restore_sanitized_tool_name(Some(&names), "unknown"),
        "unknown"
    );
    assert_eq!(restore_sanitized_tool_name(None, "name"), "name");
    assert_eq!(restore_sanitized_tool_name(Some(&names), ""), "");
}
