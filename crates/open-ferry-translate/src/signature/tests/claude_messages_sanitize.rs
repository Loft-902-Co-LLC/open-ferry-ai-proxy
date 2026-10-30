// Ported from CLIProxyAPI internal/signature/claude_messages_sanitize_compat_test.go
// (v8.0.42, MIT). https://github.com/router-for-me/CLIProxyAPI

use super::*;

fn sanitized(input: &str, model: &str, preserve_empty_thinking_blocks: bool) -> serde_json::Value {
    let mut payload = json(input);
    sanitize_claude_messages_for_claude_upstream(
        &mut payload,
        model,
        preserve_empty_thinking_blocks,
    );
    payload
}

#[test]
fn sanitize_claude_messages_for_claude_upstream_preserves_empty_thinking_in_compat_mode() {
    let input = r#"{"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"","signature":""}]}]}"#;

    let without_compat = sanitized(input, "deepseek-v4", false);
    assert_eq!(
        without_compat["messages"][0]["content"]
            .as_array()
            .map_or(0, Vec::len),
        0,
        "default sanitizer preserved empty thinking: {without_compat}"
    );

    let with_compat = sanitized(input, "deepseek-v4", true);
    let part = &with_compat["messages"][0]["content"][0];
    assert!(
        part["type"] == "thinking" && part["signature"] == "",
        "compat sanitizer dropped empty thinking: {with_compat}"
    );
}

#[test]
fn sanitize_claude_messages_for_claude_upstream_preserves_opaque_thinking_signature_in_compat_mode()
{
    let input = r#"{"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"reason","signature":"opaque-deepseek-id"}]}]}"#;

    let without_compat = sanitized(input, "deepseek-v4", false);
    assert_eq!(
        crate::json::str_of(without_compat.pointer("/messages/0/content/0/signature")),
        "",
        "default sanitizer preserved opaque signature: {without_compat}"
    );

    let with_compat = sanitized(input, "deepseek-v4", true);
    let part = &with_compat["messages"][0]["content"][0];
    assert!(
        part["type"] == "thinking" && part["signature"] == "opaque-deepseek-id",
        "compat sanitizer dropped opaque signature: {with_compat}"
    );
}

#[test]
fn commentary_markers_restore_text_before_signature_policy() {
    for model in [
        "claude-sonnet-5",
        "gpt-5.4",
        "gemini-3.5-flash",
        "unknown-model",
    ] {
        for compat in [false, true] {
            let mut payload = json(
                r#"{"messages":[{"role":"assistant","content":[
                {"type":"thinking","thinking":"commentary","signature":" cpa:commentary:v1 "},
                {"type":"thinking","thinking":"","signature":"cpa:commentary:v1"},
                {"type":"text","text":"final"}]},
                {"role":"assistant","content":[{"type":"thinking","thinking":"","signature":"cpa:commentary:v1"}]}]}"#,
            );
            if compat {
                sanitize_claude_messages_for_claude_upstream(&mut payload, model, true);
            } else {
                sanitize_claude_messages_signatures_for_model(&mut payload, model);
            }
            assert_eq!(payload["messages"].as_array().unwrap().len(), 1);
            assert_eq!(
                payload["messages"][0]["content"],
                json(r#"[{"type":"text","text":"commentary"},{"type":"text","text":"final"}]"#)
            );
            assert!(!payload.to_string().contains("cpa:commentary:v1"));
        }
    }
}

#[test]
fn r3_empty_commentary_predicate_preserves_target_whitespace_policy() {
    let marker =
        serde_json::json!({"type":"thinking","thinking":"","signature":" cpa:commentary:v1 "});
    let blank =
        serde_json::json!({"type":"thinking","thinking":" ","signature":"cpa:commentary:v1"});
    for (content, strict, trimmed) in [
        (serde_json::json!([marker.clone()]), true, true),
        (
            serde_json::json!([marker.clone(), marker.clone()]),
            true,
            true,
        ),
        (serde_json::json!([blank]), false, true),
        (serde_json::json!([]), false, false),
        (serde_json::json!(""), false, false),
        (
            serde_json::json!([marker, {"type":"text","text":""}]),
            false,
            false,
        ),
        (
            serde_json::json!([{"type":"thinking","thinking":"","signature":"opaque"}]),
            false,
            false,
        ),
        (
            serde_json::json!([{"type":"thinking","thinking":"visible","signature":"cpa:commentary:v1"}]),
            false,
            false,
        ),
    ] {
        let message = serde_json::json!({"content":content});
        assert_eq!(is_empty_codex_commentary_message(&message, false), strict);
        assert_eq!(is_empty_codex_commentary_message(&message, true), trimmed);
    }
    assert!(!is_empty_codex_commentary_message(
        &serde_json::json!({}),
        false
    ));
}
