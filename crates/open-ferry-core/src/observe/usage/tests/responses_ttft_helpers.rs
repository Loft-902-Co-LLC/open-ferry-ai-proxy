// Ported from CLIProxyAPI
// internal/runtime/executor/helps/responses_ttft_helpers_test.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests of the Responses token-event classifier and the time to first
//! token it drives. All of upstream's are ported.
//!
//! Deviations from upstream: the timer is driven with a clock the tests
//! move, not with sleeps; upstream's `ObserveResponsesTokenEvent` is the
//! classifier's answer given to [`Ttft::observe_token_event`].

use std::time::{Duration, Instant};

use super::super::ttft::{Ttft, is_responses_token_event};

/// Ports TestIsResponsesTokenEvent_Classification.
#[test]
fn is_responses_token_event_classification() {
    let cases = [
        ("empty payload", "", false),
        ("whitespace only", "   \n\t  ", false),
        (
            "codex rate limits metadata",
            r#"{"type":"codex.rate_limits","rate_limits":{"plan_type":"pro"}}"#,
            false,
        ),
        (
            "codex response metadata",
            r#"{"type":"codex.response.metadata","etag":"W/\"123\""}"#,
            false,
        ),
        (
            "responsesapi websocket timing",
            r#"{"type":"responsesapi.websocket_timing","timing":{"duration_ms":100}}"#,
            false,
        ),
        (
            "response created",
            r#"{"type":"response.created","response":{"id":"resp_123","status":"in_progress"}}"#,
            false,
        ),
        (
            "response in progress",
            r#"{"type":"response.in_progress","response":{"id":"resp_123","tools":[{"type":"function"}]}}"#,
            false,
        ),
        (
            "response output item added with encrypted content only",
            r#"{"type":"response.output_item.added","item":{"type":"reasoning","encrypted_content":"gAAAAAB..."}}"#,
            false,
        ),
        (
            "response content part added",
            r#"{"type":"response.content_part.added","part":{"type":"text","text":""}}"#,
            false,
        ),
        (
            "response reasoning summary part added",
            r#"{"type":"response.reasoning_summary_part.added","part":{"type":"summary_text","text":""}}"#,
            false,
        ),
        (
            "response reasoning summary text delta empty",
            r#"{"type":"response.reasoning_summary_text.delta","delta":""}"#,
            false,
        ),
        (
            "response reasoning summary text delta non-empty",
            r#"{"type":"response.reasoning_summary_text.delta","delta":"**Inspecting**"}"#,
            true,
        ),
        (
            "response reasoning delta non-empty",
            r#"{"type":"response.reasoning.delta","delta":"Analyzing requirements..."}"#,
            true,
        ),
        (
            "response reasoning text delta non-empty",
            r#"{"type":"response.reasoning_text.delta","delta":"Step 1: Check code"}"#,
            true,
        ),
        (
            "response output text delta non-empty",
            r#"{"type":"response.output_text.delta","delta":"Hello world"}"#,
            true,
        ),
        (
            "response text delta non-empty",
            r#"{"type":"response.text.delta","delta":"Direct text chunk"}"#,
            true,
        ),
        (
            "response function call arguments delta non-empty",
            r#"{"type":"response.function_call_arguments.delta","delta":"{\"query\":\"test\"}"}"#,
            true,
        ),
        (
            "response custom_tool_call_input delta non-empty",
            r#"{"type":"response.custom_tool_call_input.delta","delta":"{\"param\":1}"}"#,
            true,
        ),
        (
            "response code interpreter call code delta non-empty",
            r#"{"type":"response.code_interpreter_call_code.delta","delta":"import math\n"}"#,
            true,
        ),
        (
            "response mcp call arguments delta non-empty",
            r#"{"type":"response.mcp_call_arguments.delta","delta":"{\"tool\":\"lookup\"}"}"#,
            true,
        ),
        (
            "response shell call command added with non-empty command",
            r#"{"type":"response.shell_call_command.added","command":"ls -la"}"#,
            true,
        ),
        (
            "response shell call command added with empty command",
            r#"{"type":"response.shell_call_command.added","command":""}"#,
            false,
        ),
        (
            "response shell call command delta non-empty",
            r#"{"type":"response.shell_call_command.delta","delta":"ls -la\n"}"#,
            true,
        ),
        (
            "response shell call output content delta is tool execution output, not model token",
            r#"{"type":"response.shell_call_output_content.delta","delta":{"stdout":"output text\n","stderr":""}}"#,
            false,
        ),
        (
            "response shell call output content done is tool execution output, not model token",
            r#"{"type":"response.shell_call_output_content.done","output":[]}"#,
            false,
        ),
        (
            "response refusal delta non-empty",
            r#"{"type":"response.refusal.delta","delta":"I cannot fulfill this request"}"#,
            true,
        ),
        (
            "response audio transcript delta non-empty",
            r#"{"type":"response.audio.transcript.delta","delta":"Spoken text"}"#,
            true,
        ),
        (
            "response audio delta non-empty",
            r#"{"type":"response.audio.delta","delta":"UklGRi..."}"#,
            true,
        ),
        (
            "response image generation call partial image non-empty",
            r#"{"type":"response.image_generation_call.partial_image","partial_image_b64":"iVBORw0KGgo..."}"#,
            true,
        ),
        (
            "response web search call in progress",
            r#"{"type":"response.web_search_call.in_progress"}"#,
            false,
        ),
        (
            "response file search call searching",
            r#"{"type":"response.file_search_call.searching"}"#,
            false,
        ),
        (
            "response code interpreter call interpreting",
            r#"{"type":"response.code_interpreter_call.interpreting"}"#,
            false,
        ),
        (
            "response mcp call in progress",
            r#"{"type":"response.mcp_call.in_progress"}"#,
            false,
        ),
        (
            "response output item done function call empty args",
            r#"{"type":"response.output_item.done","item":{"type":"function_call","name":"lookup","arguments":""}}"#,
            false,
        ),
        (
            "response output item done function call non-empty args",
            r#"{"type":"response.output_item.done","item":{"type":"function_call","name":"lookup","arguments":"{\"q\":1}"}}"#,
            true,
        ),
        (
            "response output item done message empty",
            r#"{"type":"response.output_item.done","item":{"type":"message","content":[]}}"#,
            false,
        ),
        (
            "response output item done message with text",
            r#"{"type":"response.output_item.done","item":{"type":"message","content":[{"type":"text","text":"hello"}]}}"#,
            true,
        ),
        (
            "response completed fallback",
            r#"{"type":"response.completed","response":{"id":"resp_123","status":"completed"}}"#,
            true,
        ),
        (
            "response done fallback",
            r#"{"type":"response.done","response":{"id":"resp_123"}}"#,
            true,
        ),
        (
            "response incomplete fallback",
            r#"{"type":"response.incomplete","response":{"id":"resp_123","status":"incomplete"}}"#,
            true,
        ),
        (
            "response failed fallback",
            r#"{"type":"response.failed","response":{"id":"resp_123","status":"failed"}}"#,
            true,
        ),
        (
            "generic error fallback",
            r#"{"type":"error","error":{"message":"overloaded","code":"rate_limit_exceeded"}}"#,
            true,
        ),
        (
            "SSE data line with output text delta",
            r#"data: {"type":"response.output_text.delta","delta":"Hello SSE"}"#,
            true,
        ),
        (
            "SSE data line with response created",
            r#"data: {"type":"response.created","response":{"id":"resp_sse"}}"#,
            false,
        ),
    ];
    for (name, payload, want) in cases {
        assert_eq!(is_responses_token_event(payload.as_bytes()), want, "{name}");
    }
}

/// Gives `ttft` the classifier's answer for `payload` at `now` (upstream's
/// `ObserveResponsesTokenEvent`).
fn observe(ttft: &mut Ttft, payload: &str, now: Instant) {
    ttft.observe_token_event(is_responses_token_event(payload.as_bytes()), now);
}

/// Ports TestObserveResponsesTokenEvent_Behavior.
#[test]
fn observe_responses_token_event_behavior() {
    let start = Instant::now();
    let at = |millis| start + Duration::from_millis(millis);
    let mut ttft = Ttft::default();
    ttft.start(start);
    assert!(!ttft.is_set());

    observe(
        &mut ttft,
        r#"{"type":"codex.rate_limits","rate_limits":{"plan_type":"pro"}}"#,
        at(10),
    );
    assert!(!ttft.is_set(), "a metadata event set the TTFT");
    assert!(ttft.is_first_packet_set());

    observe(
        &mut ttft,
        r#"{"type":"response.created","response":{"id":"resp_1"}}"#,
        at(20),
    );
    assert!(!ttft.is_set(), "response.created set the TTFT");

    observe(
        &mut ttft,
        r#"{"type":"response.output_text.delta","delta":"First word"}"#,
        at(30),
    );
    assert!(ttft.is_set());
    assert_eq!(ttft.duration(), Duration::from_millis(30));

    observe(
        &mut ttft,
        r#"{"type":"response.output_text.delta","delta":"Second word"}"#,
        at(40),
    );
    assert_eq!(ttft.duration(), Duration::from_millis(30));
}

/// Ports TestObserveResponsesTokenEvent_FirstPacketFallback.
#[test]
fn observe_responses_token_event_first_packet_fallback() {
    let start = Instant::now();
    let mut ttft = Ttft::default();
    ttft.start(start);
    observe(
        &mut ttft,
        r#"{"type":"codex.rate_limits","rate_limits":{"plan_type":"pro"}}"#,
        start + Duration::from_millis(7),
    );
    observe(
        &mut ttft,
        r#"{"type":"response.created","response":{"id":"resp_1"}}"#,
        start + Duration::from_millis(9),
    );
    assert!(!ttft.is_set());
    assert!(ttft.is_first_packet_set());
    assert_eq!(ttft.duration(), Duration::from_millis(7));
}
