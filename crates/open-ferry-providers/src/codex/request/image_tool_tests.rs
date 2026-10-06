// Ported from CLIProxyAPI
// internal/runtime/executor/codex_executor_imagegen_test.go
// (TestEnsureImageGenerationTool_*) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The built-in `image_generation` tool a Codex call's tools gain
//! ([`super::ensure_image_generation_tool`]). The executor tests of the
//! same upstream file are in `codex/executor/tests.rs`.

use http::{HeaderMap, HeaderValue};
use open_ferry_core::auth::Auth;
use open_ferry_core::config::{Config, DisableImageGeneration};
use serde_json::{Value, json};

use super::{Context, add_image_generation_tool, ensure_image_generation_tool};
use crate::json::get;

fn parse(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap()
}

/// The body after the tool is ensured, for no credential and no headers.
fn ensured(raw: &str, model: &str) -> Value {
    ensured_with(raw, model, None, &HeaderMap::new())
}

fn ensured_with(raw: &str, model: &str, auth: Option<&Auth>, headers: &HeaderMap) -> Value {
    let mut body = parse(raw);
    add_image_generation_tool(&mut body, model, auth, headers);
    body
}

fn tool_types(body: &Value) -> Vec<String> {
    get(body, "tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .map(|tool| {
                    tool.get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned()
                })
                .collect()
        })
        .unwrap_or_default()
}

// TestEnsureImageGenerationTool_ResponsesLiteMetadataDoesNotInjectTool
#[test]
fn responses_lite_metadata_adds_no_tool() {
    let raw = r#"{"model":"gpt-5.6-sol","client_metadata":{"ws_request_header_x_openai_internal_codex_responses_lite":"true"},"input":[{"role":"user","content":"hello"}]}"#;
    let body = ensured(raw, "gpt-5.6-sol");
    assert_eq!(body, parse(raw));
    assert!(get(&body, "tools").is_none(), "{body}");
}

// TestEnsureImageGenerationTool_ResponsesLiteBooleanMetadataDoesNotInjectTool
#[test]
fn responses_lite_boolean_metadata_adds_no_tool() {
    let raw = r#"{"model":"gpt-5.6-sol","client_metadata":{"ws_request_header_x_openai_internal_codex_responses_lite":true},"input":"hello"}"#;
    assert_eq!(ensured(raw, "gpt-5.6-sol"), parse(raw));
}

// TestEnsureImageGenerationTool_ResponsesLiteHeaderDoesNotInjectTool
#[test]
fn responses_lite_header_adds_no_tool() {
    let raw = r#"{"model":"gpt-5.6-sol","input":"hello"}"#;
    let mut headers = HeaderMap::new();
    headers.insert(
        "X-OpenAI-Internal-Codex-Responses-Lite",
        HeaderValue::from_static("true"),
    );
    assert_eq!(ensured_with(raw, "gpt-5.6-sol", None, &headers), parse(raw));
}

// TestEnsureImageGenerationTool_ResponsesLiteFalseMetadataStillInjectsTool
#[test]
fn responses_lite_false_metadata_still_adds_the_tool() {
    let body = ensured(
        r#"{"model":"gpt-5.6-sol","client_metadata":{"ws_request_header_x_openai_internal_codex_responses_lite":"false"},"input":"hello"}"#,
        "gpt-5.6-sol",
    );
    assert_eq!(tool_types(&body), ["image_generation"], "{body}");
}

// TestEnsureImageGenerationTool_NoTools
#[test]
fn no_tools_gain_the_tool() {
    let body = ensured(r#"{"model":"gpt-5.4","input":"draw a cat"}"#, "gpt-5.4");
    assert_eq!(
        get(&body, "tools"),
        Some(&json!([{"type": "image_generation", "output_format": "png"}]))
    );
}

// TestEnsureImageGenerationTool_ExistingToolsWithoutImageGen
#[test]
fn other_tools_gain_the_tool_last() {
    let body = ensured(
        r#"{"model":"gpt-5.4","tools":[{"type":"function","name":"get_weather","parameters":{}}]}"#,
        "gpt-5.4",
    );
    assert_eq!(tool_types(&body), ["function", "image_generation"]);
}

// TestEnsureImageGenerationTool_AlreadyPresent
#[test]
fn a_present_tool_is_kept() {
    let body = ensured(
        r#"{"model":"gpt-5.4","tools":[{"type":"image_generation","output_format":"webp"},{"type":"function","name":"f1"}]}"#,
        "gpt-5.4",
    );
    assert_eq!(tool_types(&body), ["image_generation", "function"]);
    assert_eq!(
        get(&body, "tools.0.output_format"),
        Some(&json!("webp")),
        "the client's output format is kept"
    );
}

// TestEnsureImageGenerationTool_ImageGenNamespaceDoesNotInjectTool
#[test]
fn an_image_gen_namespace_adds_no_tool() {
    let raw = r#"{"model":"gpt-5.4","tools":[{"type":"namespace","name":"image_gen","tools":[{"type":"function","name":"imagegen","parameters":{}}]}]}"#;
    assert_eq!(ensured(raw, "gpt-5.4"), parse(raw));
}

// TestEnsureImageGenerationTool_FlattenedImageGenFunctionDoesNotInjectTool
#[test]
fn a_flattened_image_gen_function_adds_no_tool() {
    let raw = r#"{"model":"gpt-5.4","tools":[{"type":"function","name":"image_gen.imagegen","parameters":{}}]}"#;
    assert_eq!(ensured(raw, "gpt-5.4"), parse(raw));
}

// TestEnsureImageGenerationTool_SimilarNamespaceStillInjectsTool
#[test]
fn a_similar_namespace_still_gains_the_tool() {
    let body = ensured(
        r#"{"model":"gpt-5.4","tools":[{"type":"namespace","name":"image_tools","tools":[{"type":"function","name":"imagegen","parameters":{}}]}]}"#,
        "gpt-5.4",
    );
    assert_eq!(tool_types(&body), ["namespace", "image_generation"]);
}

// TestEnsureImageGenerationTool_EmptyToolsArray
#[test]
fn empty_tools_gain_the_tool() {
    let body = ensured(r#"{"model":"gpt-5.4","tools":[]}"#, "gpt-5.4");
    assert_eq!(tool_types(&body), ["image_generation"]);
}

// TestEnsureImageGenerationTool_WebSearchAndImageGen
#[test]
fn web_search_gains_the_tool_last() {
    let body = ensured(
        r#"{"model":"gpt-5.4","tools":[{"type":"web_search"}]}"#,
        "gpt-5.4",
    );
    assert_eq!(tool_types(&body), ["web_search", "image_generation"]);
}

// TestEnsureImageGenerationTool_GPT53CodexSparkDoesNotInjectTool
#[test]
fn a_spark_model_adds_no_tool() {
    let raw = r#"{"model":"gpt-5.3-codex-spark","input":"draw a cat"}"#;
    let body = ensured(raw, "gpt-5.3-codex-spark");
    assert_eq!(body, parse(raw));
    assert!(get(&body, "tools").is_none(), "{body}");
}

// TestEnsureImageGenerationTool_FreeCodexAuthDoesNotInjectTool
#[test]
fn a_free_codex_credential_adds_no_tool() {
    let raw = r#"{"model":"gpt-5.4","input":"draw a cat"}"#;
    let mut free = Auth {
        provider: "codex".into(),
        ..Auth::default()
    };
    free.attributes.insert("plan_type".into(), "free".into());
    let body = ensured_with(raw, "gpt-5.4", Some(&free), &HeaderMap::new());
    assert_eq!(body, parse(raw));
    assert!(get(&body, "tools").is_none(), "{body}");
}

// Not upstream's: a `tools` value that isn't an array is replaced by the
// tool, and the free plan is read in any case with spaces around it, but
// only for a Codex credential (`isCodexFreePlanAuth`).
#[test]
fn tools_and_plans_are_read_as_upstream_reads_them() {
    let body = ensured(
        r#"{"model":"gpt-5.4","tools":{"type":"web_search"}}"#,
        "gpt-5.4",
    );
    assert_eq!(tool_types(&body), ["image_generation"]);

    let raw = r#"{"model":"gpt-5.4","input":"hi"}"#;
    let mut free = Auth {
        provider: " Codex ".into(),
        ..Auth::default()
    };
    free.attributes.insert("plan_type".into(), " FREE ".into());
    assert_eq!(
        ensured_with(raw, "gpt-5.4", Some(&free), &HeaderMap::new()),
        parse(raw)
    );
    free.provider = "xai".into();
    let body = ensured_with(raw, "gpt-5.4", Some(&free), &HeaderMap::new());
    assert_eq!(tool_types(&body), ["image_generation"]);
}

// Not upstream's: upstream adds the tool only while
// `disable-image-generation` is off (`false`), or without a config; `true`,
// `chat` and `passthrough` leave the tools as the client sent them.
#[test]
fn only_an_off_setting_adds_the_tool() {
    let raw = r#"{"model":"gpt-5.4","input":"hi"}"#;
    for (mode, added) in [
        (DisableImageGeneration::Off, true),
        (DisableImageGeneration::All, false),
        (DisableImageGeneration::Chat, false),
        (DisableImageGeneration::Passthrough, false),
    ] {
        let mut config = Config::default();
        config.disable_image_generation = mode;
        let context = Context {
            config: Some(&config),
            ..Context::default()
        };
        let mut body = parse(raw);
        ensure_image_generation_tool(&mut body, "gpt-5.4", context, &HeaderMap::new());
        assert_eq!(
            get(&body, "tools").is_some(),
            added,
            "{}: {body}",
            mode.as_str()
        );
    }
    let mut body = parse(raw);
    ensure_image_generation_tool(&mut body, "gpt-5.4", Context::default(), &HeaderMap::new());
    assert_eq!(tool_types(&body), ["image_generation"], "no config");
}
