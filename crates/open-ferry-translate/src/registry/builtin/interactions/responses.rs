// Ported from CLIProxyAPI internal/translator/openai/interactions/responses/init.go
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The OpenAI Responses and Interactions translators' registrations:
//! Responses clients to an Interactions upstream (`openai-response` →
//! `interactions`), and Interactions clients to a Responses upstream
//! (`interactions` → `openai-response`). Neither pair writes token counts.
//!
//! Deviations from upstream: none.

use std::error::Error;
use std::sync::Arc;

use super::super::{events, to_vec};
use crate::openai::interactions::responses as openai_interactions;
use crate::registry::{Format, Registry, ResponseTransform, StreamTranslator};

pub(super) fn register(registry: &Registry) {
    // internal/translator/openai/interactions/responses/init.go
    registry.register(
        Format::OPENAI_RESPONSE,
        Format::INTERACTIONS,
        Some(Arc::new(|model, body, stream| {
            openai_interactions::convert_openai_responses_request_to_interactions(
                model, &body, stream,
            )
        })),
        ResponseTransform {
            stream: Some(Arc::new(|context| {
                Box::new(InteractionsToResponses(
                    openai_interactions::InteractionsToOpenAIResponsesStream::new(
                        context.model,
                        context.original_request,
                        context.request,
                    ),
                ))
            })),
            non_stream: Some(Arc::new(|context, body| {
                openai_interactions::convert_interactions_response_to_openai_responses_non_stream(
                    context.model,
                    context.original_request,
                    context.request,
                    body,
                )
                .as_ref()
                .map(to_vec)
            })),
            token_count: None,
        },
    );
    registry.register(
        Format::INTERACTIONS,
        Format::OPENAI_RESPONSE,
        Some(Arc::new(|model, body, stream| {
            openai_interactions::convert_interactions_request_to_openai_responses(
                model, &body, stream,
            )
        })),
        ResponseTransform {
            stream: Some(Arc::new(|context| {
                Box::new(ResponsesToInteractions(
                    openai_interactions::OpenAIResponsesToInteractionsStream::new(context.model),
                ))
            })),
            non_stream: Some(Arc::new(|context, body| {
                let response =
                    openai_interactions::convert_openai_responses_response_to_interactions_non_stream(
                        context.model,
                        body,
                    );
                Some(to_vec(&response))
            })),
            token_count: None,
        },
    );
}

/// Interactions → OpenAI Responses. Upstream returns one chunk per event,
/// and `data: [DONE]`, with no line break after it, as a chunk of its own.
struct InteractionsToResponses(openai_interactions::InteractionsToOpenAIResponsesStream);

impl StreamTranslator for InteractionsToResponses {
    fn translate(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        events(&self.0.translate(chunk))
    }

    fn finish(&mut self) -> Vec<Vec<u8>> {
        events(&self.0.finalize_tool_input())
    }

    fn tool_input_error(&self) -> Option<&(dyn Error + 'static)> {
        self.0.tool_input_error()
    }
}

/// OpenAI Responses → Interactions. Upstream returns one chunk per event.
struct ResponsesToInteractions(openai_interactions::OpenAIResponsesToInteractionsStream);

impl StreamTranslator for ResponsesToInteractions {
    fn translate(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        events(&self.0.translate(chunk))
    }
}

#[cfg(test)]
mod tests {
    //! The registrations, through the registry. Not upstream's: upstream has
    //! no such tests.
    //!
    //! Dropped or changed tests: none.

    use serde_json::{Value, json};

    use crate::registry::{Format, Registry, ResponseContext};

    /// `apply_patch` declared in the `functions` namespace.
    const PATCH_REQUEST: &str = r#"{"model":"gpt-5.1-codex","tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"apply_patch","format":{"type":"grammar","definition":"start: patch"}}]}]}"#;

    fn context<'a>(model: &'a str, original_request: &'a Value) -> ResponseContext<'a> {
        ResponseContext {
            model,
            original_request,
            request: &Value::Null,
        }
    }

    /// The first line of each chunk, checking that each holds one event, or
    /// is `data: [DONE]` alone.
    fn event_kinds(chunks: &[Vec<u8>]) -> Vec<String> {
        chunks
            .iter()
            .map(|chunk| {
                let text = std::str::from_utf8(chunk).expect("UTF-8");
                if text == "data: [DONE]" {
                    return text.to_owned();
                }
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
            (Format::OPENAI_RESPONSE, Format::INTERACTIONS),
            (Format::INTERACTIONS, Format::OPENAI_RESPONSE),
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
        let request = registry.translate_request(
            &Format::OPENAI_RESPONSE,
            &Format::INTERACTIONS,
            "gemini-2.5-pro",
            json!({"input": "hello"}),
            true,
        );
        assert_eq!(request["model"], "gemini-2.5-pro");
        assert_eq!(request["stream"], true);
        assert!(request["input"].is_array(), "{request}");

        let request = registry.translate_request(
            &Format::INTERACTIONS,
            &Format::OPENAI_RESPONSE,
            "gpt-5.1-codex",
            json!({"input": "hello"}),
            false,
        );
        assert_eq!(request["model"], "gpt-5.1-codex");
        assert!(request.get("stream").is_none(), "{request}");
        assert!(request["input"].is_array(), "{request}");
    }

    // Not upstream's.
    #[test]
    fn interactions_streams_become_responses_events() {
        let registry = Registry::builtin();
        let original = json!({});
        let ctx = context("gemini-2.5-pro", &original);
        let (provider, client) = (Format::INTERACTIONS, Format::OPENAI_RESPONSE);
        let mut stream = registry.response_stream(&provider, &client, &ctx);
        let mut kinds = Vec::new();
        for line in [
            r#"data: {"event_type":"interaction.created","interaction":{"id":"i1"}}"#,
            r#"data: {"event_type":"step.start","index":0,"step":{"type":"model_output"}}"#,
            r#"data: {"event_type":"step.delta","index":0,"delta":{"type":"text","text":"hi"}}"#,
            r#"data: {"event_type":"step.stop","index":0}"#,
            r#"data: {"event_type":"interaction.completed","interaction":{"id":"i1","status":"completed"}}"#,
            "data: [DONE]",
        ] {
            kinds.extend(event_kinds(&stream.translate(line.as_bytes())));
        }
        assert_eq!(
            kinds.first().map(String::as_str),
            Some("event: response.created")
        );
        assert!(
            kinds
                .iter()
                .any(|kind| kind == "event: response.output_text.delta")
        );
        assert!(kinds.iter().any(|kind| kind == "event: response.completed"));
        assert_eq!(kinds.last().map(String::as_str), Some("data: [DONE]"));
        assert!(stream.finish().is_empty());
        assert!(stream.tool_input_error().is_none());

        let whole = br#"{"id":"i1","status":"completed","steps":[{"type":"model_output","content":[{"type":"text","text":"hi"}]}]}"#;
        let response = registry
            .translate_non_stream(&provider, &client, &ctx, whole.to_vec())
            .expect("a response");
        let response: Value = serde_json::from_slice(&response).expect("JSON");
        assert_eq!(response["object"], "response");
        assert_eq!(response["id"], "i1");
        assert_eq!(response["model"], "gemini-2.5-pro");
    }

    // Not upstream's.
    #[test]
    fn a_patch_stream_cut_short_fails_at_the_end() {
        let registry = Registry::builtin();
        let original: Value = serde_json::from_str(PATCH_REQUEST).expect("JSON");
        let ctx = context("gemini-2.5-pro", &original);
        let (provider, client) = (Format::INTERACTIONS, Format::OPENAI_RESPONSE);

        // With no chunk given, upstream has nothing to finalize.
        let mut stream = registry.response_stream(&provider, &client, &ctx);
        assert!(stream.finish().is_empty());
        assert!(stream.tool_input_error().is_none());

        let mut stream = registry.response_stream(&provider, &client, &ctx);
        stream
            .translate(br#"data: {"event_type":"interaction.created","interaction":{"id":"i1"}}"#);
        assert_eq!(event_kinds(&stream.finish()), ["event: response.failed"]);
        assert!(stream.tool_input_error().is_some());
    }

    // Not upstream's.
    #[test]
    fn a_whole_response_with_a_bad_patch_gives_nothing() {
        let registry = Registry::builtin();
        let original: Value = serde_json::from_str(PATCH_REQUEST).expect("JSON");
        let ctx = context("gemini-2.5-pro", &original);
        let body = br#"{"id":"i1","steps":[{"type":"function_call","name":"functions__apply_patch","arguments":{}}]}"#;
        assert_eq!(
            registry.translate_non_stream(
                &Format::INTERACTIONS,
                &Format::OPENAI_RESPONSE,
                &ctx,
                body.to_vec()
            ),
            None
        );
    }

    // Not upstream's.
    #[test]
    fn responses_streams_become_interactions_events() {
        let registry = Registry::builtin();
        let original = json!({});
        let ctx = context("gpt-5.1-codex", &original);
        let (provider, client) = (Format::OPENAI_RESPONSE, Format::INTERACTIONS);
        let mut stream = registry.response_stream(&provider, &client, &ctx);
        let mut kinds = Vec::new();
        for line in [
            r#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5.1-codex"}}"#,
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"id":"msg_1","type":"message","role":"assistant"}}"#,
            r#"data: {"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"hi"}"#,
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}}"#,
            r#"data: {"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":3,"output_tokens":1}}}"#,
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
        assert!(stream.finish().is_empty());
        assert!(stream.tool_input_error().is_none());

        let whole = br#"{"id":"resp_1","object":"response","status":"completed","output":[{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}]}"#;
        let response = registry
            .translate_non_stream(&provider, &client, &ctx, whole.to_vec())
            .expect("a response");
        let response: Value = serde_json::from_slice(&response).expect("JSON");
        assert_eq!(response["id"], "resp_1");
        assert_eq!(response["status"], "completed");
    }
}
