//! Reasoning replay through the executor, against the mock Codex. Ported
//! from upstream's
//! `internal/runtime/executor/codex_executor_reasoning_replay_cache_test.go`:
//! the tests that run the executor. The ones that call the replay functions
//! directly are in `codex/replay/tests.rs`.
//!
//! The tests share the process's cache, as the executor does, so each uses a
//! session of its own where upstream's clear the cache first. Upstream's
//! `CacheCodexReasoningReplayItem` is `ReplayCache::store` with one item,
//! and its `GetCodexReasoningReplayItem` is `ReplayCache::get_item`.

use sha2::{Digest, Sha256};

use super::*;
use crate::codex::replay_cache::ReplayCache;
use crate::codex::replay_cache::tests::valid_encrypted_content;
use crate::json::str_at;

/// An API key credential for `base_url` with its own key.
fn api_key_auth_with(base_url: &str, key: &str) -> Arc<Auth> {
    let mut auth = Auth {
        provider: "codex".into(),
        ..Auth::default()
    };
    auth.attributes.insert("base_url".into(), base_url.into());
    auth.attributes.insert("api_key".into(), key.into());
    Arc::new(auth)
}

/// A Claude payload whose `metadata.user_id` names `session` as Claude Code
/// does, with `messages`.
fn claude_payload(session: &str, messages: &str) -> String {
    format!(
        r#"{{"model":"gpt-5.4","metadata":{{"user_id":"{{\"device_id\":\"device-test\",\"account_uuid\":\"\",\"session_id\":\"{session}\"}}"}},"messages":{messages}}}"#
    )
}

const USER_HELLO: &str = r#"[{"role":"user","content":[{"type":"text","text":"hello"}]}]"#;

const USER_NEXT: &str = r#"[{"role":"user","content":[{"type":"text","text":"next"}]}]"#;

const LOOKUP_TOOL: &str =
    r#"[{"name":"lookup","input_schema":{"type":"object","properties":{"q":{"type":"string"}}}}]"#;

/// A Claude payload with the `lookup` tool.
fn claude_tool_payload(session: &str, messages: &str) -> String {
    let payload = claude_payload(session, messages);
    let mut payload: Value = serde_json::from_str(&payload).unwrap();
    payload["tools"] = serde_json::from_str(LOOKUP_TOOL).unwrap();
    payload.to_string()
}

fn sse(events: &[String]) -> String {
    let mut body: String = events
        .iter()
        .map(|event| format!("data: {event}\n"))
        .collect();
    body.push('\n');
    body
}

fn reasoning_done(id: &str, encrypted: &str) -> String {
    format!(
        r#"{{"type":"response.output_item.done","item":{{"id":"{id}","type":"reasoning","summary":[],"encrypted_content":"{encrypted}"}},"output_index":0}}"#
    )
}

fn reasoning_added(encrypted: &str) -> String {
    format!(
        r#"{{"type":"response.output_item.added","item":{{"id":"rs_added","type":"reasoning","status":"in_progress","summary":[],"encrypted_content":"{encrypted}"}},"output_index":0}}"#
    )
}

fn completed() -> String {
    r#"{"type":"response.completed","response":{"id":"resp_1","object":"response","created_at":0,"status":"completed","model":"gpt-5.4","output":[]}}"#.to_owned()
}

fn invalid_signature_failure() -> String {
    r#"{"type":"response.failed","response":{"id":"resp_1","status":"failed","error":{"message":"Invalid signature in thinking block","type":"invalid_request_error","code":"invalid_request_error"}}}"#.to_owned()
}

/// A cached reasoning item, as upstream's tests store one.
fn reasoning_item(encrypted: &str) -> Value {
    json!({"type": "reasoning", "summary": [], "content": null, "encrypted_content": encrypted})
}

fn store_reasoning(session: &str, encrypted: &str) {
    assert!(ReplayCache::global().store("gpt-5.4", session, &[&reasoning_item(encrypted)]));
}

/// Upstream's `shortenedCodexReplayCallIDForTest`: the ID a Claude client
/// sees for a call ID longer than 64 bytes.
fn shortened_call_id(id: &str) -> String {
    const LIMIT: usize = 64;
    if id.len() <= LIMIT {
        return id.to_owned();
    }
    let sum = Sha256::digest(id.as_bytes());
    let suffix: String = std::iter::once("_".to_owned())
        .chain(sum[..8].iter().map(|byte| format!("{byte:02x}")))
        .collect();
    format!("{}{suffix}", &id[..LIMIT - suffix.len()])
}

fn input_type(body: &Value, index: usize) -> String {
    str_at(body, &format!("input.{index}.type"))
}

// TestCodexExecutorReasoningReplayCacheStoresFinalDoneAndInjectsNextClaudeRequest.
#[tokio::test]
async fn stores_final_done_and_injects_next_claude_request() {
    let added = valid_encrypted_content(1);
    let done = valid_encrypted_content(2);
    let mock = Mock::start(Reply::sse(&sse(&[
        reasoning_added(&added),
        reasoning_done("rs_done", &done),
        r#"{"type":"response.completed","response":{"id":"resp_1","object":"response","created_at":0,"status":"completed","model":"gpt-5.4","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#.to_owned(),
    ])))
    .await;
    for messages in [USER_HELLO, USER_NEXT] {
        executor()
            .execute(
                api_key_auth(&mock.url),
                request("gpt-5.4", &claude_payload("session-1", messages)),
                options("claude"),
            )
            .await
            .unwrap();
    }

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let second = requests[1].json();
    assert_eq!(input_type(&second, 0), "reasoning", "{second}");
    assert_eq!(
        str_at(&second, "input.0.encrypted_content"),
        done,
        "{second}"
    );
    assert_eq!(str_at(&second, "input.1.role"), "user", "{second}");
}

// TestCodexExecutorReasoningReplayCacheSharesSameSessionAcrossCodexAuths.
#[tokio::test]
async fn shares_same_session_across_codex_auths() {
    let encrypted = valid_encrypted_content(12);
    let mock = Mock::start(Reply::sse(&sse(&[
        reasoning_done("rs_done", &encrypted),
        completed(),
    ])))
    .await;
    for (key, messages) in [("test-a", USER_HELLO), ("test-b", USER_NEXT)] {
        executor()
            .execute(
                api_key_auth_with(&mock.url, key),
                request("gpt-5.4", &claude_payload("session-auth-switch", messages)),
                options("claude"),
            )
            .await
            .unwrap();
    }

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let second = requests[1].json();
    assert_eq!(input_type(&second, 0), "reasoning", "{second}");
    assert_eq!(str_at(&second, "input.0.encrypted_content"), encrypted);
}

// TestCodexExecutorReasoningReplayCacheDoesNotInjectNativeResponsesRequest.
#[tokio::test]
async fn does_not_inject_native_responses_request() {
    store_reasoning("prompt-cache:native-session", &valid_encrypted_content(3));
    let mock = Mock::start(Reply::sse(&sse(&[completed()]))).await;
    executor()
        .execute(
            api_key_auth(&mock.url),
            request(
                "gpt-5.4",
                r#"{"model":"gpt-5.4","prompt_cache_key":"native-session","input":[{"role":"user","content":"native"}]}"#,
            ),
            options("openai-response"),
        )
        .await
        .unwrap();

    let body = mock.last().json();
    assert_ne!(input_type(&body, 0), "reasoning", "{body}");
    assert_eq!(str_at(&body, "input.0.role"), "user", "{body}");
}

// TestCodexExecutorReasoningReplayCacheDoesNotStoreNativeResponsesRequest.
#[tokio::test]
async fn does_not_store_native_responses_request() {
    let encrypted = valid_encrypted_content(4);
    let mock = Mock::start(Reply::sse(&sse(&[format!(
        r#"{{"type":"response.completed","response":{{"id":"resp_1","object":"response","created_at":0,"status":"completed","model":"gpt-5.4","output":[{{"id":"rs_native","type":"reasoning","summary":[],"encrypted_content":"{encrypted}"}}]}}}}"#
    )])))
    .await;
    executor()
        .execute(
            api_key_auth(&mock.url),
            request(
                "gpt-5.4",
                r#"{"model":"gpt-5.4","prompt_cache_key":"native-store","input":[{"role":"user","content":"native"}]}"#,
            ),
            options("openai-response"),
        )
        .await
        .unwrap();

    assert_eq!(
        ReplayCache::global().get_item("gpt-5.4", "prompt-cache:native-store"),
        None
    );
}

// TestCodexExecutorReasoningReplayCacheDoesNotDuplicateClaudeClientReasoning.
#[tokio::test]
async fn does_not_duplicate_claude_client_reasoning() {
    let cached = valid_encrypted_content(5);
    let client = valid_encrypted_content(6);
    store_reasoning("claude:session-2:agent:main", &cached);
    let mock = Mock::start(Reply::sse(&sse(&[completed()]))).await;
    let messages = format!(
        r#"[{{"role":"assistant","content":[{{"type":"thinking","thinking":"client summary","signature":"{client}"}},{{"type":"text","text":"answer"}}]}},{{"role":"user","content":[{{"type":"text","text":"next"}}]}}]"#
    );
    executor()
        .execute(
            api_key_auth(&mock.url),
            request("gpt-5.4", &claude_payload("session-2", &messages)),
            options("claude"),
        )
        .await
        .unwrap();

    let body = mock.last().json();
    assert_eq!(str_at(&body, "input.0.encrypted_content"), client, "{body}");
    let reasoning = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| str_at(item, "type") == "reasoning")
        .count();
    assert_eq!(reasoning, 1, "{body}");
}

// TestCodexExecutorReasoningReplayCacheInsertsReasoningBeforeAssistantOutputInClaudeHistory.
#[tokio::test]
async fn inserts_reasoning_before_assistant_output_in_claude_history() {
    let cached = valid_encrypted_content(7);
    store_reasoning("claude:session-history:agent:main", &cached);
    let mock = Mock::start(Reply::sse(&sse(&[completed()]))).await;
    let messages = r#"[
        {"role":"user","content":[{"type":"text","text":"first"}]},
        {"role":"assistant","content":[{"type":"text","text":"answer"}]},
        {"role":"user","content":[{"type":"text","text":"next"}]}
    ]"#;
    executor()
        .execute(
            api_key_auth(&mock.url),
            request("gpt-5.4", &claude_payload("session-history", messages)),
            options("claude"),
        )
        .await
        .unwrap();

    let body = mock.last().json();
    assert_eq!(str_at(&body, "input.0.role"), "user", "{body}");
    assert_eq!(input_type(&body, 1), "reasoning", "{body}");
    assert_eq!(str_at(&body, "input.1.encrypted_content"), cached, "{body}");
    assert_eq!(str_at(&body, "input.2.role"), "assistant", "{body}");
    assert_eq!(str_at(&body, "input.3.role"), "user", "{body}");
}

// TestCodexExecutorReasoningReplayCacheExecuteStreamStoresFinalDoneForClaude.
#[tokio::test]
async fn execute_stream_stores_final_done_for_claude() {
    let added = valid_encrypted_content(7);
    let done = valid_encrypted_content(8);
    let mock = Mock::start(Reply::sse(&sse(&[
        reasoning_added(&added),
        reasoning_done("rs_done", &done),
        completed(),
    ])))
    .await;
    let response = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request("gpt-5.4", &claude_payload("stream-session-1", USER_HELLO)),
            stream_options("claude"),
        )
        .await
        .unwrap();
    let (_, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");

    executor()
        .execute(
            api_key_auth(&mock.url),
            request("gpt-5.4", &claude_payload("stream-session-1", USER_NEXT)),
            options("claude"),
        )
        .await
        .unwrap();

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let second = requests[1].json();
    assert_eq!(
        str_at(&second, "input.0.encrypted_content"),
        done,
        "{second}"
    );
}

// TestCodexExecutorReasoningReplayCacheClearsOnNonStreamResponseFailedInvalidSignature.
#[tokio::test]
async fn clears_on_non_stream_response_failed_invalid_signature() {
    let session = "claude:session-invalid-nonstream:agent:main";
    store_reasoning(session, &valid_encrypted_content(9));
    let mock = Mock::start(Reply::sse(&sse(&[invalid_signature_failure()]))).await;
    let result = executor()
        .execute(
            api_key_auth(&mock.url),
            request(
                "gpt-5.4",
                &claude_payload("session-invalid-nonstream", USER_NEXT),
            ),
            options("claude"),
        )
        .await;
    assert!(result.is_err(), "expected the invalid signature error");
    assert_eq!(ReplayCache::global().get_item("gpt-5.4", session), None);
}

// TestCodexExecutorReasoningReplayCacheClearsOnStreamResponseFailedInvalidSignature.
#[tokio::test]
async fn clears_on_stream_response_failed_invalid_signature() {
    let session = "claude:session-invalid-stream:agent:main";
    store_reasoning(session, &valid_encrypted_content(10));
    let mock = Mock::start(Reply::sse(&sse(&[invalid_signature_failure()]))).await;
    let response = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request(
                "gpt-5.4",
                &claude_payload("session-invalid-stream", USER_NEXT),
            ),
            stream_options("claude"),
        )
        .await
        .unwrap();
    let (_, error) = collect(response).await;
    assert!(error.is_some(), "expected the invalid signature error");
    assert_eq!(ReplayCache::global().get_item("gpt-5.4", session), None);
}

// TestCodexExecutorReasoningReplayCacheReplaysFunctionCallForClaudeToolResult.
#[tokio::test]
async fn replays_function_call_for_claude_tool_result() {
    let encrypted = valid_encrypted_content(8);
    let call = r#"{"id":"fc_1","type":"function_call","call_id":"call_1","name":"lookup","arguments":"{\"q\":\"weather\"}","status":"STATUS"}"#;
    let mock = Mock::start(Reply::sse(&sse(&[
        reasoning_done("rs_1", &encrypted),
        format!(
            r#"{{"type":"response.output_item.added","item":{},"output_index":1}}"#,
            call.replace("STATUS", "in_progress")
        ),
        format!(
            r#"{{"type":"response.output_item.done","item":{},"output_index":1}}"#,
            call.replace("STATUS", "completed")
        ),
        completed(),
    ])))
    .await;
    let first = r#"[{"role":"user","content":[{"type":"text","text":"call lookup"}]}]"#;
    let second = r#"[
        {"role":"user","content":[{"type":"text","text":"call lookup"}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"sunny"}]}
    ]"#;
    for messages in [first, second] {
        executor()
            .execute(
                api_key_auth(&mock.url),
                request(
                    "gpt-5.4",
                    &claude_tool_payload("claude-session-tool", messages),
                ),
                options("claude"),
            )
            .await
            .unwrap();
    }

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let body = requests[1].json();
    assert_eq!(input_type(&body, 0), "message", "{body}");
    assert_eq!(input_type(&body, 1), "reasoning", "{body}");
    assert_eq!(input_type(&body, 2), "function_call", "{body}");
    assert_eq!(str_at(&body, "input.2.call_id"), "call_1", "{body}");
    assert_eq!(input_type(&body, 3), "function_call_output", "{body}");
    assert_eq!(str_at(&body, "input.3.call_id"), "call_1", "{body}");
}

// TestCodexExecutorReasoningReplayCacheMatchesShortenedClaudeToolResultCallID.
#[tokio::test]
async fn matches_shortened_claude_tool_result_call_id() {
    let long_id = format!("call_{}", "a".repeat(62));
    let short_id = shortened_call_id(&long_id);
    assert!(
        long_id.len() > 64 && short_id.len() <= 64 && short_id != long_id,
        "invalid test setup: long={long_id} short={short_id}"
    );
    let encrypted = valid_encrypted_content(13);
    let mock = Mock::start(Reply::sse(&sse(&[
        reasoning_done("rs_long", &encrypted),
        format!(
            r#"{{"type":"response.output_item.done","item":{{"id":"fc_long","type":"function_call","call_id":"{long_id}","name":"lookup","arguments":"{{\"q\":\"weather\"}}","status":"completed"}},"output_index":1}}"#
        ),
        completed(),
    ])))
    .await;
    let first = r#"[{"role":"user","content":[{"type":"text","text":"call lookup"}]}]"#;
    let second = format!(
        r#"[
        {{"role":"user","content":[{{"type":"text","text":"call lookup"}}]}},
        {{"role":"user","content":[{{"type":"tool_result","tool_use_id":"{short_id}","content":"sunny"}}]}}
    ]"#
    );
    for messages in [first, second.as_str()] {
        executor()
            .execute(
                api_key_auth(&mock.url),
                request(
                    "gpt-5.4",
                    &claude_tool_payload("claude-session-short-tool", messages),
                ),
                options("claude"),
            )
            .await
            .unwrap();
    }

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let body = requests[1].json();
    assert_eq!(input_type(&body, 0), "message", "{body}");
    assert_eq!(input_type(&body, 1), "reasoning", "{body}");
    assert_eq!(input_type(&body, 2), "function_call", "{body}");
    assert_eq!(str_at(&body, "input.2.call_id"), short_id, "{body}");
    assert_eq!(input_type(&body, 3), "function_call_output", "{body}");
    assert_eq!(str_at(&body, "input.3.call_id"), short_id, "{body}");
}

// Not upstream's: two turns of a Claude Code session named by its header,
// the first streamed and the second not. The second request carries the
// first turn's reasoning before the answer it led to, and the session goes
// to Codex in no header and no `prompt_cache_key`.
#[tokio::test]
async fn two_claude_turns_carry_reasoning() {
    let encrypted = valid_encrypted_content(51);
    let mock = Mock::start(Reply::sse(&sse(&[
        reasoning_done("rs_1", &encrypted),
        r#"{"type":"response.output_item.done","item":{"id":"msg_1","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Sunny.","annotations":[]}]},"output_index":1}"#.to_owned(),
        completed(),
    ])))
    .await;
    let session =
        |options: Options| with_header(options, "x-claude-code-session-id", "two-turn-session");
    let payload =
        |messages: &str| format!(r#"{{"model":"gpt-5.4","max_tokens":64,"messages":{messages}}}"#);

    let response = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request(
                "gpt-5.4",
                &payload(r#"[{"role":"user","content":"Weather?"}]"#),
            ),
            session(stream_options("claude")),
        )
        .await
        .unwrap();
    let (text, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    assert!(text.contains("Sunny."), "{text}");

    executor()
        .execute(
            api_key_auth(&mock.url),
            request(
                "gpt-5.4",
                &payload(
                    r#"[
                        {"role":"user","content":"Weather?"},
                        {"role":"assistant","content":[{"type":"text","text":"Sunny."}]},
                        {"role":"user","content":"Thanks."}
                    ]"#,
                ),
            ),
            session(options("claude")),
        )
        .await
        .unwrap();

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let first = requests[0].json();
    assert!(
        first["input"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| str_at(item, "type") != "reasoning"),
        "{first}"
    );
    let second = requests[1].json();
    let types: Vec<String> = second["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| str_at(item, "type"))
        .collect();
    assert_eq!(
        types,
        ["message", "reasoning", "message", "message"],
        "{second}"
    );
    assert_eq!(str_at(&second, "input.1.encrypted_content"), encrypted);
    assert_eq!(str_at(&second, "input.2.role"), "assistant", "{second}");
    for seen in &requests {
        assert!(!exists(&seen.json(), "prompt_cache_key"), "{}", seen.body);
        assert_own_identity(seen);
    }
}

// Not upstream's: a status error that rejects a reasoning signature clears
// the session too, and one that doesn't leaves it.
#[tokio::test]
async fn clears_on_status_error_invalid_signature() {
    let session = "claude:session-invalid-status:agent:main";
    store_reasoning(session, &valid_encrypted_content(52));
    let call = |mock: Mock| async move {
        executor()
            .execute(
                api_key_auth(&mock.url),
                request(
                    "gpt-5.4",
                    &claude_payload("session-invalid-status", USER_NEXT),
                ),
                options("claude"),
            )
            .await
    };

    let other = Mock::start(Reply::error(
        400,
        r#"{"error":{"message":"Invalid input.","type":"invalid_request_error"}}"#,
    ))
    .await;
    assert!(call(other).await.is_err());
    assert!(ReplayCache::global().get_item("gpt-5.4", session).is_some());

    let invalid = Mock::start(Reply::error(
        400,
        r#"{"error":{"message":"Invalid signature in thinking block","type":"invalid_request_error"}}"#,
    ))
    .await;
    assert!(call(invalid).await.is_err());
    assert_eq!(ReplayCache::global().get_item("gpt-5.4", session), None);
}
