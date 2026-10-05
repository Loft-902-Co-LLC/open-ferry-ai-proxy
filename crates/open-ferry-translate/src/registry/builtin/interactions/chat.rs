// Ported from CLIProxyAPI internal/translator/openai/interactions/chat-completions/init.go
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The Chat Completions and Interactions translators' registrations: Chat
//! Completions clients to an Interactions upstream (`openai` →
//! `interactions`), and Interactions clients to a Chat Completions upstream
//! (`interactions` → `openai`).
//!
//! Deviations from upstream: none.

use std::sync::Arc;

use super::super::{events, to_vec};
use super::parse;
use crate::openai::interactions::chat_completions as chat;
use crate::registry::{Format, Registry, ResponseTransform, StreamTranslator};

pub(super) fn register(registry: &Registry) {
    registry.register(
        Format::OPENAI,
        Format::INTERACTIONS,
        Some(Arc::new(|model, body, stream| {
            chat::convert_openai_request_to_interactions(model, &body, stream)
        })),
        ResponseTransform {
            stream: Some(Arc::new(|context| {
                Box::new(InteractionsToChat(chat::InteractionsToOpenAIStream::new(
                    context.model,
                )))
            })),
            non_stream: Some(Arc::new(|context, body| {
                let response = chat::convert_interactions_response_to_openai_non_stream(
                    context.model,
                    &parse(body),
                );
                Some(to_vec(&response))
            })),
            token_count: None,
        },
    );
    registry.register(
        Format::INTERACTIONS,
        Format::OPENAI,
        Some(Arc::new(|model, body, stream| {
            chat::convert_interactions_request_to_openai(model, &body, stream)
        })),
        ResponseTransform {
            stream: Some(Arc::new(|context| {
                Box::new(ChatToInteractions(chat::OpenAIToInteractionsStream::new(
                    context.model,
                )))
            })),
            non_stream: Some(Arc::new(|context, body| {
                let response = chat::convert_openai_response_to_interactions_non_stream(
                    context.model,
                    &parse(body),
                );
                Some(to_vec(&response))
            })),
            token_count: None,
        },
    );
}

/// Interactions → Chat Completions. Upstream returns one chunk per Chat
/// Completions chunk.
struct InteractionsToChat(chat::InteractionsToOpenAIStream);

impl StreamTranslator for InteractionsToChat {
    fn translate(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        self.0.translate(chunk).iter().map(to_vec).collect()
    }
}

/// Chat Completions → Interactions. Upstream returns one chunk per event.
struct ChatToInteractions(chat::OpenAIToInteractionsStream);

impl StreamTranslator for ChatToInteractions {
    fn translate(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        events(&self.0.translate(chunk))
    }
}

#[cfg(test)]
mod tests {
    //! The registrations, through the registry. Not ported: upstream has no
    //! such tests.
    //!
    //! Dropped or changed tests: none.

    use serde_json::{Value, json};

    use crate::registry::{Format, Registry, ResponseContext};

    fn context(original_request: &Value) -> ResponseContext<'_> {
        ResponseContext {
            model: "gpt-test",
            original_request,
            request: &Value::Null,
        }
    }

    #[test]
    fn both_pairs_are_registered() {
        let registry = Registry::builtin();
        for (client, provider) in [
            (Format::OPENAI, Format::INTERACTIONS),
            (Format::INTERACTIONS, Format::OPENAI),
        ] {
            assert!(registry.has_request_transformer(&client, &provider));
            assert!(registry.has_stream_response_transformer(&client, &provider));
            assert!(registry.has_non_stream_response_transformer(&client, &provider));
        }

        let request = registry.translate_request(
            &Format::OPENAI,
            &Format::INTERACTIONS,
            "gemini-3-flash",
            json!({"messages": [{"role": "user", "content": "hi"}]}),
            true,
        );
        assert_eq!(
            request,
            json!({
                "model": "gemini-3-flash",
                "input": [{"type": "user_input", "content": [{"type": "text", "text": "hi"}]}],
                "stream": true
            })
        );

        let request = registry.translate_request(
            &Format::INTERACTIONS,
            &Format::OPENAI,
            "gpt-test",
            json!({"input": "hi"}),
            false,
        );
        assert_eq!(
            request,
            json!({"model": "gpt-test", "messages": [{"role": "user", "content": "hi"}]})
        );
    }

    #[test]
    fn interactions_streams_give_a_chunk_per_chat_chunk() {
        let registry = Registry::builtin();
        let original = json!({});
        let ctx = context(&original);
        let mut stream = registry.response_stream(&Format::INTERACTIONS, &Format::OPENAI, &ctx);
        let chunks = stream.translate(
            br#"data: {"event_type":"step.delta","index":0,"delta":{"type":"text","text":"hi"}}"#,
        );
        let chunks: Vec<Value> = chunks
            .iter()
            .map(|chunk| serde_json::from_slice(chunk).expect("a JSON chunk"))
            .collect();
        assert_eq!(chunks.len(), 2, "{chunks:?}");
        assert_eq!(
            chunks[0]["choices"][0]["delta"],
            json!({"role": "assistant"})
        );
        assert_eq!(chunks[1]["choices"][0]["delta"], json!({"content": "hi"}));
        assert!(stream.finish().is_empty());
    }

    #[test]
    fn chat_streams_give_a_chunk_per_event() {
        let registry = Registry::builtin();
        let original = json!({});
        let ctx = context(&original);
        let mut stream = registry.response_stream(&Format::OPENAI, &Format::INTERACTIONS, &ctx);
        let chunks =
            stream.translate(br#"data: {"id":"c1","choices":[{"delta":{"content":"hi"}}]}"#);
        let kinds: Vec<&str> = chunks
            .iter()
            .map(|chunk| {
                let text = std::str::from_utf8(chunk).expect("UTF-8");
                assert!(text.ends_with("\n\n"), "{text}");
                assert_eq!(text.matches("event: ").count(), 1, "{text}");
                text.lines().next().expect("event line")
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "event: interaction.created",
                "event: interaction.status_update",
                "event: step.start",
                "event: step.delta",
            ]
        );
    }

    #[test]
    fn whole_responses_are_translated() {
        let registry = Registry::builtin();
        let original = json!({});
        let ctx = context(&original);
        let body = br#"{"id":"i1","steps":[{"type":"model_output","content":"hi"}]}"#.to_vec();
        let out = registry
            .translate_non_stream(&Format::INTERACTIONS, &Format::OPENAI, &ctx, body)
            .expect("a response");
        let out: Value = serde_json::from_slice(&out).expect("JSON");
        assert_eq!(out["id"], "i1");
        assert_eq!(out["choices"][0]["message"]["content"], "hi");

        let body = br#"{"id":"c1","choices":[{"message":{"content":"hi"}}]}"#.to_vec();
        let out = registry
            .translate_non_stream(&Format::OPENAI, &Format::INTERACTIONS, &ctx, body)
            .expect("a response");
        let out: Value = serde_json::from_slice(&out).expect("JSON");
        assert_eq!(out["id"], "c1");
        assert_eq!(
            out["steps"],
            json!([{"type": "model_output", "content": [{"type": "text", "text": "hi"}]}])
        );
    }

    // Not upstream's: a whole response's tool call arguments keep each
    // number as written, as upstream copies them (checked with Go).
    #[test]
    fn whole_responses_keep_numbers_as_written() {
        let registry = Registry::builtin();
        let original = json!({});
        let ctx = context(&original);
        let spelled = r#"{"x":-0,"y":1E20,"z":[1e5,0.10]}"#;

        let body = format!(
            r#"{{"id":"i1","steps":[{{"type":"function_call","id":"c1","name":"f","arguments":{spelled}}}]}}"#
        );
        let out = registry
            .translate_non_stream(&Format::INTERACTIONS, &Format::OPENAI, &ctx, body.into())
            .expect("a response");
        let out: Value = serde_json::from_slice(&out).expect("JSON");
        assert_eq!(
            out["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            spelled
        );

        let body = format!(
            r#"{{"id":"c1","choices":[{{"message":{{"role":"assistant","tool_calls":[{{"id":"t1","type":"function","function":{{"name":"f","arguments":{spelled}}}}}]}},"finish_reason":"tool_calls"}}]}}"#
        );
        let out = registry
            .translate_non_stream(&Format::OPENAI, &Format::INTERACTIONS, &ctx, body.into())
            .expect("a response");
        let out = String::from_utf8(out).expect("UTF-8");
        assert!(out.contains(&format!(r#""arguments":{spelled}"#)), "{out}");
    }
}
