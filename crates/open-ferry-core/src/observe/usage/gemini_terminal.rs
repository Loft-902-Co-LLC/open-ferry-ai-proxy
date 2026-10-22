// Ported from CLIProxyAPI internal/runtime/executor/helps/gemini_stream_terminal.go
// (IsGeminiTerminalStreamChunk) (v8.0.38, MIT).
// Extended for Interactions completion, including native SSE event names
// and the legacy finish event accepted by the Interactions translators.
// https://github.com/router-for-me/CLIProxyAPI

//! Successful terminal output returned by the shared Gemini/Vertex stream.
//! This reads translated chunks, never the provider's raw traffic.
//! JSON parsing shares the deviations documented in [`super::json`].

use super::json::{self, Doc};
use super::parse::json_payload;

pub(super) fn is_terminal_chunk(chunk: &[u8]) -> bool {
    // A successful type needs its literal value, a finish-reason path its
    // key, and native SSE completion its event name. JSON escaping can spell
    // any of them differently, so every backslash falls through to parsing.
    (chunk.contains(&b'\\')
        || [
            b"[DONE]".as_slice(),
            b"finish",
            b"response.completed",
            b"response.incomplete",
            b"response.done",
            b"message_stop",
            b"interaction.completed",
        ]
        .iter()
        .any(|needle| chunk.windows(needle.len()).any(|part| part == *needle)))
        && parse_terminal_chunk(chunk)
}

fn parse_terminal_chunk(chunk: &[u8]) -> bool {
    let mut interactions_event = false;
    chunk.split(|&b| b == b'\n').any(|line| {
        let trimmed = json::trim_space(line);
        if trimmed.is_empty() {
            interactions_event = false;
        }
        if let Some(event) = trimmed.strip_prefix(b"event:") {
            interactions_event = is_interactions_terminal(json::trim_space(event));
        }
        if trimmed == b"data: [DONE]" || trimmed == b"[DONE]" {
            return true;
        }
        let Some(payload) = json_payload(line).filter(|payload| json::valid(payload)) else {
            return false;
        };
        let doc = Doc::parse(payload);
        let interaction_type = doc.get("type").string();
        let event_type = doc.get("event_type").string();
        if [interaction_type.as_ref(), event_type.as_ref()]
            .iter()
            .any(|kind| is_interactions_terminal(kind.as_bytes()))
        {
            return true;
        }
        if interactions_event
            && ![interaction_type.as_ref(), event_type.as_ref()]
                .iter()
                .any(|kind| matches!(*kind, "interaction.failed" | "response.failed" | "error"))
        {
            return true;
        }
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

fn is_interactions_terminal(kind: &[u8]) -> bool {
    matches!(kind, b"interaction.completed" | b"finish")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Upstream has no helper matrix: cover every terminal form it accepts,
    // including raw JSON, SSE, whitespace, envelopes and multiple lines.
    fn terminal_fixtures() -> Vec<(String, bool)> {
        let mut fixtures = Vec::new();
        for payload in [
            "[DONE]",
            r#"{"type":"response.completed"}"#,
            r#"{"type":"response.incomplete"}"#,
            r#"{"type":"response.done"}"#,
            r#"{"type":"message_stop"}"#,
            r#"{"type":"interaction.completed"}"#,
            r#"{"event_type":"interaction.completed"}"#,
            r#"{"event_type":"finish"}"#,
            r#"{"\u0065vent_type":"interaction.\u0063ompleted"}"#,
            r#"{"\u0074ype":"response.completed"}"#,
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
                fixtures.push((chunk, true));
            }
        }
        for chunk in [
            "event: interaction.completed\ndata: {\"status\":\"completed\"}\n\n",
            " event:  interaction.completed\r\n data: {}\r\n\r\n",
            "event: finish\ndata: {}\n\n",
        ] {
            fixtures.push((chunk.to_owned(), true));
        }
        // gjson coerces non-string finish reasons; only the first candidate counts.
        fixtures.push((r#"{"choices":[{"finish_reason":false}]}"#.to_owned(), true));
        for chunk in [
            "",
            "event: response.completed\n\n",
            "data: [DONE] trailing",
            r#"data: {"type":"response.failed"}"#,
            r#"data: {"type":"error"}"#,
            r#"data: {"event_type":"interaction.failed"}"#,
            r#"data: {"type":"interaction.failed"}"#,
            r#"data: {"event_type":"response.failed"}"#,
            "event: interaction.completed\n\n",
            "event: interaction.completed\ndata: malformed\n\n",
            "event: interaction.completed\n\ndata: {}\n\n",
            "event: interaction.failed\ndata: {}\n\n",
            "event: error\ndata: {}\n\n",
            "event: interaction.completed\ndata: {\"type\":\"interaction.failed\"}\n\n",
            r#"data: {"choices":[{"finish_reason":null}]}"#,
            r#"data: {"candidates":[{"finishReason":"  "}]}"#,
            r#"data: {"candidates":[{}, {"finishReason":"STOP"}]}"#,
            r#"data: {"promptFeedback":{"blockReason":"SAFETY"}}"#,
            r#"data: {"type":"response.completed""#,
        ] {
            fixtures.push((chunk.to_owned(), false));
        }
        fixtures
    }

    #[test]
    fn recognizes_translated_terminal_forms() {
        for (chunk, expected) in terminal_fixtures() {
            assert_eq!(is_terminal_chunk(chunk.as_bytes()), expected, "{chunk}");
        }
    }

    // Compare the fast path with the unfiltered parser, including keys and
    // values whose JSON escapes would defeat a literal-key-only filter.
    #[test]
    fn terminal_prefilter_matches_unfiltered_parser() {
        use serde_json::json;

        let mut cases: Vec<_> = terminal_fixtures()
            .into_iter()
            .map(|(chunk, _)| chunk.into_bytes())
            .collect();
        for key in [
            "type",
            "event_type",
            "finish_reason",
            "finishReason",
            "text",
        ] {
            for value in [
                json!("response.completed"),
                json!("response.incomplete"),
                json!("response.done"),
                json!("message_stop"),
                json!("interaction.completed"),
                json!("finish"),
                json!("interaction.failed"),
                json!("response.failed"),
                json!("error"),
                json!("STOP"),
                json!("  "),
                json!(null),
                json!(false),
                json!(0),
                json!({}),
            ] {
                for root in [
                    json!({(key): value}),
                    json!({"choices":[{(key): value}]}),
                    json!({"candidates":[{(key): value}]}),
                    json!({"response":{"choices":[{(key): value}],"candidates":[{(key): value}]}}),
                ] {
                    let plain = root.to_string();
                    let escaped_key = plain.replace(
                        &format!("\"{key}\""),
                        &format!("\"\\u{:04x}{}\"", key.as_bytes()[0], &key[1..]),
                    );
                    let escaped_value = plain.replace("completed", "\\u0063ompleted");
                    for payload in [plain, escaped_key, escaped_value] {
                        for event in [
                            "",
                            "interaction.completed",
                            "finish",
                            "interaction.failed",
                            "error",
                        ] {
                            for chunk in [
                                payload.clone(),
                                format!("event: {event}\ndata: {payload}\n\n"),
                            ] {
                                cases.push(chunk.into_bytes());
                            }
                        }
                    }
                }
            }
        }
        // Arbitrary non-JSON bytes and truncated prefixes exercise rejection.
        for seed in 0..256_u16 {
            cases.push(
                (0..80_u16)
                    .map(|index| (seed.wrapping_mul(37).wrapping_add(index * 13)) as u8)
                    .collect(),
            );
        }
        for chunk in cases {
            assert_eq!(
                is_terminal_chunk(&chunk),
                parse_terminal_chunk(&chunk),
                "{}",
                String::from_utf8_lossy(&chunk)
            );
            for end in [chunk.len() / 2, chunk.len().saturating_sub(1)] {
                assert_eq!(
                    is_terminal_chunk(&chunk[..end]),
                    parse_terminal_chunk(&chunk[..end]),
                    "truncated {chunk:?}"
                );
            }
        }
    }
}
