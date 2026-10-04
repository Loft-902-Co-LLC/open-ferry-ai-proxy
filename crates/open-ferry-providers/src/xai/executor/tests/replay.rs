//! Reasoning replay through the executor, ported from upstream's
//! `xai_executor_test.go`: what a turn leaves in the cache, what the next
//! request is given back, and what a compact call does to it. The replay's
//! own parts are tested in `replay/tests.rs`.
//!
//! The process's cache is shared by every test, and upstream's
//! `ClearXAIReasoningReplayCache` isn't ported, so each test uses a session
//! and a caller of its own.
//!
//! Adapted: upstream's tests sign in with OAuth, and name a caller by putting
//! an API key in the request's context. These use a dummy API key, and put
//! the caller in the options' observation, where the server records the key
//! a client authenticated with. Upstream's tests each serve a different
//! answer to a second request from one server; here the second request's
//! answer is the first's again, which the tests never read. Where upstream
//! calls `prepareResponsesRequest` these call [`prepare`].

use open_ferry_core::observe::{Observation, RequestContext};

use super::*;
use crate::codex::xai_replay_cache::XaiReplayCache;
use crate::codex::xai_replay_cache::tests::grok_content;
use crate::xai::replay::{self as xai_replay, Scope};

/// The options of a call that a client made with the proxy key `key`.
fn called_with(mut options: Options, key: &str) -> Options {
    let context = Arc::new(RequestContext::new(
        http::Method::POST,
        "/v1/responses".to_owned(),
    ));
    context.set_client_key(key);
    options.observation = Some(Arc::new(Observation::new(context, Vec::new())));
    options
}

/// The options of a call in a WebSocket's execution session.
fn in_execution_session(mut options: Options, session: &str) -> Options {
    options.metadata.execution_session_id = Some(session.to_owned());
    options
}

/// The scope of `request` as the executor finds it (upstream's
/// `xaiReasoningReplayScopeFromRequest`).
fn scope_of(request: &Request, options: &Options) -> Scope {
    let mut body: Value = serde_json::from_slice(&request.payload).expect("the payload parses");
    xai_replay::apply(&mut body, request, options).expect("the replay applies")
}

/// One `data:` line of an output item event.
fn item_event(kind: &str, item: &Value, index: usize) -> String {
    format!(
        "data: {}\n",
        json!({"type": kind, "item": item, "output_index": index})
    )
}

/// A reasoning item as xAI sends it.
fn reasoning_item(id: &str, content: &str) -> Value {
    json!({"id": id, "type": "reasoning", "summary": [], "encrypted_content": content})
}

/// An assistant message as xAI sends it.
fn message_item(text: &str) -> Value {
    json!({"id": "msg_1", "type": "message", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": text}]})
}

/// A `response.completed` event whose output is empty, as xAI sends it after
/// the items it has already finished.
fn completed(model: &str) -> String {
    format!(
        "data: {}\n\n",
        json!({"type": "response.completed", "response": {"id": "resp_1", "object": "response", "created_at": 0, "status": "completed", "model": model, "output": [], "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}})
    )
}

/// A Claude Messages request in `session`.
fn claude_request(model: &str, session: &str, messages: Value) -> Value {
    let user_id = json!({"device_id": "device-test", "account_uuid": "", "session_id": session});
    json!({
        "model": model,
        "metadata": {"user_id": user_id.to_string()},
        "messages": messages,
    })
}

/// A Claude user message of `text`.
fn claude_user(text: &str) -> Value {
    json!({"role": "user", "content": [{"type": "text", "text": text}]})
}

/// The type of each item of a body's input.
fn input_types(body: &Value) -> Vec<String> {
    body["input"]
        .as_array()
        .expect("the body has an input")
        .iter()
        .map(|item| str_at(item, "type"))
        .collect()
}

// TestXAIExecutorCompactClearsReplayBeforePostCompactTurn.
#[tokio::test]
async fn compact_clears_replay_before_post_compact_turn() {
    let mock = Mock::start(Reply::json(
        r#"{"id":"resp_compact","object":"response.compaction","output":[{"type":"compaction","encrypted_content":"opaque-out"}]}"#,
    ))
    .await;
    let caller = "xai-compact-caller";
    let compaction = grok_content(41);
    let compact_payload = json!({"model": "grok-4.3", "prompt_cache_key": "compact-session", "input": [{"type": "compaction", "encrypted_content": compaction}, {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "compact"}]}]}).to_string();
    let compact_request = request("grok-4.3", &compact_payload);
    let compact = called_with(compact_options("openai-response"), caller);
    let scope = scope_of(&compact_request, &compact);
    assert!(scope.valid(), "compact replay scope must be valid");
    let cache = XaiReplayCache::global();
    let reasoning =
        json!({"type": "reasoning", "summary": [], "encrypted_content": grok_content(42)});
    let answer = json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "pre-compact answer"}]});
    cache.store(
        &scope.model_name,
        &scope.session_key,
        &[&reasoning, &answer],
    );
    assert!(
        cache.get(&scope.model_name, &scope.session_key).is_some(),
        "the cache was not seeded"
    );

    let auth = api_key_auth(&mock.url);
    executor()
        .execute(Arc::clone(&auth), compact_request, compact)
        .await
        .unwrap();
    assert_eq!(mock.last().path, "/responses/compact");
    assert!(
        cache.get(&scope.model_name, &scope.session_key).is_none(),
        "successful compact must clear the pre-compact replay batch"
    );

    let post_compact = json!({"model": "grok-4.3", "prompt_cache_key": "compact-session", "input": [{"type": "compaction", "encrypted_content": compaction}, {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "after compact"}]}]}).to_string();
    let executor = executor();
    let prepared = prepare(
        executor.context(&auth),
        &request("grok-4.3", &post_compact),
        &called_with(options("openai-response"), caller),
        false,
        Format::CODEX,
    )
    .unwrap();
    let input = prepared.body["input"].as_array().expect("an input");
    assert!(
        input.len() == 2 && input[0]["type"] == "compaction" && input[1]["role"] == "user",
        "post-compact input contains stale replay state: {}",
        prepared.body
    );
}

// TestXAIExecutorCompactFailureRetainsReplay.
#[tokio::test]
async fn compact_failure_retains_replay() {
    let mock = Mock::start(Reply::error(
        500,
        r#"{"error":{"message":"compact failed"}}"#,
    ))
    .await;
    let payload = r#"{"model":"grok-4.3","prompt_cache_key":"compact-failure-session","input":[{"type":"message","role":"user","content":"compact"}]}"#;
    let compact_request = request("grok-4.3", payload);
    let compact = called_with(
        compact_options("openai-response"),
        "xai-compact-failure-caller",
    );
    let scope = scope_of(&compact_request, &compact);
    assert!(scope.valid(), "compact replay scope must be valid");
    let cache = XaiReplayCache::global();
    let reasoning =
        json!({"type": "reasoning", "summary": [], "encrypted_content": grok_content(43)});
    cache.store(&scope.model_name, &scope.session_key, &[&reasoning]);

    let result = executor()
        .execute(api_key_auth(&mock.url), compact_request, compact)
        .await;
    assert!(
        result.is_err(),
        "compact error = none, want upstream failure"
    );
    assert_eq!(mock.requests().len(), 1);
    assert!(
        cache.get(&scope.model_name, &scope.session_key).is_some(),
        "failed compact must retain the previous replay batch"
    );
}

// TestXAIExecutorReasoningReplayCacheStoresFinalDoneAndInjectsNextClaudeRequest.
#[tokio::test]
async fn reasoning_replay_cache_stores_final_done_and_injects_next_claude_request() {
    let added = grok_content(1);
    let done = grok_content(2);
    let sse = format!(
        "{}{}{}",
        item_event(
            "response.output_item.added",
            &json!({"id": "rs_added", "type": "reasoning", "status": "in_progress", "summary": [], "encrypted_content": added}),
            0
        ),
        item_event(
            "response.output_item.done",
            &reasoning_item("rs_done", &done),
            0
        ),
        completed("grok-4.3"),
    );
    let mock = Mock::start(Reply::sse(&sse)).await;
    for text in ["hello", "next"] {
        let payload = claude_request("grok-4.3", "xai-session-1", json!([claude_user(text)]));
        executor()
            .execute(
                api_key_auth(&mock.url),
                request("grok-4.3", &payload.to_string()),
                called_with(options("claude"), "xai-replay-caller"),
            )
            .await
            .unwrap();
    }

    let requests = mock.requests();
    assert_eq!(requests.len(), 2, "upstream request count");
    let second = requests[1].json();
    assert_eq!(second["input"][0]["type"], "reasoning", "{second}");
    assert_eq!(
        second["input"][0]["encrypted_content"], done,
        "injected encrypted_content wants the final done: {second}"
    );
    assert_eq!(second["input"][1]["role"], "user", "{second}");
}

// TestXAIExecutorResponsesSSEReplaysEncryptedReasoningAndAssistantMessage.
#[tokio::test]
async fn responses_sse_replays_encrypted_reasoning_and_assistant_message() {
    let content = grok_content(9);
    let sse = format!(
        "{}{}{}",
        item_event(
            "response.output_item.done",
            &reasoning_item("rs_1", &content),
            0
        ),
        item_event(
            "response.output_item.done",
            &message_item("first answer"),
            1
        ),
        completed("grok-4.5"),
    );
    let mock = Mock::start(Reply::sse(&sse)).await;
    let mut streams = Vec::new();
    for text in ["first", "second"] {
        let payload = json!({"model": "grok-4.5", "stream": true, "store": false, "prompt_cache_key": "codex-sse-session", "include": ["reasoning.encrypted_content"], "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}]});
        streams.push(
            streamed(
                executor()
                    .execute_stream(
                        api_key_auth(&mock.url),
                        request("grok-4.5", &payload.to_string()),
                        called_with(stream_options("openai-response"), "codex-sse-api-key"),
                    )
                    .await,
            )
            .await,
        );
    }

    let requests = mock.requests();
    assert_eq!(requests.len(), 2, "upstream request count");
    let first = requests[0].json();
    assert_eq!(
        first["include"],
        json!(["reasoning.encrypted_content"]),
        "first request include was not preserved: {first}"
    );
    let downstream = sse_events(&streams[0])
        .into_iter()
        .find(|event| {
            event["type"] == "response.output_item.done" && event["item"]["type"] == "reasoning"
        })
        .unwrap_or_else(|| panic!("no reasoning in {}", streams[0]));
    assert_eq!(
        downstream["item"]["encrypted_content"], content,
        "downstream encrypted_content wants the upstream Grok blob"
    );
    let second = requests[1].json();
    assert_eq!(second["input"][0]["type"], "reasoning", "{second}");
    assert_eq!(second["input"][0]["encrypted_content"], content, "{second}");
    assert_eq!(second["input"][1]["type"], "message", "{second}");
    assert_eq!(
        second["input"][1]["content"][0]["text"], "first answer",
        "{second}"
    );
    assert_eq!(
        second["input"][2]["content"][0]["text"], "second",
        "{second}"
    );
}

// TestXAIExecutorClaudeInjectsLatestCachedReasoningWhenHistoryHasOnlyOlderSignature.
#[tokio::test]
async fn claude_injects_latest_cached_reasoning_when_history_has_only_older_signature() {
    let old = grok_content(21);
    let latest = grok_content(22);
    let sse = format!(
        "{}{}{}",
        item_event(
            "response.output_item.done",
            &reasoning_item("rs_latest", &latest),
            0
        ),
        item_event(
            "response.output_item.done",
            &message_item("latest answer"),
            1
        ),
        completed("grok-4.5"),
    );
    let mock = Mock::start(Reply::sse(&sse)).await;
    let session = "claude-missing-latest";
    let caller = "claude-missing-sig-key";

    // Turn 1: user only -> cache latest R+M.
    let first = claude_request("grok-4.5", session, json!([claude_user("hello")]));
    executor()
        .execute(
            api_key_auth(&mock.url),
            request("grok-4.5", &first.to_string()),
            called_with(options("claude"), caller),
        )
        .await
        .unwrap();

    // Turn 2 (actual failure shape): client keeps an OLDER thinking signature
    // and the assistant text, but does not resend the latest encrypted/signature
    // blob.
    let second = claude_request(
        "grok-4.5",
        session,
        json!([
            claude_user("hello"),
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "older summary", "signature": old},
                {"type": "text", "text": "latest answer"},
            ]},
            claude_user("next"),
        ]),
    );
    executor()
        .execute(
            api_key_auth(&mock.url),
            request("grok-4.5", &second.to_string()),
            called_with(options("claude"), caller),
        )
        .await
        .unwrap();

    let requests = mock.requests();
    assert_eq!(requests.len(), 2, "upstream requests");
    // Upstream must include BOTH older client signature (as reasoning) and
    // latest cached blob.
    let body = requests[1].json();
    let input = body["input"].as_array().expect("an input");
    let has_reasoning = |content: &str| {
        input
            .iter()
            .any(|item| item["type"] == "reasoning" && item["encrypted_content"] == content)
    };
    assert!(
        has_reasoning(&latest),
        "latest cached encrypted_content missing from upstream body (broken Claude missing-signature scenario): {body}"
    );
    assert!(
        has_reasoning(&old),
        "older client signature/reasoning missing after translate: {body}"
    );
    let assistants = input
        .iter()
        .filter(|item| item["type"] == "message" && item["role"] == "assistant")
        .count();
    assert_eq!(
        assistants, 1,
        "no partial double-message inject; body={body}"
    );
}

// TestXAIExecutorReasoningReplayCacheReplaysFunctionCallForClaudeToolResult.
#[tokio::test]
async fn reasoning_replay_cache_replays_function_call_for_claude_tool_result() {
    let content = grok_content(3);
    let call = json!({"id": "fc_1", "type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{\"q\":\"weather\"}", "status": "completed"});
    let sse = format!(
        "{}{}{}{}",
        item_event(
            "response.output_item.done",
            &reasoning_item("rs_1", &content),
            0
        ),
        item_event(
            "response.output_item.added",
            &json!({"id": "fc_1", "type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{\"q\":\"weather\"}", "status": "in_progress"}),
            1
        ),
        item_event("response.output_item.done", &call, 1),
        completed("grok-4.3"),
    );
    let mock = Mock::start(Reply::sse(&sse)).await;
    let tools = json!([{"name": "lookup", "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}}}]);
    let session = "xai-session-tool";

    let mut first = claude_request("grok-4.3", session, json!([claude_user("call lookup")]));
    first["tools"] = tools.clone();
    let mut second = claude_request(
        "grok-4.3",
        session,
        json!([
            claude_user("call lookup"),
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": "sunny"}]},
        ]),
    );
    second["tools"] = tools;
    for payload in [first, second] {
        executor()
            .execute(
                api_key_auth(&mock.url),
                request("grok-4.3", &payload.to_string()),
                called_with(options("claude"), "xai-tool-replay-caller"),
            )
            .await
            .unwrap();
    }

    let requests = mock.requests();
    assert_eq!(requests.len(), 2, "upstream request count");
    let body = requests[1].json();
    assert_eq!(
        input_types(&body),
        [
            "message",
            "reasoning",
            "function_call",
            "function_call_output"
        ],
        "{body}"
    );
    assert_eq!(body["input"][2]["call_id"], "call_1", "{body}");
    assert_eq!(body["input"][3]["call_id"], "call_1", "{body}");
}

// TestXAIExecutorAliasesReplayedReasoningCacheWebSearchCall.
#[test]
fn aliases_replayed_reasoning_cache_web_search_call() {
    let session = "test-session-replay-websearch";
    let scope = Scope {
        model_name: "grok-4.6".into(),
        session_key: format!("execution:{session}"),
    };
    xai_replay::cache_completed(
        &scope,
        br#"{"response":{"output":[{"type":"function_call","call_id":"call_search_1","name":"web_search","arguments":"{\"query\":\"artificial analysis ranking\"}"}]}}"#,
    );

    let payload = json!({
        "model": "grok-4.6",
        "tools": [{"type": "function", "name": "web_search", "parameters": {"type": "object"}}],
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "download ranking"}]},
            {"type": "function_call_output", "call_id": "call_search_1", "output": "rankings text"},
        ],
    });
    let executor = executor();
    let auth = api_key_auth("http://127.0.0.1:9");
    let prepared = prepare(
        executor.context(&auth),
        &request("grok-4.6", &payload.to_string()),
        &Options {
            stream: true,
            ..in_execution_session(options("openai-response"), session)
        },
        true,
        Format::CODEX,
    )
    .unwrap();

    // In the prepared body, the replayed function_call must have been aliased
    // to clientfn_web_search.
    let replayed: Vec<&Value> = prepared.body["input"]
        .as_array()
        .expect("an input")
        .iter()
        .filter(|item| item["type"] == "function_call" && item["call_id"] == "call_search_1")
        .collect();
    assert_eq!(
        replayed.len(),
        1,
        "replayed function_call missing from input; body={}",
        prepared.body
    );
    assert_eq!(
        replayed[0]["name"], "clientfn_web_search",
        "body={}",
        prepared.body
    );
}

// Not upstream's: a client without a key the server recorded, and not in a
// WebSocket's execution session, has no replay: nothing is kept for it and
// nothing is given back (upstream falls back to a session derived from the
// API key, which this proxy doesn't).
#[tokio::test]
async fn replay_needs_a_caller_or_an_execution_session() {
    let sse = format!(
        "{}{}",
        item_event(
            "response.output_item.done",
            &reasoning_item("rs_1", &grok_content(51)),
            0
        ),
        completed("grok-4.3"),
    );
    let mock = Mock::start(Reply::sse(&sse)).await;
    for text in ["hello", "next"] {
        let payload = claude_request("grok-4.3", "keyless-session", json!([claude_user(text)]));
        executor()
            .execute(
                api_key_auth(&mock.url),
                request("grok-4.3", &payload.to_string()),
                options("claude"),
            )
            .await
            .unwrap();
    }

    let second = mock.requests()[1].json();
    assert_eq!(input_types(&second), ["message"], "{second}");
}

// Not upstream's: a turn is given back to the caller that made it and to no
// other, even if the other names the same session.
#[tokio::test]
async fn replay_stays_with_the_caller_that_made_the_turn() {
    let content = grok_content(52);
    let sse = format!(
        "{}{}",
        item_event(
            "response.output_item.done",
            &reasoning_item("rs_1", &content),
            0
        ),
        completed("grok-4.3"),
    );
    let mock = Mock::start(Reply::sse(&sse)).await;
    let session = "shared-session-name";
    for (caller, text) in [
        ("first-caller", "hello"),
        ("second-caller", "mine"),
        ("first-caller", "next"),
    ] {
        let payload = claude_request("grok-4.3", session, json!([claude_user(text)]));
        executor()
            .execute(
                api_key_auth(&mock.url),
                request("grok-4.3", &payload.to_string()),
                called_with(options("claude"), caller),
            )
            .await
            .unwrap();
    }

    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    let other = requests[1].json();
    assert_eq!(
        input_types(&other),
        ["message"],
        "another caller was given the first's reasoning: {other}"
    );
    let first_again = requests[2].json();
    assert_eq!(
        input_types(&first_again),
        ["reasoning", "message"],
        "{first_again}"
    );
    assert_eq!(first_again["input"][0]["encrypted_content"], content);
}

// Not upstream's: a WebSocket's execution session is a session the client
// named, and needs no caller (upstream's
// `TestXAIReasoningReplayScopeAllowsTrustedExecutionSessionWithoutAPIKey`,
// through a call).
#[tokio::test]
async fn execution_session_replays_without_a_caller() {
    let content = grok_content(53);
    let sse = format!(
        "{}{}{}",
        item_event(
            "response.output_item.done",
            &reasoning_item("rs_1", &content),
            0
        ),
        item_event("response.output_item.done", &message_item("an answer"), 1),
        completed("grok-4.3"),
    );
    let mock = Mock::start(Reply::sse(&sse)).await;
    for text in ["first", "second"] {
        let payload = json!({"model": "grok-4.3", "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}]});
        executor()
            .execute(
                api_key_auth(&mock.url),
                request("grok-4.3", &payload.to_string()),
                in_execution_session(options("openai-response"), "executor-test-session"),
            )
            .await
            .unwrap();
    }

    let second = mock.requests()[1].json();
    assert_eq!(
        input_types(&second),
        ["reasoning", "message", "message"],
        "{second}"
    );
    assert_eq!(second["input"][0]["encrypted_content"], content);
    assert_eq!(second["input"][1]["role"], "assistant", "{second}");
}

// Not upstream's: a turn that ends incomplete leaves nothing to replay.
#[tokio::test]
async fn incomplete_turn_is_not_kept() {
    let incomplete = json!({"type": "response.incomplete", "response": {"id": "resp_1", "object": "response", "created_at": 0, "status": "incomplete", "model": "grok-4.3", "output": [], "incomplete_details": {"reason": "max_output_tokens"}, "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}});
    let sse = format!(
        "{}data: {incomplete}\n\n",
        item_event(
            "response.output_item.done",
            &reasoning_item("rs_1", &grok_content(54)),
            0
        ),
    );
    let mock = Mock::start(Reply::sse(&sse)).await;
    for text in ["first", "second"] {
        let payload = json!({"model": "grok-4.3", "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}]});
        executor()
            .execute(
                api_key_auth(&mock.url),
                request("grok-4.3", &payload.to_string()),
                in_execution_session(options("openai-response"), "incomplete-test-session"),
            )
            .await
            .unwrap();
    }

    let second = mock.requests()[1].json();
    assert_eq!(input_types(&second), ["message"], "{second}");
}
