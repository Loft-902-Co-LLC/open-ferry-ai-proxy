// Ported from CLIProxyAPI internal/runtime/executor/openai_compat_executor_ttft_test.go (v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The usage tap observes upstream bytes before translation or downstream
//! backpressure. A manual clock replaces upstream's fake HTTP transport.

use super::support::{ClientCall, Harness, auth, bool_at, int_at, str_field};
use crate::exec::{ExecError, Format};
use crate::observe::{AttemptKind, Outcome};

#[test]
fn openai_compat_stream_ttft_waits_for_effective_tokens() {
    let cases = [
        (
            "text",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n",
        ),
        (
            "reasoning_content",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking\"}}]}\n\n",
        ),
        (
            "reasoning",
            "data: {\"choices\":[{\"delta\":{\"reasoning\":\"thinking\"}}]}\n\n",
        ),
        (
            "tool_call",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"function\":{\"name\":\"lookup\",\"arguments\":\"{}\"}}]}}]}\n\n",
        ),
        (
            "multiline",
            "data: {\"choices\":[\ndata: {\"delta\":{\"content\":\"hello\"}}]}\n\n",
        ),
    ];
    for (name, token) in cases {
        let h = Harness::new();
        let mut call = ClientCall::new("glm").stream();
        if name == "tool_call" {
            call.options.source_format = Format::OPENAI_RESPONSE;
        }
        let driver = call.tap(&h);
        driver.attempt(
            AttemptKind::Stream,
            "openai",
            "glm",
            &auth("a", "0", "openai"),
        );
        h.advance_ms(250);
        driver.head(200, &[("content-type", "text/event-stream")]);
        driver.chunk(": keepalive\n\ndata: {\"model\":\"glm-served\",\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\ndata: {\"choices\":[{\"delta\":{}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":0}}\n\n");
        h.advance_ms(84000);
        driver.chunk(token);
        // The client's delay and subsequent packets cannot replace token time.
        h.advance_ms(2000);
        driver.chunk("data: {\"model\":\"glm-served\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":1,\"total_tokens\":101}}\n\ndata: [DONE]\n\n");
        driver.finish(Outcome::Completed);
        let record = h.record();
        assert_eq!(int_at(&record, "/ttft_ms"), 84250, "{name}");
        assert_eq!(int_at(&record, "/latency_ms"), 86250, "{name}");
        assert!(!bool_at(&record, "/failed"), "{name}");
        assert_eq!(str_field(&record, "response_model"), "glm-served");
        assert_eq!(int_at(&record, "/tokens/output_tokens"), 1);
    }
}

#[test]
fn openai_compat_error_response_retains_first_byte_fallback() {
    let h = Harness::new();
    let driver = ClientCall::new("glm").stream().tap(&h);
    driver.attempt(
        AttemptKind::Stream,
        "openai",
        "glm",
        &auth("a", "0", "openai"),
    );
    h.advance_ms(250);
    driver.head(429, &[]);
    driver.chunk(r#"{"error":{"message":"rate limit"}}"#);
    driver.fail(&ExecError::upstream(429, "rate limit"));
    let record = h.record();
    assert!(bool_at(&record, "/failed"));
    assert_eq!(int_at(&record, "/ttft_ms"), 250);
}

#[test]
fn openai_compat_ttft_uses_complete_frames_and_keeps_packet_fallback() {
    for with_token in [false, true] {
        let h = Harness::new();
        let driver = ClientCall::new("glm").stream().tap(&h);
        driver.attempt(
            AttemptKind::Stream,
            "openai",
            "glm",
            &auth("a", "0", "openai"),
        );
        h.advance_ms(10);
        // The first non-empty body read sets packet fallback at 10 ms;
        // only a complete token frame can replace it (at 40 ms).
        driver.chunk(": keepalive\n\n");
        h.advance_ms(10);
        driver.chunk("data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n");
        if with_token {
            h.advance_ms(10);
            driver.chunk("data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n");
            h.advance_ms(10);
            driver.chunk("\n");
        }
        h.advance_ms(100);
        driver.fail(&ExecError::upstream(502, "read failed"));
        assert_eq!(
            int_at(&h.record(), "/ttft_ms"),
            if with_token { 40 } else { 10 }
        );
    }
}

#[test]
fn image_streams_keep_first_byte_timing() {
    for provider in ["codex", "openai"] {
        let h = Harness::new();
        let mut call = ClientCall::new("gpt-image-2").stream();
        call.options.source_format = Format::OPENAI_IMAGE;
        let driver = call.tap(&h);
        driver.attempt_with(
            AttemptKind::Stream,
            provider,
            "gpt-image-2",
            &Format::OPENAI_IMAGE,
            &auth("a", "0", provider),
            &[],
            "{}",
        );
        h.advance_ms(10);
        driver.chunk(": keepalive\n\n");
        h.advance_ms(20);
        driver.chunk("data: {\"data\":[{\"b64_json\":\"image\"}]}\n\n");
        h.advance_ms(20);
        driver.chunk("data: [DONE]\n\n");
        driver.finish(Outcome::Completed);
        let record = h.record();
        assert_eq!(int_at(&record, "/ttft_ms"), 10, "{provider}");
        assert!(!bool_at(&record, "/failed"), "{provider}");
    }
}

#[test]
fn done_after_unfinished_frame_keeps_packet_fallback() {
    for partial in [
        "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n",
        "data: {\"choices\":[\n",
    ] {
        let h = Harness::new();
        let driver = ClientCall::new("glm").stream().tap(&h);
        driver.attempt(
            AttemptKind::Stream,
            "openai",
            "glm",
            &auth("a", "0", "openai"),
        );
        h.advance_ms(10);
        driver.chunk(partial);
        h.advance_ms(20);
        // No blank line separated the data: upstream joins these lines and
        // rejects the frame instead of treating [DONE] as a terminal event.
        driver.chunk("data: [DONE]\n\n");
        driver.fail(&ExecError::upstream(
            502,
            "upstream stream ended with incomplete data before [DONE]",
        ));
        let record = h.record();
        assert_eq!(int_at(&record, "/ttft_ms"), 10);
        assert!(bool_at(&record, "/failed"));
    }
}
