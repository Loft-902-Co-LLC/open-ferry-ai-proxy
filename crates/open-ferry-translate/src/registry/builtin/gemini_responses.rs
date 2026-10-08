// Ported from CLIProxyAPI internal/translator/gemini/openai/responses/init.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The Responses → Gemini translator's registration.
//!
//! Deviations from upstream: none.

use std::error::Error;
use std::sync::Arc;

use super::{checked, events, to_vec};
use crate::gemini::openai::responses as gemini_responses;
use crate::registry::{Format, Registry, ResponseTransform, StreamTranslator};

pub(super) fn register(registry: &Registry) {
    // internal/translator/gemini/openai/responses/init.go
    registry.register(
        Format::OPENAI_RESPONSE,
        Format::GEMINI,
        checked(|model, body, stream| {
            gemini_responses::convert_openai_responses_request_to_gemini(model, &body, stream)
        }),
        ResponseTransform {
            stream: Some(Arc::new(|context| {
                Box::new(GeminiToResponses(
                    gemini_responses::GeminiToOpenAIResponsesStream::new(
                        context.model,
                        context.original_request,
                        context.request,
                    ),
                ))
            })),
            non_stream: Some(Arc::new(|context, body| {
                gemini_responses::convert_gemini_response_to_openai_responses_non_stream(
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
}

/// Gemini → Responses. Upstream returns one chunk per event.
struct GeminiToResponses(gemini_responses::GeminiToOpenAIResponsesStream);

impl StreamTranslator for GeminiToResponses {
    fn translate(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        events(&self.0.translate_line(chunk))
    }

    fn finish(&mut self) -> Vec<Vec<u8>> {
        events(&self.0.finalize_tool_input())
    }

    fn tool_input_error(&self) -> Option<&(dyn Error + 'static)> {
        self.0.tool_input_error()
    }
}

#[cfg(test)]
mod tests {
    //! The registration, through the registry. Not ported: upstream has no
    //! such tests.
    //!
    //! Dropped or changed tests: none.

    use serde_json::{Value, json};

    use crate::registry::{Format, Registry, ResponseContext};

    const PATCH_REQUEST: &str = r#"{"model":"gemini-2.5-pro","tools":[{"type":"custom","name":"apply_patch","format":{"type":"grammar","syntax":"lark","definition":"start: patch"}}]}"#;

    const TEXT: &[u8] = br#"data: {"candidates":[{"content":{"role":"model","parts":[{"text":"hi"}]}}],"responseId":"r1"}"#;

    const BAD_PATCH: &[u8] = br#"data: {"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"apply_patch","args":{}}}]}}],"responseId":"r1"}"#;

    const STOP: &[u8] = br#"data: {"candidates":[{"content":{"role":"model","parts":[]},"finishReason":"STOP"}],"responseId":"r1"}"#;

    fn context<'a>(original_request: &'a Value) -> ResponseContext<'a> {
        ResponseContext {
            model: "gemini-2.5-pro",
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

    #[test]
    fn the_pair_is_registered() {
        let registry = Registry::builtin();
        let (client, provider) = (Format::OPENAI_RESPONSE, Format::GEMINI);
        assert!(registry.has_request_transformer(&client, &provider));
        assert!(registry.has_stream_response_transformer(&client, &provider));
        assert!(registry.has_non_stream_response_transformer(&client, &provider));
        assert!(!registry.has_request_transformer(&provider, &client));

        let request = registry.translate_request(
            &client,
            &provider,
            "gemini-2.5-pro",
            json!({"model": "gemini-2.5-pro", "input": "hello"}),
            false,
        );
        assert_eq!(
            request["contents"],
            json!([{"role": "user", "parts": [{"text": "hello"}]}])
        );
    }

    #[test]
    fn the_stream_gives_one_chunk_per_event() {
        let registry = Registry::builtin();
        let original = json!({"model": "gemini-2.5-pro"});
        let ctx = context(&original);
        let mut stream = registry.response_stream(&Format::GEMINI, &Format::OPENAI_RESPONSE, &ctx);
        let mut kinds = event_kinds(&stream.translate(TEXT));
        kinds.extend(event_kinds(&stream.translate(STOP)));
        // A finish reason without usage waits for usage or `[DONE]`.
        kinds.extend(event_kinds(&stream.translate(b"data: [DONE]")));
        assert_eq!(
            kinds,
            [
                "event: response.created",
                "event: response.in_progress",
                "event: response.output_item.added",
                "event: response.content_part.added",
                "event: response.output_text.delta",
                "event: response.output_text.done",
                "event: response.content_part.done",
                "event: response.output_item.done",
                "event: response.completed",
            ]
        );
        assert!(stream.finish().is_empty());
        assert!(stream.tool_input_error().is_none());
    }

    #[test]
    fn the_stream_fails_a_bad_patch() {
        let registry = Registry::builtin();
        let original: Value = serde_json::from_str(PATCH_REQUEST).expect("JSON");
        let ctx = context(&original);
        let mut stream = registry.response_stream(&Format::GEMINI, &Format::OPENAI_RESPONSE, &ctx);
        stream.translate(TEXT);
        let failed = stream.translate(BAD_PATCH);
        assert!(
            failed
                .iter()
                .any(|chunk| chunk.starts_with(b"event: response.failed\n")),
            "{failed:?}"
        );
        assert!(stream.tool_input_error().is_some());
        assert!(stream.translate(STOP).is_empty());
        assert!(stream.finish().is_empty());
    }

    #[test]
    fn the_stream_fails_when_cut_short() {
        let registry = Registry::builtin();
        let original: Value = serde_json::from_str(PATCH_REQUEST).expect("JSON");
        let ctx = context(&original);
        let mut stream = registry.response_stream(&Format::GEMINI, &Format::OPENAI_RESPONSE, &ctx);
        stream.translate(TEXT);
        let failed = stream.finish();
        assert_eq!(event_kinds(&failed), ["event: response.failed"]);
        assert!(stream.tool_input_error().is_some());
    }

    #[test]
    fn unreadable_input_fails_only_with_apply_patch() {
        let registry = Registry::builtin();
        let patch: Value = serde_json::from_str(PATCH_REQUEST).expect("JSON");
        let plain = json!({"model": "gemini-2.5-pro"});
        let (provider, client) = (Format::GEMINI, Format::OPENAI_RESPONSE);

        for line in [&b"data: {not json"[..], b"garbage", b"data: \xff"] {
            let ctx = context(&patch);
            let mut stream = registry.response_stream(&provider, &client, &ctx);
            assert_eq!(
                event_kinds(&stream.translate(line)),
                ["event: response.failed"],
                "{line:?}"
            );
            assert!(stream.tool_input_error().is_some(), "{line:?}");
            assert!(stream.translate(TEXT).is_empty(), "{line:?}");

            let ctx = context(&plain);
            let mut stream = registry.response_stream(&provider, &client, &ctx);
            assert!(stream.translate(line).is_empty(), "{line:?}");
            assert!(stream.tool_input_error().is_none(), "{line:?}");
        }

        // Empty data and an early `[DONE]` are not failures.
        let ctx = context(&patch);
        let mut stream = registry.response_stream(&provider, &client, &ctx);
        for line in [&b"data: "[..], b"  ", b"data: [DONE]"] {
            assert!(stream.translate(line).is_empty(), "{line:?}");
        }
        assert!(stream.tool_input_error().is_none());

        let ctx = context(&patch);
        let whole =
            |body: &[u8]| registry.translate_non_stream(&provider, &client, &ctx, body.to_vec());
        assert_eq!(whole(b"{not json"), None);
        assert!(whole(b"").is_some());
        let ctx = context(&plain);
        let whole =
            |body: &[u8]| registry.translate_non_stream(&provider, &client, &ctx, body.to_vec());
        assert!(whole(b"{not json").is_some());
    }

    #[test]
    fn the_whole_response_fails_a_bad_patch() {
        let registry = Registry::builtin();
        let original: Value = serde_json::from_str(PATCH_REQUEST).expect("JSON");
        let ctx = context(&original);
        let body = br#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"apply_patch","args":{}}}]},"finishReason":"STOP"}],"responseId":"r1"}"#;
        assert_eq!(
            registry.translate_non_stream(
                &Format::GEMINI,
                &Format::OPENAI_RESPONSE,
                &ctx,
                body.to_vec()
            ),
            None
        );

        let body = br#"{"candidates":[{"content":{"parts":[{"text":"hi"}]},"finishReason":"STOP"}],"responseId":"r1"}"#;
        let done = registry
            .translate_non_stream(
                &Format::GEMINI,
                &Format::OPENAI_RESPONSE,
                &ctx,
                body.to_vec(),
            )
            .expect("translated");
        let done: Value = serde_json::from_slice(&done).expect("JSON");
        assert_eq!(done["id"], "resp_r1");
        assert_eq!(done["object"], "response");
        assert_eq!(done["output"][0]["content"][0]["text"], "hi");
    }
}
