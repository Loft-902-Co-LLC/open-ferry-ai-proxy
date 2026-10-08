// Ported from CLIProxyAPI internal/translator/gemini/gemini/gemini_gemini_response.go
// (PassthroughGeminiResponseStream, PassthroughGeminiResponseNonStream and GeminiTokenCount)
// and internal/translator/common/bytes.go (GeminiTokenCountJSON) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Gemini responses → Gemini responses.
//!
//! Each line of a stream passes through, a `data:` line's payload without the
//! prefix, except `[DONE]`. Unlike the Chat Completions passthrough, lines
//! after `[DONE]` still pass.
//!
//! Deviations from upstream: none.

use serde_json::{Value, json};

use crate::go;

/// The chunk to send the client for one line of a Gemini stream: a `data:`
/// line's payload, trimmed, or any other line as it is. Gives nothing for
/// `[DONE]`. The chunk may be empty.
pub fn passthrough_gemini_response_stream(line: &[u8]) -> Option<&[u8]> {
    let line = match line.strip_prefix(b"data:") {
        Some(data) => go::trim_space(data),
        None => line,
    };
    (line != b"[DONE]").then_some(line)
}

/// Passes a whole Gemini response through as it is.
pub fn passthrough_gemini_response_non_stream(body: &[u8]) -> &[u8] {
    body
}

/// Converts a `countTokens` result into a Gemini `countTokens` response body.
///
/// Upstream builds this with `GeminiTokenCountJSON`, which another part of
/// this port owns; this is a copy of it.
pub fn gemini_token_count(count: i64) -> Value {
    json!({
        "totalTokens": count,
        "promptTokensDetails": [{ "modality": "TEXT", "tokenCount": count }],
    })
}

#[cfg(test)]
mod tests {
    //! Upstream has no tests of gemini_gemini_response.go.

    use super::*;

    #[test]
    fn stream_strips_data_prefix_and_drops_done() {
        for (line, want) in [
            (&b"data: {\"a\":1}  "[..], Some(&b"{\"a\":1}"[..])),
            (b"data:{\"a\":1}", Some(b"{\"a\":1}")),
            (b"{\"a\":1}", Some(b"{\"a\":1}")),
            (b"  {\"a\":1}  ", Some(b"  {\"a\":1}  ")),
            (b"data: [DONE]", None),
            (b"[DONE]", None),
            (b" [DONE]", Some(b" [DONE]")),
            (b"data:", Some(b"")),
            (b"", Some(b"")),
            (b"event: x", Some(b"event: x")),
        ] {
            assert_eq!(passthrough_gemini_response_stream(line), want, "{line:?}");
        }
    }

    #[test]
    fn stream_keeps_passing_after_done() {
        assert_eq!(passthrough_gemini_response_stream(b"data: [DONE]"), None);
        assert_eq!(
            passthrough_gemini_response_stream(b"data: {}"),
            Some(&b"{}"[..])
        );
    }

    #[test]
    fn non_stream_passes_through() {
        let body = b"{\"candidates\":[]}\n";
        assert_eq!(passthrough_gemini_response_non_stream(body), body);
    }

    #[test]
    fn token_count_matches_upstream_bytes() {
        assert_eq!(
            gemini_token_count(42).to_string(),
            r#"{"totalTokens":42,"promptTokensDetails":[{"modality":"TEXT","tokenCount":42}]}"#
        );
        assert_eq!(
            gemini_token_count(-1).to_string(),
            r#"{"totalTokens":-1,"promptTokensDetails":[{"modality":"TEXT","tokenCount":-1}]}"#
        );
    }
}
