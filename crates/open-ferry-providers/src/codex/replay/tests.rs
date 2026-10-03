//! Ported from upstream's
//! `internal/runtime/executor/codex_executor_reasoning_replay_cache_test.go`:
//! the tests that call the replay functions directly. The ones that run the
//! executor against a mock Codex are in `codex/executor/tests/replay.rs`.
//!
//! The tests share the process's cache, as the executor does, so each uses a
//! session of its own where upstream's clear the cache first.
//!
//! Changed:
//! - `TestCodexExecutorReasoningReplayCacheSharesSameSessionAcrossClientKeys`
//!   checks that two requests of one session get the same scope and the
//!   second gets the first's reasoning. The client's proxy API key isn't
//!   part of a request here, so there are no two keys to compare.
//! - `TestCodexExecutorReasoningReplaySessionKeyCanonicalizesSessionHeaderAliases`:
//!   `Session_id` and `session_id` are one header name here, so the test
//!   checks that each spelling, and `Session-Id`, gives the same key.
//! - `TestCodexReplayPrefixFingerprintsMatchesDirectComputation` doesn't ask
//!   for the prefix at `-1`, as the index is unsigned.

use http::{HeaderMap, HeaderName, HeaderValue};
use open_ferry_core::exec::Metadata;
use serde_json::json;

use super::*;
use crate::codex::replay_cache::tests::valid_encrypted_content;

fn request(model: &str, payload: &str) -> Request {
    Request {
        model: model.into(),
        payload: payload.to_owned().into(),
    }
}

fn claude_options() -> Options {
    Options::new(Format::CLAUDE)
}

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        headers.append(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    headers
}

fn parse(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap()
}

/// A scope with no request fingerprint, as upstream's tests build them.
fn scope(session_key: &str) -> Scope {
    Scope {
        model: "gpt-5.4".into(),
        session_key: session_key.into(),
        request_fingerprint: String::new(),
    }
}

/// A completed response with `output`.
fn completed(output: &[Value]) -> Value {
    json!({"type": "response.completed", "response": {"output": output}})
}

fn reasoning(encrypted: &str) -> Value {
    json!({"type": "reasoning", "summary": [], "content": null, "encrypted_content": encrypted})
}

fn assistant(text: &str) -> Value {
    json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]})
}

fn types(body: &Value) -> Vec<String> {
    input_items(body)
        .iter()
        .map(|item| str_at(item, "type"))
        .collect()
}

fn encrypted_at(body: &Value, index: usize) -> String {
    str_at(body, &format!("input.{index}.encrypted_content"))
}

/// A Claude Code payload whose `metadata.user_id` names `session`.
fn claude_code_payload(session: &str) -> String {
    let user_id = json!({"session_id": session}).to_string();
    json!({"metadata": {"user_id": user_id}}).to_string()
}

#[test]
fn shares_same_session_across_requests() {
    let request = request(
        "gpt-5.4",
        r#"{"model":"gpt-5.4","metadata":{"user_id":"{\"device_id\":\"device-test\",\"account_uuid\":\"\",\"session_id\":\"session-only\"}"},"messages":[{"role":"user","content":[{"type":"text","text":"next"}]}]}"#,
    );
    let body = parse(
        r#"{"model":"gpt-5.4","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"next"}]}]}"#,
    );
    let encrypted = valid_encrypted_content(11);

    let first = scope_from_request(&request, &claude_options(), &body);
    assert!(first.valid(), "{first:?}");
    save(&first, &completed(&[reasoning(&encrypted)]));

    let mut second_body = body.clone();
    let second = apply(&request, &claude_options(), &mut second_body);
    assert_eq!(second, first);
    assert_eq!(str_at(&second_body, "input.0.type"), "reasoning");
    assert_eq!(encrypted_at(&second_body, 0), encrypted);
}

#[test]
fn session_key_uses_claude_code_json_session_id() {
    let request = request(
        "gpt-5.4",
        r#"{
            "model":"gpt-5.4",
            "metadata":{"user_id":"{\"device_id\":\"device-a\",\"account_uuid\":\"\",\"session_id\":\"session-json-1\"}"},
            "messages":[{"role":"user","content":[{"type":"text","text":"next"}]}]
        }"#,
    );
    let body = parse(
        r#"{"model":"gpt-5.4","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"next"}]}]}"#,
    );
    assert_eq!(
        session_key(&request, &claude_options(), &body),
        "claude:session-json-1:agent:main"
    );
}

#[test]
fn session_key_isolates_claude_code_agents() {
    let request = request(
        "local-alias-high",
        r#"{"model":"local-alias","messages":[{"role":"user","content":"next"}]}"#,
    );
    let body = parse(
        r#"{"model":"gpt-5.4","prompt_cache_key":"shared-client-key","input":[{"type":"message","role":"user","content":"next"}]}"#,
    );
    let options = |agent: Option<&str>| {
        let mut pairs = vec![("X-Claude-Code-Session-Id", "session-agents")];
        if let Some(agent) = agent {
            pairs.push(("X-Claude-Code-Agent-Id", agent));
        }
        Options {
            headers: headers(&pairs),
            metadata: Metadata {
                execution_session_id: Some("shared-execution-session".into()),
                ..Metadata::default()
            },
            ..claude_options()
        }
    };
    let root = scope_from_request(&request, &options(None), &body);
    let child_a = scope_from_request(&request, &options(Some("agent-a")), &body);
    let child_b = scope_from_request(&request, &options(Some("agent-b")), &body);
    for scope in [&root, &child_a, &child_b] {
        assert_eq!(scope.model, "gpt-5.4", "{scope:?}");
    }
    assert!(
        root.session_key != child_a.session_key
            && child_a.session_key != child_b.session_key
            && root.session_key != child_b.session_key,
        "{root:?} {child_a:?} {child_b:?}"
    );
}

#[test]
fn session_key_rejects_bare_claude_user_id() {
    let request = request(
        "gpt-5.4",
        r#"{"model":"gpt-5.4","metadata":{"user_id":"same-user-across-chats"},"messages":[{"role":"user","content":[{"type":"text","text":"next"}]}]}"#,
    );
    let body = parse(
        r#"{"model":"gpt-5.4","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"next"}]}]}"#,
    );
    assert_eq!(session_key(&request, &claude_options(), &body), "");
}

#[test]
fn session_key_canonicalizes_session_header_aliases() {
    let keys: Vec<String> = ["Session_id", "session_id", "Session-Id"]
        .into_iter()
        .map(|name| session_key_from_headers(&headers(&[(name, "session-alias")])))
        .collect();
    assert_eq!(keys, ["session-id:session-alias"; 3]);
}

#[test]
fn session_key_canonicalizes_window_header_with_payload() {
    let from_payload = session_key_from_payload(&parse(
        r#"{"client_metadata":{"x-codex-window-id":"window-1"}}"#,
    ));
    let from_header = session_key_from_headers(&headers(&[("X-Codex-Window-Id", "window-1")]));
    assert_eq!(from_payload, from_header);
    assert_eq!(from_header, "window:window-1");
}

#[test]
fn restores_cumulative_tool_turns() {
    let scope = scope("claude:session-cumulative-tools:agent:main");
    let first = valid_encrypted_content(21);
    let second = valid_encrypted_content(22);
    save(
        &scope,
        &completed(&[
            reasoning(&first),
            json!({"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": r#"{"q":"first"}"#}),
        ]),
    );
    save(
        &scope,
        &completed(&[
            reasoning(&second),
            json!({"type": "function_call", "call_id": "call_2", "name": "lookup", "arguments": r#"{"q":"second"}"#}),
        ]),
    );

    let mut body = parse(concat!(
        r#"{"model":"gpt-5.4","input":["#,
        r#"{"type":"message","role":"user","content":"first"},"#,
        r#"{"type":"function_call_output","call_id":"call_1","output":"one"},"#,
        r#"{"type":"message","role":"user","content":"second"},"#,
        r#"{"type":"function_call_output","call_id":"call_2","output":"two"},"#,
        r#"{"type":"message","role":"user","content":"third"}"#,
        r#"]}"#,
    ));
    let request = request("gpt-5.4", &claude_code_payload("session-cumulative-tools"));
    let got = apply(&request, &claude_options(), &mut body);
    assert_eq!(
        (&got.model, &got.session_key),
        (&scope.model, &scope.session_key)
    );
    assert_eq!(
        types(&body),
        [
            "message",
            "reasoning",
            "function_call",
            "function_call_output",
            "message",
            "reasoning",
            "function_call",
            "function_call_output",
            "message",
        ],
        "{body}"
    );
    assert_eq!(encrypted_at(&body, 1), first, "{body}");
    assert_eq!(encrypted_at(&body, 5), second, "{body}");
}

#[test]
fn restores_cumulative_assistant_turns() {
    let scope = scope("claude:session-cumulative-messages:agent:main");
    let first = valid_encrypted_content(23);
    let second = valid_encrypted_content(24);
    save(
        &scope,
        &completed(&[reasoning(&first), assistant("first answer")]),
    );
    save(
        &scope,
        &completed(&[reasoning(&second), assistant("second answer")]),
    );

    let mut body = parse(concat!(
        r#"{"model":"gpt-5.4","input":["#,
        r#"{"type":"message","role":"user","content":"first"},"#,
        r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"first answer"}]},"#,
        r#"{"type":"message","role":"user","content":"second"},"#,
        r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"second answer"}]},"#,
        r#"{"type":"message","role":"user","content":"third"}"#,
        r#"]}"#,
    ));
    let request = request(
        "gpt-5.4",
        &claude_code_payload("session-cumulative-messages"),
    );
    let got = apply(&request, &claude_options(), &mut body);
    assert_eq!(
        (&got.model, &got.session_key),
        (&scope.model, &scope.session_key)
    );
    assert_eq!(
        types(&body),
        [
            "message",
            "reasoning",
            "message",
            "message",
            "reasoning",
            "message",
            "message"
        ],
        "{body}"
    );
    assert_eq!(encrypted_at(&body, 1), first, "{body}");
    assert_eq!(encrypted_at(&body, 4), second, "{body}");
}

#[test]
fn skips_detached_turn_after_compaction() {
    let scope = scope("claude:session-compacted:agent:main");
    let detached = valid_encrypted_content(25);
    let retained = valid_encrypted_content(26);
    save(
        &scope,
        &completed(&[reasoning(&detached), assistant("removed answer")]),
    );
    save(
        &scope,
        &completed(&[reasoning(&retained), assistant("retained answer")]),
    );

    let mut body = parse(concat!(
        r#"{"model":"gpt-5.4","input":["#,
        r#"{"type":"message","role":"user","content":"compacted summary"},"#,
        r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"retained answer"}]},"#,
        r#"{"type":"message","role":"user","content":"continue"}"#,
        r#"]}"#,
    ));
    let request = request("gpt-5.4", &claude_code_payload("session-compacted"));
    apply(&request, &claude_options(), &mut body);
    assert_eq!(input_items(&body).len(), 4, "{body}");
    assert_eq!(encrypted_at(&body, 1), retained, "{body}");
    assert!(
        input_items(&body)
            .iter()
            .all(|item| str_at(item, "encrypted_content") != detached),
        "{body}"
    );
}

#[test]
fn matches_newest_duplicate_assistant_after_compaction() {
    let scope = scope("claude:session-duplicate-compaction:agent:main");
    let old = valid_encrypted_content(27);
    let new = valid_encrypted_content(28);
    for encrypted in [&old, &new] {
        save(
            &scope,
            &completed(&[reasoning(encrypted), assistant("Done")]),
        );
    }

    let mut body = parse(concat!(
        r#"{"model":"gpt-5.4","input":["#,
        r#"{"type":"message","role":"user","content":"compacted summary"},"#,
        r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done"}]},"#,
        r#"{"type":"message","role":"user","content":"continue"}"#,
        r#"]}"#,
    ));
    let request = request(
        "gpt-5.4",
        &claude_code_payload("session-duplicate-compaction"),
    );
    apply(&request, &claude_options(), &mut body);
    assert_eq!(input_items(&body).len(), 4, "{body}");
    assert_eq!(encrypted_at(&body, 1), new, "{body}");
    assert!(
        input_items(&body)
            .iter()
            .all(|item| str_at(item, "encrypted_content") != old),
        "{body}"
    );
}

#[test]
fn uses_request_prefix_for_duplicate_out_of_order_turns() {
    let mut body = parse(concat!(
        r#"{"model":"gpt-5.4","input":["#,
        r#"{"type":"message","role":"user","content":"first"},"#,
        r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done"}]},"#,
        r#"{"type":"message","role":"user","content":"second"},"#,
        r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done"}]},"#,
        r#"{"type":"message","role":"user","content":"third"}"#,
        r#"]}"#,
    ));
    let input = input_items(&body).to_vec();
    let base = scope("claude:session-duplicate-prefix:agent:main");
    let old = valid_encrypted_content(29);
    let new = valid_encrypted_content(30);
    let new_scope = Scope {
        request_fingerprint: prefix_fingerprint(&input[..3]),
        ..base.clone()
    };
    save(
        &new_scope,
        &completed(&[reasoning(&new), assistant("Done")]),
    );
    let old_scope = Scope {
        request_fingerprint: prefix_fingerprint(&input[..1]),
        ..base
    };
    save(
        &old_scope,
        &completed(&[reasoning(&old), assistant("Done")]),
    );

    let request = request("gpt-5.4", &claude_code_payload("session-duplicate-prefix"));
    apply(&request, &claude_options(), &mut body);
    assert_eq!(input_items(&body).len(), 7, "{body}");
    assert_eq!(encrypted_at(&body, 1), old, "{body}");
    assert_eq!(encrypted_at(&body, 4), new, "{body}");
}

#[test]
fn drops_function_call_without_matching_output() {
    let scope = scope("claude:session-dropped-tool:agent:main");
    save(
        &scope,
        &completed(&[
            reasoning(&valid_encrypted_content(14)),
            json!({"type": "function_call", "call_id": "call_dropped", "name": "TaskCreate", "arguments": "{}"}),
        ]),
    );

    let mut body = parse(
        r#"{"model":"gpt-5.4","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"next"}]}]}"#,
    );
    let request = request(
        "gpt-5.4",
        r#"{
            "model":"gpt-5.4",
            "metadata":{"user_id":"{\"device_id\":\"device-test\",\"account_uuid\":\"\",\"session_id\":\"session-dropped-tool\"}"},
            "messages":[{"role":"user","content":[{"type":"text","text":"next"}]}]
        }"#,
    );
    let got = apply(&request, &claude_options(), &mut body);
    assert_eq!(
        (&got.model, &got.session_key),
        (&scope.model, &scope.session_key)
    );
    assert_eq!(str_at(&body, "input.0.role"), "user", "{body}");
    assert!(
        input_items(&body).iter().all(|item| {
            str_at(item, "type") != "reasoning" && str_at(item, "call_id") != "call_dropped"
        }),
        "{body}"
    );
}

#[test]
fn prefix_fingerprints_match_direct_computation() {
    let items = [
        json!({"type": "message", "role": "user", "content": "a"}),
        json!({"type": "reasoning", "encrypted_content": "abc"}),
        json!({"type": "function_call", "call_id": "call_1"}),
        json!({"type": "function_call_output", "call_id": "call_1", "output": "ok"}),
    ];
    let mut cache = PrefixFingerprints::new(&items);
    // Out-of-order and repeated probes, as the anchor search makes.
    for end in [4, 2, 0, 3, 1, 4, 2] {
        let want = prefix_fingerprint(&items[..end]);
        assert_eq!(cache.at(end), want, "prefix {end}");
    }
    assert_eq!(cache.at(5), "");
}

// Not upstream's: replay only runs for Claude clients on `/responses`
// calls, and the session key's fallbacks come in upstream's order.
#[test]
fn scope_needs_a_claude_responses_call_and_a_session() {
    let payload = claude_code_payload("session-scope");
    let body = || parse(r#"{"model":"gpt-5.4","input":[]}"#);
    let suffixed = request("gpt-5.4(high)", &payload);
    for kind in [Kind::Execute, Kind::Stream] {
        let scope = prepare(kind, &suffixed, &claude_options(), &mut body());
        assert_eq!(scope.session_key, "claude:session-scope:agent:main");
        assert!(!scope.request_fingerprint.is_empty());
    }
    for kind in [Kind::Compact, Kind::CountTokens] {
        assert_eq!(
            prepare(kind, &suffixed, &claude_options(), &mut body()),
            Scope::default()
        );
    }
    let native = Options::new(Format::OPENAI_RESPONSE);
    assert_eq!(
        prepare(Kind::Execute, &suffixed, &native, &mut body()),
        Scope::default()
    );

    // The model falls back to the request's, without its thinking suffix.
    let scope = scope_from_request(&suffixed, &claude_options(), &parse(r#"{"input":[]}"#));
    assert_eq!(scope.model, "gpt-5.4");

    let bare = request("gpt-5.4", r#"{"messages":[]}"#);
    let key = |options: &Options, body: &str| session_key(&bare, options, &parse(body));
    let with_execution = Options {
        metadata: Metadata {
            execution_session_id: Some(" ws-1 ".into()),
            ..Metadata::default()
        },
        ..claude_options()
    };
    assert_eq!(
        key(&with_execution, r#"{"prompt_cache_key":"p"}"#),
        "execution:ws-1"
    );
    assert_eq!(
        key(&claude_options(), r#"{"prompt_cache_key":" p "}"#),
        "prompt-cache:p"
    );
    assert_eq!(
        key(
            &claude_options(),
            r#"{"client_metadata":{"x-codex-turn-metadata":"{\"window_id\":\"w\"}"}}"#
        ),
        "window:w"
    );
    let from_payload = request("gpt-5.4", r#"{"prompt_cache_key":"from-payload"}"#);
    assert_eq!(
        session_key(&from_payload, &claude_options(), &parse("{}")),
        "prompt-cache:from-payload"
    );
    let with_headers = |pairs: &[(&str, &str)]| Options {
        headers: headers(pairs),
        ..claude_options()
    };
    assert_eq!(
        key(
            &with_headers(&[
                ("x-codex-turn-metadata", r#"{"prompt_cache_key":"t"}"#),
                ("x-codex-window-id", "w")
            ]),
            "{}"
        ),
        "prompt-cache:t"
    );
    assert_eq!(
        key(
            &with_headers(&[
                ("x-codex-turn-metadata", "not json"),
                ("x-codex-window-id", "w")
            ]),
            "{}"
        ),
        "window:w"
    );
    assert_eq!(
        key(&with_headers(&[("conversation_id", "c")]), "{}"),
        "conversation_id:c"
    );
    assert_eq!(key(&claude_options(), "{}"), "");
}

// Not upstream's: a turn is saved for `response.completed` and
// `response.done` but not `response.incomplete`, and only a rejected
// reasoning signature clears the session.
#[test]
fn saves_completed_turns_and_clears_on_invalid_signature() {
    let scope = scope("claude:session-hooks:agent:main");
    let event = |kind: &str, seed: u8| json!({"type": kind, "response": {"output": [reasoning(&valid_encrypted_content(seed))]}});
    let cache = ReplayCache::global;
    on_completed(&scope, &event("response.incomplete", 41));
    assert_eq!(cache().get(&scope.model, &scope.session_key), None);
    on_completed(&scope, &event("response.done", 42));
    on_completed(&scope, &event("response.completed", 43));
    let saved = cache().get(&scope.model, &scope.session_key).unwrap();
    assert_eq!(saved.len(), 4);
    assert_eq!(
        str_at(&saved[3], "encrypted_content"),
        valid_encrypted_content(43)
    );

    on_failure(&scope, 400, br#"{"error":{"message":"Invalid input."}}"#);
    assert!(cache().get(&scope.model, &scope.session_key).is_some());
    on_failure(
        &scope,
        400,
        br#"{"error":{"message":"Invalid signature in thinking block"}}"#,
    );
    assert_eq!(cache().get(&scope.model, &scope.session_key), None);
}

// Not upstream's: a replayed call takes the shortened ID its result has in
// a Claude client's history, and a call the input already has isn't added.
#[test]
fn aligns_and_skips_known_tool_calls() {
    let long_id = format!("call_{}", "a".repeat(62));
    let short_id = shorten_call_id(&long_id).into_owned();
    assert_ne!(short_id, long_id);
    let saved = [
        json!({"type": "function_call", "call_id": long_id, "name": "lookup", "arguments": "{}"}),
        json!({"type": "function_call", "call_id": "call_known", "name": "lookup", "arguments": "{}"}),
    ];
    let mut body = parse(&format!(
        concat!(
            r#"{{"input":["#,
            r#"{{"type":"function_call","call_id":"call_known","name":"lookup","arguments":"{{}}"}},"#,
            r#"{{"type":"function_call_output","call_id":"call_known","output":"a"}},"#,
            r#"{{"type":"function_call_output","call_id":"{}","output":"b"}}"#,
            r#"]}}"#,
        ),
        short_id
    ));
    assert!(insert_turns(&mut body, &saved));
    assert_eq!(
        types(&body),
        [
            "function_call",
            "function_call_output",
            "function_call",
            "function_call_output"
        ],
        "{body}"
    );
    assert_eq!(str_at(&body, "input.2.call_id"), short_id);
    assert!(!insert_turns(&mut parse(r#"{"input":"text"}"#), &saved));
}
