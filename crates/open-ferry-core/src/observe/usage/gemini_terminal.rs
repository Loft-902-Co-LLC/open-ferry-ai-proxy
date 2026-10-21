// Ported from CLIProxyAPI internal/runtime/executor/helps/gemini_stream_terminal.go
// (IsGeminiTerminalStreamChunk) (v8.0.38, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Successful terminal output delivered by the shared Gemini/Vertex stream.
//! This reads translated chunks, never the provider's raw traffic.
//! JSON parsing shares the deviations documented in [`super::json`].

use super::json::{self, Doc};
use super::parse::json_payload;

pub(super) fn is_terminal_chunk(chunk: &[u8]) -> bool {
    chunk.split(|&b| b == b'\n').any(|line| {
        let trimmed = json::trim_space(line);
        if trimmed == b"data: [DONE]" || trimmed == b"[DONE]" {
            return true;
        }
        let Some(payload) = json_payload(line).filter(|payload| json::valid(payload)) else {
            return false;
        };
        let doc = Doc::parse(payload);
        if matches!(
            doc.get("type").string().as_ref(),
            "response.completed" | "response.incomplete" | "response.done" | "message_stop"
        ) {
            return true;
        }
        [
            "choices.0.finish_reason",
            "response.choices.0.finish_reason",
            "candidates.0.finishReason",
            "response.candidates.0.finishReason",
        ]
        .iter()
        .any(|path| !doc.get(path).string().trim().is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Upstream has no helper matrix: cover every terminal form it accepts,
    // including raw JSON, SSE, whitespace, envelopes and multiple lines.
    #[test]
    fn recognizes_translated_terminal_forms() {
        for payload in [
            "[DONE]",
            r#"{"type":"response.completed"}"#,
            r#"{"type":"response.incomplete"}"#,
            r#"{"type":"response.done"}"#,
            r#"{"type":"message_stop"}"#,
            r#"{"choices":[{"finish_reason":"stop"}]}"#,
            r#"{"response":{"choices":[{"finish_reason":"length"}]}}"#,
            r#"{"candidates":[{"finishReason":"STOP"}]}"#,
            r#"{"response":{"candidates":[{"finishReason":"MAX_TOKENS"}]}}"#,
        ] {
            for chunk in [
                payload.to_owned(),
                format!("  data: {payload}\r\n\n"),
                format!("event: ignored\ndata: {{}}\ndata: {payload}\n\n"),
            ] {
                assert!(is_terminal_chunk(chunk.as_bytes()), "{chunk}");
            }
        }
        // gjson coerces non-string finish reasons; only the first candidate counts.
        assert!(is_terminal_chunk(
            br#"{"choices":[{"finish_reason":false}]}"#
        ));
        for chunk in [
            "",
            "event: response.completed\n\n",
            "data: [DONE] trailing",
            r#"data: {"type":"response.failed"}"#,
            r#"data: {"type":"error"}"#,
            r#"data: {"choices":[{"finish_reason":null}]}"#,
            r#"data: {"candidates":[{"finishReason":"  "}]}"#,
            r#"data: {"candidates":[{}, {"finishReason":"STOP"}]}"#,
            r#"data: {"promptFeedback":{"blockReason":"SAFETY"}}"#,
            r#"data: {"type":"response.completed""#,
        ] {
            assert!(!is_terminal_chunk(chunk.as_bytes()), "{chunk}");
        }
    }
}
