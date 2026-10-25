//! Claude's billed response speed, including initial and delta stream usage.

use super::super::parse::{StreamUsageBuffer, parse_claude_usage};
use super::support::{ClientCall, Harness, auth, str_field};
use crate::exec::Format;
use crate::observe::{AttemptKind, Outcome};

#[test]
fn claude_speed_reports_response_service_tier() {
    for (speed, want) in [
        (Some("fast"), "fast"),
        (Some("standard"), "standard"),
        (Some(" fast \t"), "fast"),
        (Some(" \n"), ""),
        (None, ""),
    ] {
        let mut body = serde_json::json!({"usage":{"input_tokens":10,"output_tokens":5}});
        if let Some(speed) = speed {
            body["usage"]["speed"] = speed.into();
        }
        let detail = parse_claude_usage(body.to_string().as_bytes());
        assert_eq!(detail.response_service_tier, want);
        assert_eq!(
            (
                detail.input_tokens,
                detail.output_tokens,
                detail.total_tokens
            ),
            (10, 5, 15)
        );
        let harness = Harness::new();
        let driver = ClientCall::new("model").tap(&harness);
        driver.attempt(
            AttemptKind::Execute,
            "claude",
            "model",
            &auth("id", "0", "claude"),
        );
        driver.chunk(&body.to_string());
        driver.finish(Outcome::Completed);
        let record = harness.record();
        assert_eq!(
            record
                .get("response_service_tier")
                .and_then(|v| v.as_str())
                .unwrap_or_default(),
            want
        );
    }
}

#[test]
fn claude_speed_keeps_initial_speed_across_delta_and_queues_it() {
    for (start_speed, delta_speed, want) in [
        (Some("fast"), None, "fast"),
        (None, Some("fast"), "fast"),
        (Some("fast"), Some("standard"), "standard"),
        (Some("standard"), Some(" \t"), "standard"),
        (None, None, ""),
    ] {
        let mut start = serde_json::json!({"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":20,"output_tokens":1}}});
        let mut delta = serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":15}});
        if let Some(speed) = start_speed {
            start["message"]["usage"]["speed"] = speed.into();
        }
        if let Some(speed) = delta_speed {
            delta["usage"]["speed"] = speed.into();
        }
        let start = format!("data: {start}\n\n");
        let delta = format!("data: {delta}\n\n");
        let mut buffer = StreamUsageBuffer::default();
        buffer.observe_claude_stream(start.as_bytes());
        buffer.observe_claude_stream(delta.as_bytes());
        let detail = buffer.detail().unwrap();
        assert_eq!(detail.response_service_tier, want);
        assert_eq!((detail.input_tokens, detail.output_tokens), (20, 15));

        let harness = Harness::new();
        let driver = ClientCall::new("model")
            .body(r#"{"service_tier":"requested"}"#)
            .stream()
            .tap(&harness);
        driver.attempt_with(
            AttemptKind::Stream,
            "claude",
            "model",
            &Format::CLAUDE,
            &auth("id", "0", "claude"),
            &[],
            r#"{"service_tier":"effective"}"#,
        );
        driver.chunk(&start);
        driver.chunk(&delta);
        driver.chunk("data: {\"type\":\"message_stop\"}\n\n");
        driver.finish(Outcome::Completed);
        let record = harness.record();
        assert_eq!(str_field(&record, "service_tier"), "requested");
        assert_eq!(str_field(&record, "effective_service_tier"), "effective");
        assert_eq!(
            record
                .get("response_service_tier")
                .and_then(|v| v.as_str())
                .unwrap_or_default(),
            want
        );
    }
}
