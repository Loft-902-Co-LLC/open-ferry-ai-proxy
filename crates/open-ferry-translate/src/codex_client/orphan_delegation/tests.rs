//! Ported from CLIProxyAPI internal/client/codex/optimize-multi-agent-v2/
//! orphan_delegation_test.go (v8.0.10, MIT).
//!
//! `TestRewriteCodexOrphanDelegationInput` is ported case by case.
//! `TestTranslateRequestWithCodexMultiAgentV2OrphanDelegation` tests the
//! translation hook, and is ported with it in open-ferry-providers'
//! `codex/compat/tests.rs`.
//!
//! Changed: the case-insensitive header test passes the header's value, as
//! callers find the header in a map that ignores the case of names.
//!
//! Added: the quoted text of an output that isn't a string, and the order of
//! the user message's fields.

use serde_json::Value;

use super::*;

const HANDOFF: &str = "<codex_delegation><message>handoff</message></codex_delegation>";

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

fn rewritten(text: &str) -> Value {
    let mut body = json(text);
    rewrite(&mut body, "collab_spawn", true);
    body
}

fn item(body: &Value, index: usize) -> &Value {
    &body["input"][index]
}

fn kind(body: &Value, index: usize) -> String {
    str_of(item(body, index).get("type")).into_owned()
}

fn is_user_message(body: &Value, index: usize) -> bool {
    kind(body, index) == "message" && item(body, index)["role"] == "user"
}

fn text(body: &Value, index: usize) -> String {
    str_of(item(body, index).pointer("/content/0/text")).into_owned()
}

fn single_create_thread() -> String {
    format!(
        r#"{{
        "model": "deepseek-v4-pro",
        "input": [
            {{
                "type": "function_call_output",
                "name": "create_thread",
                "namespace": "codex_app",
                "output": "{HANDOFF}"
            }}
        ]
    }}"#
    )
}

#[test]
fn disabled_leaves_payload_unchanged() {
    let mut body = json(&single_create_thread());
    assert!(!rewrite(&mut body, "collab_spawn", false));
    assert_eq!(body, json(&single_create_thread()));
}

#[test]
fn missing_subagent_header_leaves_payload_unchanged() {
    let mut body = json(&single_create_thread());
    assert!(!rewrite(&mut body, "", true));
    assert_eq!(body, json(&single_create_thread()));
}

#[test]
fn different_subagent_header_leaves_payload_unchanged() {
    let mut body = json(&single_create_thread());
    assert!(!rewrite(&mut body, "other_subagent", true));
    assert_eq!(body, json(&single_create_thread()));
}

#[test]
fn rewrites_orphan_create_thread_without_call_id() {
    let body = rewritten(&format!(
        r#"{{
        "model": "deepseek-v4-pro",
        "input": [
            {{
                "type": "function_call_output",
                "name": "create_thread",
                "namespace": "codex_app",
                "output": "{HANDOFF}"
            }},
            {{
                "type": "message",
                "role": "user",
                "content": [{{"type": "input_text", "text": "please continue"}}]
            }}
        ]
    }}"#
    ));
    assert!(is_user_message(&body, 0));
    assert_eq!(
        text(&body, 0),
        format!("Tool output from codex_app__create_thread:\n{HANDOFF}")
    );
    assert_eq!(kind(&body, 1), "message");
    assert_eq!(text(&body, 1), "please continue");
    // The message's fields are in upstream's order.
    assert_eq!(
        item(&body, 0).to_string(),
        format!(
            r#"{{"type":"message","role":"user","content":[{{"type":"input_text","text":"Tool output from codex_app__create_thread:\n{}"}}]}}"#,
            HANDOFF
        )
    );
}

#[test]
fn header_value_case_is_ignored() {
    let text = r#"{"model": "deepseek-v4-pro", "input": [{"type": "function_call_output", "name": "create_thread", "namespace": "codex_app", "output": "msg"}]}"#;
    let mut body = json(text);
    assert!(rewrite(&mut body, "COLLAB_SPAWN", true));
    assert!(is_user_message(&body, 0));
}

#[test]
fn rewrites_orphan_send_message_to_thread_with_stale_call_id() {
    let body = rewritten(
        r#"{
        "model": "deepseek-v4-pro",
        "input": [
            {
                "type": "function_call_output",
                "call_id": "call_stale_123",
                "name": "send_message_to_thread",
                "namespace": "codex_app",
                "output": "<codex_delegation>msg</codex_delegation>"
            }
        ]
    }"#,
    );
    assert!(is_user_message(&body, 0));
    assert_eq!(
        text(&body, 0),
        "Tool output from codex_app__send_message_to_thread:\n<codex_delegation>msg</codex_delegation>"
    );
}

#[test]
fn preserves_paired_create_thread_tool_call() {
    let body = rewritten(
        r#"{
        "model": "deepseek-v4-pro",
        "input": [
            {"type": "function_call", "call_id": "call_active_123", "name": "create_thread", "namespace": "codex_app", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_active_123", "name": "create_thread", "namespace": "codex_app", "output": "<codex_delegation>valid</codex_delegation>"}
        ]
    }"#,
    );
    assert_eq!(kind(&body, 1), "function_call_output");
    assert_eq!(item(&body, 1)["call_id"], "call_active_123");
}

#[test]
fn preserves_non_whitelisted_tools() {
    let body = rewritten(
        r#"{
        "model": "deepseek-v4-pro",
        "input": [
            {"type": "function_call_output", "name": "automation_update", "namespace": "codex_app", "output": "ignored"},
            {"type": "function_call_output", "name": "create_thread", "namespace": "other_namespace", "output": "ignored"}
        ]
    }"#,
    );
    assert_eq!(kind(&body, 0), "function_call_output");
    assert_eq!(kind(&body, 1), "function_call_output");
}

#[test]
fn handles_empty_output() {
    let body = rewritten(
        r#"{"model": "deepseek-v4-pro", "input": [{"type": "function_call_output", "name": "create_thread", "namespace": "codex_app", "output": ""}]}"#,
    );
    assert!(is_user_message(&body, 0));
    assert_eq!(
        text(&body, 0),
        "Tool output from codex_app__create_thread:\n"
    );
}

#[test]
fn call_id_whitespace_difference_is_not_paired() {
    let body = rewritten(
        r#"{
        "model": "deepseek-v4-pro",
        "input": [
            {"type": "function_call", "call_id": "call_1", "name": "create_thread", "namespace": "codex_app", "arguments": "{}"},
            {"type": "function_call_output", "call_id": " call_1 ", "name": "create_thread", "namespace": "codex_app", "output": "mismatch"}
        ]
    }"#,
    );
    assert_eq!(kind(&body, 1), "message");
}

#[test]
fn preserves_structured_image_output_as_exact_text() {
    let raw_output = r#"[{"type":"input_text","text":"diagram"},{"type":"input_image","image_url":"https://example.com/img.png"}]"#;
    let body = rewritten(&format!(
        r#"{{
        "model": "deepseek-v4-pro",
        "input": [
            {{"type": "function_call_output", "name": "create_thread", "namespace": "codex_app", "output": {raw_output}}}
        ]
    }}"#
    ));
    assert!(is_user_message(&body, 0));
    assert_eq!(
        text(&body, 0),
        format!("Tool output from codex_app__create_thread:\n{raw_output}")
    );
}

#[test]
fn quotes_other_outputs_as_json() {
    for (output, want) in [
        ("null", "null"),
        ("1.50", "1.50"),
        ("true", "true"),
        (r#"{"b":1, "a":[]}"#, r#"{"b":1,"a":[]}"#),
    ] {
        let body = rewritten(&format!(
            r#"{{"input": [{{"type": "function_call_output", "name": "create_thread", "namespace": "codex_app", "output": {output}}}]}}"#
        ));
        assert_eq!(
            text(&body, 0),
            format!("Tool output from codex_app__create_thread:\n{want}"),
            "{output}"
        );
    }
    let body = rewritten(
        r#"{"input": [{"type": "function_call_output", "name": "create_thread", "namespace": "codex_app"}]}"#,
    );
    assert_eq!(
        text(&body, 0),
        "Tool output from codex_app__create_thread:\n"
    );
}

#[test]
fn call_and_output_are_paired_regardless_of_order() {
    let body = rewritten(
        r#"{
        "model": "deepseek-v4-pro",
        "input": [
            {"type": "function_call_output", "call_id": "call_future_1", "name": "create_thread", "namespace": "codex_app", "output": "early"},
            {"type": "function_call", "call_id": "call_future_1", "name": "create_thread", "namespace": "codex_app", "arguments": "{}"}
        ]
    }"#,
    );
    assert_eq!(kind(&body, 0), "function_call_output");
    assert_eq!(kind(&body, 1), "function_call");
}

#[test]
fn duplicate_output_consumes_call_once() {
    let body = rewritten(
        r#"{
        "model": "deepseek-v4-pro",
        "input": [
            {"type": "function_call", "call_id": "call_once", "name": "create_thread", "namespace": "codex_app", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_once", "name": "create_thread", "namespace": "codex_app", "output": "first"},
            {"type": "function_call_output", "call_id": "call_once", "name": "create_thread", "namespace": "codex_app", "output": "second"}
        ]
    }"#,
    );
    assert_eq!(kind(&body, 1), "function_call_output");
    assert_eq!(kind(&body, 2), "message");
}

#[test]
fn custom_tool_call_does_not_pair_with_function_call_output() {
    let body = rewritten(
        r#"{
        "model": "deepseek-v4-pro",
        "input": [
            {"type": "custom_tool_call", "call_id": "call_custom", "name": "create_thread", "input": "{}"},
            {"type": "function_call_output", "call_id": "call_custom", "name": "create_thread", "namespace": "codex_app", "output": "orphan"}
        ]
    }"#,
    );
    assert_eq!(kind(&body, 1), "message");
}

#[test]
fn assistant_message_tool_calls_do_not_pair() {
    let body = rewritten(
        r#"{
        "model": "deepseek-v4-pro",
        "input": [
            {"type": "message", "role": "assistant", "tool_calls": [{"id": "call_in_msg", "type": "function", "function": {"name": "create_thread"}}]},
            {"type": "function_call_output", "call_id": "call_in_msg", "name": "create_thread", "namespace": "codex_app", "output": "orphan"}
        ]
    }"#,
    );
    assert_eq!(kind(&body, 1), "message");
}

#[test]
fn exact_namespace_and_name_required() {
    let body = rewritten(
        r#"{
        "model": "deepseek-v4-pro",
        "input": [
            {"type": "function_call_output", "name": "codex_app__create_thread", "output": "orphan"},
            {"type": "function_call_output", "name": "create_thread", "namespace": " codex_app ", "output": "orphan"},
            {"type": "function_call_output", "name": "other_tool", "namespace": "codex_app", "output": "orphan"}
        ]
    }"#,
    );
    for index in 0..3 {
        assert_eq!(kind(&body, index), "function_call_output", "{index}");
    }
}
