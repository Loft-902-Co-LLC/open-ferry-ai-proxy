// Ported from CLIProxyAPI sdk/cliproxy/auth/codex_forcemap_ws_forward_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A force-mapped Codex stream that arrives one SSE line per chunk, as the
//! Responses WebSocket forwards it, still hands on `response.completed`.
//!
//! Upstream's `rewriteForceMappedStreamChunk(r, chunk)` is
//! `StreamRewriter::rewrite_stream_chunk` and
//! `finishForceMappedStreamChunks(r)` is `StreamRewriter::finish`.
//!
//! Deviations from upstream:
//! - None.

use serde_json::Value;

#[allow(unused_imports)] // Nothing from the harness is needed here.
use super::support::*;
use crate::manager::rewrite::{StreamRewriter, normalize_glued_sse_events};

/// The `type` of each JSON `data:` line in the forwarded chunks (upstream's
/// `parseWSDataEventTypesFromForwardedChunks`).
fn parse_ws_data_event_types_from_forwarded_chunks(forwarded: Vec<Vec<u8>>) -> Vec<String> {
    let mut types = Vec::new();
    for ch in forwarded {
        let ch = normalize_glued_sse_events(ch);
        for ln in ch.split(|b| *b == b'\n') {
            let Some(rest) = ln.trim_ascii().strip_prefix(b"data:") else {
                continue;
            };
            if let Ok(value) = serde_json::from_slice::<Value>(rest.trim_ascii()) {
                types.push(match value.get("type") {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                    None => String::new(),
                });
            }
        }
    }
    types
}

/// Upstream's `replayCodexForceMapLines`.
fn replay_codex_force_map_lines(lines: &[&str]) -> Vec<String> {
    let mut r = StreamRewriter::new("gpt-5.4-fast");
    let mut forwarded = Vec::new();
    for line in lines {
        let out = r.rewrite_stream_chunk(line.as_bytes());
        if !out.is_empty() {
            forwarded.push(out);
        }
    }
    let tail = r.finish();
    if !tail.is_empty() {
        forwarded.push(tail);
    }
    parse_ws_data_event_types_from_forwarded_chunks(forwarded)
}

#[test]
fn codex_force_map_per_line_sse_forwards_completed() {
    let lines = [
        "event: response.created",
        r#"data: {"type":"response.created","response":{"model":"gpt-5.4"}}"#,
        "event: response.output_text.delta",
        r#"data: {"type":"response.output_text.delta","delta":"OK"}"#,
        "event: response.completed",
        r#"data: {"type":"response.completed","response":{"model":"gpt-5.4","output":[]}}"#,
    ];
    let types = replay_codex_force_map_lines(&lines);
    assert!(
        types.iter().any(|t| t == "response.completed"),
        "missing response.completed, types={types:?}"
    );
}

#[test]
fn rewrite_force_mapped_stream_chunk_fallback_when_pending_buffers_event() {
    let mut r = StreamRewriter::new("gpt-5.4-fast");
    let _ = r.rewrite_stream_chunk(b"event: response.completed");
    let out = r.rewrite_stream_chunk(
        br#"data: {"type":"response.completed","response":{"model":"gpt-5.4","output":[]}}"#,
    );
    if out.is_empty() {
        let tail = String::from_utf8_lossy(&r.finish()).into_owned();
        assert!(
            tail.contains("response.completed"),
            "expected completed in tail, got {tail:?}"
        );
        return;
    }
    let out = String::from_utf8_lossy(&out);
    assert!(out.contains("response.completed"), "out={out:?}");
}
