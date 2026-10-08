// Ported from CLIProxyAPI internal/translator/interactions/claude/init.go and
// internal/translator/claude/interactions/init.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The Claude and Interactions translators' registrations: Claude Messages
//! clients to an Interactions upstream (`claude` → `interactions`), and
//! Interactions clients to a Claude upstream (`interactions` → `claude`).
//! Neither pair writes token counts.
//!
//! Deviations from upstream: none.

use std::sync::Arc;

use super::super::{checked, to_vec};
use crate::claude::interactions as claude_interactions;
use crate::interactions::claude as interactions_claude;
use crate::registry::{Format, Registry, ResponseTransform, StreamTranslator};

pub(super) fn register(registry: &Registry) {
    // internal/translator/interactions/claude/init.go
    registry.register(
        Format::CLAUDE,
        Format::INTERACTIONS,
        checked(|model, body, stream| {
            interactions_claude::convert_claude_request_to_interactions(model, &body, stream)
        }),
        ResponseTransform {
            stream: Some(Arc::new(|context| {
                Box::new(InteractionsToClaude(
                    interactions_claude::InteractionsToClaudeStream::new(context.model),
                ))
            })),
            non_stream: Some(Arc::new(|context, body| {
                let response =
                    interactions_claude::convert_interactions_response_to_claude_non_stream(
                        context.model,
                        body,
                    );
                Some(to_vec(&response))
            })),
            token_count: None,
        },
    );

    // internal/translator/claude/interactions/init.go
    registry.register(
        Format::INTERACTIONS,
        Format::CLAUDE,
        checked(|model, body, stream| {
            claude_interactions::convert_interactions_request_to_claude(model, &body, stream)
        }),
        ResponseTransform {
            stream: Some(Arc::new(|context| {
                Box::new(ClaudeToInteractions(
                    claude_interactions::ClaudeToInteractionsStream::new(context.model),
                ))
            })),
            non_stream: Some(Arc::new(|context, body| {
                let response =
                    claude_interactions::convert_claude_response_to_interactions_non_stream(
                        context.model,
                        body,
                    );
                Some(to_vec(&response))
            })),
            token_count: None,
        },
    );
}

/// Interactions → Claude Messages. Upstream returns one chunk per event.
struct InteractionsToClaude(interactions_claude::InteractionsToClaudeStream);

impl StreamTranslator for InteractionsToClaude {
    fn translate(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        self.0
            .translate(chunk)
            .into_iter()
            .map(String::into_bytes)
            .collect()
    }
}

/// Claude Messages → Interactions. Upstream returns one chunk per event.
struct ClaudeToInteractions(claude_interactions::ClaudeToInteractionsStream);

impl StreamTranslator for ClaudeToInteractions {
    fn translate(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        self.0
            .translate(chunk)
            .into_iter()
            .map(String::into_bytes)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    //! The registrations, through the registry. Not upstream's: upstream has
    //! no such tests.

    use serde_json::{Value, json};

    use crate::registry::{Format, Registry, ResponseContext};

    fn context<'a>(model: &'a str, original_request: &'a Value) -> ResponseContext<'a> {
        ResponseContext {
            model,
            original_request,
            request: &Value::Null,
        }
    }

    /// The event line of each chunk, checking that each holds one event.
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

    // Not upstream's.
    #[test]
    fn both_pairs_are_registered() {
        let registry = Registry::builtin();
        for (client, provider) in [
            (Format::CLAUDE, Format::INTERACTIONS),
            (Format::INTERACTIONS, Format::CLAUDE),
        ] {
            assert!(registry.has_request_transformer(&client, &provider));
            assert!(registry.has_stream_response_transformer(&client, &provider));
            assert!(registry.has_non_stream_response_transformer(&client, &provider));
        }
    }

    // Not upstream's.
    #[test]
    fn requests_go_through_in_both_directions() {
        let registry = Registry::builtin();
        let claude = json!({"messages": [{"role": "user", "content": "hello"}]});
        let request = registry.translate_request(
            &Format::CLAUDE,
            &Format::INTERACTIONS,
            "gemini-2.5-pro",
            claude,
            true,
        );
        assert_eq!(request["model"], "gemini-2.5-pro");
        assert_eq!(request["stream"], true);

        let interactions = json!({"input": "hello"});
        let request = registry.translate_request(
            &Format::INTERACTIONS,
            &Format::CLAUDE,
            "claude-sonnet-4-5",
            interactions,
            false,
        );
        assert_eq!(request["model"], "claude-sonnet-4-5");
        assert_eq!(
            request["messages"],
            json!([{"role": "user", "content": [{"type": "text", "text": "hello"}]}])
        );
    }

    // Not upstream's.
    #[test]
    fn interactions_streams_become_claude_events() {
        let registry = Registry::builtin();
        let original = json!({});
        let ctx = context("gemini-2.5-pro", &original);
        let mut stream = registry.response_stream(&Format::INTERACTIONS, &Format::CLAUDE, &ctx);
        let mut kinds = Vec::new();
        for line in [
            r#"data: {"interaction":{"id":"i1"},"event_type":"interaction.created"}"#,
            r#"data: {"index":0,"step":{"type":"model_output"},"event_type":"step.start"}"#,
            r#"data: {"index":0,"delta":{"type":"text","text":"hi"},"event_type":"step.delta"}"#,
            r#"data: {"index":0,"event_type":"step.stop"}"#,
            r#"data: {"interaction":{"id":"i1"},"event_type":"interaction.completed"}"#,
            "data: [DONE]",
        ] {
            kinds.extend(event_kinds(&stream.translate(line.as_bytes())));
        }
        assert_eq!(
            kinds.first().map(String::as_str),
            Some("event: message_start")
        );
        assert!(
            kinds
                .iter()
                .any(|kind| kind == "event: content_block_delta")
        );
        assert_eq!(
            kinds.last().map(String::as_str),
            Some("event: message_stop")
        );

        let whole = br#"{"id":"i1","status":"completed","steps":[{"type":"model_output","content":[{"type":"text","text":"hi"}]}]}"#;
        let response = registry
            .translate_non_stream(&Format::INTERACTIONS, &Format::CLAUDE, &ctx, whole.to_vec())
            .expect("a response");
        let response: Value = serde_json::from_slice(&response).expect("JSON");
        assert_eq!(response["type"], "message");
        assert_eq!(response["model"], "gemini-2.5-pro");
    }

    // Not upstream's.
    #[test]
    fn claude_streams_become_interactions_events() {
        let registry = Registry::builtin();
        let original = json!({});
        let ctx = context("claude-sonnet-4-5", &original);
        let mut stream = registry.response_stream(&Format::CLAUDE, &Format::INTERACTIONS, &ctx);
        let mut kinds = Vec::new();
        for line in [
            r#"data: {"type":"message_start","message":{"id":"msg_1","model":"claude-sonnet-4-5","usage":{"input_tokens":3}}}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
            r#"data: {"type":"message_stop"}"#,
        ] {
            kinds.extend(event_kinds(&stream.translate(line.as_bytes())));
        }
        assert_eq!(
            kinds.first().map(String::as_str),
            Some("event: interaction.created")
        );
        assert!(kinds.iter().any(|kind| kind == "event: step.delta"));
        assert!(
            kinds
                .iter()
                .any(|kind| kind == "event: interaction.completed")
        );

        let whole = br#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":1}}"#;
        let response = registry
            .translate_non_stream(&Format::CLAUDE, &Format::INTERACTIONS, &ctx, whole.to_vec())
            .expect("a response");
        let response: Value = serde_json::from_slice(&response).expect("JSON");
        assert_eq!(response["status"], "completed");
    }
}
