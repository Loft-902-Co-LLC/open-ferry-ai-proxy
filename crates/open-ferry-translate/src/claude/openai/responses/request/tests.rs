// Ported from CLIProxyAPI internal/translator/claude/openai/responses/claude_openai-responses_request_test.go
// (v8.0.15, MIT) and claude_openai-responses_user_turn_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use std::collections::HashMap;

use serde_json::{Value, json};

use super::super::test_support::{claude_thinking_signature, gpt_reasoning_signature, sse_events};
use super::super::tools::{RequestTools, qualify};
use super::super::{
    ClaudeToOpenAIResponsesStream, convert_claude_response_to_openai_responses_non_stream,
};
use super::*;
use crate::json::{bool_of, str_of};
use crate::models::ModelCatalog;

/// What a strict apply_patch tool description must explain.
const PATCH_INSTRUCTIONS: [&str; 10] = [
    "*** Begin Patch",
    "*** End Patch",
    "*** Add File:",
    "*** Delete File:",
    "*** Update File:",
    "*** Move to:",
    "*** End of File",
    "@@",
    "start: patch",
    "JSON object",
];

/// A conversion's body, which must come without a refusal.
fn sent((body, err): (Value, Option<UnsupportedPartError>)) -> Value {
    assert_eq!(err, None, "refused: {body}");
    body
}

fn convert(model: &str, request: &Value) -> Value {
    sent(convert_openai_responses_request_to_claude(
        model,
        request,
        false,
        ModelCatalog::embedded(),
    ))
}

fn convert_streaming(model: &str, request: &Value) -> Value {
    sent(convert_openai_responses_request_to_claude(
        model,
        request,
        true,
        ModelCatalog::embedded(),
    ))
}

fn convert_with_compat(model: &str, request: &Value) -> Value {
    sent(convert_with_compat_checked(model, request))
}

/// `ConvertOpenAIResponsesRequestToClaudeWithCompat`, with its refusal.
fn convert_with_compat_checked(
    model: &str,
    request: &Value,
) -> (Value, Option<UnsupportedPartError>) {
    convert_openai_responses_request_to_claude_with_compat(
        model,
        request,
        false,
        ModelCatalog::embedded(),
    )
}

/// gjson `String()`: missing and null read as "".
fn text(value: &Value) -> String {
    str_of(Some(value)).into_owned()
}

/// An array's items; nothing for any other value.
fn items(value: &Value) -> &[Value] {
    value.as_array().map_or(&[], Vec::as_slice)
}

/// Every content block of every message.
fn all_blocks(out: &Value) -> impl Iterator<Item = &Value> {
    items(&out["messages"])
        .iter()
        .flat_map(|message| items(&message["content"]))
}

/// gjson `tools.#(name=="...")`: the first tool with this name.
fn tool_named<'v>(out: &'v Value, name: &str) -> Option<&'v Value> {
    items(&out["tools"])
        .iter()
        .find(|tool| tool["name"] == name)
}

/// The block types of a message's content.
fn block_types(message: &Value) -> Vec<String> {
    items(&message["content"])
        .iter()
        .map(|block| text(&block["type"]))
        .collect()
}

/// The apply_patch instructions a tool description lacks.
fn missing_patch_instructions(description: &str) -> Vec<&'static str> {
    PATCH_INSTRUCTIONS
        .into_iter()
        .filter(|instruction| !description.contains(instruction))
        .collect()
}

/// Whether a schema takes exactly one `input` and nothing else.
fn is_strict_patch_schema(schema: &Value) -> bool {
    schema.get("additionalProperties").is_some()
        && !bool_of(&schema["additionalProperties"])
        && schema["required"][0] == "input"
}

#[test]
fn sanitizes_tool_call_ids_for_claude() {
    let out = convert(
        "claude-sonnet-4-5",
        &json!({
            "model": "gpt-4.1",
            "input": [
                {
                    "type": "function_call",
                    "call_id": "call.with space:1",
                    "name": "Read",
                    "arguments": r#"{"path":"README.md"}"#
                },
                {"type": "function_call_output", "call_id": "call.with space:1", "output": "ok"}
            ]
        }),
    );
    assert_eq!(
        out["messages"][0]["content"][0]["id"], "call_with_space_1",
        "tool_use id: {out}"
    );
    assert_eq!(
        out["messages"][1]["content"][0]["tool_use_id"], "call_with_space_1",
        "tool_result should carry the same sanitized id: {out}"
    );
}

#[test]
fn max_tokens_default_and_model_limit() {
    let cases = [
        (
            "fable defaults to 64k",
            "claude-fable-5-1",
            json!({"model": "claude-fable-5-1", "input": "hello"}),
            64000,
        ),
        (
            "preserves explicit 128k limit",
            "claude-fable-5-1",
            json!({"model": "claude-fable-5-1", "max_output_tokens": 128000, "input": "hello"}),
            128000,
        ),
        (
            "does not exceed registered model maximum",
            "claude-3-5-haiku-20241022",
            json!({"model": "claude-3-5-haiku-20241022", "input": "hello"}),
            8192,
        ),
        (
            "clamps explicit limit exceeding registered model maximum",
            "claude-3-5-haiku-20241022",
            json!({"model": "claude-3-5-haiku-20241022", "max_output_tokens": 128000, "input": "hello"}),
            8192,
        ),
        (
            "null max_output_tokens retains default 64k",
            "claude-fable-5-1",
            json!({"model": "claude-fable-5-1", "max_output_tokens": null, "input": "hello"}),
            64000,
        ),
    ];
    for (name, model, request, want) in cases {
        let out = convert_streaming(model, &request);
        assert_eq!(out["max_tokens"], want, "{name}: {out}");
    }
}

#[test]
fn reasoning_item_becomes_thinking_block() {
    let (raw_signature, expected_signature) = claude_thinking_signature();
    let out = convert(
        "claude-test",
        &json!({
            "model": "claude-test",
            "input": [
                {
                    "type": "reasoning",
                    "encrypted_content": raw_signature,
                    "summary": [{"type": "summary_text", "text": "internal reasoning"}]
                },
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "visible answer"}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "continue"}]}
            ]
        }),
    );
    let assistant = &out["messages"][0];
    assert_eq!(assistant["role"], "assistant", "{out}");
    assert_eq!(assistant["content"][0]["type"], "thinking", "{out}");
    assert_eq!(
        assistant["content"][0]["signature"],
        expected_signature.as_str(),
        "{out}"
    );
    assert_eq!(
        assistant["content"][0]["thinking"], "internal reasoning",
        "{out}"
    );
    assert_eq!(assistant["content"][1]["type"], "text", "{out}");
    assert_eq!(assistant["content"][1]["text"], "visible answer", "{out}");
    assert_eq!(out["messages"][1]["role"], "user", "{out}");
}

#[test]
fn signature_only_reasoning_flushes_before_user() {
    let (raw_signature, expected_signature) = claude_thinking_signature();
    let out = convert(
        "claude-test",
        &json!({
            "model": "claude-test",
            "input": [
                {"type": "reasoning", "encrypted_content": raw_signature, "summary": []},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "continue"}]}
            ]
        }),
    );
    let thinking = &out["messages"][0]["content"][0];
    assert_eq!(thinking["type"], "thinking", "{out}");
    assert_eq!(thinking["signature"], expected_signature.as_str(), "{out}");
    assert_eq!(text(&thinking["thinking"]), "", "{out}");
    assert_eq!(out["messages"][1]["role"], "user", "{out}");
}

#[test]
fn redacted_reasoning_item_restores_redacted_thinking() {
    const DATA: &str = "EroBCkYIBRgCKkA";
    let out = convert(
        "claude-test",
        &json!({
            "model": "claude-test",
            "input": [
                {
                    "type": "reasoning",
                    "encrypted_content": format!("{REDACTED_THINKING_PREFIX}{DATA}"),
                    "summary": []
                },
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "visible answer"}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "continue"}]}
            ]
        }),
    );
    let block = &out["messages"][0]["content"][0];
    assert_eq!(block["type"], "redacted_thinking", "{out}");
    assert_eq!(block["data"], DATA, "{out}");
    assert!(
        block.get("signature").is_none(),
        "redacted_thinking must not carry a signature: {out}"
    );
    assert_eq!(
        out["messages"][0]["content"][1]["text"], "visible answer",
        "{out}"
    );
}

#[test]
fn empty_redacted_reasoning_item_is_dropped() {
    let out = convert(
        "claude-test",
        &json!({
            "model": "claude-test",
            "input": [
                {"type": "reasoning", "encrypted_content": REDACTED_THINKING_PREFIX, "summary": []},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "continue"}]}
            ]
        }),
    );
    assert_eq!(
        items(&out["messages"]).len(),
        1,
        "only the user turn should remain: {out}"
    );
    assert_eq!(out["messages"][0]["role"], "user", "{out}");
}

#[test]
fn reasoning_content_text_rebuilds_thinking() {
    let (raw_signature, expected_signature) = claude_thinking_signature();
    let out = convert(
        "claude-test",
        &json!({
            "model": "claude-test",
            "input": [
                {
                    "type": "reasoning",
                    "encrypted_content": raw_signature,
                    "summary": [],
                    "content": [{"type": "reasoning_text", "text": "restored from content"}]
                },
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "continue"}]}
            ]
        }),
    );
    let thinking = &out["messages"][0]["content"][0];
    assert_eq!(thinking["thinking"], "restored from content", "{out}");
    assert_eq!(thinking["signature"], expected_signature.as_str(), "{out}");
}

#[test]
fn reasoning_summary_sets_thinking_display() {
    let cases = [
        ("auto", Some("auto"), "summarized"),
        ("concise", Some("concise"), "summarized"),
        ("none", Some("none"), "omitted"),
        ("absent", None, ""),
    ];
    for (name, summary, want) in cases {
        let mut reasoning = json!({"effort": "high"});
        if let Some(summary) = summary {
            reasoning["summary"] = json!(summary);
        }
        let out = convert(
            "claude-opus-5-5",
            &json!({"model": "claude-opus-5-5", "reasoning": reasoning, "input": "hi"}),
        );
        assert_eq!(text(&out["thinking"]["display"]), want, "{name}: {out}");
    }
}

#[test]
fn summary_wins_over_duplicated_reasoning_content() {
    let (raw_signature, _) = claude_thinking_signature();
    let out = convert(
        "claude-test",
        &json!({
            "model": "claude-test",
            "input": [
                {
                    "type": "reasoning",
                    "encrypted_content": raw_signature,
                    "summary": [{"type": "summary_text", "text": "chain of thought"}],
                    "content": [{"type": "reasoning_text", "text": "chain of thought"}]
                },
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "continue"}]}
            ]
        }),
    );
    assert_eq!(
        out["messages"][0]["content"][0]["thinking"], "chain of thought",
        "the summary text should appear exactly once: {out}"
    );
}

#[test]
fn drops_incompatible_reasoning_signature() {
    let out = convert(
        "claude-test",
        &json!({
            "model": "claude-test",
            "input": [
                {
                    "type": "reasoning",
                    "encrypted_content": gpt_reasoning_signature(),
                    "summary": [{"type": "summary_text", "text": "must not become Claude thinking"}]
                },
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "continue"}]}
            ]
        }),
    );
    let first = &out["messages"][0]["content"][0];
    assert_ne!(
        first["type"], "thinking",
        "GPT encrypted_content should not become Claude thinking: {out}"
    );
    assert!(
        first.get("signature").is_none(),
        "an incompatible signature should not be forwarded: {out}"
    );
    assert_eq!(out["messages"][0]["role"], "user", "{out}");
}

#[test]
fn groups_assistant_and_tool_result_turns() {
    let (raw_signature, expected_signature) = claude_thinking_signature();
    let out = convert(
        "claude-test",
        &json!({
            "model": "claude-test",
            "input": [
                {
                    "type": "reasoning",
                    "encrypted_content": raw_signature,
                    "summary": [{"type": "summary_text", "text": "internal reasoning"}]
                },
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "visible answer"}]},
                {"type": "function_call", "call_id": "call_first", "name": "read_file", "arguments": r#"{"path":"first"}"#},
                {"type": "function_call", "call_id": "call_second", "name": "read_file", "arguments": r#"{"path":"second"}"#},
                {"type": "function_call_output", "call_id": "call_first", "output": "first result"},
                {"type": "function_call_output", "call_id": "call_second", "output": "second result"}
            ]
        }),
    );
    assert_eq!(items(&out["messages"]).len(), 2, "message count: {out}");

    let assistant = &out["messages"][0];
    assert_eq!(assistant["role"], "assistant", "{out}");
    assert_eq!(
        block_types(assistant),
        ["thinking", "text", "tool_use", "tool_use"],
        "assistant content: {out}"
    );
    assert_eq!(
        assistant["content"][0]["signature"],
        expected_signature.as_str(),
        "{out}"
    );
    assert_eq!(assistant["content"][2]["id"], "call_first", "{out}");
    assert_eq!(assistant["content"][3]["id"], "call_second", "{out}");

    let user = &out["messages"][1];
    assert_eq!(user["role"], "user", "{out}");
    let content = items(&user["content"]);
    assert_eq!(content.len(), 2, "user content count: {out}");
    for (block, want_id) in content.iter().zip(["call_first", "call_second"]) {
        assert_eq!(block["type"], "tool_result", "{out}");
        assert_eq!(block["tool_use_id"], want_id, "{out}");
    }
}

#[test]
fn merges_consecutive_user_messages_and_preserves_cache_control() {
    let out = convert(
        "claude-test",
        &json!({
            "model": "claude-test",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "cache_control": {"type": "ephemeral"},
                    "content": [{"type": "input_text", "text": "first"}]
                },
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "second"}]}
            ]
        }),
    );
    assert_eq!(items(&out["messages"]).len(), 1, "message count: {out}");
    let content = items(&out["messages"][0]["content"]);
    assert_eq!(content.len(), 2, "content count: {out}");
    assert_eq!(content[0]["text"], "first", "{out}");
    assert_eq!(content[0]["cache_control"]["type"], "ephemeral", "{out}");
    assert_eq!(content[1]["text"], "second", "{out}");
    assert!(
        content[1].get("cache_control").is_none(),
        "the second block should not have cache_control: {out}"
    );
}

#[test]
fn does_not_merge_across_role_changes() {
    let out = convert(
        "claude-test",
        &json!({
            "model": "claude-test",
            "input": [
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "first assistant"}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "user reply"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "second assistant"}]}
            ]
        }),
    );
    let roles: Vec<String> = items(&out["messages"])
        .iter()
        .map(|message| text(&message["role"]))
        .collect();
    assert_eq!(roles, ["assistant", "user", "assistant"], "{out}");
}

#[test]
fn empty_string_content_does_not_break_assistant_turn() {
    let out = convert(
        "claude-test",
        &json!({
            "model": "claude-test",
            "input": [
                {"type": "message", "role": "assistant", "content": "first assistant"},
                {"type": "message", "role": "user", "content": ""},
                {"type": "message", "role": "assistant", "content": "second assistant"}
            ]
        }),
    );
    let messages = items(&out["messages"]);
    assert_eq!(messages.len(), 1, "message count: {out}");
    assert_eq!(messages[0]["role"], "assistant", "{out}");
    let content = items(&messages[0]["content"]);
    assert_eq!(content.len(), 2, "content count: {out}");
    for (block, want) in content.iter().zip(["first assistant", "second assistant"]) {
        assert_eq!(block["type"], "text", "{out}");
        assert_eq!(block["text"], want, "{out}");
    }
}

#[test]
fn function_call_output_preserves_input_image() {
    const IMAGE_B64: &str = "iVBORw0KGgo=";
    let out = convert(
        "claude-test",
        &json!({
            "model": "claude-test",
            "input": [
                {"type": "function_call", "call_id": "call_view_image_1", "name": "view_image", "arguments": "{}"},
                {
                    "type": "function_call_output",
                    "call_id": "call_view_image_1",
                    "output": [{
                        "type": "input_image",
                        "image_url": format!("data:image/png;base64,{IMAGE_B64}"),
                        "detail": "high"
                    }]
                }
            ]
        }),
    );
    let tool_result = &out["messages"][1]["content"][0];
    assert_eq!(tool_result["type"], "tool_result", "{out}");
    assert_eq!(tool_result["content"][0]["type"], "image", "{out}");
    assert_eq!(
        tool_result["content"][0]["source"]["media_type"], "image/png",
        "{out}"
    );
    assert_eq!(
        tool_result["content"][0]["source"]["data"], IMAGE_B64,
        "the image data should be raw base64, without the data URL prefix: {out}"
    );
    assert!(
        !tool_result["content"].to_string().contains("data:image"),
        "tool_result content must not embed the data URL as text: {out}"
    );
}

#[test]
fn standalone_tool_output_becomes_user_text() {
    // Codex create_thread seeds a new thread with a function_call_output whose
    // call_id never appears as a function_call in the same input. Claude
    // rejects tool_result blocks without a matching tool_use, so it must
    // degrade to text.
    let out = convert(
        "claude-test",
        &json!({
            "model": "claude-test",
            "input": [
                {
                    "type": "function_call_output",
                    "call_id": "toolu_1789312108939888000_16",
                    "output": "<codex_delegation>Launched from another task.</codex_delegation>"
                },
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Reply with PROBE_OK."}]}
            ]
        }),
    );
    assert!(
        all_blocks(&out).all(|block| block["type"] != "tool_result"),
        "unexpected tool_result for a standalone output: {out}"
    );
    assert_eq!(out["messages"][0]["role"], "user", "{out}");
    assert_eq!(out["messages"][0]["content"][0]["type"], "text", "{out}");
    assert_eq!(
        out["messages"][0]["content"][0]["text"],
        "<codex_delegation>Launched from another task.</codex_delegation>",
        "{out}"
    );
}

#[test]
fn synthesizes_result_for_dangling_tool_use() {
    // A session that died mid-tool leaves a function_call with no output.
    // Anthropic requires a tool_result at the start of the next user message,
    // so one is synthesized.
    let out = convert(
        "claude-test",
        &json!({
            "model": "claude-test",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Run the tests."}]},
                {"type": "function_call", "call_id": "call_1", "name": "shell", "arguments": r#"{"cmd":"npm test"}"#},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Session was restarted; carry on."}]}
            ]
        }),
    );
    assert_eq!(items(&out["messages"]).len(), 3, "message count: {out}");
    assert_eq!(
        out["messages"][1]["content"][0]["type"], "tool_use",
        "{out}"
    );
    let synthesized = &out["messages"][2]["content"][0];
    assert_eq!(synthesized["type"], "tool_result", "{out}");
    assert_eq!(synthesized["tool_use_id"], "call_1", "{out}");
    assert!(
        bool_of(&synthesized["is_error"]),
        "a synthesized tool_result should be is_error: {out}"
    );
    assert_eq!(
        out["messages"][2]["content"][1]["text"], "Session was restarted; carry on.",
        "{out}"
    );
}

#[test]
fn synthesizes_result_for_trailing_tool_use() {
    let out = convert(
        "claude-test",
        &json!({
            "model": "claude-test",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Run ls."}]},
                {"type": "function_call", "call_id": "call_tail", "name": "exec", "arguments": "{}"}
            ]
        }),
    );
    assert_eq!(items(&out["messages"]).len(), 3, "message count: {out}");
    assert_eq!(out["messages"][2]["role"], "user", "{out}");
    assert_eq!(
        out["messages"][2]["content"][0]["type"], "tool_result",
        "{out}"
    );
    assert_eq!(
        out["messages"][2]["content"][0]["tool_use_id"], "call_tail",
        "{out}"
    );
}

#[test]
fn moves_tool_results_ahead_of_injected_text() {
    // A heartbeat seed can land between a tool call and its output. Anthropic
    // requires tool_result blocks to lead the user message.
    let out = convert(
        "claude-test",
        &json!({
            "model": "claude-test",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Run ls."}]},
                {"type": "function_call", "call_id": "call_hb", "name": "exec", "arguments": "{}"},
                {
                    "type": "function_call_output",
                    "id": "fco_seed",
                    "name": "automation_update",
                    "namespace": "codex_app",
                    "output": "<heartbeat>tick</heartbeat>"
                },
                {"type": "function_call_output", "call_id": "call_hb", "output": "a.txt"},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Continue."}]}
            ]
        }),
    );
    assert_eq!(items(&out["messages"]).len(), 3, "message count: {out}");
    let blocks = items(&out["messages"][2]["content"]);
    assert_eq!(blocks.len(), 3, "blocks in messages[2]: {out}");
    assert_eq!(blocks[0]["type"], "tool_result", "{out}");
    assert_eq!(blocks[0]["tool_use_id"], "call_hb", "{out}");
    assert_eq!(blocks[0]["content"], "a.txt", "{out}");
    assert_eq!(blocks[1]["text"], "<heartbeat>tick</heartbeat>", "{out}");
    assert_eq!(blocks[2]["text"], "Continue.", "{out}");
}

#[test]
fn synthesizes_result_for_trailing_tool_use_on_prefill_rejecting_model() {
    // Models that reject assistant prefill (fable/opus-5/sonnet-4-6) used to
    // drop the trailing tool_use before repair could answer it, losing the
    // call history entirely. The synthesized tool_result must survive instead.
    let out = convert(
        "claude-fable-5-1",
        &json!({
            "model": "claude-fable-5-1",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Run ls."}]},
                {"type": "function_call", "call_id": "call_tail", "name": "exec", "arguments": "{}"}
            ]
        }),
    );
    assert_eq!(items(&out["messages"]).len(), 3, "message count: {out}");
    assert_eq!(
        out["messages"][1]["content"][0]["type"], "tool_use",
        "{out}"
    );
    assert_eq!(out["messages"][2]["role"], "user", "{out}");
    assert_eq!(
        out["messages"][2]["content"][0]["type"], "tool_result",
        "{out}"
    );
    assert_eq!(
        out["messages"][2]["content"][0]["tool_use_id"], "call_tail",
        "{out}"
    );
}

#[test]
fn late_orphan_tool_result_folds_to_text() {
    // An output can pass the emittedToolUses gate but still land after an
    // intervening assistant message, where its tool_result no longer answers
    // the immediately preceding tool_use. The repair must fold it into text
    // and synthesize the missing answer for the real dangling call.
    let out = convert(
        "claude-test",
        &json!({
            "model": "claude-test",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Run ls."}]},
                {"type": "function_call", "call_id": "call_a", "name": "exec", "arguments": "{}"},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "wait"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "interlude"}]},
                {"type": "function_call_output", "call_id": "call_a", "output": "a.txt"}
            ]
        }),
    );
    // user | assistant(tool_use call_a) | user | assistant(text) | user
    assert_eq!(items(&out["messages"]).len(), 5, "message count: {out}");
    // The user message right after the dangling tool_use must lead with a
    // synthesized error tool_result for call_a.
    let synthesized = &out["messages"][2]["content"][0];
    assert_eq!(synthesized["type"], "tool_result", "{out}");
    assert_eq!(synthesized["tool_use_id"], "call_a", "{out}");
    assert!(
        bool_of(&synthesized["is_error"]),
        "a synthesized tool_result should be is_error: {out}"
    );
    // The late real output must not remain a tool_result after the interlude
    // assistant message; it folds into plain text.
    let last = &out["messages"][4];
    assert_eq!(last["role"], "user", "{out}");
    assert!(
        items(&last["content"])
            .iter()
            .all(|block| block["type"] != "tool_result"),
        "the late output must not remain a tool_result: {out}"
    );
    assert_eq!(last["content"][0]["text"], "a.txt", "{out}");
}

#[test]
fn empty_standalone_tool_output_keeps_marker() {
    // A standalone output with empty content must still produce a non-empty
    // user message; Anthropic rejects empty content arrays too. Both the
    // string form and the structured array form with an empty text part
    // collapse to the marker text.
    for output in [json!(""), json!([{"type": "input_text", "text": ""}])] {
        let out = convert(
            "claude-test",
            &json!({
                "model": "claude-test",
                "input": [{"type": "function_call_output", "call_id": "orphan", "output": output}]
            }),
        );
        assert_eq!(
            items(&out["messages"]).len(),
            1,
            "message count for output {output}: {out}"
        );
        let content = &out["messages"][0]["content"];
        if let Some(blocks) = content.as_array() {
            assert!(
                !blocks.is_empty(),
                "empty content array for output {output}: {out}"
            );
            assert_eq!(blocks[0]["type"], "text", "output {output}: {out}");
            assert!(
                !text(&blocks[0]["text"]).trim().is_empty(),
                "empty text block for output {output}: {out}"
            );
        } else {
            assert!(
                !text(content).trim().is_empty(),
                "empty string content for output {output}: {out}"
            );
        }
    }
}

#[test]
fn sanitized_id_collision_keeps_orphan_as_text() {
    // "call.custom:1" and "call_custom_1" sanitize to the same Claude id, but
    // they are distinct calls. The unpaired raw id must degrade to text while
    // the real pairing stays a tool_result.
    let out = convert(
        "claude-test",
        &json!({
            "model": "claude-test",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Run."}]},
                {"type": "function_call", "call_id": "call.custom:1", "name": "exec", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_custom_1", "output": "unrelated context"},
                {"type": "function_call_output", "call_id": "call.custom:1", "output": "real result"}
            ]
        }),
    );
    let blocks = items(&out["messages"][2]["content"]);
    assert_eq!(blocks.len(), 2, "blocks in messages[2]: {out}");
    assert_eq!(blocks[0]["type"], "tool_result", "{out}");
    assert_eq!(blocks[0]["content"], "real result", "{out}");
    assert_eq!(blocks[1]["type"], "text", "{out}");
    assert_eq!(blocks[1]["text"], "unrelated context", "{out}");
}

#[test]
fn empty_late_orphan_tool_result_keeps_non_empty_user_message() {
    // An empty late orphan output folds to nothing; the user message must
    // still carry a marker instead of degenerating to content:[] which
    // Anthropic also rejects.
    let out = convert(
        "claude-fable-5-1",
        &json!({
            "model": "claude-fable-5-1",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Run the tool."}]},
                {"type": "function_call", "call_id": "call_a", "name": "exec", "arguments": "{}"},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Wait."}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Interlude."}]},
                {"type": "function_call_output", "call_id": "call_a", "output": ""}
            ]
        }),
    );
    for message in items(&out["messages"]) {
        assert!(
            message["content"]
                .as_array()
                .is_none_or(|blocks| !blocks.is_empty()),
            "empty content array in a message: {out}"
        );
    }
    assert!(
        !all_blocks(&out).any(|block| block["type"] == "tool_result"
            && block["tool_use_id"] == "call_a"
            && !bool_of(&block["is_error"])),
        "the late empty orphan must not remain a tool_result: {out}"
    );
    let last = &out["messages"][4];
    assert_eq!(last["role"], "user", "{out}");
    assert_eq!(
        last["content"][0]["text"], "Tool result was empty.",
        "marker text: {out}"
    );
}

#[test]
fn late_orphan_tool_result_array_of_empty_text_keeps_marker() {
    // A late orphan result whose content is an array of empty text blocks
    // must not emit empty text blocks; the whole result folds to the marker.
    let out = convert(
        "claude-fable-5-1",
        &json!({
            "model": "claude-fable-5-1",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Run."}]},
                {"type": "function_call", "call_id": "a", "name": "exec", "arguments": "{}"},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Wait."}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Interlude."}]},
                {
                    "type": "function_call_output",
                    "call_id": "a",
                    "output": [{"type": "input_text", "text": ""}, {"type": "input_text", "text": ""}]
                }
            ]
        }),
    );
    for block in all_blocks(&out) {
        assert!(
            !(block["type"] == "text" && text(&block["text"]).trim().is_empty()),
            "empty text block emitted: {out}"
        );
        assert!(
            !(block["type"] == "tool_result"
                && block["tool_use_id"] == "a"
                && !bool_of(&block["is_error"])),
            "the late orphan must not remain a tool_result: {out}"
        );
    }
    assert_eq!(
        out["messages"][4]["content"][0]["text"], "Tool result was empty.",
        "marker text: {out}"
    );
}

#[test]
fn standalone_tool_output_drops_empty_text_in_mixed_array() {
    // A standalone output whose array mixes an empty text part with a real
    // one must not carry the empty block into the user message.
    let out = convert(
        "claude-test",
        &json!({
            "model": "claude-test",
            "input": [{
                "type": "function_call_output",
                "call_id": "orphan",
                "output": [{"type": "input_text", "text": ""}, {"type": "input_text", "text": "context"}]
            }]
        }),
    );
    for block in all_blocks(&out) {
        assert!(
            !(block["type"] == "text" && text(&block["text"]).trim().is_empty()),
            "empty text block emitted: {out}"
        );
        assert_ne!(
            block["type"], "tool_result",
            "a standalone output must not remain a tool_result: {out}"
        );
    }
    assert_eq!(out["messages"][0]["content"], "context", "{out}");
}

#[test]
fn message_invariant_problems_reports_broken_pairing() {
    let clean = [
        json!({"role": "user", "content": "hi"}),
        json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "exec", "input": {}}]}),
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "ok"},
            {"type": "text", "text": "go on"}
        ]}),
    ];
    let problems = message_invariant_problems(&clean);
    assert!(
        problems.is_empty(),
        "a clean history reported problems: {problems:?}"
    );

    let broken = [
        json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t0", "content": "orphan"}]}),
        json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "exec", "input": {}}]}),
        json!({"role": "user", "content": [
            {"type": "text", "text": "x"},
            {"type": "tool_result", "tool_use_id": "t2", "content": "ok"}
        ]}),
    ];
    let problems = message_invariant_problems(&broken);
    for fragment in [
        "tool_result t0 has no tool_use",
        "tool_result after non-tool_result block",
        "tool_use t1 has no tool_result",
        "tool_result t2 has no tool_use",
    ] {
        assert!(
            problems.iter().any(|problem| problem.contains(fragment)),
            "expected a problem containing {fragment:?}, got {problems:?}"
        );
    }

    let first_not_user = [json!({
        "role": "assistant",
        "content": [{"type": "tool_use", "id": "t1", "name": "exec", "input": {}}]
    })];
    let problems = message_invariant_problems(&first_not_user);
    assert!(
        problems
            .first()
            .is_some_and(|problem| problem.contains("first message is not user")),
        "expected the first-message problem, got {problems:?}"
    );
}

#[test]
fn keeps_tool_use_adjacent_to_tool_result() {
    let out = convert(
        "claude-test",
        &json!({
            "model": "claude-test",
            "input": [
                {
                    "type": "function_call",
                    "call_id": "call_00_awGuheXs4aRbtedNK8LE3743",
                    "name": "js",
                    "arguments": r#"{"code":"nodeRepl.write('ok')","title":"List Obsidian vault contents"}"#
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "I'll check your Obsidian vault for articles."}]
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_00_awGuheXs4aRbtedNK8LE3743",
                    "output": "Wall time: 0.1963 seconds\nOutput:\n[{\"type\":\"text\",\"text\":\"\"}]"
                }
            ]
        }),
    );
    assert_eq!(items(&out["messages"]).len(), 2, "message count: {out}");
    let assistant = &out["messages"][0];
    assert_eq!(assistant["role"], "assistant", "{out}");
    assert_eq!(
        assistant["content"][0]["text"], "I'll check your Obsidian vault for articles.",
        "{out}"
    );
    assert_eq!(assistant["content"][1]["type"], "tool_use", "{out}");
    assert_eq!(
        assistant["content"][1]["id"], "call_00_awGuheXs4aRbtedNK8LE3743",
        "{out}"
    );
    let result = &out["messages"][1]["content"][0];
    assert_eq!(result["type"], "tool_result", "{out}");
    assert_eq!(
        result["tool_use_id"], "call_00_awGuheXs4aRbtedNK8LE3743",
        "{out}"
    );
}

#[test]
fn keeps_apply_patch_custom_tool() {
    let out = convert(
        "claude-test",
        &json!({
            "model": "claude-test",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            "tools": [
                {
                    "type": "custom",
                    "name": "apply_patch",
                    "description": "Use the apply_patch tool to edit files.",
                    "format": {"type": "grammar", "syntax": "lark", "definition": "start: patch"}
                },
                {
                    "type": "function",
                    "name": "exec_command",
                    "description": "Runs a command.",
                    "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"]}
                }
            ]
        }),
    );
    assert_eq!(items(&out["tools"]).len(), 2, "tool count: {out}");
    let tool = tool_named(&out, "apply_patch").unwrap_or_else(|| panic!("no apply_patch: {out}"));
    let missing = missing_patch_instructions(&text(&tool["description"]));
    assert!(
        missing.is_empty(),
        "missing patch instructions {missing:?}: {tool}"
    );
    assert!(
        is_strict_patch_schema(&tool["input_schema"]),
        "the patch schema is not strict: {tool}"
    );
}

#[test]
fn normalizes_root_tool_schema_union() {
    let out = convert(
        "claude-test",
        &json!({
            "model": "claude-test",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            "tools": [{
                "type": "function",
                "name": "lookup",
                "parameters": {
                    "type": "object",
                    "properties": {"query": {"type": "string"}, "id": {"type": "string"}},
                    "oneOf": [{"required": ["query"]}, {"required": ["id"]}]
                }
            }]
        }),
    );
    let schema = &out["tools"][0]["input_schema"];
    assert_eq!(schema["type"], "object", "{out}");
    assert!(
        schema.get("oneOf").is_none(),
        "input_schema should not have a root oneOf: {out}"
    );
    assert!(
        schema["properties"].get("query").is_some() && schema["properties"].get("id").is_some(),
        "input_schema should keep the query and id properties: {out}"
    );
    assert!(
        schema.get("required").is_none(),
        "input_schema should not merge the alternatives' required fields: {out}"
    );
}

#[test]
fn merges_additional_tools_and_prefers_top_level() {
    let out = convert(
        "claude-test",
        &json!({
            "model": "claude-test",
            "tools": [
                {
                    "type": "function",
                    "name": "exec",
                    "description": "top-level exec",
                    "parameters": {"type": "object", "properties": {"command": {"type": "string"}}}
                },
                {
                    "type": "namespace",
                    "name": "collaboration",
                    "tools": [{"type": "function", "name": "spawn", "description": "top-level spawn", "parameters": {"type": "object", "properties": {}}}]
                }
            ],
            "input": [
                {
                    "type": "additional_tools",
                    "role": "developer",
                    "tools": [
                        {"type": "custom", "name": "exec", "description": "additional exec"},
                        {"type": "function", "name": "wait", "parameters": {"type": "object", "properties": {}}},
                        {"type": "namespace", "name": "collaboration", "tools": [
                            {"type": "function", "name": "spawn", "parameters": {"type": "object", "properties": {}}},
                            {"type": "custom", "name": "send", "description": "send a message"}
                        ]}
                    ]
                },
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]}
            ]
        }),
    );
    assert_eq!(items(&out["tools"]).len(), 4, "tool count: {out}");
    assert_eq!(
        tool_named(&out, "exec").map(|tool| text(&tool["description"])),
        Some("top-level exec".to_owned()),
        "{out}"
    );
    assert!(
        tool_named(&out, "wait").is_some(),
        "missing the additional function: {out}"
    );
    assert!(
        tool_named(&out, "collaboration__spawn").is_some(),
        "missing the namespace function: {out}"
    );
    let custom = tool_named(&out, "collaboration__send")
        .unwrap_or_else(|| panic!("missing the namespace custom tool: {out}"));
    assert_eq!(
        custom["input_schema"]["properties"]["input"]["type"], "string",
        "{out}"
    );
}

#[test]
fn deduplicates_expanded_tool_names() {
    let request = json!({
        "model": "claude-test",
        "tools": [{
            "type": "function",
            "name": "collaboration__send",
            "description": "top-level send",
            "parameters": {"type": "object", "properties": {}}
        }],
        "input": [{"type": "additional_tools", "tools": [{"type": "namespace", "name": "collaboration", "tools": [
            {"type": "function", "name": "send", "description": "additional send", "parameters": {"type": "object", "properties": {}}},
            {"type": "function", "name": "other", "parameters": {"type": "object", "properties": {}}}
        ]}]}]
    });
    let out = convert("claude-test", &request);
    assert_eq!(items(&out["tools"]).len(), 2, "tool count: {out}");
    assert_eq!(
        tool_named(&out, "collaboration__send").map(|tool| text(&tool["description"])),
        Some("top-level send".to_owned()),
        "{out}"
    );
    assert!(
        tool_named(&out, "collaboration__other").is_some(),
        "the unique namespace child was dropped: {out}"
    );
    let tools = RequestTools::new(&request);
    assert!(
        !tools.custom_names().contains("collaboration__send"),
        "a final-name collision should keep the top-level function type"
    );
    assert_eq!(
        tools.split("collaboration__send"),
        ("collaboration__send".to_owned(), String::new())
    );
}

#[test]
fn direct_tool_wins_over_earlier_namespace_collision() {
    let request = json!({
        "model": "claude-test",
        "tools": [
            {"type": "namespace", "name": "n", "tools": [{"type": "function", "name": "x", "parameters": {"type": "object", "properties": {}}}]},
            {"type": "custom", "name": "n__x"}
        ],
        "tool_choice": {"type": "custom", "name": "n__x"}
    });
    let out = convert("claude-test", &request);
    assert_eq!(items(&out["tools"]).len(), 1, "tool count: {out}");
    assert_eq!(out["tools"][0]["name"], "n__x", "{out}");
    assert_eq!(
        out["tools"][0]["input_schema"]["properties"]["input"]["type"], "string",
        "the winner is a custom tool: {out}"
    );
    assert_eq!(out["tool_choice"]["name"], "n__x", "{out}");
    assert!(
        RequestTools::new(&request).custom_names().contains("n__x"),
        "the winning direct custom tool was not classified as custom"
    );
}

#[test]
fn prefers_direct_tool_across_additional_sources() {
    let request = json!({
        "model": "claude-test",
        "input": [
            {"type": "additional_tools", "tools": [{"type": "namespace", "name": "n", "tools": [
                {"type": "function", "name": "x", "description": "namespace x", "parameters": {"type": "object", "properties": {}}}
            ]}]},
            {"type": "additional_tools", "tools": [{"type": "custom", "name": "n__x", "description": "direct x"}]}
        ]
    });
    let out = convert("claude-test", &request);
    assert_eq!(items(&out["tools"]).len(), 1, "tool count: {out}");
    let tool = &out["tools"][0];
    assert_eq!(tool["name"], "n__x", "{out}");
    assert_eq!(tool["description"], "direct x", "{out}");
    assert_eq!(
        tool["input_schema"]["properties"]["input"]["type"], "string",
        "the winner is a custom tool: {out}"
    );
    assert!(
        RequestTools::new(&request).custom_names().contains("n__x"),
        "the direct custom tool should win classification across additional sources"
    );
}

#[test]
fn preserves_tool_declaration_order() {
    let out = convert(
        "claude-test",
        &json!({
            "model": "claude-test",
            "tools": [
                {"type": "function", "name": "first", "parameters": {"type": "object", "properties": {}}},
                {"type": "namespace", "name": "n", "tools": [{"type": "function", "name": "middle", "parameters": {"type": "object", "properties": {}}}]},
                {"type": "function", "name": "last", "parameters": {"type": "object", "properties": {}}}
            ]
        }),
    );
    let names: Vec<String> = items(&out["tools"])
        .iter()
        .map(|tool| text(&tool["name"]))
        .collect();
    assert_eq!(names, ["first", "n__middle", "last"], "{out}");
}

#[test]
fn replays_custom_tool_call_history() {
    let out = convert(
        "claude-test",
        &json!({
            "model": "claude-test",
            "input": [
                {"type": "custom_tool_call", "call_id": "call.custom:1", "name": "exec", "input": "pwd"},
                {"type": "custom_tool_call_output", "call_id": "call.custom:1", "output": "/workspace"}
            ]
        }),
    );
    let tool_use = &out["messages"][0]["content"][0];
    assert_eq!(tool_use["type"], "tool_use", "{out}");
    assert_eq!(tool_use["id"], "call_custom_1", "{out}");
    assert_eq!(tool_use["input"]["input"], "pwd", "{out}");
    let tool_result = &out["messages"][1]["content"][0];
    assert_eq!(tool_result["type"], "tool_result", "{out}");
    assert_eq!(tool_result["tool_use_id"], "call_custom_1", "{out}");
    assert_eq!(tool_result["content"], "/workspace", "{out}");
}

#[test]
fn replays_namespaced_function_call_history() {
    let out = convert(
        "claude-test",
        &json!({
            "model": "claude-test",
            "input": [
                {"type": "additional_tools", "tools": [{"type": "namespace", "name": "mcp__node_repl", "tools": [
                    {"type": "function", "name": "js", "parameters": {"type": "object", "properties": {}}}
                ]}]},
                {
                    "type": "function_call",
                    "call_id": "call.namespace",
                    "name": "js",
                    "namespace": "mcp__node_repl",
                    "arguments": r#"{"code":"pwd"}"#
                },
                {"type": "function_call_output", "call_id": "call.namespace", "output": "ok"}
            ]
        }),
    );
    assert!(
        tool_named(&out, "mcp__node_repl__js").is_some(),
        "missing the qualified namespace tool declaration: {out}"
    );
    assert_eq!(
        out["messages"][0]["content"][0]["name"], "mcp__node_repl__js",
        "{out}"
    );
    assert_eq!(
        out["messages"][1]["content"][0]["tool_use_id"], "call_namespace",
        "{out}"
    );
}

#[test]
fn maps_custom_and_namespaced_tool_choice() {
    let cases = [
        (
            "custom",
            json!({
                "model": "claude-test",
                "tools": [{"type": "custom", "name": "exec"}],
                "tool_choice": {"type": "custom", "name": "exec"}
            }),
            "exec",
        ),
        (
            "namespace",
            json!({
                "model": "claude-test",
                "input": [{"type": "additional_tools", "tools": [{"type": "namespace", "name": "mcp__node_repl", "tools": [{"type": "function", "name": "js"}]}]}],
                "tool_choice": {"type": "function", "name": "js", "namespace": "mcp__node_repl"}
            }),
            "mcp__node_repl__js",
        ),
        (
            "top-level short name wins",
            json!({
                "model": "claude-test",
                "tools": [{"type": "function", "name": "foo"}],
                "input": [{"type": "additional_tools", "tools": [{"type": "namespace", "name": "mcp__tools", "tools": [{"type": "function", "name": "foo"}]}]}],
                "tool_choice": {"type": "function", "name": "foo"}
            }),
            "foo",
        ),
    ];
    for (name, request, want) in cases {
        let out = convert("claude-test", &request);
        assert_eq!(out["tool_choice"]["type"], "tool", "{name}: {out}");
        assert_eq!(out["tool_choice"]["name"], want, "{name}: {out}");
    }
}

#[test]
fn qualified_namespace_tool_names_avoid_prefix_collision() {
    let cases = [
        ("collab", "collaboration", "collab__collaboration"),
        ("collab", "collab__send", "collab__send"),
        ("collab__", "send", "collab__send"),
        ("mcp__node_repl", "mcp__node_repl__js", "mcp__node_repl__js"),
    ];
    for (namespace, child, want) in cases {
        assert_eq!(
            qualify(namespace, child),
            want,
            "qualify({namespace:?}, {child:?})"
        );
    }

    let out = convert(
        "claude-test",
        &json!({
            "tools": [{"type": "namespace", "name": "collab", "tools": [{"type": "function", "name": "collaboration"}]}]
        }),
    );
    assert_eq!(out["tools"][0]["name"], "collab__collaboration", "{out}");
}

#[test]
fn splits_qualified_function_call_from_additional_tools() {
    let request = json!({
        "input": [{"type": "additional_tools", "tools": [{"type": "namespace", "name": "mcp__node_repl", "tools": [{"type": "function", "name": "js"}]}]}]
    });
    assert_eq!(
        RequestTools::new(&request).split("mcp__node_repl__js"),
        ("js".to_owned(), "mcp__node_repl".to_owned())
    );
}

#[test]
fn preserves_content_part_cache_control() {
    let out = convert(
        "claude-sonnet-4-5",
        &json!({
            "model": "gpt-4.1",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "cached prefix", "cache_control": {"type": "ephemeral"}},
                    {"type": "input_text", "text": "fresh question"}
                ]
            }]
        }),
    );
    let content = &out["messages"][0]["content"];
    assert!(
        content.is_array(),
        "content should be an array when cache_control is present: {out}"
    );
    assert_eq!(content[0]["cache_control"]["type"], "ephemeral", "{out}");
    assert!(
        content[1].get("cache_control").is_none(),
        "the second part should not have cache_control: {out}"
    );
}

#[test]
fn system_level_inputs_become_separate_system_blocks() {
    let out = convert(
        "claude-sonnet-4-5",
        &json!({
            "model": "gpt-4.1",
            "instructions": "I1",
            "input": [
                {"type": "message", "role": "system", "content": [{"type": "input_text", "text": "S1"}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "U1"}]},
                {"type": "message", "role": "developer", "content": "D1"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "A1"}]},
                {"type": "message", "role": "system", "content": [{"type": "input_text", "text": "S2"}]}
            ]
        }),
    );
    assert_eq!(
        out["system"],
        json!([
            {"type": "text", "text": "I1"},
            {"type": "text", "text": "S1"},
            {"type": "text", "text": "D1"},
            {"type": "text", "text": "S2"}
        ]),
        "{out}"
    );
    let messages = items(&out["messages"]);
    assert_eq!(messages.len(), 2, "message count: {out}");
    assert_eq!(messages[0]["role"], "user", "{out}");
    assert_eq!(messages[1]["role"], "assistant", "{out}");
    let raw_messages = out["messages"].to_string();
    for system_text in ["I1", "S1", "D1"] {
        assert!(
            !raw_messages.contains(system_text),
            "system-level text {system_text} must not be downgraded into messages: {out}"
        );
    }
    assert!(
        !raw_messages.contains(r#""role":"system""#),
        "the translator must not emit role=system messages: {out}"
    );
}

#[test]
fn system_only_input_keeps_fallback_user_message() {
    let out = convert(
        "claude-opus-5",
        &json!({"model": "gpt-4.1", "instructions": "I1"}),
    );
    assert_eq!(items(&out["system"]).len(), 1, "system blocks: {out}");
    let messages = items(&out["messages"]);
    assert_eq!(messages.len(), 1, "message count: {out}");
    assert_eq!(messages[0]["role"], "user", "{out}");
}

#[test]
fn system_non_text_part_kept_as_typed_marker() {
    let out = convert(
        "claude-opus-5",
        &json!({
            "model": "gpt-4.1",
            "input": [
                {"type": "message", "role": "developer", "content": [
                    {"type": "input_text", "text": "D1"},
                    {"type": "input_image", "image_url": "data:image/png;base64,AAAA"}
                ]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "U1"}]}
            ]
        }),
    );
    let system = items(&out["system"]);
    assert_eq!(system.len(), 2, "system blocks: {out}");
    assert_eq!(system[0]["text"], "D1", "{out}");
    assert_eq!(system[1]["type"], "input_image", "{out}");
    assert!(
        system[1].get("source").is_none(),
        "an unsupported marker must not copy the payload: {out}"
    );
}

#[test]
fn system_item_cache_control_applies_to_last_block() {
    let out = convert(
        "claude-opus-5",
        &json!({
            "model": "gpt-4.1",
            "input": [
                {"type": "message", "role": "system", "cache_control": {"type": "ephemeral"}, "content": [
                    {"type": "input_text", "text": "S1"},
                    {"type": "input_text", "text": "S2"}
                ]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "U1"}]}
            ]
        }),
    );
    let system = items(&out["system"]);
    assert_eq!(system.len(), 2, "system blocks: {out}");
    assert!(
        system[0].get("cache_control").is_none(),
        "the first block must not carry cache_control: {out}"
    );
    assert_eq!(system[1]["cache_control"]["type"], "ephemeral", "{out}");
}

#[test]
fn deduplicates_tool_outputs() {
    // Duplicate outputs collapse to the final payload, emitted where the first
    // occurrence was (before later assistant turns); outputs with and without
    // IDs each behave properly.
    let out = convert(
        "claude-test",
        &json!({
            "model": "claude-test",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Use lookup."}]},
                {"type": "function_call", "call_id": "toolu_dup", "name": "lookup", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "toolu_dup", "output": "first result"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Intermediate step"}]},
                {"type": "function_call", "call_id": "toolu_parallel", "name": "other", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "toolu_dup", "output": "final result"},
                {"type": "custom_tool_call_output", "call_id": "call.custom:dup", "output": "custom first"},
                {"type": "custom_tool_call_output", "call_id": "call.custom:dup", "output": "custom final"},
                {"type": "function_call_output", "call_id": "toolu_parallel", "output": "parallel result"},
                {"type": "function_call_output", "call_id": "", "output": "empty id output"}
            ]
        }),
    );
    let messages = items(&out["messages"]);
    assert!(messages.len() >= 5, "message count: {out}");

    assert_eq!(messages[0]["role"], "user", "{out}");

    // The assistant's tool_use for toolu_dup.
    assert_eq!(messages[1]["content"][0]["type"], "tool_use", "{out}");
    assert_eq!(messages[1]["content"][0]["id"], "toolu_dup", "{out}");

    // Its tool_result carries the final payload, before the next assistant turn.
    assert_eq!(messages[2]["role"], "user", "{out}");
    assert_eq!(messages[2]["content"][0]["type"], "tool_result", "{out}");
    assert_eq!(
        messages[2]["content"][0]["tool_use_id"], "toolu_dup",
        "{out}"
    );
    assert_eq!(
        messages[2]["content"][0]["content"], "final result",
        "{out}"
    );

    // The intermediate text, then the tool_use for toolu_parallel.
    assert_eq!(messages[3]["role"], "assistant", "{out}");
    assert_eq!(
        messages[3]["content"][0]["text"], "Intermediate step",
        "{out}"
    );
    assert_eq!(messages[3]["content"][1]["id"], "toolu_parallel", "{out}");

    // toolu_parallel's real tool_result leads the user message. call_custom_dup
    // never had a tool_use, so its final payload is plain user text (Claude
    // rejects orphan tool_result blocks); so is the output with an empty ID.
    let blocks = items(&messages[4]["content"]);
    assert_eq!(blocks.len(), 3, "blocks in messages[4]: {out}");
    assert_eq!(blocks[0]["tool_use_id"], "toolu_parallel", "{out}");
    assert_eq!(blocks[0]["content"], "parallel result", "{out}");
    assert_eq!(blocks[1]["type"], "text", "{out}");
    assert_eq!(blocks[1]["text"], "custom final", "{out}");
    assert_eq!(blocks[2]["type"], "text", "{out}");
    assert_eq!(blocks[2]["text"], "empty id output", "{out}");
}

#[test]
fn priority_service_tier_becomes_fast_speed() {
    let cases = [
        ("absent service_tier omits speed", None, None, None),
        (
            "default service_tier omits speed",
            Some("default"),
            None,
            None,
        ),
        (
            "standard service_tier omits speed",
            Some("standard"),
            None,
            None,
        ),
        (
            "unsupported service_tier omits speed",
            Some("flex"),
            None,
            None,
        ),
        (
            "priority service_tier emits fast speed",
            Some("priority"),
            None,
            Some("fast"),
        ),
        (
            "priority with low reasoning effort",
            Some("priority"),
            Some("low"),
            Some("fast"),
        ),
        (
            "priority with medium reasoning effort",
            Some("priority"),
            Some("medium"),
            Some("fast"),
        ),
        (
            "priority with high reasoning effort",
            Some("priority"),
            Some("high"),
            Some("fast"),
        ),
        (
            "priority with xhigh reasoning effort",
            Some("priority"),
            Some("xhigh"),
            Some("fast"),
        ),
        (
            "priority with max reasoning effort",
            Some("priority"),
            Some("max"),
            Some("fast"),
        ),
    ];
    for (name, service_tier, effort, want) in cases {
        let mut request = json!({
            "model": "claude-3-7-sonnet-20250219",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]}]
        });
        if let Some(service_tier) = service_tier {
            request["service_tier"] = json!(service_tier);
        }
        if let Some(effort) = effort {
            request["reasoning"] = json!({"effort": effort});
        }
        let out = convert("claude-3-7-sonnet-20250219", &request);
        assert_eq!(
            out.get("speed").and_then(Value::as_str),
            want,
            "{name}: {out}"
        );
    }
}

#[test]
fn preserves_caller_supplied_metadata_user_id() {
    let cases = [
        ("plain string", "custom-resp-user-123"),
        ("special characters and json string", "foo\"bar\nbaz\\qux"),
        (
            "claude code json format",
            r#"{"device_id":"0000000000000000000000000000000000000000000000000000000000000000","session_id":"11111111-2222-4333-8444-555555555555"}"#,
        ),
    ];
    for (name, user_id) in cases {
        let out = convert(
            "claude-test",
            &json!({
                "model": "claude-test",
                "metadata": {"user_id": user_id},
                "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]}]
            }),
        );
        assert_eq!(
            out["metadata"],
            json!({"user_id": user_id}),
            "{name}: {out}"
        );
    }
}

#[test]
fn preserves_user_field() {
    let out = convert(
        "claude-test",
        &json!({
            "model": "claude-test",
            "user": "openai-resp-user-456",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]}]
        }),
    );
    assert_eq!(
        out["metadata"],
        json!({"user_id": "openai-resp-user-456"}),
        "{out}"
    );
}

// Upstream derives distinct IDs from the session key; we never make IDs up, so neither gets one.
#[test]
fn session_key_does_not_produce_a_user_id() {
    for session in ["resp-session-a", "resp-session-b"] {
        let out = convert(
            "claude-test",
            &json!({
                "model": "claude-test",
                "prompt_cache_key": session,
                "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]}]
            }),
        );
        assert_eq!(out["metadata"], json!({}), "{session}: {out}");
    }
}

// Replaces upstream's derived-ID test: only an ID the client sent becomes metadata.user_id.
#[test]
fn metadata_user_id_comes_only_from_the_client() {
    for question in ["user question A", "user question B"] {
        let out = convert(
            "claude-test",
            &json!({
                "model": "claude-test",
                "instructions": "global instruction",
                "input": [
                    {"type": "message", "role": "system", "content": "system context"},
                    {"type": "message", "role": "user", "content": question}
                ]
            }),
        );
        assert_eq!(out["metadata"], json!({}), "{question}: {out}");
    }

    let cases = [
        (
            "blank metadata.user_id falls back to user",
            json!({"metadata": {"user_id": "  "}, "user": "resp-user"}),
            json!({"user_id": "resp-user"}),
        ),
        (
            "non-string metadata.user_id falls back to user",
            json!({"metadata": {"user_id": 42}, "user": "resp-user"}),
            json!({"user_id": "resp-user"}),
        ),
        (
            "metadata.user_id is passed on as sent",
            json!({"metadata": {"user_id": " padded "}, "user": "resp-user"}),
            json!({"user_id": " padded "}),
        ),
        ("blank user gives none", json!({"user": " \t"}), json!({})),
        ("non-string user gives none", json!({"user": 7}), json!({})),
    ];
    for (name, mut request, want) in cases {
        request["input"] = json!("hello");
        let out = convert("claude-test", &request);
        assert_eq!(out["metadata"], want, "{name}: {out}");
    }
}

#[test]
fn fable_strips_trailing_assistant_prefill() {
    let out = convert(
        "claude-fable-5",
        &json!({
            "model": "claude-fable-5",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "progress update"}]}
            ]
        }),
    );
    let messages = items(&out["messages"]);
    assert_eq!(
        messages.len(),
        1,
        "the trailing assistant prefill should be stripped: {out}"
    );
    assert_eq!(messages[0]["role"], "user", "{out}");
    assert_eq!(messages[0]["content"], "hello", "{out}");
}

#[test]
fn fable_only_assistant_message_yields_fallback_user() {
    let out = convert(
        "claude-fable-5",
        &json!({
            "model": "claude-fable-5",
            "input": [
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "orphan progress"}]}
            ]
        }),
    );
    let messages = items(&out["messages"]);
    assert_eq!(
        messages.len(),
        1,
        "expected one fallback user message: {out}"
    );
    assert_eq!(messages[0]["role"], "user", "{out}");
}

#[test]
fn unsupported_prefill_models_strip_trailing_assistant() {
    for model in [
        "claude-fable-5",
        "claude-opus-5",
        "claude-sonnet-4-6",
        "fable",
        "opus-5",
        "sonnet-4.6",
        "anthropic/claude-opus-5-thinking",
        "claude-sonnet-4.6",
        "claude-sonnet-4-7",
        "claude-sonnet-4-10",
        "claude-sonnet-5",
        "claude-opus-6",
        "claude-opus-5.1",
        "claude-sonnet-4-6-20260217",
        " ANTHROPIC/CLAUDE-OPUS-5-THINKING ",
    ] {
        let out = convert(
            model,
            &json!({
                "model": model,
                "input": [
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]},
                    {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "progress update"}]}
                ]
            }),
        );
        let messages = items(&out["messages"]);
        assert_eq!(
            messages.len(),
            1,
            "{model}: the trailing assistant prefill should be stripped: {out}"
        );
        assert_eq!(messages[0]["role"], "user", "{model}: {out}");
        assert_eq!(messages[0]["content"], "hello", "{model}: {out}");
    }
}

#[test]
fn supported_prefill_models_preserve_assistant_prefill() {
    for model in [
        "claude-sonnet-4-5",
        "claude-haiku-4-5",
        "claude-3-opus-20240229",
        "claude-opus-20240229",
        "claude-sonnet-4-20260217",
        "claude-sonnet-4.5",
        "not-a-fable-model",
        "my-custom-opus-5-wrapper",
        "my-sonnet-4-6-wrapper",
        "claude-fabled-5",
        "claude-opus-5foo",
        "claude-sonnet-4-6foo",
        "fable/gpt-4o",
        "opus",
        "sonnet",
        "",
    ] {
        let out = convert(
            model,
            &json!({
                "model": model,
                "input": [
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]},
                    {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "prefill text"}]}
                ]
            }),
        );
        let messages = items(&out["messages"]);
        assert_eq!(
            messages.len(),
            2,
            "{model}: the assistant prefill should be kept: {out}"
        );
        assert_eq!(messages[1]["role"], "assistant", "{model}: {out}");
        assert_eq!(messages[1]["content"], "prefill text", "{model}: {out}");
    }
}

#[test]
fn with_compat_fable_preserves_assistant_prefill() {
    let out = convert_with_compat(
        "claude-fable-5",
        &json!({
            "model": "claude-fable-5",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "prefill text"}]}
            ]
        }),
    );
    let messages = items(&out["messages"]);
    assert_eq!(messages.len(), 2, "message count in compat mode: {out}");
    assert_eq!(messages[1]["role"], "assistant", "{out}");
}

/// A `claude-haiku-4-5-20251001` request with these input items.
fn haiku_request(input: Value) -> Value {
    json!({"model": "claude-haiku-4-5-20251001", "input": input})
}

#[test]
fn trailing_thinking_alone_drops_the_assistant_message() {
    let (raw_signature, _) = claude_thinking_signature();
    let out = convert(
        "claude-haiku-4-5-20251001",
        &haiku_request(json!([
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]},
            {"type": "reasoning", "encrypted_content": raw_signature, "summary": [{"type": "summary_text", "text": "thought"}]}
        ])),
    );
    let messages = items(&out["messages"]);
    assert_eq!(
        messages.len(),
        1,
        "only the user message should remain: {out}"
    );
    assert_eq!(messages[0]["role"], "user", "{out}");
    assert_eq!(messages[0]["content"], "hello", "{out}");
}

#[test]
fn trailing_thinking_is_stripped_after_assistant_text() {
    let (raw_signature, _) = claude_thinking_signature();
    let out = convert(
        "claude-haiku-4-5-20251001",
        &haiku_request(json!([
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "prefill text"}]},
            {"type": "reasoning", "encrypted_content": raw_signature, "summary": [{"type": "summary_text", "text": "thought"}]}
        ])),
    );
    let messages = items(&out["messages"]);
    assert_eq!(
        messages.len(),
        2,
        "user and assistant text should remain: {out}"
    );
    assert_eq!(messages[1]["role"], "assistant", "{out}");
    assert_eq!(messages[1]["content"], "prefill text", "{out}");
}

#[test]
fn trailing_redacted_thinking_is_stripped() {
    let (raw_signature, _) = claude_thinking_signature();
    let out = convert(
        "claude-haiku-4-5-20251001",
        &haiku_request(json!([
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "prefill text"}]},
            {"type": "reasoning", "encrypted_content": raw_signature, "summary": []},
            {
                "type": "reasoning",
                "encrypted_content": format!("{REDACTED_THINKING_PREFIX}redacted-data"),
                "summary": []
            }
        ])),
    );
    let messages = items(&out["messages"]);
    assert_eq!(
        messages.len(),
        2,
        "user and assistant text should remain: {out}"
    );
    assert_eq!(messages[1]["content"], "prefill text", "{out}");
}

#[test]
fn only_reasoning_item_yields_fallback_user_message() {
    let (raw_signature, _) = claude_thinking_signature();
    let out = convert(
        "claude-haiku-4-5-20251001",
        &haiku_request(json!([
            {"type": "reasoning", "encrypted_content": raw_signature, "summary": [{"type": "summary_text", "text": "thought"}]}
        ])),
    );
    let messages = items(&out["messages"]);
    assert_eq!(
        messages.len(),
        1,
        "expected one fallback user message: {out}"
    );
    assert_eq!(messages[0]["role"], "user", "{out}");
}

#[test]
fn with_compat_preserves_trailing_thinking() {
    let (raw_signature, _) = claude_thinking_signature();
    let out = convert_with_compat(
        "claude-haiku-4-5-20251001",
        &haiku_request(json!([
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]},
            {"type": "reasoning", "encrypted_content": raw_signature, "summary": [{"type": "summary_text", "text": "thought"}]}
        ])),
    );
    assert_eq!(
        items(&out["messages"]).len(),
        2,
        "message count in compat mode: {out}"
    );
}

#[test]
fn keeps_agent_message_text() {
    let request = json!({"model": "claude-fable-5-1", "input": [
        {"type": "agent_message", "content": [{"type": "input_text", "text": "do X"}]},
        {"type": "agent_message", "content": [{"type": "encrypted_content", "encrypted_content": "secret task"}]},
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "plain"}]}
    ]});
    for (name, out) in [
        ("standard", convert("claude-fable-5-1", &request)),
        ("compat", convert_with_compat("claude-fable-5-1", &request)),
    ] {
        let messages = items(&out["messages"]);
        assert_eq!(messages.len(), 1, "{name}: message count: {out}");
        assert_eq!(messages[0]["role"], "user", "{name}: {out}");
        let texts: Vec<String> = items(&messages[0]["content"])
            .iter()
            .map(|part| text(&part["text"]))
            .collect();
        assert_eq!(texts, ["do X", "secret task", "plain"], "{name}: {out}");
    }

    let out = convert(
        "claude-fable-5-1",
        &json!({"model": "claude-fable-5-1", "input": [
            {"type": "agent_message", "content": [
                {"type": "input_text", "text": "step 1"},
                {"type": "encrypted_content", "encrypted_content": "step 2"}
            ]}
        ]}),
    );
    let texts: Vec<String> = items(&out["messages"][0]["content"])
        .iter()
        .map(|part| text(&part["text"]))
        .collect();
    assert_eq!(
        texts,
        ["step 1", "step 2"],
        "mixed content in one agent message: {out}"
    );
}

/// The texts of a translated request's system blocks.
fn system_texts(out: &Value) -> Vec<String> {
    items(&out["system"])
        .iter()
        .map(|block| text(&block["text"]))
        .collect()
}

#[test]
fn json_schema_text_format_becomes_system_instruction() {
    let out = convert(
        "claude-sonnet-4-6",
        &json!({
            "model": "claude-sonnet-4-6",
            "input": "Extract facts.",
            "text": {
                "format": {
                    "type": "json_schema",
                    "name": "extracted_facts",
                    "schema": {
                        "type": "object",
                        "properties": {"facts": {"type": "array", "items": {"type": "string"}}},
                        "required": ["facts"]
                    }
                }
            }
        }),
    );
    let system = system_texts(&out);
    assert!(!system.is_empty(), "system blocks missing: {out}");
    assert!(
        system.iter().any(|text| text.contains("facts")),
        "expected the schema instruction in the system prompt: {out}"
    );
}

#[test]
fn json_object_text_format_becomes_system_instruction() {
    let out = convert(
        "claude-sonnet-4-6",
        &json!({
            "model": "claude-sonnet-4-6",
            "input": "Return JSON.",
            "text": {"format": {"type": "json_object"}}
        }),
    );
    let system = system_texts(&out);
    assert!(!system.is_empty(), "system blocks missing: {out}");
    assert!(
        system.iter().any(|text| text.contains("JSON object")),
        "expected the JSON object instruction in the system prompt: {out}"
    );
}

#[test]
fn text_format_keeps_instructions_and_beats_response_format() {
    let out = convert(
        "claude-sonnet-4-6",
        &json!({
            "model": "claude-sonnet-4-6",
            "instructions": "Be concise.",
            "input": "Extract facts.",
            "text": {
                "format": {
                    "type": "json_schema",
                    "name": "winning_schema",
                    "description": "Primary facts",
                    "schema": {"type": "object", "properties": {"item": {"type": "string"}}}
                }
            },
            "response_format": {"type": "json_object"}
        }),
    );
    let system = system_texts(&out);
    assert!(
        system.len() >= 2,
        "expected at least 2 system blocks: {out}"
    );
    assert!(
        system.iter().any(|text| text.contains("Be concise.")),
        "the instructions are missing: {out}"
    );
    assert!(
        system.iter().any(|text| text.contains("winning_schema")
            && text.contains("Primary facts")
            && text.contains("item")),
        "the winning schema is missing: {out}"
    );
    assert!(
        !system.iter().any(|text| text.contains("valid JSON object")),
        "response_format should not apply: {out}"
    );
}

#[test]
fn function_call_output_alternate_ids_and_queue_fallback() {
    let cases = [
        ("call_id standard", Some(("call_id", "call_123"))),
        (
            "tool_call_id alternate field",
            Some(("tool_call_id", "call_123")),
        ),
        ("callId alternate field", Some(("callId", "call_123"))),
        ("id alternate field", Some(("id", "call_123"))),
        ("missing call_id falls back to the pending queue", None),
    ];
    for (name, id_field) in cases {
        let mut output = json!({"type": "function_call_output", "output": "tool_result_ok"});
        if let Some((key, id)) = id_field {
            output[key] = json!(id);
        }
        let out = convert(
            "claude-sonnet-4-6",
            &json!({
                "model": "claude-sonnet-4-6",
                "input": [
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "run"}]},
                    {"type": "function_call", "call_id": "call_123", "name": "Bash", "arguments": r#"{"command":"ls"}"#},
                    output
                ]
            }),
        );
        let messages = items(&out["messages"]);
        assert_eq!(
            messages.len(),
            3,
            "{name}: expected user, assistant, user: {out}"
        );
        let tool_use = &messages[1]["content"][0];
        assert_eq!(tool_use["type"], "tool_use", "{name}: {out}");
        assert_eq!(tool_use["id"], "call_123", "{name}: {out}");
        let tool_result = &messages[2]["content"][0];
        assert_eq!(tool_result["type"], "tool_result", "{name}: {out}");
        assert_eq!(tool_result["tool_use_id"], "call_123", "{name}: {out}");
        assert_eq!(tool_result["content"], "tool_result_ok", "{name}: {out}");
    }
}

/// Each tool_result's content in the user messages, by tool_use_id.
fn tool_results_by_id(out: &Value) -> HashMap<String, String> {
    items(&out["messages"])
        .iter()
        .filter(|message| message["role"] == "user")
        .flat_map(|message| items(&message["content"]))
        .filter(|part| part["type"] == "tool_result")
        .map(|part| (text(&part["tool_use_id"]), text(&part["content"])))
        .collect()
}

#[test]
fn mixed_missing_and_explicit_parallel_outputs() {
    // Calls A and B. The first output has no ID (it is B's); the second names
    // call_a. The first must not take call_a; it gets call_b.
    let out = convert(
        "claude-sonnet-4-6",
        &json!({
            "model": "claude-sonnet-4-6",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "run"}]},
                {"type": "function_call", "call_id": "call_a", "name": "tool_a", "arguments": "{}"},
                {"type": "function_call", "call_id": "call_b", "name": "tool_b", "arguments": "{}"},
                {"type": "function_call_output", "output": "result_b"},
                {"type": "function_call_output", "call_id": "call_a", "output": "result_a"}
            ]
        }),
    );
    let messages = items(&out["messages"]);
    assert_eq!(messages.len(), 3, "message count: {out}");
    assert_eq!(
        items(&messages[2]["content"]).len(),
        2,
        "tool_result count: {out}"
    );
    let results = tool_results_by_id(&out);
    assert_eq!(
        results.get("call_a").map(String::as_str),
        Some("result_a"),
        "{out}"
    );
    assert_eq!(
        results.get("call_b").map(String::as_str),
        Some("result_b"),
        "{out}"
    );
}

#[test]
fn mixed_missing_and_explicit_parallel_outputs_across_user_message() {
    // As above, with a user message between the two outputs.
    let out = convert(
        "claude-sonnet-4-6",
        &json!({
            "model": "claude-sonnet-4-6",
            "input": [
                {"type": "function_call", "call_id": "call_a", "name": "tool_a", "arguments": "{}"},
                {"type": "function_call", "call_id": "call_b", "name": "tool_b", "arguments": "{}"},
                {"type": "function_call_output", "output": "result_b"},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "status?"}]},
                {"type": "function_call_output", "call_id": "call_a", "output": "result_a"}
            ]
        }),
    );
    let results = tool_results_by_id(&out);
    assert_eq!(
        results.get("call_a").map(String::as_str),
        Some("result_a"),
        "{out}"
    );
    assert_eq!(
        results.get("call_b").map(String::as_str),
        Some("result_b"),
        "{out}"
    );
}

#[test]
fn string_input_becomes_user_message() {
    let out = convert(
        "claude-sonnet-4-6",
        &json!({"model": "claude-sonnet-4-6", "input": "hi", "max_output_tokens": 16, "stream": false}),
    );
    let messages = items(&out["messages"]);
    assert_eq!(messages.len(), 1, "message count: {out}");
    assert_eq!(messages[0]["role"], "user", "{out}");
    let content = &messages[0]["content"];
    let message_text = match content.as_array() {
        Some(parts) => {
            assert_eq!(parts.len(), 1, "content part count: {out}");
            assert_eq!(parts[0]["type"], "text", "{out}");
            text(&parts[0]["text"])
        }
        None => text(content),
    };
    assert_eq!(message_text, "hi", "{out}");

    // With instructions: a system block and the user message.
    let out = convert(
        "claude-sonnet-4-6",
        &json!({
            "model": "claude-sonnet-4-6",
            "instructions": "Be concise.",
            "input": "hello world",
            "max_output_tokens": 32,
            "stream": false
        }),
    );
    assert_eq!(system_texts(&out), ["Be concise."], "{out}");
    let messages = items(&out["messages"]);
    assert_eq!(messages.len(), 1, "message count with instructions: {out}");
    assert_eq!(messages[0]["role"], "user", "{out}");
    assert_eq!(messages[0]["content"], "hello world", "{out}");

    // Quotes, newlines and non-ASCII text.
    let complex = "line 1\n\"line 2\"\n你好，世界 🌍";
    let out = convert(
        "claude-sonnet-4-6",
        &json!({"model": "claude-sonnet-4-6", "input": complex, "max_output_tokens": 16, "stream": false}),
    );
    let messages = items(&out["messages"]);
    assert_eq!(messages.len(), 1, "message count with complex input: {out}");
    assert_eq!(messages[0]["content"], complex, "{out}");
}

const ACME_NAMESPACE: &str = "mcp__example_apps__acme_inventory_service";
const ACME_PRICES: &str = "acme_inventory_service_get_item_prices";
const ACME_METRICS: &str = "acme_inventory_service_get_item_metrics";

/// A namespace whose qualified child names are too long for Claude.
fn acme_namespace() -> Value {
    json!({
        "type": "namespace",
        "name": ACME_NAMESPACE,
        "tools": [
            {"type": "function", "name": ACME_PRICES, "description": "prices", "parameters": {"type": "object", "properties": {}}},
            {"type": "function", "name": ACME_METRICS, "description": "metrics", "parameters": {"type": "object", "properties": {}}}
        ]
    })
}

#[test]
fn long_tool_names_are_unique_and_reversible() {
    let request = json!({
        "model": "claude-sonnet-5-5",
        "input": "test",
        "stream": false,
        "tools": [acme_namespace()]
    });
    let out = convert("claude-sonnet-5-5", &request);
    let tools = items(&out["tools"]);
    assert_eq!(tools.len(), 2, "tool count: {out}");
    let name0 = text(&tools[0]["name"]);
    let name1 = text(&tools[1]["name"]);
    assert!(
        name0.len() <= 64 && name1.len() <= 64,
        "tool names must not exceed 64 bytes: {name0:?}, {name1:?}"
    );
    assert_ne!(name0, name1, "tool names must be unique");

    let names = RequestTools::new(&request);
    assert_eq!(
        names.split(&name0),
        (ACME_PRICES.to_owned(), ACME_NAMESPACE.to_owned()),
        "split({name0:?})"
    );
    assert_eq!(
        names.split(&name1),
        (ACME_METRICS.to_owned(), ACME_NAMESPACE.to_owned()),
        "split({name1:?})"
    );

    // The non-stream response restores the original name and namespace.
    let response = [
        r#"data: {"type":"message_start","message":{"id":"msg_test_123","usage":{"input_tokens":10,"output_tokens":5}}}"#.to_owned(),
        format!(
            "data: {}",
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "call_test_0", "name": name0, "input": {}}})
        ),
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"item_id\":\"42\"}"}}"#.to_owned(),
        r#"data: {"type":"content_block_stop","index":0}"#.to_owned(),
        r#"data: {"type":"message_stop"}"#.to_owned(),
    ]
    .join("\n");
    let translated = convert_claude_response_to_openai_responses_non_stream(
        &request,
        &Value::Null,
        response.as_bytes(),
    );
    let item = &translated["output"][0];
    assert_eq!(item["type"], "function_call", "{translated}");
    assert_eq!(item["name"], ACME_PRICES, "{translated}");
    assert_eq!(item["namespace"], ACME_NAMESPACE, "{translated}");

    // So does the stream.
    let chunks = [
        r#"data: {"type":"message_start","message":{"id":"msg_stream_123","usage":{"input_tokens":10,"output_tokens":5}}}"#.to_owned(),
        format!(
            "data: {}",
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "call_test_1", "name": name1, "input": {}}})
        ),
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"metric\":\"latency\"}"}}"#.to_owned(),
        r#"data: {"type":"content_block_stop","index":0}"#.to_owned(),
        r#"data: {"type":"message_stop"}"#.to_owned(),
    ];
    let mut stream =
        ClaudeToOpenAIResponsesStream::new("claude-sonnet-5-5", &request, &Value::Null);
    let (mut added, mut done, mut completed) = (Value::Null, Value::Null, Value::Null);
    for chunk in &chunks {
        for (event, data) in sse_events(&stream.translate_line(chunk.as_bytes())) {
            match event.as_str() {
                "response.output_item.added" if data["item"]["type"] == "function_call" => {
                    added = data;
                }
                "response.output_item.done" if data["item"]["type"] == "function_call" => {
                    done = data;
                }
                "response.completed" => completed = data,
                _ => {}
            }
        }
    }
    for (event, item) in [
        ("added", &added["item"]),
        ("done", &done["item"]),
        ("completed", &completed["response"]["output"][0]),
    ] {
        assert_eq!(item["name"], ACME_METRICS, "stream {event}: {item}");
        assert_eq!(item["namespace"], ACME_NAMESPACE, "stream {event}: {item}");
    }
}

#[test]
fn sanitization_collisions_get_distinct_reversible_names() {
    let request = json!({
        "model": "claude-sonnet-5-5",
        "input": "test",
        "stream": false,
        "tools": [
            {"type": "function", "name": "a_b", "description": "ab", "parameters": {"type": "object", "properties": {}}},
            {"type": "function", "name": "a.b", "description": "a.b", "parameters": {"type": "object", "properties": {}}}
        ]
    });
    let out = convert("claude-sonnet-5-5", &request);
    let tools = items(&out["tools"]);
    assert_eq!(tools.len(), 2, "tool count: {out}");
    let name0 = text(&tools[0]["name"]);
    let name1 = text(&tools[1]["name"]);
    assert_ne!(name0, name1, "tool names must not collide");
    assert_eq!(name0, "a_b", "{out}");
    assert!(
        name1.starts_with("a_b_"),
        "tool 1 name = {name1:?}, want a_b_<hash>"
    );

    let names = RequestTools::new(&request);
    assert_eq!(names.split(&name0), ("a_b".to_owned(), String::new()));
    assert_eq!(names.split(&name1), ("a.b".to_owned(), String::new()));
}

#[test]
fn long_tool_choice_and_history_use_the_declared_name() {
    let out = convert(
        "claude-sonnet-5-5",
        &json!({
            "model": "claude-sonnet-5-5",
            "input": [
                {"type": "message", "role": "user", "content": "check item"},
                {
                    "type": "function_call",
                    "call_id": "call_123",
                    "namespace": ACME_NAMESPACE,
                    "name": ACME_PRICES,
                    "arguments": "{}"
                },
                {"type": "function_call_output", "call_id": "call_123", "output": r#"{"price": 100}"#}
            ],
            "stream": false,
            "tool_choice": {"type": "function", "namespace": ACME_NAMESPACE, "name": ACME_PRICES},
            "tools": [acme_namespace()]
        }),
    );
    let declared = &out["tools"][0]["name"];
    assert_eq!(&out["tool_choice"]["name"], declared, "tool_choice: {out}");
    let history_name = items(&out["messages"])
        .iter()
        .filter(|message| message["role"] == "assistant")
        .flat_map(|message| items(&message["content"]))
        .filter(|part| part["type"] == "tool_use")
        .map(|part| &part["name"])
        .next_back();
    assert_eq!(history_name, Some(declared), "history tool_use: {out}");
}

#[test]
fn apply_patch_request_contract_and_history() {
    let patch = "*** Begin Patch\n*** Add File: a.txt\n+hello\n*** End Patch";
    let out = convert(
        "test",
        &json!({
            "tools": [{"type": "namespace", "name": "editor", "tools": [{
                "type": "custom",
                "name": "apply_patch",
                "description": "Edit files. This is a FREEFORM tool, so do not wrap the patch in JSON.",
                "format": {"type": "grammar", "syntax": "lark", "definition": "start: patch"},
                "cache_control": {"type": "ephemeral"}
            }]}],
            "input": [
                {"type": "custom_tool_call", "namespace": "editor", "name": "apply_patch", "call_id": "c1", "input": patch},
                {"type": "custom_tool_call_output", "call_id": "c1", "output": "done"}
            ]
        }),
    );
    let tool = &out["tools"][0];
    let description = text(&tool["description"]);
    let missing = missing_patch_instructions(&description);
    assert!(
        missing.is_empty(),
        "missing instructions {missing:?}: {tool}"
    );
    assert!(
        !description.contains("do not wrap the patch in JSON"),
        "contradictory freeform instructions: {tool}"
    );
    assert!(
        is_strict_patch_schema(&tool["input_schema"]),
        "the schema is not strict: {tool}"
    );
    assert_eq!(
        tool["cache_control"]["type"], "ephemeral",
        "cache control lost: {tool}"
    );
    let call = &out["messages"][0]["content"][0];
    assert_eq!(call["name"], "editor__apply_patch", "{out}");
    assert_eq!(call["input"]["input"], patch, "{out}");
    assert_eq!(call["id"], "c1", "{out}");
    assert_eq!(
        out["messages"][1]["content"][0]["tool_use_id"], "c1",
        "{out}"
    );
    assert_eq!(out["messages"][1]["content"][0]["content"], "done", "{out}");
}

#[test]
fn custom_tool_input_unwraps_like_upstream() {
    // Expected values from upstream's unwrapCustomToolInput.
    let cases = [
        (
            r#"{"input":"*** Begin Patch\n*** End Patch"}"#,
            "*** Begin Patch\n*** End Patch",
        ),
        (r#"  {"input": 5}  "#, "5"),
        (
            r#"{"input":{"b":1,"a":[true,null]}}"#,
            r#"{"b":1,"a":[true,null]}"#,
        ),
        (r#"{"input":null}"#, "null"),
        // Truncated arguments are read up to the end, escapes and all.
        (
            r#"{"input":"line\nmore \u00e9\ud83d\ude00 x\"#,
            "line\nmore \u{e9}\u{1f600} x\\",
        ),
        (r#"{"input":"\ud800 x"}"#, "\u{fffd} x"),
        (r#"{"input":"\udc00\ud800z"}"#, "\u{fffd}z"),
        (r#"{"input":"\ud83dz"}"#, "\u{fffd}z"),
        (r#"  {"patch":"x"} "#, r#"  {"patch":"x"} "#),
        (r#"{"a":{"input":"x\"y"}}"#, "x\"y"),
        (r#""input" : "abc"#, "abc"),
        (r#"{"input":"abc\u12"#, r"abc\u12"),
        ("", ""),
        // gjson reads a field from malformed JSON too, and decodes a string
        // up to an escape it doesn't know.
        (r#"{"input":"a\qb"}"#, "a"),
        (r#"{"input":"\u00zz"}"#, "\u{0}"),
        (r#"{"input" 1}"#, "1"),
        ("{\"input\":\"\u{e9}\\xc3\\xa9\"}", "\u{e9}"),
        (r#"{"input":"cut.txt\+no end"}"#, "cut.txt"),
        (r#"{"input":"first","input":"second"}"#, "first"),
        (r#"{"input": {"b" : 1} }"#, r#"{"b" : 1}"#),
    ];
    for (arguments, want) in cases {
        assert_eq!(unwrap_custom_tool_input(arguments), want, "{arguments}");
    }
}

const RESPONSES_USER_HELLO: &str =
    r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}"#;
const RESPONSES_ASSISTANT_HI: &str =
    r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}"#;
const RESPONSES_USER_NEXT: &str =
    r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"next"}]}"#;
const RESPONSES_FILE_ID_PART: &str = r#"{"type":"input_file","file_id":"file-1"}"#;
const RESPONSES_AUDIO_PART: &str =
    r#"{"type":"input_audio","input_audio":{"data":"UklGRg==","format":"wav"}}"#;
const RESPONSES_TEXT_PART: &str = r#"{"type":"input_text","text":"keep me"}"#;
const RESPONSES_INLINE_FILE: &str = r#"{"type":"input_file","filename":"a.pdf","file_data":"data:application/pdf;base64,JVBERi0xLjQK"}"#;
const RESPONSES_DEVELOPER_MSG: &str =
    r#"{"type":"message","role":"developer","content":[{"type":"input_text","text":"dev"}]}"#;

fn responses_user_turn(parts: &[&str]) -> String {
    format!(
        r#"{{"type":"message","role":"user","content":[{}]}}"#,
        parts.join(",")
    )
}

fn responses_payload(instructions: &str, items: &[&str]) -> Value {
    let mut prefix = r#"{"model":"claude-sonnet-4","#.to_owned();
    if !instructions.is_empty() {
        prefix += &format!(r#""instructions":"{instructions}","#);
    }
    serde_json::from_str(&format!(r#"{prefix}"input":[{}]}}"#, items.join(","))).unwrap()
}

// TestConvertOpenAIResponsesRequestToClaude_RefusesAnyEmptiedUserTurn
#[test]
fn refuses_any_emptied_user_turn() {
    let file_id_turn = responses_user_turn(&[RESPONSES_FILE_ID_PART]);
    let audio_turn = responses_user_turn(&[RESPONSES_AUDIO_PART]);
    let file_url_turn =
        responses_user_turn(&[r#"{"type":"input_file","file_url":"https://example.test/a.pdf"}"#]);
    let cases = [
        (
            "history then file id only",
            responses_payload(
                "",
                &[RESPONSES_USER_HELLO, RESPONSES_ASSISTANT_HI, &file_id_turn],
            ),
            "input_file",
        ),
        (
            "audio only after history",
            responses_payload(
                "",
                &[RESPONSES_USER_HELLO, RESPONSES_ASSISTANT_HI, &audio_turn],
            ),
            "input_audio",
        ),
        (
            "instructions and developer prompt do not hide the empty turn",
            responses_payload("sys", &[RESPONSES_DEVELOPER_MSG, &file_id_turn]),
            "input_file",
        ),
        (
            "emptied turn before a later text turn",
            responses_payload(
                "",
                &[&file_id_turn, RESPONSES_ASSISTANT_HI, RESPONSES_USER_NEXT],
            ),
            "input_file",
        ),
        (
            "file url carries no bytes",
            responses_payload("", &[&file_url_turn]),
            "input_file",
        ),
    ];
    for (name, input, want) in cases {
        let (body, err) = convert_with_compat_checked("claude-sonnet-4", &input);
        let err = err.unwrap_or_else(|| panic!("{name}: no refusal; body = {body}"));
        assert_eq!(err.part_type, want, "{name}");
        assert_eq!(err.status_code(), 400, "{name}");
        assert_eq!(
            err.to_string(),
            format!("unsupported content part: {want}"),
            "{name}"
        );
        assert!(body.is_object(), "{name}: {body}");

        let err = crate::registry::Registry::global()
            .translate_request_checked(
                &"openai-response".into(),
                &"claude".into(),
                "claude-sonnet-4",
                input,
                false,
            )
            .expect_err(name);
        assert_eq!(err.part_type, want, "{name}: registry");
    }
}

// TestConvertOpenAIResponsesRequestToClaude_KeepsTurnWithTextBesideAttachment
#[test]
fn keeps_turn_with_text_beside_attachment() {
    for (name, attachment) in [
        ("file id", RESPONSES_FILE_ID_PART),
        ("audio", RESPONSES_AUDIO_PART),
    ] {
        let turn = responses_user_turn(&[RESPONSES_TEXT_PART, attachment]);
        let input = responses_payload("", &[RESPONSES_USER_HELLO, RESPONSES_ASSISTANT_HI, &turn]);
        let (body, err) = convert_with_compat_checked("claude-sonnet-4", &input);
        assert_eq!(err, None, "{name}");
        assert_eq!(
            text(&body["messages"][2]["content"]),
            "keep me",
            "{name}: {body}"
        );
    }
}

// TestConvertOpenAIResponsesRequestToClaude_InlineFileStaysADocument
#[test]
fn inline_file_stays_a_document() {
    let turn = responses_user_turn(&[RESPONSES_INLINE_FILE]);
    let input = responses_payload("", &[RESPONSES_USER_HELLO, RESPONSES_ASSISTANT_HI, &turn]);
    let body = convert_with_compat("claude-sonnet-4", &input);
    let part = &body["messages"][2]["content"][0];
    assert_eq!(text(&part["type"]), "document", "{body}");
    assert_eq!(text(&part["source"]["data"]), "JVBERi0xLjQK", "{body}");
}

// TestConvertOpenAIResponsesRequestToClaude_ExportedWrappersKeepAJSONBody
#[test]
fn exported_wrappers_keep_a_json_body() {
    let input = responses_payload("", &[&responses_user_turn(&[RESPONSES_FILE_ID_PART])]);
    let (plain, err_plain) = convert_openai_responses_request_to_claude(
        "claude-sonnet-4",
        &input,
        false,
        ModelCatalog::embedded(),
    );
    let (compat, err_compat) = convert_with_compat_checked("claude-sonnet-4", &input);
    assert!(plain.is_object() && compat.is_object());
    assert!(err_plain.is_some() && err_compat.is_some());
}
