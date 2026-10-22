//! Each listed path accepts only the client's JSON written compactly.
use crate::cases::Case;
use crate::compare::{Deviation, compare};
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
    let cmp = compare(&at(path, &go), &at(path, &rust), paths);
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
