//! The translators for Gemini clients, through the registry.

use serde_json::{Value, json};

use crate::registry::{Format, Registry, ResponseContext};

const PROVIDERS: [Format; 3] = [Format::CODEX, Format::CLAUDE, Format::OPENAI];

fn context<'a>(model: &'a str, original_request: &'a Value) -> ResponseContext<'a> {
    ResponseContext {
        model,
        original_request,
        request: &Value::Null,
    }
}

fn parse(chunk: &[u8]) -> Value {
    serde_json::from_slice(chunk).expect("chunk is JSON")
}

/// The first candidate's parts in each chunk.
fn parts(chunks: &[Vec<u8>]) -> Vec<Value> {
    chunks
        .iter()
        .map(|chunk| parse(chunk)["candidates"][0]["content"]["parts"].clone())
        .collect()
}

#[test]
fn gemini_pairs_are_registered() {
    let registry = Registry::builtin();
    for provider in PROVIDERS {
        assert!(registry.has_request_transformer(&Format::GEMINI, &provider));
        assert!(registry.has_stream_response_transformer(&Format::GEMINI, &provider));
        assert!(registry.has_non_stream_response_transformer(&Format::GEMINI, &provider));
    }
}

#[test]
fn gemini_clients_count_tokens() {
    let registry = Registry::builtin();
    for provider in PROVIDERS {
        assert_eq!(
            parse(&registry.translate_token_count(&provider, &Format::GEMINI, 7, b"raw".to_vec())),
            json!({"totalTokens": 7, "promptTokensDetails": [{"modality": "TEXT", "tokenCount": 7}]}),
            "{provider}"
        );
    }
}

#[test]
fn requests_are_translated() {
    let registry = Registry::builtin();
    let body = json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
    let translate = |provider: &Format| {
        registry.translate_request(&Format::GEMINI, provider, "m", body.clone(), true)
    };

    let codex = translate(&Format::CODEX);
    assert_eq!(codex["model"], "m");
    assert_eq!(codex["input"][0]["content"][0]["text"], "hi");

    let claude = translate(&Format::CLAUDE);
    assert_eq!(claude["model"], "m");
    assert_eq!(claude["stream"], true);
    assert_eq!(claude["messages"][0]["content"][0]["text"], "hi");
    assert_eq!(claude["metadata"], json!({}), "no user ID is made up");

    let openai = translate(&Format::OPENAI);
    assert_eq!(
        openai,
        json!({"model": "m", "messages": [{"role": "user", "content": "hi"}], "stream": true})
    );
}

#[test]
fn codex_stream_and_non_stream() {
    let registry = Registry::builtin();
    let original = json!({});
    let ctx = context("gemini-x", &original);
    let mut stream = registry.response_stream(&Format::CODEX, &Format::GEMINI, &ctx);
    assert!(
        stream
            .translate(b"event: response.output_text.delta")
            .is_empty()
    );
    let chunks = stream.translate(br#"data: {"type":"response.output_text.delta","delta":"hi"}"#);
    assert_eq!(parts(&chunks), [json!([{"text": "hi"}])]);
    assert_eq!(parse(&chunks[0])["modelVersion"], "gemini-x");

    let translate = |body: &[u8]| {
        registry.translate_non_stream(&Format::CODEX, &Format::GEMINI, &ctx, body.to_vec())
    };
    assert_eq!(
        translate(br#"{"type":"response.created"}"#),
        Some(Vec::new())
    );
    let done = translate(
        br#"{"type":"response.completed","response":{"id":"r","output":[{"type":"message","content":[{"type":"output_text","text":"ok"}]}]}}"#,
    )
    .expect("translated");
    let done = parse(&done);
    assert_eq!(
        done["candidates"][0]["content"]["parts"],
        json!([{"text": "ok"}])
    );
    assert_eq!(done["responseId"], "r");
}

#[test]
fn claude_stream_and_non_stream() {
    let registry = Registry::builtin();
    let original = json!({});
    let ctx = context("gemini-x", &original);
    let events = [
        r#"data: {"type":"message_start","message":{"id":"msg_1"}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
    ];

    let mut stream = registry.response_stream(&Format::CLAUDE, &Format::GEMINI, &ctx);
    assert!(stream.translate(events[0].as_bytes()).is_empty());
    let chunks = stream.translate(events[1].as_bytes());
    assert_eq!(parts(&chunks), [json!([{"text": "hi"}])]);
    assert_eq!(parse(&chunks[0])["responseId"], "msg_1");

    let body = events.join("\n").into_bytes();
    let whole = registry
        .translate_non_stream(&Format::CLAUDE, &Format::GEMINI, &ctx, body)
        .expect("translated");
    let whole = parse(&whole);
    assert_eq!(
        whole["candidates"][0]["content"]["parts"],
        json!([{"text": "hi"}])
    );
    assert_eq!(whole["modelVersion"], "gemini-x");
}

#[test]
fn openai_stream_and_non_stream() {
    let registry = Registry::builtin();
    let original = json!({});
    let ctx = context("gemini-x", &original);

    let mut stream = registry.response_stream(&Format::OPENAI, &Format::GEMINI, &ctx);
    let chunks = stream
        .translate(br#"data: {"model":"m","choices":[{"delta":{"reasoning_content":["a","b"]}}]}"#);
    assert_eq!(
        parts(&chunks),
        [
            json!([{"thought": true, "text": "a"}]),
            json!([{"thought": true, "text": "b"}])
        ]
    );
    assert!(stream.translate(b"data: [DONE]").is_empty());

    let whole = registry
        .translate_non_stream(
            &Format::OPENAI,
            &Format::GEMINI,
            &ctx,
            br#"{"choices":[{"index":0,"message":{"content":"ok"},"finish_reason":"stop"}]}"#
                .to_vec(),
        )
        .expect("translated");
    assert_eq!(
        parse(&whole),
        json!({"candidates": [{"content": {"parts": [{"text": "ok"}], "role": "model"}, "index": 0, "finishReason": "STOP"}]})
    );
}
