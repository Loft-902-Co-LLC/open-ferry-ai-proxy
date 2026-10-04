//! Tests of the replay's parts, ported from upstream's
//! `xai_executor_test.go`, and of the cache beside it. The executor's own
//! replay tests, which go through a mock server, are in
//! `executor/tests/replay.rs`.
//!
//! The process's cache is shared by every test, and upstream's
//! `ClearXAIReasoningReplayCache` isn't ported, so each test that writes to
//! it uses a session of its own.
//!
//! Adapted: where upstream names a caller by putting an API key in the
//! request's context, these put it in the options' observation, which is
//! where the server records the key a client authenticated with.
//!
//! Dropped: `TestApplyXAIReasoningReplayCacheFallsBackWhenReadFails`, which
//! makes Home mode's KV store fail a read. That store isn't ported, so a read
//! can't fail.

use std::sync::Arc;

use bytes::Bytes;
use open_ferry_core::observe::{Observation, RequestContext};
use serde_json::{Value, json};

use super::*;
use crate::codex::xai_replay_cache::tests::grok_content;

fn request(model: &str, payload: &str) -> Request {
    Request {
        model: model.to_owned(),
        payload: Bytes::from(payload.to_owned()),
    }
}

fn options(format: Format) -> Options {
    Options::new(format)
}

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

fn json_of(text: &str) -> Value {
    serde_json::from_str(text).expect("the test's JSON parses")
}

/// A reasoning item as the cache keeps it.
fn reasoning(seed: u8) -> Value {
    json!({"type": "reasoning", "summary": [], "content": null, "encrypted_content": grok_content(seed)})
}

fn assistant(text: &str) -> Value {
    json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]})
}

/// The items [`filter_items_for_input`] keeps, as values.
fn filtered(body: &Value, items: &[Value]) -> Vec<Value> {
    filter_items_for_input(body, items)
        .into_iter()
        .cloned()
        .collect()
}

fn types(body: &Value) -> Vec<String> {
    match body.get("input") {
        Some(Value::Array(input)) => input.iter().map(|item| str_at(item, "type")).collect(),
        other => panic!("no input: {other:?}"),
    }
}

// TestXAIReasoningReplayScopeIsolatesOpenAIResponsePromptCacheKeyByAPIKey.
#[test]
fn scope_isolates_openai_response_prompt_cache_key_by_api_key() {
    let payload = r#"{"model":"grok-4.5","prompt_cache_key":"shared-session","input":[]}"#;
    let body = json_of(payload);
    let request = request("grok-4.5", payload);
    let source = options(Format::OPENAI_RESPONSE);

    let scope_a = scope_from_request(&request, &called_with(source.clone(), "api-key-a"), &body);
    let scope_b = scope_from_request(&request, &called_with(source.clone(), "api-key-b"), &body);
    assert!(
        scope_a.valid() && scope_b.valid(),
        "{scope_a:?} {scope_b:?}"
    );
    assert_ne!(scope_a.session_key, scope_b.session_key);
    assert!(
        scope_a.session_key.starts_with("caller:")
            && scope_a.session_key.contains("prompt-cache:shared-session"),
        "{scope_a:?}"
    );
    // Not in upstream's test: the namespace is the first eight bytes of the
    // key's SHA-256, in hex, and never the key.
    assert_eq!(
        scope_a.session_key,
        "caller:a51fcb537444c486:prompt-cache:shared-session"
    );
    assert_eq!(
        scope_b.session_key,
        "caller:aea82cf54627800c:prompt-cache:shared-session"
    );
    assert!(!scope_a.session_key.contains("api-key-a"));
    assert_eq!(scope_a.model_name, "grok-4.5");

    let no_key = scope_from_request(&request, &source, &body);
    assert!(
        !no_key.valid(),
        "OpenAI Responses without a caller key must disable replay: {no_key:?}"
    );
}

// TestXAIReasoningReplayScopeDisablesClaudeWithoutAPIKey.
#[test]
fn scope_disables_claude_without_api_key() {
    let payload = r#"{"model":"grok-4.3","metadata":{"user_id":"{\"session_id\":\"shared-session\"}"},"messages":[{"role":"user","content":"hello"}]}"#;
    let body = json_of(payload);
    let request = request("grok-4.3", payload);
    let source = options(Format::CLAUDE);

    let no_key = scope_from_request(&request, &source, &body);
    assert!(
        !no_key.valid(),
        "Claude without a caller key must disable replay: {no_key:?}"
    );

    let with_key = scope_from_request(&request, &called_with(source, "api-key-a"), &body);
    assert!(with_key.valid(), "Claude with a caller key must replay");
    assert!(
        with_key.session_key.starts_with("caller:")
            && with_key.session_key.contains("claude:shared-session"),
        "{with_key:?}"
    );
}

// TestXAIReasoningReplayScopeAllowsTrustedExecutionSessionWithoutAPIKey.
#[test]
fn scope_allows_trusted_execution_session_without_api_key() {
    let payload = r#"{"model":"grok-4.3","messages":[{"role":"user","content":"hello"}]}"#;
    let scope = scope_from_request(
        &request("grok-4.3", payload),
        &in_execution_session(options(Format::CLAUDE), "trusted-session"),
        &json_of(payload),
    );
    assert!(
        scope.valid(),
        "a trusted execution session must replay without a caller key"
    );
    assert_eq!(scope.session_key, "execution:trusted-session");
}

// TestXAIReasoningReplayScopeSkipsIncrementalWebsocketPreviousResponse,
// adapted: the request is made with a caller key, so that the websocket's
// previous_response_id is the only thing that disables it (without a key,
// upstream's disables it whatever the id is).
#[test]
fn scope_skips_incremental_websocket_previous_response() {
    let payload = r#"{"model":"grok-4.5","previous_response_id":"resp_1","prompt_cache_key":"codex-ws-session","input":[]}"#;
    let body = json_of(r#"{"model":"grok-4.5","prompt_cache_key":"codex-ws-session","input":[]}"#);
    let request = request("grok-4.5", payload);
    let mut options = called_with(options(Format::OPENAI_RESPONSE), "ws-caller");

    // An HTTP request that names a previous response is replayed as usual.
    assert!(scope_from_request(&request, &options, &body).valid());

    options.downstream_websocket = true;
    let scope = scope_from_request(&request, &options, &body);
    assert!(
        !scope.valid(),
        "an incremental websocket request must not replay: {scope:?}"
    );
}

// Not upstream's: a WebSocket request is only left alone by a
// previous_response_id that isn't blank.
#[test]
fn scope_replays_websocket_request_without_previous_response() {
    let mut options = called_with(options(Format::OPENAI_RESPONSE), "ws-caller");
    options.downstream_websocket = true;
    let body = json_of(r#"{"prompt_cache_key":"codex-ws-session","input":[]}"#);
    for payload in [
        r#"{"model":"grok-4.5","prompt_cache_key":"codex-ws-session","input":[]}"#,
        r#"{"model":"grok-4.5","previous_response_id":"","input":[]}"#,
        r#"{"model":"grok-4.5","previous_response_id":"  ","input":[]}"#,
        r#"{"model":"grok-4.5","previous_response_id":null,"input":[]}"#,
        "not json",
    ] {
        let scope = scope_from_request(&request("grok-4.5", payload), &options, &body);
        assert!(scope.valid(), "{payload}: {scope:?}");
    }
    let padded = r#"{"model":"grok-4.5","previous_response_id":" resp_1 ","input":[]}"#;
    assert!(!scope_from_request(&request("grok-4.5", padded), &options, &body).valid());
}

// Not upstream's: only Claude and OpenAI Responses clients are replayed, the
// model is named without its thinking suffix, and a session of the client's
// gets the caller's namespace but an execution session doesn't.
#[test]
fn scope_follows_source_model_and_session() {
    let payload = r#"{"model":"grok-4.5(high)","prompt_cache_key":"s","input":[]}"#;
    let body = json_of(payload);
    let request = request("grok-4.5(high)", payload);
    for format in [
        Format::OPENAI,
        Format::CODEX,
        Format::GEMINI,
        Format::from(String::new()),
    ] {
        let options = in_execution_session(called_with(options(format), "key"), "e");
        let scope = scope_from_request(&request, &options, &body);
        assert_eq!(scope, Scope::default());
    }

    let responses = options(Format::OPENAI_RESPONSE);
    let options = in_execution_session(called_with(responses.clone(), "key"), "e");
    let scope = scope_from_request(&request, &options, &body);
    // The execution session wins over the body's prompt_cache_key, and keeps
    // its form, with the model named without its suffix.
    assert_eq!(scope.model_name, "grok-4.5");
    assert_eq!(scope.session_key, "execution:e");

    // A request that names no session isn't replayed: upstream makes none up
    // from the key, and neither does this.
    let unnamed = r#"{"model":"grok-4.5","input":[{"role":"user","content":"hi"}]}"#;
    let scope = scope_from_request(
        &self::request("grok-4.5", unnamed),
        &called_with(responses, "key"),
        &json_of(unnamed),
    );
    assert!(!scope.valid(), "{scope:?}");
    assert_eq!(scope.session_key, "");
}

// Not upstream's: a blank session, or a session of blanks, is no session,
// and a trimmed execution session is still the server's.
#[test]
fn isolate_session_key_trims_and_keeps_execution_sessions() {
    let keyed = called_with(options(Format::CLAUDE), "  caller-key  ");
    assert_eq!(isolate_session_key(&keyed, ""), "");
    assert_eq!(isolate_session_key(&keyed, "  \t"), "");
    assert_eq!(isolate_session_key(&keyed, " execution:x "), "execution:x");
    let plain = isolate_session_key(&keyed, "window:w");
    // The key is trimmed before it is hashed.
    let trimmed = called_with(options(Format::CLAUDE), "caller-key");
    assert_eq!(plain, isolate_session_key(&trimmed, " window:w "));
    assert!(plain.starts_with("caller:") && plain.ends_with(":window:w"));
    assert_eq!(plain.len(), "caller:".len() + 16 + ":window:w".len());

    // A key of blanks is no key.
    let blank = called_with(options(Format::CLAUDE), "   ");
    assert_eq!(isolate_session_key(&blank, "window:w"), "");
    assert_eq!(isolate_session_key(&blank, "execution:x"), "execution:x");
    assert_eq!(
        isolate_session_key(&options(Format::CLAUDE), "window:w"),
        ""
    );
}

// TestFilterXAIReasoningReplayItemsSkipsMatchingCachedTurn.
#[test]
fn filter_skips_matching_cached_turn() {
    let body = json!({"input": [
        {"type": "reasoning", "summary": [], "encrypted_content": grok_content(10)},
        assistant("first answer"),
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "second"}]},
    ]});
    let items = [reasoning(10), assistant("first answer")];
    let kept = filtered(&body, &items);
    assert!(
        kept.is_empty(),
        "no replay items for client-provided history: {kept:?}"
    );
}

// TestFilterXAIReasoningReplayItemsSkipsAmbiguousCachedTurnWhenInputHasOlderReasoning.
#[test]
fn filter_skips_ambiguous_cached_turn_when_input_has_older_reasoning() {
    let body = json!({"input": [
        {"type": "reasoning", "summary": [], "encrypted_content": grok_content(10)},
        assistant("older answer"),
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "next"}]},
    ]});
    let items = [reasoning(12), assistant("new answer")];
    let kept = filtered(&body, &items);
    assert!(
        kept.is_empty(),
        "no replay items when the cached assistant isn't the history's: {kept:?}"
    );
}

// TestFilterXAIReasoningReplayItemsSkipsDuplicateAssistantMessage.
#[test]
fn filter_skips_duplicate_assistant_message() {
    let body = json!({"input": [
        assistant("first answer"),
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "second"}]},
    ]});
    let items = [reasoning(11), assistant("first answer")];
    let kept = filtered(&body, &items);
    assert_eq!(kept, [reasoning(11)], "reasoning only");
}

// TestFilterXAIReasoningReplayItemsRecognizesRoleOnlyAssistantMessage.
#[test]
fn filter_recognizes_role_only_assistant_message() {
    let mut body = json!({"input": [
        {"role": "assistant", "content": "first answer"},
        {"role": "user", "content": "second"},
    ]});
    let items = [reasoning(31), assistant("first answer")];
    let kept = filter_items_for_input(&body, &items);
    assert_eq!(kept, [&items[0]], "reasoning only");

    assert!(insert_items(&mut body, &kept));
    let Some(Value::Array(input)) = body.get("input") else {
        panic!("no input in {body}");
    };
    assert!(
        input.len() == 3
            && str_at(&input[0], "type") == "reasoning"
            && str_at(&input[1], "role") == "assistant",
        "unexpected role-only replay order: {body}"
    );
    let assistants = input
        .iter()
        .filter(|item| eq_fold(&str_at(item, "role"), "assistant"))
        .count();
    assert_eq!(assistants, 1, "assistant messages after replay: {body}");
}

// TestFilterXAIReasoningReplayItemsDoesNotMatchOlderAssistantMessage.
#[test]
fn filter_does_not_match_older_assistant_message() {
    let body = json!({"input": [
        assistant("OK"),
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "continue"}]},
        assistant("different answer"),
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "next"}]},
    ]});
    let items = [reasoning(13), assistant("OK")];
    let kept = filtered(&body, &items);
    assert!(
        kept.is_empty(),
        "no replay items when the last assistant differs from the cached turn: {kept:?}"
    );
}

// TestFilterXAIReasoningReplayItemsSkipsAmbiguousTurnWhenLastAssistantTextDrifts.
#[test]
fn filter_skips_ambiguous_turn_when_last_assistant_text_drifts() {
    let body = json!({"input": [
        assistant("first answer."),
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "second"}]},
    ]});
    let items = [reasoning(20), assistant("first answer")];
    let kept = filtered(&body, &items);
    assert!(
        kept.is_empty(),
        "no replay for an ambiguous drifted assistant: {kept:?}"
    );
}

// TestCacheXAIReasoningReplayFromCompletedClearsPreviousEntryWhenNoReplayableState.
#[test]
fn cache_completed_clears_previous_entry_when_no_replayable_state() {
    let (model_name, session_key) = ("grok-4.5", "prompt-cache:clear-previous");
    let cache = XaiReplayCache::global();
    let previous = [reasoning(14), assistant("previous answer")];
    assert_eq!(
        cache.store(model_name, session_key, &[&previous[0], &previous[1]]),
        Store::Stored,
        "failed to seed the cache"
    );

    cache_completed(
        &Scope {
            model_name: model_name.to_owned(),
            session_key: session_key.to_owned(),
        },
        br#"{"response":{"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"message without reasoning"}]}]}}"#,
    );
    assert_eq!(
        cache.get(model_name, session_key),
        None,
        "the previous entry must be cleared by a completed output with nothing to replay"
    );
}

// TestXAIReasoningReplayCacheReplaysFunctionCallWithoutReasoning.
#[test]
fn replays_function_call_without_reasoning() {
    const SESSION: &str = "xai-tool-call-only";
    cache_completed(
        &Scope {
            model_name: "grok-4.3".to_owned(),
            session_key: format!("execution:{SESSION}"),
        },
        br#"{"response":{"output":[{"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{\"q\":\"weather\"}"}]}}"#,
    );

    let payload = r#"{"model":"grok-4.3","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"call lookup"}]},{"type":"function_call_output","call_id":"call_1","output":"sunny"}]}"#;
    let mut body = json_of(payload);
    let scope = apply(
        &mut body,
        &request("grok-4.3", payload),
        &in_execution_session(options(Format::CLAUDE), SESSION),
    )
    .expect("replay never fails");
    assert!(scope.valid(), "a tool-call-only replay scope must be valid");
    assert_eq!(
        types(&body),
        ["message", "function_call", "function_call_output"],
        "{body}"
    );
    assert_eq!(body["input"][1]["call_id"], "call_1", "{body}");
    assert_eq!(body["input"][1]["name"], "lookup", "{body}");
}

// Not upstream's: what apply puts back through the cache. A completed
// turn's reasoning goes before its assistant message, which the client's
// history already has, and a turn is only put back once.
#[test]
fn apply_puts_cached_turn_back_once() {
    const SESSION: &str = "apply-puts-turn-back";
    let scope = Scope {
        model_name: "grok-4.5".to_owned(),
        session_key: format!("execution:{SESSION}"),
    };
    let completed = json!({"type": "response.completed", "response": {"output": [
        {"id": "rs_1", "type": "reasoning", "summary": [], "encrypted_content": grok_content(60)},
        {"id": "ig_1", "type": "image_generation_call"},
        {"id": "msg_1", "type": "message", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": "the answer"}]},
    ]}});
    cache_completed(&scope, completed.to_string().as_bytes());

    // The client kept the assistant text but dropped the reasoning.
    let payload = r#"{"model":"grok-4.5","input":[{"role":"user","content":"one"},{"role":"assistant","content":"the answer"},{"role":"user","content":"two"}]}"#;
    let in_session = in_execution_session(options(Format::OPENAI_RESPONSE), SESSION);
    let mut body = json_of(payload);
    let applied = apply(&mut body, &request("grok-4.5", payload), &in_session).unwrap();
    assert_eq!(applied, scope);
    assert_eq!(types(&body), ["", "reasoning", "", ""], "{body}");
    assert_eq!(body["input"][1]["encrypted_content"], grok_content(60));
    assert_eq!(body["input"][1]["summary"], json!([]));
    assert_eq!(body["input"][2]["role"], "assistant");

    // The reasoning is in the input now, so there is nothing to put back.
    let again = body.clone();
    apply(&mut body, &request("grok-4.5", payload), &in_session).unwrap();
    assert_eq!(body, again);

    // A request that names no session, or has no input array, or whose
    // history has moved on, is left as it is.
    let unchanged = [
        (
            r#"{"model":"grok-4.5","input":[{"role":"user","content":"x"}]}"#,
            None,
        ),
        (r#"{"model":"grok-4.5","input":"text"}"#, Some(SESSION)),
        (
            r#"{"model":"grok-4.5","input":[{"role":"assistant","content":"another answer"},{"role":"user","content":"x"}]}"#,
            Some(SESSION),
        ),
    ];
    for (payload, session) in unchanged {
        let mut options = options(Format::OPENAI_RESPONSE);
        if let Some(session) = session {
            options = in_execution_session(options, session);
        }
        let mut body = json_of(payload);
        let before = body.clone();
        apply(&mut body, &request("grok-4.5", payload), &options).unwrap();
        assert_eq!(body, before, "{payload}");
    }
}

// Not upstream's: a tool call is only put back if the input has its result
// and not the call, a reasoning item the input has is left out, and the
// calls of a turn are put before the first result that answers one.
#[test]
fn filter_keeps_calls_that_the_input_answers() {
    let call = |id: &str| json!({"type": "function_call", "call_id": id, "name": "lookup", "arguments": "{}"});
    let custom = json!({"type": "custom_tool_call", "status": "completed", "call_id": "call_c", "name": "apply_patch", "input": "patch"});
    let items = [
        reasoning(70),
        call("call_a"),
        call("call_b"),
        call("call_has"),
        custom.clone(),
        call("call_a"),
        json!({"type": "web_search_call", "id": "ws"}),
    ];
    let body = json!({"input": [
        {"type": "message", "role": "user", "content": "go"},
        {"type": "function_call", "call_id": "call_has", "name": "lookup", "arguments": "{}"},
        {"type": "function_call_output", "call_id": "call_a", "output": "a"},
        {"type": "custom_tool_call_output", "call_id": "call_c", "output": "c"},
        {"type": "function_call_output", "call_id": "call_has", "output": "has"},
    ]});
    // call_a has its result and no call; call_b has no result; call_has has
    // its call already; call_c's result is a custom one; the second call_a
    // repeats the first.
    assert_eq!(
        filtered(&body, &items),
        [reasoning(70), call("call_a"), custom]
    );

    // The same reasoning in the input is left out, wherever it is.
    let with_reasoning = json!({"input": [
        {"type": "reasoning", "encrypted_content": grok_content(70), "summary": []},
        {"type": "function_call_output", "call_id": "call_a", "output": "a"},
    ]});
    assert_eq!(
        filtered(&with_reasoning, &items),
        [call("call_a")],
        "the call is kept, the reasoning isn't"
    );
    // A reasoning item whose content isn't a string, or isn't this one, or
    // is something else with the same content, doesn't count.
    let others = json!({"input": [
        {"type": "reasoning", "encrypted_content": 7},
        {"type": "reasoning", "encrypted_content": grok_content(71)},
        {"type": "message", "role": "user", "encrypted_content": grok_content(70)},
    ]});
    assert_eq!(filtered(&others, &items[..1]), [reasoning(70)]);
}

// Not upstream's: content is equal if it says the same, in text or parts, and
// never equal if it has a part that isn't text or a refusal.
#[test]
fn assistant_content_equal_compares_text_and_refusal_parts() {
    let equal = |left: Value, right: Value| assistant_content_equal(Some(&left), Some(&right));
    let text = |text: &str| json!([{"type": "output_text", "text": text}]);
    assert!(equal(json!("hi"), text("hi")));
    assert!(equal(text("hi"), json!("hi")));
    assert!(equal(text("hi"), text("hi")));
    assert!(equal(json!(""), text("")));
    assert!(!equal(json!("hi"), text("hi ")));
    assert!(!equal(json!("hi"), text("Hi")));

    let parts = json!([{"type": "output_text", "text": "a"}, {"type": "refusal", "refusal": "no"}]);
    assert!(equal(parts.clone(), parts.clone()));
    assert!(equal(
        json!([{"type": " output_text ", "text": "a", "annotations": []}, {"type": "refusal", "refusal": "no"}]),
        parts.clone()
    ));
    assert!(!equal(
        json!([{"type": "refusal", "refusal": "no"}, {"type": "output_text", "text": "a"}]),
        parts.clone()
    ));
    assert!(!equal(text("a"), parts.clone()));
    // A refusal isn't the text it says.
    assert!(!equal(
        json!([{"type": "refusal", "refusal": "no"}]),
        text("no")
    ));

    // Anything else isn't comparable, not even to itself.
    let input_text = json!([{"type": "input_text", "text": "hi"}]);
    assert!(!equal(input_text.clone(), input_text));
    let no_text = json!([{"type": "output_text"}]);
    assert!(!equal(no_text.clone(), no_text));
    let numeric = json!([{"type": "output_text", "text": 1}]);
    assert!(!equal(numeric.clone(), numeric));
    assert!(!equal(json!([]), json!([])));
    assert!(!equal(json!(7), json!(7)));
    assert!(!equal(
        json!({"type": "output_text", "text": "hi"}),
        text("hi")
    ));
    assert!(!assistant_content_equal(None, None));
    assert!(!assistant_content_equal(Some(&json!("hi")), None));
}

// Not upstream's: an input assistant message whose content can't be compared
// is ambiguous too, and the type and role are read as upstream's are, with
// space and case ignored.
#[test]
fn filter_reads_roles_and_types_loosely() {
    let items = [reasoning(80), assistant("answer")];
    // A last assistant message in parts that can't be compared: nothing.
    let body = json!({"input": [
        {"type": "message", "role": "assistant", "content": [{"type": "input_text", "text": "answer"}]},
        {"role": "user", "content": "next"},
    ]});
    assert!(filtered(&body, &items).is_empty());

    // The last one decides, whatever the case and padding of its role.
    let body = json!({"input": [
        {"type": " message ", "role": " ASSISTANT ", "content": "answer"},
        {"role": "user", "content": "next"},
    ]});
    assert_eq!(filtered(&body, &items), [reasoning(80)]);

    // A tool call that isn't a message, or another role, isn't one: with no
    // assistant message in the input, the cached one comes back too.
    let body = json!({"input": [
        {"type": "function_call", "role": "assistant", "call_id": "c", "name": "n", "arguments": "{}"},
        {"type": "message", "role": "developer", "content": "different"},
        {"role": "user", "content": "next"},
    ]});
    assert_eq!(
        filtered(&body, &items),
        [reasoning(80), assistant("answer")]
    );

    // The cached assistant message is only an assistant's: a cached message
    // of another role isn't compared with the input's, so it makes nothing
    // ambiguous and comes back with the rest.
    let user_message = json!({"type": "message", "role": "user", "content": [{"type": "output_text", "text": "x"}]});
    let user_only = [reasoning(81), user_message.clone()];
    let body = json!({"input": [{"role": "assistant", "content": "different"}]});
    assert_eq!(filtered(&body, &user_only), [reasoning(81), user_message]);
}

// Not upstream's: items go before the tool result they answer, else before
// the last assistant message, else before the first message that isn't the
// system's or developer's, else last, and a result that names a shortened
// call ID gives the call that ID.
#[test]
fn insert_places_items_and_aligns_call_ids() {
    let call =
        json!({"type": "function_call", "call_id": "call_long_id", "name": "n", "arguments": "{}"});
    let mut body = json!({"input": [
        {"role": "system", "content": "s"},
        {"role": "user", "content": "u"},
        {"type": "function_call_output", "call_id": "call_long_id", "output": "o"},
    ]});
    assert!(insert_items(&mut body, &[&reasoning(90), &call]));
    assert_eq!(
        types(&body),
        ["", "", "reasoning", "function_call", "function_call_output"]
    );

    // Without a result or an assistant message, before the first message
    // that isn't the system's or developer's.
    let mut body = json!({"input": [
        {"role": "developer", "content": "d"},
        {"role": "system", "content": "s"},
        {"role": "user", "content": "u"},
    ]});
    assert!(insert_items(&mut body, &[&reasoning(90)]));
    assert_eq!(types(&body), ["", "", "reasoning", ""]);

    // With nothing but those, at the end.
    let mut body = json!({"input": [{"role": "system", "content": "s"}]});
    assert!(insert_items(&mut body, &[&reasoning(90)]));
    assert_eq!(types(&body), ["", "reasoning"]);
    let mut body = json!({"input": []});
    assert!(insert_items(&mut body, &[&reasoning(90)]));
    assert_eq!(types(&body), ["reasoning"]);

    // Nothing to put, nowhere to put it.
    assert!(!insert_items(&mut body, &[]));
    let mut no_input = json!({"model": "m"});
    assert!(!insert_items(&mut no_input, &[&reasoning(90)]));
    let mut text_input = json!({"input": "text"});
    assert!(!insert_items(&mut text_input, &[&reasoning(90)]));
    assert_eq!(text_input, json!({"input": "text"}));
}

// Not upstream's: a completed turn that isn't a successful one with an
// output array changes nothing, and neither does a scope that isn't valid.
#[test]
fn cache_completed_ignores_what_it_cannot_use() {
    let (model_name, session_key) = ("grok-4.5", "prompt-cache:cache-completed-ignores");
    let scope = Scope {
        model_name: model_name.to_owned(),
        session_key: session_key.to_owned(),
    };
    let cache = XaiReplayCache::global();
    let kept = reasoning(50);
    assert_eq!(
        cache.store(model_name, session_key, &[&kept]),
        Store::Stored
    );

    for completed in [
        r#"{"response":{}}"#,
        r#"{"response":{"output":"text"}}"#,
        r#"{"response":{"output":null}}"#,
        r#"{"output":[]}"#,
        "not json",
        "",
    ] {
        cache_completed(&scope, completed.as_bytes());
        assert_eq!(
            cache.get(model_name, session_key),
            Some(vec![kept.clone()]),
            "{completed}"
        );
    }
    // A scope that isn't valid neither writes nor clears.
    let blank = Scope {
        model_name: String::new(),
        session_key: session_key.to_owned(),
    };
    cache_completed(&blank, br#"{"response":{"output":[]}}"#);
    clear_after_compaction(&blank);
    clear_after_compaction(&Scope::default());
    assert_eq!(cache.get(model_name, session_key), Some(vec![kept]));

    // An empty output is a turn with nothing to replay, and clears.
    cache_completed(&scope, br#"{"response":{"output":[]}}"#);
    assert_eq!(cache.get(model_name, session_key), None);
}

// Not upstream's: a completed turn replaces the one before it whole, and
// compaction forgets it.
#[test]
fn cache_completed_replaces_and_compaction_clears() {
    let scope = Scope {
        model_name: "grok-4.5".to_owned(),
        session_key: "execution:cache-completed-replaces".to_owned(),
    };
    let cache = XaiReplayCache::global();
    let turn = |seed: u8, text: &str| {
        json!({"type": "response.completed", "response": {"output": [
            {"type": "reasoning", "summary": [{"type": "summary_text", "text": "s"}], "encrypted_content": grok_content(seed)},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]},
            {"type": "web_search_call", "id": "ws_1"},
        ]}})
        .to_string()
    };
    cache_completed(&scope, turn(1, "first").as_bytes());
    cache_completed(&scope, turn(2, "second").as_bytes());
    assert_eq!(
        cache.get(&scope.model_name, &scope.session_key),
        Some(vec![reasoning(2), assistant("second")])
    );

    clear_after_compaction(&scope);
    assert_eq!(cache.get(&scope.model_name, &scope.session_key), None);
    // Forgetting what isn't there is fine.
    clear_after_compaction(&scope);
}
