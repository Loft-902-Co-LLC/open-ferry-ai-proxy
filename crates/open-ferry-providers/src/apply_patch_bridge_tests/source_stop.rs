// Ported from CLIProxyAPI internal/runtime/executor/apply_patch_source_stop_test.go
// (TestApplyPatchInteractionsSourceStopActualFailures) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A Gemini Interactions `apply_patch` call whose step stops before its
//! arguments are whole, through the Interactions executor: arguments that
//! come after the stop, alone or with a snapshot of the step, mustn't
//! complete the call, so the response fails.
//!
//! Deviations from upstream:
//! - Upstream also checks that the call published one failed usage record
//!   (`task6CaptureFailureUsage`); here the manager's usage tap records the
//!   failure from the stream's error, so the test checks the stream ends
//!   with exactly one error, the 502.

use bytes::Bytes;
use open_ferry_core::exec::{Format, Options, Request};
use serde_json::json;

use super::{
    TASK6_PATCH_REQUEST, Upstream, WAIT, assert_patch_error, auth, collect, executor, payloads, sse,
};
use crate::json::str_at;

// TestApplyPatchInteractionsSourceStopActualFailures: the source trace run
// through the Interactions executor.
#[tokio::test]
async fn interactions_source_stop_actual_failures() {
    for first in ["item", "call", "neither"] {
        for late_name in [false, true] {
            for input in ["partial", "complete"] {
                for discovery in ["terminal", "coincident-snapshot"] {
                    let case = format!("{first}/late-name={late_name}/{input}/{discovery}");
                    let id = if first == "item" { "a" } else { "" };
                    let call = if first == "call" { "c" } else { "" };
                    let name = if late_name { "" } else { "apply_patch" };
                    let (before, after, last) = if input == "complete" {
                        (r#"{"input":"p"}"#, " \t\n", "p")
                    } else {
                        (r#"{"input":"p"#, r#"q"}"#, "pq")
                    };
                    let snapshot = json!({"index": 0, "type": "function_call", "id": "a", "call_id": "c", "name": "apply_patch", "arguments": {"input": last}, "provider_secret": "RAW_SECRET"});
                    let post = if discovery == "coincident-snapshot" {
                        json!({"event_type": "step.delta", "index": 0, "step": snapshot, "delta": {"type": "arguments_delta", "arguments": after}})
                    } else {
                        json!({"event_type": "step.delta", "index": 0, "delta": {"type": "arguments_delta", "arguments": after}})
                    };
                    let source = [
                        json!({"event_type": "step.start", "index": 0, "step": {"type": "function_call", "id": id, "call_id": call, "name": name}}).to_string(),
                        json!({"event_type": "step.delta", "index": 0, "delta": {"type": "arguments_delta", "arguments": before}}).to_string(),
                        json!({"event_type": "step.stop", "index": 0}).to_string(),
                        post.to_string(),
                        json!({"event_type": "interaction.completed", "steps": [snapshot], "usage": {"input_tokens": 5, "output_tokens": 3}}).to_string(),
                        "[DONE]".to_owned(),
                    ];
                    let upstream = Upstream::answering(&sse(&source)).await;
                    let executor = executor("gemini-interactions");
                    let auth = auth(
                        "gemini-interactions",
                        executor.as_ref(),
                        &upstream.url,
                        false,
                    );
                    let request = Request {
                        model: "grok-4".into(),
                        payload: Bytes::from_static(TASK6_PATCH_REQUEST.as_bytes()),
                    };
                    let options = Options {
                        stream: true,
                        ..Options::new(Format::OPENAI_RESPONSE)
                    };
                    let response =
                        tokio::time::timeout(WAIT, executor.execute_stream(auth, request, options))
                            .await
                            .unwrap_or_else(|_| panic!("{case}: the call didn't start"))
                            .unwrap_or_else(|error| panic!("{case}: {error:?}"));
                    let (chunks, errors) = collect(response).await;
                    for error in &errors {
                        assert_patch_error(error);
                    }
                    let mut failed = 0;
                    for chunk in &chunks {
                        assert!(
                            !chunk.contains("RAW_SECRET") && !chunk.contains("[DONE]"),
                            "{case}: the failure leaked or completed: {chunk}"
                        );
                        let events = payloads(chunk);
                        assert!(
                            chunk.is_empty() || !events.is_empty(),
                            "{case}: raw fallback: {chunk}"
                        );
                        for event in events {
                            assert!(
                                event["type"] == "response.failed"
                                    && str_at(&event, "response.error.code")
                                        == "invalid_tool_arguments",
                                "{case}: a source-stopped call published success or a raw fallback: {event}"
                            );
                            failed += 1;
                        }
                    }
                    assert!(
                        failed == 1 && errors.len() == 1,
                        "{case}: failed={failed} errors={errors:?}\n{chunks:?}"
                    );
                }
            }
        }
    }
}
