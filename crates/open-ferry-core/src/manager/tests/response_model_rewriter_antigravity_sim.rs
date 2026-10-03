// Ported from CLIProxyAPI sdk/cliproxy/auth/response_model_rewriter_antigravity_sim_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A force-mapped stream of the Responses events upstream's Gemini
//! translator makes from a live Antigravity reply still ends with
//! `response.completed` after the rewriter.
//!
//! Upstream's `rewriteForceMappedStreamChunk(rewriter, chunk)` is
//! `StreamRewriter::rewrite_stream_chunk` and
//! `finishForceMappedStreamChunks(rewriter)` is `StreamRewriter::finish`.
//!
//! Deviations from upstream:
//! - Upstream builds its chunks by running two live Antigravity replies
//!   through its Gemini-to-Responses translator, which the port doesn't
//!   have. [`ANTIGRAVITY_LIVE_SSE_CHUNKS`] is that translator's output,
//!   captured from upstream v8.0.10 (11 chunks; `created_at` is the time of
//!   the capture).
//! - `TestAntigravityTranslatorEmitsCompletedWithoutRewriter` is dropped: it
//!   checks only that translator, which isn't ported and isn't the
//!   manager's.
//! - `TestRewriteForceMappedStreamChunk_AntigravityGluedEventFramesFlushCompleted`
//!   drops its `t.Log` on the rewriter's pending buffer, which is logging
//!   only.

#[allow(unused_imports)] // Nothing from the harness is needed here.
use super::support::*;
use crate::manager::rewrite::StreamRewriter;

/// Upstream's `antigravityLiveSSEChunks`, captured (see the module docs).
const ANTIGRAVITY_LIVE_SSE_CHUNKS: [&str; 11] = [
    concat!(
        "event: response.created\n",
        "data: ",
        r#"{"type":"response.created","sequence_number":1,"response":{"id":"resp_tjVCavaJBYjgz7IP-NnfSQ","object":"response","created_at":1791011366,"status":"in_progress","background":false,"error":null,"output":[],"model":"gemini-3.5-flash"}}"#,
        "\n\n"
    ),
    concat!(
        "event: response.in_progress\n",
        "data: ",
        r#"{"type":"response.in_progress","sequence_number":2,"response":{"id":"resp_tjVCavaJBYjgz7IP-NnfSQ","object":"response","created_at":1791011366,"status":"in_progress","output":[],"model":"gemini-3.5-flash"}}"#,
        "\n\n"
    ),
    concat!(
        "event: response.output_item.added\n",
        "data: ",
        r#"{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"msg_resp_tjVCavaJBYjgz7IP-NnfSQ_0","type":"message","status":"in_progress","content":[],"role":"assistant"}}"#,
        "\n\n"
    ),
    concat!(
        "event: response.content_part.added\n",
        "data: ",
        r#"{"type":"response.content_part.added","sequence_number":4,"item_id":"msg_resp_tjVCavaJBYjgz7IP-NnfSQ_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}}"#,
        "\n\n"
    ),
    concat!(
        "event: response.output_text.delta\n",
        "data: ",
        r#"{"type":"response.output_text.delta","sequence_number":5,"item_id":"msg_resp_tjVCavaJBYjgz7IP-NnfSQ_0","output_index":0,"content_index":0,"delta":"OK","logprobs":[]}"#,
        "\n\n"
    ),
    concat!(
        "event: response.output_text.done\n",
        "data: ",
        r#"{"type":"response.output_text.done","sequence_number":6,"item_id":"msg_resp_tjVCavaJBYjgz7IP-NnfSQ_0","output_index":0,"content_index":0,"text":"OK","logprobs":[]}"#,
        "\n\n"
    ),
    concat!(
        "event: response.content_part.done\n",
        "data: ",
        r#"{"type":"response.content_part.done","sequence_number":7,"item_id":"msg_resp_tjVCavaJBYjgz7IP-NnfSQ_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"OK"}}"#,
        "\n\n"
    ),
    concat!(
        "event: response.output_item.done\n",
        "data: ",
        r#"{"type":"response.output_item.done","sequence_number":8,"output_index":0,"item":{"id":"msg_resp_tjVCavaJBYjgz7IP-NnfSQ_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"OK"}],"role":"assistant"}}"#,
        "\n\n"
    ),
    concat!(
        "event: response.output_item.added\n",
        "data: ",
        r#"{"type":"response.output_item.added","sequence_number":9,"output_index":1,"item":{"id":"rs_resp_tjVCavaJBYjgz7IP-NnfSQ_detached_after_1","type":"reasoning","status":"in_progress","encrypted_content":"cpa-gemini-responses-carrier-v1:previous:text:c2ln","summary":[]}}"#,
        "\n\n"
    ),
    concat!(
        "event: response.output_item.done\n",
        "data: ",
        r#"{"type":"response.output_item.done","sequence_number":10,"output_index":1,"item":{"id":"rs_resp_tjVCavaJBYjgz7IP-NnfSQ_detached_after_1","type":"reasoning","encrypted_content":"cpa-gemini-responses-carrier-v1:previous:text:c2ln","summary":[]}}"#,
        "\n\n"
    ),
    concat!(
        "event: response.completed\n",
        "data: ",
        r#"{"type":"response.completed","sequence_number":11,"response":{"id":"resp_tjVCavaJBYjgz7IP-NnfSQ","object":"response","created_at":1791011366,"status":"completed","background":false,"error":null,"model":"gemini-3.5-flash","output":[{"id":"msg_resp_tjVCavaJBYjgz7IP-NnfSQ_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"OK"}],"role":"assistant"},{"id":"rs_resp_tjVCavaJBYjgz7IP-NnfSQ_detached_after_1","type":"reasoning","encrypted_content":"cpa-gemini-responses-carrier-v1:previous:text:c2ln","summary":[]}],"usage":{"input_tokens":21,"input_tokens_details":{"cached_tokens":0},"output_tokens":110,"output_tokens_details":{"reasoning_tokens":109},"total_tokens":131}}}"#,
        "\n\n"
    ),
];

/// Whether a `data:` line, or the whole payload, is a `response.completed`
/// event (upstream's `parseCompletedFromSSE`).
fn parse_completed_from_sse(payload: &[u8]) -> bool {
    if payload.is_empty() {
        return false;
    }
    let is_completed = |json: &str| {
        serde_json::from_str::<serde_json::Value>(json)
            .ok()
            .and_then(|value| value.get("type")?.as_str().map(str::to_owned))
            .is_some_and(|t| t == "response.completed")
    };
    let text = String::from_utf8_lossy(payload);
    for line in text.split('\n') {
        let Some(rest) = line.trim().strip_prefix("data:") else {
            continue;
        };
        if is_completed(rest.trim()) {
            return true;
        }
    }
    let trim = text.trim();
    trim.starts_with('{') && is_completed(trim)
}

/// The first `n` bytes of `s`, for failure messages (upstream's `trunc`).
fn trunc(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_owned();
    }
    let mut end = n;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

/// The chunks through a fresh rewriter, then its tail.
fn rewrite_all(rewriter: &mut StreamRewriter) -> Vec<u8> {
    let mut out = Vec::new();
    for ch in ANTIGRAVITY_LIVE_SSE_CHUNKS {
        out.extend(rewriter.rewrite_stream_chunk(ch.as_bytes()));
    }
    out.extend(rewriter.finish());
    out
}

#[test]
fn rewrite_force_mapped_stream_chunk_antigravity_translator_event_chunks_preserves_completed() {
    let mut rewriter = StreamRewriter::new("gemini-3.5-flash");
    let out = rewrite_all(&mut rewriter);
    assert!(
        parse_completed_from_sse(&out),
        "rewriter output missing response.completed; preview={:?}",
        trunc(&String::from_utf8_lossy(&out), 400)
    );
}

#[test]
fn rewrite_force_mapped_stream_chunk_antigravity_glued_event_frames_flush_completed() {
    let mut rewriter = StreamRewriter::new("gemini-3.5-flash");
    let out = rewrite_all(&mut rewriter);
    assert!(
        parse_completed_from_sse(&out),
        "expected completed after glued frames flush; preview={:?}",
        trunc(&String::from_utf8_lossy(&out), 400)
    );
}
