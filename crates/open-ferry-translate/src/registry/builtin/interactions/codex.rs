// Ported from CLIProxyAPI internal/translator/codex/interactions/init.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The Interactions → Codex translators' registration: Interactions clients
//! talking to a Codex upstream (`interactions` → `codex`).
//!
//! Deviations from upstream: none.

use std::sync::Arc;

use super::super::{events, to_vec};
use super::parse;
use crate::codex::interactions as codex_interactions;
use crate::registry::{Format, Registry, ResponseTransform, StreamTranslator};

pub(super) fn register(registry: &Registry) {
    // internal/translator/codex/interactions/init.go
    registry.register(
        Format::INTERACTIONS,
        Format::CODEX,
        Some(Arc::new(|model, body, stream| {
            codex_interactions::convert_interactions_request_to_codex(model, &body, stream)
        })),
        ResponseTransform {
            stream: Some(Arc::new(|context| {
                Box::new(CodexToInteractions(
                    codex_interactions::CodexToInteractionsStream::new(context.model),
                ))
            })),
            non_stream: Some(Arc::new(|context, body| {
                let response =
                    codex_interactions::convert_codex_response_to_interactions_non_stream(
                        context.model,
                        &parse(body),
                    );
                Some(to_vec(&response))
            })),
            token_count: None,
        },
    );
}

/// Codex → Interactions. Upstream returns one chunk per event.
struct CodexToInteractions(codex_interactions::CodexToInteractionsStream);

impl StreamTranslator for CodexToInteractions {
    fn translate(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        events(&self.0.translate_line(chunk))
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

    fn context(request: &Value) -> ResponseContext<'_> {
        ResponseContext {
            model: "gpt-5-codex",
            original_request: request,
            request,
        }
    }

    #[test]
    fn the_pair_is_registered() {
        let registry = Registry::builtin();
        let (client, provider) = (Format::INTERACTIONS, Format::CODEX);
        assert!(registry.has_request_transformer(&client, &provider));
        assert!(registry.has_stream_response_transformer(&client, &provider));
        assert!(registry.has_non_stream_response_transformer(&client, &provider));
        assert!(!registry.has_request_transformer(&provider, &client));

        let out = registry.translate_request(
            &client,
            &provider,
            "gpt-5-codex",
            json!({"input": "hi", "generation_config": {"thinking_level": "high"}}),
            true,
        );
        assert_eq!(
            out,
            json!({
                "model": "gpt-5-codex",
                "instructions": "",
                "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
                "stream": true,
                "reasoning": {"effort": "high"}
            })
        );
    }

    #[test]
    fn the_stream_gives_one_chunk_per_event() {
        let registry = Registry::builtin();
        let request = json!({});
        let ctx = context(&request);
        let mut stream = registry.response_stream(&Format::CODEX, &Format::INTERACTIONS, &ctx);
        let chunks = stream.translate(
            br#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5-codex"}}"#,
        );
        let want: [&[u8]; 2] = [
            b"event: interaction.created
data: {\"interaction\":{\"id\":\"resp_1\",\"status\":\"in_progress\",\"object\":\"interaction\",\"model\":\"gpt-5-codex\"},\"event_type\":\"interaction.created\"}

",
            b"event: interaction.status_update
data: {\"interaction_id\":\"resp_1\",\"status\":\"in_progress\",\"event_type\":\"interaction.status_update\"}

",
        ];
        assert_eq!(chunks, want);
        assert!(stream.finish().is_empty());
    }

    #[test]
    fn whole_responses_are_translated() {
        let registry = Registry::builtin();
        let request = json!({});
        let out = registry
            .translate_non_stream(
                &Format::CODEX,
                &Format::INTERACTIONS,
                &context(&request),
                br#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"ok"}]}]}}"#.to_vec(),
            )
            .expect("the pair translates whole responses");
        let out: Value = serde_json::from_slice(&out).expect("the response is JSON");
        assert_eq!(
            out,
            json!({
                "id": "resp_1",
                "object": "interaction",
                "status": "completed",
                "model": "gpt-5-codex",
                "steps": [{"type": "model_output", "content": [{"type": "text", "text": "ok"}]}]
            })
        );
    }

    // Not upstream's: a whole response's function call arguments keep each
    // number as written, as upstream copies them (checked with Go).
    #[test]
    fn whole_responses_keep_numbers_as_written() {
        let registry = Registry::builtin();
        let request = json!({});
        let spelled = r#"{"x":-0,"y":1E20,"z":[1e5,0.10]}"#;
        let body = format!(
            r#"{{"type":"response.completed","response":{{"id":"r1","output":[{{"type":"function_call","call_id":"c1","name":"f","arguments":{spelled}}}]}}}}"#
        );
        let out = registry
            .translate_non_stream(
                &Format::CODEX,
                &Format::INTERACTIONS,
                &context(&request),
                body.into(),
            )
            .expect("the pair translates whole responses");
        let out = String::from_utf8(out).expect("UTF-8");
        assert!(out.contains(&format!(r#""arguments":{spelled}"#)), "{out}");
    }
}
