// Ported from CLIProxyAPI sdk/cliproxy/auth/response_model_rewriter_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The force-mapping response rewriter: SSE `data:` lines with and without
//! the space, events split over chunks or line by line, glued events, whole
//! JSON bodies, and when nothing is rewritten.
//!
//! Upstream's `rewriteForceMappedStreamChunk(rewriter, chunk)` is
//! `StreamRewriter::rewrite_stream_chunk` and
//! `finishForceMappedStreamChunks(rewriter)` is `StreamRewriter::finish`.
//!
//! Deviations from upstream:
//! - `TestRewriteForceMappedStreamChunk_NoRewriteWhenRewriterNil` is adapted:
//!   the port has no nil rewriter to pass in. A stream without a forced
//!   alias has no rewriter, so the test runs one through the manager and
//!   checks the chunk comes out unchanged.
//! - `TestStreamRewriter_LoggedOnceAndRewrittenChunksCount` is adapted: its
//!   counters (`rewrittenChunks`, `loggedPaths`, `loggedFinished`) only feed
//!   upstream's debug logging, which isn't ported. The test checks what they
//!   count instead: the three JSON chunks have their `modelVersion`
//!   rewritten, `[DONE]` passes through, and finishing flushes nothing.

use bytes::Bytes;
use http::HeaderMap;
use serde_json::Value;

use super::support::*;
use crate::auth::Status;
use crate::exec::{Dispatcher, Response};
use crate::manager::execute::rewrite_force_mapped_response;
use crate::manager::models::AliasResult;
use crate::manager::rewrite::{
    StreamRewriter, extract_sse_data_line, normalize_glued_sse_events, rewrite_model_in_response,
    rewrite_sse_payload_lines,
};

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// gjson's `ValidBytes`.
fn valid_json(data: &[u8]) -> bool {
    serde_json::from_slice::<Value>(data).is_ok()
}

#[test]
fn stream_rewriter_rewrite_chunk_kimi_messages_data_prefix_without_space() {
    let mut rewriter = StreamRewriter::new("k2.5");
    let chunk = "event:message_start\n".to_owned()
        + r#"data:{"type":"message_start","message":{"model":"kimi-k2.5"}}"#
        + "\n\n";

    let got = text(&rewriter.rewrite_chunk(chunk.as_bytes()));
    assert!(
        got.contains(r#""model":"k2.5""#),
        "rewritten chunk = {got:?}, want alias model k2.5"
    );
    assert!(
        !got.contains("kimi-k2.5"),
        "rewritten chunk still contains upstream model: {got:?}"
    );
    assert!(
        got.contains("data:{"),
        "rewritten chunk should preserve data: prefix without space: {got:?}"
    );
}

#[test]
fn stream_rewriter_rewrite_chunk_anthropic_messages_data_prefix_with_space() {
    let mut rewriter = StreamRewriter::new("grok-latest");
    let chunk =
        r#"data: {"type":"message_start","message":{"model":"grok-4.3"}}"#.to_owned() + "\n\n";

    let got = text(&rewriter.rewrite_chunk(chunk.as_bytes()));
    assert!(
        got.contains(r#""model":"grok-latest""#),
        "rewritten chunk = {got:?}, want alias model grok-latest"
    );
    assert!(
        !got.contains("grok-4.3"),
        "rewritten chunk still contains upstream model: {got:?}"
    );
    assert!(
        got.contains("data: {"),
        "rewritten chunk should preserve spaced data: prefix: {got:?}"
    );
}

#[test]
fn stream_rewriter_finish_flushes_codex_responses_event_chunk() {
    let mut rewriter = StreamRewriter::new("gpt-5.4-fast");
    let part1 = "event: response.created\n";
    let part2 =
        r#"data: {"type":"response.created","response":{"model":"gpt-5.4"}}"#.to_owned() + "\n\n";

    let got1 = rewriter.rewrite_chunk(part1.as_bytes());
    assert!(
        got1.is_empty(),
        "first partial chunk should buffer, got {:?}",
        text(&got1)
    );
    let got2 = text(&rewriter.rewrite_chunk(part2.as_bytes()));
    let got_tail = text(&rewriter.finish());
    let combined = got2 + &got_tail;
    assert!(
        combined.contains("gpt-5.4-fast"),
        "combined output = {combined:?}, want rewritten alias"
    );
    assert!(
        !combined.contains(r#""model":"gpt-5.4""#),
        "combined output still has upstream model: {combined:?}"
    );
}

#[test]
fn stream_rewriter_rewrite_chunk_codex_responses_line_chunks() {
    let mut rewriter = StreamRewriter::new("gpt-5.4-fast");
    let lines = [
        "event: response.created\n".to_owned(),
        r#"data: {"type":"response.created","response":{"model":"gpt-5.4"}}"#.to_owned() + "\n",
        "\n".to_owned(),
        "event: response.completed\n".to_owned(),
        r#"data: {"type":"response.completed","response":{"model":"gpt-5.4"}}"#.to_owned() + "\n",
        "\n".to_owned(),
    ];
    let mut out = Vec::new();
    for line in &lines {
        out.extend(rewriter.rewrite_chunk(line.as_bytes()));
    }
    out.extend(rewriter.finish());
    let got = text(&out);
    assert!(
        got.contains("gpt-5.4-fast"),
        "rewritten output = {got:?}, want alias gpt-5.4-fast"
    );
    assert!(
        !got.contains(r#""model":"gpt-5.4""#),
        "rewritten output still contains upstream model: {got:?}"
    );
}

#[test]
fn rewrite_force_mapped_stream_chunk_codex_line_chunks_do_not_duplicate_buffered_event() {
    let mut rewriter = StreamRewriter::new("gpt-5.4-fast");
    let chunks = [
        "event: response.created\n".to_owned(),
        r#"data: {"type":"response.created","response":{"model":"gpt-5.4"}}"#.to_owned() + "\n\n",
    ];

    let mut out = Vec::new();
    for chunk in &chunks {
        out.extend(rewriter.rewrite_stream_chunk(chunk.as_bytes()));
    }
    out.extend(rewriter.finish());

    let got = text(&out);
    let count = got.matches("event: response.created").count();
    assert_eq!(count, 1, "event count; output={got:?}");
    assert!(
        got.ends_with("\n\n"),
        "rewritten output = {got:?}, want complete SSE frame terminator"
    );
    assert!(
        got.contains(r#""model":"gpt-5.4-fast""#),
        "rewritten output = {got:?}, want alias model"
    );
    assert!(
        !got.contains(r#""model":"gpt-5.4""#),
        "rewritten output still contains upstream model: {got:?}"
    );
}

#[test]
fn rewrite_model_in_response_antigravity_model_version() {
    let payload = r#"{"response":{"modelVersion":"gemini-3-flash","candidates":[{"content":{"role":"model","parts":[{"text":"AGYMSG"}]}}]}}"#;
    let got = text(&rewrite_model_in_response(
        payload.as_bytes(),
        "claude-haiku-4-5-20251001",
    ));
    assert!(
        got.contains(r#""modelVersion":"claude-haiku-4-5-20251001""#),
        "rewritten payload = {got:?}, want alias modelVersion"
    );
    assert!(
        !got.contains("gemini-3-flash"),
        "rewritten payload still contains upstream modelVersion: {got:?}"
    );
}

#[test]
fn stream_rewriter_rewrite_chunk_live_derived_provider_chunks() {
    let cases = [
        (
            "kimi_chat_stream",
            "k2.5",
            "kimi-k2.5",
            r#"data:{"id":"chatcmpl-live","object":"chat.completion.chunk","created":1782272323,"model":"kimi-k2.5","choices":[{"index":0,"delta":{"content":"KCHATS"},"finish_reason":null}]}"#.to_owned()
                + "\n\n",
        ),
        (
            "kimi_messages_stream",
            "k2.5",
            "kimi-k2.5",
            "event:message_start\n".to_owned()
                + r#"data:{"type":"message_start","message":{"model":"kimi-k2.5"}}"#
                + "\n\n",
        ),
        (
            "xai_messages_stream",
            "grok-latest",
            "grok-4.3",
            r#"data: {"type":"message_start","message":{"model":"grok-4.3"}}"#.to_owned() + "\n\n",
        ),
    ];
    for (name, rewrite_model, upstream, chunk) in cases {
        let mut rewriter = StreamRewriter::new(rewrite_model);
        let got = text(&rewriter.rewrite_chunk(chunk.as_bytes()));
        assert!(
            got.contains(rewrite_model),
            "{name}: rewritten chunk = {got:?}, want alias {rewrite_model:?}"
        );
        assert!(
            !got.contains(upstream),
            "{name}: rewritten chunk still contains upstream {upstream:?}: {got:?}"
        );
    }
}

#[test]
fn rewrite_sse_payload_lines_codex_responses_live_frame() {
    let chunk = "event: response.created\n".to_owned()
        + r#"data: {"type":"response.created","response":{"model":"gpt-5.4"}}"#
        + "\n\n"
        + "event: response.completed\n"
        + r#"data: {"type":"response.completed","response":{"model":"gpt-5.4"}}"#
        + "\n\n";
    let got = text(&rewrite_sse_payload_lines(chunk.as_bytes(), "gpt-5.4-fast"));
    assert!(
        got.contains("gpt-5.4-fast"),
        "rewritten chunk = {got:?}, want alias gpt-5.4-fast"
    );
    assert!(
        !got.contains(r#""model":"gpt-5.4""#),
        "rewritten chunk still contains upstream model: {got:?}"
    );
}

#[test]
fn rewrite_force_mapped_response_no_rewrite_when_force_mapping_disabled() {
    let upstream = r#"{"model":"gpt-5.4","choices":[]}"#;
    let mut resp = Response {
        payload: Bytes::from_static(upstream.as_bytes()),
        headers: HeaderMap::new(),
    };
    rewrite_force_mapped_response(
        &mut resp,
        &AliasResult {
            upstream_model: "gpt-5.4".into(),
            force_mapping: false,
            original_alias: "gpt-5.4-fast".into(),
        },
    );
    assert_eq!(text(&resp.payload), upstream, "payload, want unchanged");
}

#[tokio::test(start_paused = true)]
async fn rewrite_force_mapped_stream_chunk_no_rewrite_when_rewriter_nil() {
    let chunk = r#"data: {"model":"gpt-5.4"}"#.to_owned() + "\n\n";
    let h = Harness::new(Default::default());
    let reply_chunk = Bytes::from(chunk.clone());
    let executor = FakeExecutor::with("codex", move |_| {
        Reply::chunks(vec![Ok(reply_chunk.clone())])
    });
    h.executor(&executor);
    let mut credential = auth("codex-auth", "codex");
    credential.status = Status::Active;
    h.add(credential, &["gpt-5.4"]);

    // No alias, so the stream has no rewriter.
    let stream = h
        .manager
        .execute_stream(&providers(&["codex"]), request("gpt-5.4"), options())
        .await
        .expect("execute stream");
    let (chunks, err) = collect(stream).await;
    assert!(err.is_none(), "stream error: {err:?}");
    assert_eq!(chunks, [chunk], "chunk, want unchanged upstream payload");
}

#[test]
fn normalize_glued_sse_events_splits_valid_glue_only() {
    let glued = b"event: response.created\ndata: {\"type\":\"response.created\"}event: response.completed\ndata: {\"type\":\"response.completed\"}";
    let got = text(&normalize_glued_sse_events(glued.to_vec()));
    assert!(
        got.contains("}\n\nevent:"),
        "expected glued frame split, got {got:?}"
    );

    let inside = b"event: response.output_text.delta\ndata: {\"type\":\"delta\",\"text\":\"literal }event: inside string\"}";
    let got_inside = text(&normalize_glued_sse_events(inside.to_vec()));
    assert!(
        !got_inside.contains("}\n\nevent:"),
        "should not split inside JSON string, got {got_inside:?}"
    );
    for line in inside.split(|b| *b == b'\n') {
        if line.starts_with(b"data:") {
            let data = extract_sse_data_line(line).map(|(_, json)| json);
            assert!(data.is_some_and(valid_json), "baseline invalid");
        }
    }
    for line in got_inside.as_bytes().split(|b| *b == b'\n') {
        if line.starts_with(b"data:") {
            let data = extract_sse_data_line(line).map(|(_, json)| json);
            assert!(
                data.is_some_and(valid_json),
                "corrupted JSON after normalize: {got_inside:?}"
            );
        }
    }
}

#[test]
fn normalize_glued_sse_events_splits_codex_data_glue_only() {
    let glued = br#"data: {"type":"response.created"}data: {"type":"response.completed"}"#;
    let got = text(&normalize_glued_sse_events(glued.to_vec()));
    assert!(
        got.contains("}\ndata:"),
        "expected codex glued split, got {got:?}"
    );
    let inside = br#"data: {"type":"delta","text":"literal }data: inside"}"#;
    let got_inside = text(&normalize_glued_sse_events(inside.to_vec()));
    if got_inside.contains("}\ndata:") && got_inside.as_bytes() != inside {
        // Only fail if a split was actually inserted (unchanged is OK).
        for line in got_inside.as_bytes().split(|b| *b == b'\n') {
            if line.starts_with(b"data:") {
                let data = extract_sse_data_line(line).map(|(_, json)| json);
                assert!(
                    data.is_some_and(valid_json),
                    "corrupted JSON: {got_inside:?}"
                );
            }
        }
    }
}

/// The `type` of each JSON event in `payload` (upstream's
/// `parseResponsesWSDataEventTypes`).
fn parse_responses_ws_data_event_types(payload: &[u8]) -> Vec<String> {
    let mut types = Vec::new();
    for line in payload.split(|b| *b == b'\n') {
        let mut line = line.trim_ascii();
        if line.is_empty() || line.starts_with(b"event:") {
            continue;
        }
        if let Some(rest) = line.strip_prefix(b"data:") {
            line = rest.trim_ascii();
        }
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        types.push(
            value
                .get("type")
                .map(|t| match t {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_default(),
        );
    }
    types
}

#[test]
fn rewrite_force_mapped_stream_chunk_codex_data_lines_without_newlines_finish_parses_completed() {
    let mut rewriter = StreamRewriter::new("gpt-5.4-fast");
    let lines = [
        r#"data: {"type":"response.created","response":{"model":"gpt-5.4"}}"#,
        r#"data: {"type":"response.in_progress","response":{"model":"gpt-5.4"}}"#,
        r#"data: {"type":"response.completed","response":{"model":"gpt-5.4","output":[]}}"#,
    ];
    let mut types = Vec::new();
    for ln in lines {
        let out = rewriter.rewrite_stream_chunk(ln.as_bytes());
        if !out.is_empty() {
            types.extend(parse_responses_ws_data_event_types(&out));
        }
    }
    let tail = rewriter.finish();
    if !tail.is_empty() {
        types.extend(parse_responses_ws_data_event_types(&tail));
    }
    assert!(
        types.iter().any(|t| t == "response.completed"),
        "missing response.completed; types={types:?}"
    );
}

#[test]
fn stream_rewriter_logged_once_and_rewritten_chunks_count() {
    let mut rewriter = StreamRewriter::new("gemini-3.8-flash");
    let chunks: [&[u8]; 4] = [
        b"data: {\"modelVersion\":\"gemini-3.8-flash-high\",\"text\":\"part 1\"}\n\n",
        b"data: {\"modelVersion\":\"gemini-3.8-flash-high\",\"text\":\"part 2\"}\n\n",
        b"data: {\"modelVersion\":\"gemini-3.8-flash-high\",\"text\":\"part 3\"}\n\n",
        b"data: [DONE]\n\n",
    ];

    let mut outputs = Vec::new();
    for c in chunks {
        let out = rewriter.rewrite_chunk(c);
        assert!(!out.is_empty(), "unexpected empty chunk output");
        outputs.push(text(&out));
    }
    let tail = rewriter.finish();

    // In place of rewrittenChunks == 3 and loggedPaths["modelVersion"].
    for (i, out) in outputs[..3].iter().enumerate() {
        assert!(
            out.contains(r#""modelVersion":"gemini-3.8-flash""#)
                && !out.contains("gemini-3.8-flash-high"),
            "chunk {i} = {out:?}, want modelVersion rewritten"
        );
    }
    // In place of loggedFinished: [DONE] passes and nothing is left.
    assert_eq!(outputs[3], "data: [DONE]\n\n");
    assert!(tail.is_empty(), "finish = {:?}, want nothing", text(&tail));
}
