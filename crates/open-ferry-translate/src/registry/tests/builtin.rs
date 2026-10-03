//! The built-in translators, through the registry.

use serde_json::{Value, json};

use super::super::*;

const PAIRS: [(Format, Format); 8] = [
    (Format::CLAUDE, Format::CODEX),
    (Format::OPENAI, Format::CODEX),
    (Format::OPENAI_RESPONSE, Format::CODEX),
    (Format::OPENAI, Format::CLAUDE),
    (Format::OPENAI_RESPONSE, Format::CLAUDE),
    (Format::CLAUDE, Format::OPENAI),
    (Format::OPENAI, Format::OPENAI),
    (Format::OPENAI_RESPONSE, Format::OPENAI),
];

fn context<'a>(
    model: &'a str,
    original_request: &'a Value,
    request: &'a Value,
) -> ResponseContext<'a> {
    ResponseContext {
        model,
        original_request,
        request,
    }
}

fn parse(chunk: &[u8]) -> Value {
    serde_json::from_slice(chunk).expect("chunk is JSON")
}

#[test]
fn builtin_pairs_are_registered() {
    let registry = Registry::builtin();
    for (client, provider) in PAIRS {
        assert!(
            registry.has_request_transformer(&client, &provider),
            "{client} -> {provider}"
        );
        assert!(
            registry.has_stream_response_transformer(&client, &provider),
            "{client} -> {provider}"
        );
        assert!(
            registry.has_non_stream_response_transformer(&client, &provider),
            "{client} -> {provider}"
        );
        assert_eq!(
            registry.has_request_transformer(&provider, &client),
            PAIRS.contains(&(provider.clone(), client.clone())),
            "{provider} -> {client}"
        );
    }
    assert!(!registry.has_request_transformer(&Format::CLAUDE, &Format::CLAUDE));
    assert!(!registry.has_request_transformer(&Format::CLAUDE, &Format::GEMINI));
    assert!(Registry::global().has_request_transformer(&Format::CLAUDE, &Format::CODEX));
    assert!(!Registry::new().has_response_transformer(&Format::CLAUDE, &Format::CODEX));
}

#[test]
fn only_claude_clients_count_tokens() {
    let registry = Registry::builtin();
    for provider in [Format::CODEX, Format::OPENAI] {
        assert_eq!(
            registry.translate_token_count(&provider, &Format::CLAUDE, 7, b"raw".to_vec()),
            br#"{"input_tokens":7}"#,
            "{provider}"
        );
    }
    for (provider, client) in [
        (Format::CODEX, Format::OPENAI),
        (Format::OPENAI, Format::OPENAI),
        (Format::CLAUDE, Format::OPENAI),
        (Format::OPENAI, Format::OPENAI_RESPONSE),
    ] {
        assert_eq!(
            registry.translate_token_count(&provider, &client, 7, b"raw".to_vec()),
            b"raw",
            "{provider} -> {client}"
        );
    }
}

#[test]
fn chat_effort_shows_codex_summaries() {
    let registry = Registry::builtin();
    let translate = |effort: &str| {
        registry.translate_request(
            &Format::OPENAI,
            &Format::CODEX,
            "gpt-5.6-luna",
            json!({"model": "gpt-5.6-luna", "messages": [], "reasoning_effort": effort}),
            true,
        )
    };
    assert_eq!(
        translate("high")["reasoning"],
        json!({"effort": "high", "summary": "auto"})
    );
    assert_eq!(translate("none")["reasoning"], json!({"effort": "none"}));
}

#[test]
fn claude_display_reaches_codex() {
    let registry = Registry::builtin();
    let translate = |display: &str| {
        registry.translate_request(
            &Format::CLAUDE,
            &Format::CODEX,
            "gpt-5.6-luna",
            json!({
                "model": "gpt-5.6-luna",
                "max_tokens": 1000,
                "messages": [],
                "thinking": {"type": "adaptive", "display": display}
            }),
            true,
        )
    };
    assert_eq!(translate("summarized")["reasoning"]["summary"], "auto");
    assert!(translate("omitted")["reasoning"].get("summary").is_none());
}

#[test]
fn codex_to_claude_stream_gives_one_chunk_per_line() {
    let registry = Registry::builtin();
    let original = json!({});
    let ctx = context("gpt-5.6-luna", &original, &Value::Null);
    let mut stream = registry.response_stream(&Format::CODEX, &Format::CLAUDE, &ctx);
    assert!(stream.translate(b"event: response.created").is_empty());
    let created =
        stream.translate(br#"data: {"type":"response.created","response":{"id":"r","model":"m"}}"#);
    assert_eq!(created.len(), 1);
    assert!(created[0].starts_with(b"event: message_start\n"));
    // A line that gives no events gives no chunk.
    assert!(
        stream
            .translate(br#"data: {"type":"response.in_progress"}"#)
            .is_empty()
    );
    let deltas = stream.translate(br#"data: {"type":"response.output_text.delta","delta":"hi"}"#);
    assert_eq!(deltas.len(), 1, "{deltas:?}");
    let text = String::from_utf8(deltas[0].clone()).expect("UTF-8");
    assert_eq!(text.matches("event: ").count(), 2, "{text}");
}

#[test]
fn codex_to_claude_non_stream_needs_the_final_event() {
    let registry = Registry::builtin();
    let original = json!({});
    let ctx = context("m", &original, &Value::Null);
    let translate = |body: &[u8]| {
        registry.translate_non_stream(&Format::CODEX, &Format::CLAUDE, &ctx, body.to_vec())
    };
    assert_eq!(
        translate(br#"{"type":"response.created"}"#),
        Some(Vec::new())
    );
    assert_eq!(translate(b"not json"), Some(Vec::new()));
    let done = translate(
        br#"{"type":"response.completed","response":{"id":"r","model":"m","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"hi"}]}]}}"#,
    )
    .expect("translated");
    let done = parse(&done);
    assert_eq!(done["type"], "message");
    assert_eq!(done["content"][0]["text"], "hi");
}

#[test]
fn codex_to_chat_stream_and_non_stream() {
    let registry = Registry::builtin();
    let original = json!({"model": "gpt-5.6-luna"});
    let ctx = context("gpt-5.6-luna", &original, &Value::Null);
    let mut stream = registry.response_stream(&Format::CODEX, &Format::OPENAI, &ctx);
    assert!(stream.translate(b"event: x").is_empty());
    assert!(
        stream
            .translate(br#"data: {"type":"response.created","response":{"id":"r","created_at":1}}"#)
            .is_empty()
    );
    let delta = stream.translate(br#"data: {"type":"response.output_text.delta","delta":"hi"}"#);
    assert_eq!(delta.len(), 1);
    let delta = parse(&delta[0]);
    assert_eq!(delta["object"], "chat.completion.chunk");
    assert_eq!(delta["choices"][0]["delta"]["content"], "hi");

    let translate = |body: &[u8]| {
        registry.translate_non_stream(&Format::CODEX, &Format::OPENAI, &ctx, body.to_vec())
    };
    assert_eq!(
        translate(br#"{"type":"response.output_text.delta"}"#),
        Some(Vec::new())
    );
    let done = translate(
        br#"{"type":"response.completed","response":{"id":"r","created_at":1,"model":"m","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"hi"}]}]}}"#,
    )
    .expect("translated");
    assert_eq!(parse(&done)["choices"][0]["message"]["content"], "hi");
}

#[test]
fn codex_to_responses_passes_lines_through() {
    let registry = Registry::builtin();
    let original = json!({"model": "client-model"});
    let ctx = context("gpt-5.6-luna", &original, &Value::Null);
    let mut stream = registry.response_stream(&Format::CODEX, &Format::OPENAI_RESPONSE, &ctx);
    assert_eq!(
        stream.translate(b"event: response.created"),
        [b"event: response.created"]
    );
    assert!(stream.translate(b"").is_empty());
    let line = br#"data:{"type":"response.output_text.delta", "delta":"hi"}"#;
    assert_eq!(stream.translate(line), [line]);
    let created = stream.translate(br#"data: {"type":"response.created","response":{"id":"r"}}"#);
    assert_eq!(
        created,
        [br#"data: {"type":"response.created","response":{"id":"r","model":"client-model"}}"#]
    );
}

#[test]
fn codex_to_responses_non_stream_keeps_codex_text() {
    let registry = Registry::builtin();
    let ctx = context("m", &Value::Null, &Value::Null);
    let translate = |body: &[u8]| {
        registry.translate_non_stream(
            &Format::CODEX,
            &Format::OPENAI_RESPONSE,
            &ctx,
            body.to_vec(),
        )
    };
    assert_eq!(
        translate(br#"{"type":"response.completed", "response" : { "id" : "r", "n" : 1.50 } }"#),
        Some(br#"{ "id" : "r", "n" : 1.50 }"#.to_vec())
    );
    assert_eq!(
        translate(br#"{"type":"response.incomplete","response":{"id":"r"},"response":{"id":"s"}}"#),
        Some(br#"{"id":"r"}"#.to_vec()),
        "gjson reads the first of repeated keys"
    );
    let response = br#"{ "id": "r", "output": [] }"#;
    assert_eq!(translate(response), Some(response.to_vec()));
    assert_eq!(
        translate(br#"{"type":"response.completed"}"#),
        Some(Vec::new())
    );
    assert_eq!(
        translate(br#"{"type":"response.created","response":{}}"#),
        Some(Vec::new())
    );
    assert_eq!(translate(br#"{"output":{}}"#), Some(Vec::new()));
    assert_eq!(translate(b"not json"), Some(Vec::new()));
}

#[test]
fn claude_to_chat_stream_and_non_stream() {
    let registry = Registry::builtin();
    let ctx = context("claude-opus-5", &Value::Null, &Value::Null);
    let mut stream = registry.response_stream(&Format::CLAUDE, &Format::OPENAI, &ctx);
    assert!(stream.translate(b"event: message_start").is_empty());
    let start = stream.translate(
        br#"data: {"type":"message_start","message":{"id":"msg_1","model":"claude-opus-5","usage":{"input_tokens":1}}}"#,
    );
    assert_eq!(start.len(), 1);
    assert_eq!(parse(&start[0])["model"], "claude-opus-5");

    let body = concat!(
        "event: message_start\n",
        r#"data: {"type":"message_start","message":{"id":"msg_1","model":"claude-opus-5"}}"#,
        "\n\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
        "\n",
        r#"data: {"type":"message_stop"}"#,
        "\n",
    );
    let done = registry
        .translate_non_stream(
            &Format::CLAUDE,
            &Format::OPENAI,
            &ctx,
            body.as_bytes().to_vec(),
        )
        .expect("translated");
    assert_eq!(parse(&done)["choices"][0]["message"]["content"], "hi");
}

const PATCH_REQUEST: &str = r#"{"tools":[{"type":"custom","name":"apply_patch","format":{"type":"grammar","syntax":"lark","definition":"start: patch"}}]}"#;

const PATCH_START: &[u8] = br#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"c1","name":"apply_patch","input":{}}}"#;

const BAD_FRAGMENT: &[u8] = br#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#;

const MESSAGE_START: &[u8] =
    br#"data: {"type":"message_start","message":{"id":"msg_1","model":"claude-opus-5"}}"#;

#[test]
fn claude_to_responses_stream_gives_one_chunk_per_event() {
    let registry = Registry::builtin();
    let original = json!({"model": "claude-opus-5"});
    let ctx = context("claude-opus-5", &original, &Value::Null);
    let mut stream = registry.response_stream(&Format::CLAUDE, &Format::OPENAI_RESPONSE, &ctx);
    let chunks = stream.translate(MESSAGE_START);
    let kinds: Vec<_> = chunks
        .iter()
        .map(|chunk| {
            let text = std::str::from_utf8(chunk).expect("UTF-8");
            assert!(text.ends_with("\n\n"), "{text}");
            assert_eq!(text.matches("event: ").count(), 1, "{text}");
            text.lines().next().expect("event line").to_owned()
        })
        .collect();
    assert_eq!(
        kinds,
        ["event: response.created", "event: response.in_progress"]
    );
    assert!(stream.finish().is_empty(), "no apply_patch declared");
    assert!(stream.tool_input_error().is_none());
}

#[test]
fn claude_to_responses_stream_fails_a_bad_patch() {
    let registry = Registry::builtin();
    let original: Value = serde_json::from_str(PATCH_REQUEST).expect("JSON");
    let ctx = context("claude-opus-5", &original, &Value::Null);
    let mut stream = registry.response_stream(&Format::CLAUDE, &Format::OPENAI_RESPONSE, &ctx);
    stream.translate(MESSAGE_START);
    stream.translate(PATCH_START);
    let failed = stream.translate(BAD_FRAGMENT);
    assert!(
        failed
            .iter()
            .any(|chunk| chunk.starts_with(b"event: response.failed\n")),
        "{failed:?}"
    );
    assert!(stream.tool_input_error().is_some());
    assert!(
        stream
            .translate(br#"data: {"type":"message_stop"}"#)
            .is_empty()
    );
    assert!(stream.finish().is_empty());
}

#[test]
fn claude_to_responses_stream_fails_when_cut_short() {
    let registry = Registry::builtin();
    let original: Value = serde_json::from_str(PATCH_REQUEST).expect("JSON");
    let ctx = context("claude-opus-5", &original, &Value::Null);
    let mut stream = registry.response_stream(&Format::CLAUDE, &Format::OPENAI_RESPONSE, &ctx);
    stream.translate(MESSAGE_START);
    let failed = stream.finish();
    assert_eq!(failed.len(), 1);
    assert!(failed[0].starts_with(b"event: response.failed\n"));
    assert!(stream.tool_input_error().is_some());
}

#[test]
fn claude_to_responses_non_stream_fails_a_bad_patch() {
    let registry = Registry::builtin();
    let original: Value = serde_json::from_str(PATCH_REQUEST).expect("JSON");
    let ctx = context("claude-opus-5", &original, &Value::Null);
    let body = [
        MESSAGE_START,
        PATCH_START,
        BAD_FRAGMENT,
        br#"data: {"type":"message_stop"}"#,
    ]
    .join(&b'\n');
    assert_eq!(
        registry.translate_non_stream(&Format::CLAUDE, &Format::OPENAI_RESPONSE, &ctx, body),
        None
    );

    let body = [MESSAGE_START, br#"data: {"type":"message_stop"}"#].join(&b'\n');
    let done = registry
        .translate_non_stream(&Format::CLAUDE, &Format::OPENAI_RESPONSE, &ctx, body)
        .expect("translated");
    let done = parse(&done);
    assert_eq!(done["object"], "response");
    assert_eq!(done["id"], "msg_1");
}

/// The event line of each chunk, checking that each holds one whole event.
fn event_kinds(chunks: &[Vec<u8>]) -> Vec<String> {
    chunks
        .iter()
        .map(|chunk| {
            let text = std::str::from_utf8(chunk).expect("UTF-8");
            assert!(text.ends_with("\n\n"), "{text}");
            assert_eq!(text.matches("event: ").count(), 1, "{text}");
            text.lines().next().expect("event line").to_owned()
        })
        .collect()
}

#[test]
fn chat_to_claude_stream_gives_one_chunk_per_event() {
    let registry = Registry::builtin();
    let original = json!({"model": "claude-opus-5", "stream": true});
    let ctx = context("gpt-5.6-luna", &original, &Value::Null);
    let mut stream = registry.response_stream(&Format::OPENAI, &Format::CLAUDE, &ctx);
    let chunks = stream.translate(CHAT_TEXT);
    assert_eq!(
        event_kinds(&chunks),
        [
            "event: message_start",
            "event: content_block_start",
            "event: content_block_delta"
        ]
    );
    assert!(stream.translate(b"").is_empty());
    assert!(stream.finish().is_empty());
    assert!(stream.tool_input_error().is_none());
}

#[test]
fn chat_to_claude_non_stream() {
    let registry = Registry::builtin();
    let original = json!({"model": "claude-opus-5"});
    let ctx = context("gpt-5.6-luna", &original, &Value::Null);
    let translate = |body: &[u8]| {
        let done = registry
            .translate_non_stream(&Format::OPENAI, &Format::CLAUDE, &ctx, body.to_vec())
            .expect("translated");
        parse(&done)
    };
    let done = translate(
        br#"{"id":"chatcmpl-1","model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}"#,
    );
    assert_eq!(done["type"], "message");
    assert_eq!(done["content"][0]["text"], "hi");
    assert_eq!(done["stop_reason"], "end_turn");
    assert_eq!(translate(b"not json")["type"], "message");
}

#[test]
fn chat_to_chat_passes_payloads_through() {
    let registry = Registry::builtin();
    let ctx = context("m", &Value::Null, &Value::Null);
    let mut stream = registry.response_stream(&Format::OPENAI, &Format::OPENAI, &ctx);
    assert!(stream.is_translated());
    assert_eq!(stream.translate(br#"data: {"a":1} "#), [br#"{"a":1}"#]);
    assert_eq!(stream.translate(b"not data"), [b"not data"]);
    // Upstream gives an empty chunk for these, which its stream manager drops.
    assert!(stream.translate(b"data:  ").is_empty());
    assert!(stream.translate(b"").is_empty());
    assert!(stream.translate(b"data: [DONE]").is_empty());
    assert!(stream.translate(br#"data: {"b":2}"#).is_empty());
    assert!(stream.finish().is_empty());

    assert_eq!(
        registry.translate_non_stream(&Format::OPENAI, &Format::OPENAI, &ctx, b"not json".to_vec()),
        Some(b"not json".to_vec())
    );
}

const CHAT_TEXT: &[u8] = br#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":"hi"}}]}"#;

const CHAT_PATCH_START: &[u8] = br#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"apply_patch","arguments":""}}]}}]}"#;

const CHAT_BAD_FRAGMENT: &[u8] = br#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{}"}}]}}]}"#;

#[test]
fn chat_to_responses_stream_gives_one_chunk_per_event() {
    let registry = Registry::builtin();
    let original = json!({"model": "gpt-5.6-luna"});
    let ctx = context("gpt-5.6-luna", &original, &Value::Null);
    let mut stream = registry.response_stream(&Format::OPENAI, &Format::OPENAI_RESPONSE, &ctx);
    let chunks = stream.translate(CHAT_TEXT);
    assert_eq!(
        event_kinds(&chunks),
        [
            "event: response.created",
            "event: response.in_progress",
            "event: response.output_item.added",
            "event: response.content_part.added",
            "event: response.output_text.delta",
        ]
    );
    assert!(stream.finish().is_empty(), "no apply_patch declared");
    assert!(stream.tool_input_error().is_none());
}

#[test]
fn chat_to_responses_stream_fails_a_bad_patch() {
    let registry = Registry::builtin();
    let original: Value = serde_json::from_str(PATCH_REQUEST).expect("JSON");
    let ctx = context("gpt-5.6-luna", &original, &Value::Null);
    let mut stream = registry.response_stream(&Format::OPENAI, &Format::OPENAI_RESPONSE, &ctx);
    stream.translate(CHAT_PATCH_START);
    let failed = stream.translate(CHAT_BAD_FRAGMENT);
    assert!(
        failed
            .iter()
            .any(|chunk| chunk.starts_with(b"event: response.failed\n")),
        "{failed:?}"
    );
    assert!(stream.tool_input_error().is_some());
    assert!(stream.translate(b"data: [DONE]").is_empty());
    assert!(stream.finish().is_empty());
}

#[test]
fn chat_to_responses_stream_fails_when_cut_short() {
    let registry = Registry::builtin();
    let original: Value = serde_json::from_str(PATCH_REQUEST).expect("JSON");
    let ctx = context("gpt-5.6-luna", &original, &Value::Null);
    let mut stream = registry.response_stream(&Format::OPENAI, &Format::OPENAI_RESPONSE, &ctx);
    stream.translate(CHAT_TEXT);
    let failed = stream.finish();
    assert_eq!(failed.len(), 1);
    assert!(failed[0].starts_with(b"event: response.failed\n"));
    assert!(stream.tool_input_error().is_some());
}

#[test]
fn chat_to_responses_non_stream_fails_a_bad_patch() {
    let registry = Registry::builtin();
    let original: Value = serde_json::from_str(PATCH_REQUEST).expect("JSON");
    let ctx = context("gpt-5.6-luna", &original, &Value::Null);
    let translate = |body: &[u8]| {
        registry.translate_non_stream(
            &Format::OPENAI,
            &Format::OPENAI_RESPONSE,
            &ctx,
            body.to_vec(),
        )
    };
    assert_eq!(
        translate(
            br#"{"id":"chatcmpl-1","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":"call_1","type":"function","function":{"name":"apply_patch","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#
        ),
        None
    );

    let done = translate(
        br#"{"id":"chatcmpl-1","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}"#,
    )
    .expect("translated");
    let done = parse(&done);
    assert_eq!(done["object"], "response");
    assert_eq!(done["id"], "chatcmpl-1");
    assert_eq!(done["output"][0]["content"][0]["text"], "hi");
}
