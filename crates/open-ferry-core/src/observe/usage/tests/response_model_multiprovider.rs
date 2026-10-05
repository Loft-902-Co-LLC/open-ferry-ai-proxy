// Ported from CLIProxyAPI
// internal/runtime/executor/helps/response_model_multiprovider_test.go
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests of the served model of each provider's answers. All of upstream's
//! are ported.
//!
//! Deviations from upstream: the reporter tests run an attempt through the
//! usage tap, where upstream publishes counts it is given, so the Claude
//! stream is given a `message_delta` with counts: a Claude stream without
//! any makes no record. A Gemini Interactions provider's served model is
//! read as Gemini's, as upstream reads it, and the test's `openai`
//! provider stands for an OpenAI-compatible one.

use super::super::parse::StreamUsageBuffer;
use super::super::response_model::{extract_response_model_event, is_model_substituted};
use super::support::{ClientCall, Harness, Warnings, auth, str_field};
use crate::exec::Format;
use crate::observe::{AttemptKind, Outcome};

/// Ports TestExtractResponseModelMultiProvider.
#[test]
fn extract_response_model_multi_provider() {
    let cases = [
        (
            "claude sse message_start",
            "claude",
            r#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-3-7-sonnet-20250219"}}"#,
            "claude-3-7-sonnet-20250219",
            false,
        ),
        (
            "claude sse message_stop",
            "claude",
            r#"data: {"type":"message_stop"}"#,
            "",
            true,
        ),
        (
            "claude raw json non-stream message",
            "claude",
            r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-3-5-haiku-20241022","content":[{"type":"text","text":"hello"}]}"#,
            "claude-3-5-haiku-20241022",
            true,
        ),
        (
            "gemini sse chunk with modelVersion",
            "gemini",
            r#"data: {"candidates":[{"content":{"parts":[{"text":"hi"}]}}],"modelVersion":"gemini-2.5-flash"}"#,
            "gemini-2.5-flash",
            false,
        ),
        (
            "gemini sse terminal chunk with finishReason",
            "gemini",
            r#"data: {"candidates":[{"content":{"parts":[{"text":""}]},"finishReason":"STOP"}],"modelVersion":"gemini-2.5-flash"}"#,
            "gemini-2.5-flash",
            true,
        ),
        (
            "gemini raw json non-stream with modelVersion",
            "gemini",
            r#"{"candidates":[{"content":{"parts":[{"text":"hello"}]},"finishReason":"STOP"}],"modelVersion":"gemini-2.5-pro"}"#,
            "gemini-2.5-pro",
            true,
        ),
        (
            "antigravity wrapped response.modelVersion",
            "antigravity",
            r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"hello"}]}}],"modelVersion":"gemini-3.7-flash"}}"#,
            "gemini-3.7-flash",
            false,
        ),
        (
            "gemini interactions sse interaction.completed with model",
            "gemini",
            r#"data: {"event_type":"interaction.completed","interaction":{"id":"i1","status":"requires_action","usage":{"total_input_tokens":2,"total_output_tokens":3,"total_tokens":5},"service_tier":"standard","model":"gemini-3.1-flash-lite"}}"#,
            "gemini-3.1-flash-lite",
            true,
        ),
        (
            "gemini interactions sse interaction.created with model",
            "gemini",
            r#"data: {"event_type":"interaction.created","interaction":{"id":"i1","model":"gemini-3.1-flash-lite"}}"#,
            "gemini-3.1-flash-lite",
            false,
        ),
        (
            "gemini interactions sse interaction.completed without model",
            "gemini",
            r#"data: {"event_type":"interaction.completed","interaction":{"id":"i1","status":"completed"}}"#,
            "",
            true,
        ),
        (
            "gemini-interactions provider sse interaction.completed with model",
            "gemini-interactions",
            r#"data: {"event_type":"interaction.completed","interaction":{"id":"i1","status":"completed","model":"gemini-3.1-flash-lite"}}"#,
            "gemini-3.1-flash-lite",
            true,
        ),
        (
            "openai sse chat completion chunk",
            "openai",
            r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","model":"gpt-4o-2024-08-06","choices":[{"index":0,"delta":{"content":"hi"}}]}"#,
            "gpt-4o-2024-08-06",
            false,
        ),
        (
            "openai sse chat completion terminal chunk",
            "openai",
            r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","model":"gpt-4o-2024-08-06","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "gpt-4o-2024-08-06",
            true,
        ),
        (
            "openai raw json chat completion",
            "openai",
            r#"{"id":"chatcmpl-1","object":"chat.completion","model":"gpt-4o-2024-08-06","choices":[{"index":0,"message":{"role":"assistant","content":"hello"}}]}"#,
            "gpt-4o-2024-08-06",
            true,
        ),
        (
            "xai sse chat completion chunk",
            "xai",
            r#"data: {"id":"chatcmpl-x1","object":"chat.completion.chunk","model":"grok-beta","choices":[{"index":0,"delta":{"content":"hi"}}]}"#,
            "grok-beta",
            false,
        ),
        (
            "generic fallback with model",
            "custom",
            r#"{"model":"custom-model-v1","object":"chat.completion"}"#,
            "custom-model-v1",
            true,
        ),
        (
            "generic interactions sse interaction.completed with model",
            "custom",
            r#"data: {"event_type":"interaction.completed","interaction":{"model":"custom-model-v2"}}"#,
            "custom-model-v2",
            true,
        ),
    ];
    for (name, provider, payload, model, terminal) in cases {
        let got = extract_response_model_event(payload.as_bytes(), provider);
        assert_eq!(got, (model.to_owned(), terminal), "{name}");
    }
}

/// Ports TestIsModelSubstitutedMultiProvider.
#[test]
fn is_model_substituted_multi_provider() {
    let cases = [
        (
            "claude dated alias match",
            "claude-3-7-sonnet",
            "claude-3-7-sonnet-20250219",
            false,
        ),
        (
            "claude latest alias match",
            "claude-3-5-sonnet-latest",
            "claude-3-5-sonnet-20241022",
            false,
        ),
        (
            "claude substitution opus to sonnet",
            "claude-opus-5",
            "claude-sonnet-5",
            true,
        ),
        (
            "claude thinking suffix match",
            "claude-3-7-sonnet(high)",
            "claude-3-7-sonnet",
            false,
        ),
        (
            "gemini dated alias match",
            "gemini-2.5-flash",
            "gemini-2.5-flash-2025-05-20",
            false,
        ),
        (
            "gemini preview is substitution",
            "gemini-2.5-flash",
            "gemini-2.5-flash-preview",
            true,
        ),
        (
            "gemini numeric version alias match",
            "gemini-1.5-pro",
            "gemini-1.5-pro-002",
            false,
        ),
        (
            "gemini substitution pro to flash",
            "gemini-2.5-pro",
            "gemini-2.5-flash",
            true,
        ),
        (
            "openai dated alias match",
            "gpt-4o",
            "gpt-4o-2024-08-06",
            false,
        ),
        (
            "openai substitution gpt-4o to gpt-4o-mini",
            "gpt-4o",
            "gpt-4o-mini",
            true,
        ),
        (
            "openai provider prefix match",
            "openai/gpt-4o",
            "gpt-4o",
            false,
        ),
        ("devin provider prefix match", "devin/swe-2", "swe-2", false),
    ];
    for (name, requested, served, want) in cases {
        assert_eq!(is_model_substituted(requested, served), want, "{name}");
    }
}

/// Ports TestUsageReporterMultiProviderSubstitutionWarning.
#[test]
fn multi_provider_substitution_warning() {
    let cases = [
        (
            "claude",
            Format::CLAUDE,
            "claude-opus-5",
            "claude-sonnet-5",
            r#"data: {"type":"message_start","message":{"id":"msg_1","model":"claude-sonnet-5"}}"#,
            r#"claude executor: upstream served model "claude-sonnet-5" for requested model "claude-opus-5""#,
        ),
        (
            "gemini",
            Format::GEMINI,
            "gemini-2.5-pro",
            "gemini-2.5-flash",
            r#"data: {"candidates":[{"content":{"parts":[{"text":"hi"}]}}],"modelVersion":"gemini-2.5-flash"}"#,
            r#"gemini executor: upstream served model "gemini-2.5-flash" for requested model "gemini-2.5-pro""#,
        ),
        (
            "openai",
            Format::OPENAI,
            "gpt-4o",
            "gpt-4o-mini",
            r#"data: {"id":"chatcmpl-1","model":"gpt-4o-mini","choices":[{"index":0,"delta":{"content":"hi"}}]}"#,
            r#"openai executor: upstream served model "gpt-4o-mini" for requested model "gpt-4o""#,
        ),
        (
            "gemini-interactions",
            Format::OPENAI,
            "gemini-2.5-pro",
            "gemini-3.1-flash-lite",
            r#"data: {"event_type":"interaction.completed","interaction":{"id":"i1","status":"completed","service_tier":"standard","model":"gemini-3.1-flash-lite"}}"#,
            r#"gemini-interactions executor: upstream served model "gemini-3.1-flash-lite" for requested model "gemini-2.5-pro""#,
        ),
    ];
    for (provider, format, requested, served, line, expected) in cases {
        let warnings = Warnings::capture();
        let harness = Harness::new();
        let driver = ClientCall::new(requested).stream().tap(&harness);
        let credential = auth(&format!("auth-{provider}"), "", provider);
        driver.attempt_with(
            AttemptKind::Stream,
            provider,
            requested,
            &format,
            &credential,
            &[],
            "{}",
        );
        driver.chunk(&format!("{line}\n"));
        if provider == "claude" {
            driver.chunk(
                "data: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":10,\"output_tokens\":20}}\n",
            );
        }
        driver.finish(Outcome::Completed);
        assert_eq!(
            str_field(&harness.record(), "response_model"),
            served,
            "{provider}"
        );
        let warned = warnings.substitutions();
        assert_eq!(warned.len(), 1, "{provider}: {warned:?}");
        assert!(warned[0].contains(expected), "{provider}: {}", warned[0]);
    }
}

/// Ports TestStreamUsageBufferMultiProviderResponseModelPropagation; the
/// reporter half runs a stream of each through the usage tap.
#[test]
fn stream_usage_buffer_multi_provider_response_model_propagation() {
    let mut openai = StreamUsageBuffer::default();
    openai.observe_openai_stream(
        br#"data: {"id":"1","model":"gpt-4o-mini","choices":[{"delta":{"content":"x"}}]}"#,
    );
    assert_eq!(openai.response_model(), "gpt-4o-mini");

    let mut claude = StreamUsageBuffer::default();
    claude.observe_claude_stream(
        br#"data: {"type":"message_start","message":{"id":"msg_1","model":"claude-3-5-haiku-20241022"}}"#,
    );
    assert_eq!(claude.response_model(), "claude-3-5-haiku-20241022");

    let cases = [
        (
            "openai",
            "gpt-4o",
            "data: {\"id\":\"1\",\"model\":\"gpt-4o-mini\",\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\ndata: {\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":5,\"total_tokens\":10}}\n",
            "gpt-4o-mini",
        ),
        (
            "claude",
            "claude-3-5-sonnet",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-3-5-haiku-20241022\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":10}}\n",
            "claude-3-5-haiku-20241022",
        ),
    ];
    for (provider, requested, stream, served) in cases {
        let harness = Harness::new();
        let driver = ClientCall::new(requested).stream().tap(&harness);
        driver.attempt(
            AttemptKind::Stream,
            provider,
            requested,
            &auth("auth-1", "0", provider),
        );
        driver.chunk(stream);
        driver.finish(Outcome::Completed);
        assert_eq!(
            str_field(&harness.record(), "response_model"),
            served,
            "{provider}"
        );
    }
}
