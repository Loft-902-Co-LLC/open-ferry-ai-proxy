//! Each listed path accepts only the client's JSON written compactly.
use crate::cases::Case;
use crate::compare::{Deviation, Numbers, compare, compare_numbers_from_request};
use crate::translator::Translator;
use serde_json::{Value, json};

fn at(path: &str, text: &str) -> Value {
    path.trim_start_matches("$.")
        .split('.')
        .rev()
        .fold(json!(text), |value, part| {
            if let Some(key) = part.strip_suffix("[*]") {
                json!({key:[value]})
            } else {
                json!({part:value})
            }
        })
}

fn check(from: &str, to: &str, path: &str, in_text: bool) {
    let case = Case::new("embedded", "m", "{}").with_options(json!({"from":from,"to":to}));
    let paths = Translator::RegistryRequest.embedded_json(&case);
    let (prefix, suffix) = if in_text && path.ends_with(".url") {
        ("data:", ";base64,[1,2]")
    } else {
        ("", "")
    };
    let go = format!("{prefix}{{ \"a\": [1, 2], \"s\": \"two  spaces\" }}{suffix}");
    let rust = format!("{prefix}{{\"a\":[1,2],\"s\":\"two  spaces\"}}{suffix}");
    let raw = r#"{ "a": [1, 2], "s": "two  spaces" }"#;
    let source = format!("{{\"input\":{raw}}}");
    let cmp = compare_numbers_from_request(
        &at(path, &go),
        &at(path, &rust),
        paths,
        Numbers::AsWritten,
        Some(&source),
    );
    // The same output spellings originating inside a client string are data.
    let client = serde_json::to_string(&json!({"input":go})).unwrap();
    assert!(
        !compare_numbers_from_request(
            &at(path, &go),
            &at(path, &rust),
            paths,
            Numbers::AsWritten,
            Some(&client)
        )
        .differences
        .is_empty(),
        "client string: {path}"
    );
    assert!(cmp.differences.is_empty(), "{path}");
    assert!(cmp.deviations.contains(&Deviation::EmbeddedJson), "{path}");
    for different in [
        rust.replace("[1,2]", "[9,2]"),
        rust.replace("two  spaces", "two spaces"),
    ] {
        assert!(
            !compare(&at(path, &go), &at(path, &different), paths)
                .differences
                .is_empty(),
            "{path}"
        );
    }
    assert!(
        !compare(&at("$.unlisted", &go), &at("$.unlisted", &rust), paths)
            .differences
            .is_empty()
    );
}

macro_rules! path_test {
    ($name:ident, $to:literal, $path:literal, $in_text:literal) => {
        #[test]
        fn $name() {
            check("interactions", $to, $path, $in_text);
        }
    };
}

path_test!(
    chat_previous_response,
    "openai",
    "$.previous_response_id",
    false
);
path_test!(chat_environment, "openai", "$.environment_id", false);
path_test!(claude_system, "claude", "$.system", true);
path_test!(
    claude_adaptive_effort,
    "claude",
    "$.output_config.effort",
    false
);

#[test]
fn saved_seed11_request_is_equivalent_without_a_marker() {
    let fixture: Value = serde_json::from_str(include_str!(
        "cases/testdata/registry-request-json-text.json"
    ))
    .unwrap();
    let case = Case::new(
        "seed11",
        fixture["model"].as_str().unwrap(),
        fixture["request_text"].as_str().unwrap(),
    )
    .with_options(fixture["options"].clone());
    assert!(case.known_difference.is_none());
    let go = crate::upstream::GoResult::Output(fixture["upstream_output"].to_string().into_bytes());
    let result = crate::evaluate(Translator::RegistryRequest, &case, &go);
    assert!(matches!(result.outcome, crate::Outcome::Equivalent(_)));
}

#[test]
fn data_url_compaction_keeps_each_component_and_all_surrounding_text() {
    let case =
        Case::new("media", "m", "{}").with_options(json!({"from":"interactions","to":"openai"}));
    let paths = Translator::RegistryRequest.embedded_json(&case);
    let path = "$.messages[*].content[*].image_url.url";
    for (go, rust) in [
        (
            r#"data:image/png;base64,{ "b": 2 }"#,
            r#"data:image/png;base64,{"b":2}"#,
        ),
        (
            r#"data:{ "a": 1 };base64,aGVsbG8="#,
            r#"data:{"a":1};base64,aGVsbG8="#,
        ),
        (
            r#"data:{ "a": 1 };base64,{ "b": 2 }"#,
            r#"data:{"a":1};base64,{"b":2}"#,
        ),
    ] {
        assert!(
            compare(&at(path, go), &at(path, rust), paths)
                .differences
                .is_empty()
        );
        assert!(
            !compare(
                &at(path, go),
                &at(path, &rust.replace("base64,", "base64, ")),
                paths
            )
            .differences
            .is_empty()
        );
    }
}
path_test!(
    chat_part_text,
    "openai",
    "$.messages[*].content[*].text",
    false
);
path_test!(
    chat_thought,
    "openai",
    "$.messages[*].reasoning_content",
    true
);
path_test!(
    chat_image,
    "openai",
    "$.messages[*].content[*].image_url.url",
    true
);
path_test!(
    chat_video,
    "openai",
    "$.messages[*].content[*].video_url.url",
    true
);
path_test!(
    chat_audio,
    "openai",
    "$.messages[*].content[*].input_audio.data",
    false
);
path_test!(
    chat_file_data,
    "openai",
    "$.messages[*].content[*].file.file_data",
    false
);
path_test!(
    chat_file_url,
    "openai",
    "$.messages[*].content[*].file.file_url",
    false
);
path_test!(
    chat_filename,
    "openai",
    "$.messages[*].content[*].file.filename",
    true
);
path_test!(
    chat_function_name,
    "openai",
    "$.messages[*].tool_calls[*].function.name",
    false
);
path_test!(
    chat_function_id,
    "openai",
    "$.messages[*].tool_calls[*].id",
    false
);
path_test!(
    chat_result_id,
    "openai",
    "$.messages[*].tool_call_id",
    false
);
path_test!(chat_tool_name, "openai", "$.tools[*].function.name", false);
path_test!(
    chat_tool_description,
    "openai",
    "$.tools[*].function.description",
    false
);
path_test!(chat_joined_text, "openai", "$.messages[*].content", true);
path_test!(claude_text, "claude", "$.messages[*].content[*].text", true);
path_test!(
    claude_thinking,
    "claude",
    "$.messages[*].content[*].thinking",
    true
);
path_test!(
    claude_media_type,
    "claude",
    "$.messages[*].content[*].source.media_type",
    false
);
path_test!(
    claude_media_data,
    "claude",
    "$.messages[*].content[*].source.data",
    false
);
path_test!(
    claude_result_text,
    "claude",
    "$.messages[*].content[*].content[*].text",
    true
);
path_test!(
    claude_result_thinking,
    "claude",
    "$.messages[*].content[*].content[*].thinking",
    true
);
path_test!(
    claude_result_media_type,
    "claude",
    "$.messages[*].content[*].content[*].source.media_type",
    false
);
path_test!(
    claude_result_media_data,
    "claude",
    "$.messages[*].content[*].content[*].source.data",
    false
);
path_test!(
    claude_tool_description,
    "claude",
    "$.tools[*].description",
    false
);

#[test]
fn saved_wrapped_json_and_collection_tool_names_require_request_witnesses() {
    let fixtures: Value =
        serde_json::from_str(include_str!("cases/testdata/retained-request-json.json")).unwrap();
    for f in fixtures.as_array().unwrap() {
        let case = Case::new(
            f["case"].as_str().unwrap(),
            f["model"].as_str().unwrap(),
            f["request_text"].as_str().unwrap(),
        )
        .with_options(f["options"].clone());
        let go = crate::upstream::GoResult::Output(f["upstream_output"].to_string().into_bytes());
        let e = crate::evaluate(Translator::RegistryRequest, &case, &go);
        assert!(
            matches!(e.outcome, crate::Outcome::Equivalent(_)),
            "{}",
            case.name
        );
        if case.name.ends_with("3825") {
            let mut changed = f["upstream_output"].clone();
            changed["tools"][3]["name"] = "wrong".into();
            let e = crate::evaluate(
                Translator::RegistryRequest,
                &case,
                &crate::upstream::GoResult::Output(changed.to_string().into_bytes()),
            );
            assert!(matches!(e.outcome, crate::Outcome::Different(_)));
            let mut request: Value = serde_json::from_str(&case.request).unwrap();
            request["tools"][2]["name"] = "{ \"a\": 1 }".into();
            let mut string_case = case.clone();
            string_case.request = request.to_string();
            let mut output = f["upstream_output"].clone();
            let rust = Translator::RegistryRequest.run_rust(&string_case).unwrap();
            assert!(!crate::embedded_names::repair(
                &string_case,
                &mut output,
                &rust
            ));
        }
    }
}

#[test]
fn responses_gemini_text_paths_reject_json_looking_client_strings() {
    let case =
        Case::new("text", "m", "{}").with_options(json!({"from":"openai-response","to":"gemini"}));
    for path in [
        "$.contents[*].parts[*].text",
        "$.systemInstruction.parts[*].text",
    ] {
        let go = if path.starts_with("$.contents") {
            "Please keep [1, 2] and {\"a\": 1} exactly as typed"
        } else {
            "{\"a\": 1}"
        };
        let rust = go
            .replace("[1, 2]", "[1,2]")
            .replace("{\"a\": 1}", "{\"a\":1}");
        let request = json!({"input":go}).to_string();
        assert!(
            !compare_numbers_from_request(
                &at(path, go),
                &at(path, &rust),
                Translator::RegistryRequest.embedded_json(&case),
                Numbers::AsWritten,
                Some(&request)
            )
            .differences
            .is_empty(),
            "{path}"
        );
    }
}

#[test]
fn complete_raw_json_string_tokens_have_lexical_provenance() {
    let fixtures: Value = serde_json::from_str(include_str!(
        "cases/testdata/embedded-request-string-tokens.json"
    ))
    .unwrap();
    for f in fixtures.as_array().unwrap() {
        let case = Case::new(
            f["case"].as_str().unwrap(),
            f["model"].as_str().unwrap(),
            f["request_text"].as_str().unwrap(),
        )
        .with_options(f["options"].clone());
        let go = crate::upstream::GoResult::Output(
            f["upstream_output_text"]
                .as_str()
                .unwrap()
                .as_bytes()
                .to_vec(),
        );
        let native = Translator::Interactions(
            crate::interactions::Kind::native(
                crate::interactions::Stage::Request,
                "interactions",
                "claude",
            )
            .unwrap(),
        );
        let e = crate::evaluate(native, &case, &go);
        if let crate::Outcome::Different(d) = &e.outcome {
            panic!(
                "{} {:?}",
                case.name,
                d.iter()
                    .map(|d| (&d.path, &d.go, &d.rust))
                    .collect::<Vec<_>>()
            );
        }
        assert!(
            matches!(e.outcome, crate::Outcome::Equivalent(_)),
            "{}",
            case.name
        );
    }
}

#[test]
fn a_complete_string_token_never_licenses_changes_inside_its_value() {
    let client = r#"Please keep [1, 2] and {"a": 1} exactly as typed"#;
    let go = serde_json::to_string(client).unwrap();
    let rust = serde_json::to_string(
        &client
            .replace("[1, 2]", "[1,2]")
            .replace("{\"a\": 1}", "{\"a\":1}"),
    )
    .unwrap();
    let request = format!("{{\"result\":{go}}}");
    for form in [
        crate::compare::JsonForm::Whole,
        crate::compare::JsonForm::InText,
    ] {
        let paths = [("$.text", form)];
        assert!(
            !compare_numbers_from_request(
                &json!({"text":go}),
                &json!({"text":rust}),
                &paths,
                Numbers::AsWritten,
                Some(&request)
            )
            .differences
            .is_empty()
        );
    }
}

path_test!(
    chat_function_arguments,
    "openai",
    "$.messages[*].tool_calls[*].function.arguments",
    false
);
path_test!(
    claude_result_content,
    "claude",
    "$.messages[*].content[*].content",
    false
);
#[test]
fn gemini_instructions_have_raw_value_provenance() {
    check(
        "openai-response",
        "gemini",
        "$.systemInstruction.parts[*].text",
        false,
    );
}
