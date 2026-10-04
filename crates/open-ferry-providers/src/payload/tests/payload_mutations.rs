//! Ported from upstream's
//! internal/runtime/executor/helps/payload_mutations_test.go.

use serde_json::Value;

use super::{Args, json, rules};
use crate::codex::request::{set_bool_if_different, set_string_if_different};
use crate::payload::image;

/// `TestSetStringIfDifferentNormalizesWrongType`.
#[test]
fn set_string_if_different_normalizes_wrong_type() {
    let mut body = json(r#"{"model":123}"#);
    set_string_if_different(&mut body, "model", "123");
    assert_eq!(body, json(r#"{"model":"123"}"#));
}

/// `TestSetBoolIfDifferentNormalizesWrongType`.
#[test]
fn set_bool_if_different_normalizes_wrong_type() {
    let mut body = json(r#"{"stream":"true"}"#);
    set_bool_if_different(&mut body, "stream", true);
    assert_eq!(body, json(r#"{"stream":true}"#));
}

/// `TestApplyPayloadConfigReusesCanonicalOverrides`: overrides that write
/// what the body already holds leave it as it is.
#[test]
fn reuses_canonical_overrides() {
    let config = r#"
payload:
  override:
    - models:
        - name: gpt-test
          protocol: openai
      params:
        stream: true
        model: gpt-test
  override-raw:
    - models:
        - name: gpt-test
          protocol: openai
      params:
        metadata: '{"source":"executor"}'
"#;
    let input =
        r#"{"model":"gpt-test","stream":true,"metadata":{"source":"executor"},"messages":[]}"#;
    let out = Args::with_root("gpt-test", "openai", "").run(config, input);
    assert_eq!(serde_json::to_string(&out).ok().as_deref(), Some(input));
}

/// `TestApplyPayloadConfigWithRequestTrackedReportsContextManagementTouches`.
#[test]
fn tracked_reports_context_management_touches() {
    const AUTOMATIC: &str = r#"{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}"#;
    let models = "    - models:\n        - name: claude-opus-5\n          protocol: claude\n";
    let without = r#"{"model":"claude-opus-5"}"#;
    let with_automatic = format!(r#"{{"model":"claude-opus-5","context_management":{AUTOMATIC}}}"#);
    let caller = r#"{"model":"claude-opus-5","context_management":{"edits":[{"type":"caller"}]}}"#;
    let cases: Vec<(&str, &str, Option<&str>, String, bool)> = vec![
        (
            "default",
            without,
            Some(without),
            format!(
                "payload:\n  default:\n{models}      params:\n        context_management:\n          edits:\n            - type: default\n"
            ),
            true,
        ),
        (
            "raw default",
            without,
            Some(without),
            format!(
                "payload:\n  default-raw:\n{models}      params:\n        context_management: '{{\"edits\":[{{\"type\":\"raw_default\"}}]}}'\n"
            ),
            true,
        ),
        (
            "canonical descendant override",
            &with_automatic,
            None,
            format!(
                "payload:\n  override:\n{models}      params:\n        context_management.edits.0.keep: all\n"
            ),
            true,
        ),
        (
            "identical raw override",
            &with_automatic,
            None,
            format!("payload:\n  override-raw:\n{models}      params:\n        context_management: '{AUTOMATIC}'\n"),
            true,
        ),
        (
            "filter already absent",
            without,
            None,
            format!("payload:\n  filter:\n{models}      params:\n        - context_management\n"),
            true,
        ),
        (
            "unrelated override",
            without,
            None,
            format!("payload:\n  override:\n{models}      params:\n        thinking.type: enabled\n"),
            false,
        ),
        (
            "nonmatching override",
            without,
            None,
            "payload:\n  override:\n    - models:\n        - name: other-model\n          protocol: claude\n      params:\n        context_management:\n          edits: []\n".to_owned(),
            false,
        ),
        (
            "default skipped for caller owned field",
            caller,
            Some(caller),
            format!(
                "payload:\n  default:\n{models}      params:\n        context_management:\n          edits:\n            - type: default\n"
            ),
            false,
        ),
    ];
    for (name, payload, original, config, want) in cases {
        let args = Args {
            model: "claude-opus-5",
            protocol: "claude",
            from: "claude",
            original,
            requested: "claude-opus-5",
            tracked: &["context_management"],
            ..Args::default()
        };
        let (_, touched) = args.apply(Some(&rules(&config)), payload);
        assert_eq!(touched.contains("context_management"), want, "{name}");
    }
}

/// `TestApplyPayloadConfigProjectionOverrideWritesEveryMatch`.
#[test]
fn projection_override_writes_every_match() {
    let config = r#"
payload:
  override:
    - models:
        - name: gpt-test
          protocol: openai
      params:
        items.#.value: [1, 2]
"#;
    let out = Args::with_root("gpt-test", "openai", "")
        .run(config, r#"{"items":[{"value":1},{"value":2}]}"#);
    assert_eq!(out, json(r#"{"items":[{"value":[1,2]},{"value":[1,2]}]}"#));
}

/// `TestApplyPayloadConfigProjectionOverrideRawWritesEveryMatch`.
#[test]
fn projection_override_raw_writes_every_match() {
    let config = r#"
payload:
  override-raw:
    - models:
        - name: gpt-test
          protocol: openai
      params:
        items.#.value: '[1,2]'
"#;
    let out = Args::with_root("gpt-test", "openai", "")
        .run(config, r#"{"items":[{"value":1},{"value":2}]}"#);
    assert_eq!(out, json(r#"{"items":[{"value":[1,2]},{"value":[1,2]}]}"#));
}

/// `TestRemoveToolTypeReusesArrayWithoutMatch`: with no image tool and no
/// `tool_choice`, there is nothing to strip.
#[test]
fn remove_tool_type_reuses_array_without_match() {
    let body =
        json(r#"{"tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}]}"#);
    assert!(image::plan(&body, "").is_none());
}

const ADDITIONAL_TOOLS_FILTER: &str = r#"
payload:
  filter:
    - models:
        - name: gpt-*
          protocol: codex
      params:
        - 'input.#(type=="additional_tools")#.tools.#(name=="functions")#.tools.#(name=="apply_patch")#'
"#;

fn filter_additional_tools(input: &str) -> Value {
    Args::with_root("gpt-5-codex", "codex", "").run(ADDITIONAL_TOOLS_FILTER, input)
}

/// `TestApplyPayloadConfig_CodexAdditionalToolsFilter`, "removes target tool
/// and preserves other tools across namespaces".
#[test]
fn codex_additional_tools_filter_removes_target_tool() {
    let out = filter_additional_tools(
        r#"{"model":"gpt-5-codex","input":[{"type":"additional_tools","role":"developer","tools":[
            {"type":"namespace","name":"functions","tools":[
                {"type":"custom","name":"apply_patch","description":"patch"},
                {"type":"custom","name":"exec_command","description":"exec"}]},
            {"type":"namespace","name":"collaboration","tools":[
                {"type":"custom","name":"share","description":"share"}]}]}]}"#,
    );
    assert_eq!(
        out,
        json(
            r#"{"model":"gpt-5-codex","input":[{"type":"additional_tools","role":"developer","tools":[
            {"type":"namespace","name":"functions","tools":[
                {"type":"custom","name":"exec_command","description":"exec"}]},
            {"type":"namespace","name":"collaboration","tools":[
                {"type":"custom","name":"share","description":"share"}]}]}]}"#
        )
    );
}

/// `TestApplyPayloadConfig_CodexAdditionalToolsFilter`, "non-matching target
/// tool is a no-op".
#[test]
fn codex_additional_tools_filter_without_match_is_a_no_op() {
    let input = r#"{"model":"gpt-5-codex","input":[{"type":"additional_tools","role":"developer","tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"exec_command","description":"exec"}]}]}]}"#;
    let out = filter_additional_tools(input);
    assert_eq!(serde_json::to_string(&out).ok().as_deref(), Some(input));
}

/// `TestApplyPayloadConfig_CodexAdditionalToolsFilter`, "removes matches
/// across multiple additional_tools elements".
#[test]
fn codex_additional_tools_filter_across_elements() {
    let out = filter_additional_tools(
        r#"{"model":"gpt-5-codex","input":[
            {"type":"additional_tools","tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"apply_patch"}]}]},
            {"type":"message","role":"user","content":"hello"},
            {"type":"additional_tools","tools":[{"type":"namespace","name":"functions","tools":[
                {"type":"custom","name":"apply_patch"},{"type":"custom","name":"view_image"}]}]}]}"#,
    );
    let text = out.to_string();
    assert!(!text.contains("apply_patch"), "{text}");
    assert!(text.contains("view_image"), "{text}");
    assert!(text.contains("hello"), "{text}");
}

/// `TestApplyPayloadConfig_CodexAdditionalToolsFilter`, "removes multiple
/// matches within the same namespace array".
#[test]
fn codex_additional_tools_filter_within_one_array() {
    let out = filter_additional_tools(
        r#"{"model":"gpt-5-codex","input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"functions","tools":[
            {"type":"custom","name":"apply_patch","id":"p1"},
            {"type":"custom","name":"exec_command"},
            {"type":"custom","name":"apply_patch","id":"p2"}]}]}]}"#,
    );
    let text = out.to_string();
    assert!(!text.contains("apply_patch"), "{text}");
    assert!(text.contains("exec_command"), "{text}");
}
