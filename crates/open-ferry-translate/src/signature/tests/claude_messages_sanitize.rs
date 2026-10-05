// Ported from CLIProxyAPI internal/signature/claude_messages_sanitize_compat_test.go
// (v8.0.15, MIT). https://github.com/router-for-me/CLIProxyAPI

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
