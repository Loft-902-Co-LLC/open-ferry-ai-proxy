// Ported from CLIProxyAPI internal/translator/gemini/openai/responses/gemini_openai-responses_request_test.go
// (testResponsesGeminiThoughtSignature), gemini_openai-responses_response_test.go
// (parseSSEEvent, differentResponsesGeminiThoughtSignature) and
// gemini_openai-responses_web_search_test.go (collectResponsesStreamEvents) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Fixtures shared by this translator's tests.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;

/// `testResponsesGeminiThoughtSignature`: a valid Gemini thought signature.
pub(super) const GEMINI_SIGNATURE: &str =
    "EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA";

/// `differentResponsesGeminiThoughtSignature`: [`GEMINI_SIGNATURE`] with its
/// last bit flipped, so a second valid signature.
pub(super) fn different_gemini_signature() -> String {
    let mut raw = STANDARD
        .decode(GEMINI_SIGNATURE)
        .expect("the test signature is base64");
    let last = raw.len() - 1;
    raw[last] ^= 1;
    STANDARD.encode(raw)
}

/// Splits translator output into its SSE events: each event's name and data.
pub(super) fn sse_events(output: &str) -> Vec<(String, Value)> {
    output
        .split("\n\n")
        .filter(|frame| !frame.trim().is_empty())
        .map(parse_sse_event)
        .collect()
}

/// `parseSSEEvent`: one SSE frame's event name and data.
pub(super) fn parse_sse_event(frame: &str) -> (String, Value) {
    let mut lines = frame.split('\n');
    let (Some(event), Some(data)) = (lines.next(), lines.next()) else {
        panic!("unexpected SSE frame: {frame:?}");
    };
    let event = event.strip_prefix("event:").unwrap_or(event).trim();
    let data = data.strip_prefix("data:").unwrap_or(data).trim();
    let data = serde_json::from_str(data)
        .unwrap_or_else(|err| panic!("invalid SSE data JSON {data:?}: {err}"));
    (event.to_owned(), data)
}

/// `collectResponsesStreamEvents`: the event names in order, and each
/// event's data by name.
pub(super) fn events_by_type(
    events: &[(String, Value)],
) -> (Vec<String>, std::collections::HashMap<String, Vec<Value>>) {
    let mut names = Vec::new();
    let mut by_type: std::collections::HashMap<String, Vec<Value>> = Default::default();
    for (name, data) in events {
        names.push(name.clone());
        by_type.entry(name.clone()).or_default().push(data.clone());
    }
    (names, by_type)
}
