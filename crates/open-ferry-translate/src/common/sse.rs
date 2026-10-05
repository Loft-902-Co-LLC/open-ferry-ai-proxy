// Ported from CLIProxyAPI internal/translator/common/bytes.go (SSEEventData)
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Writing server-sent event frames, for the translators that answer in a
//! format whose stream names each event (OpenAI Responses and Gemini
//! Interactions).
//!
//! bytes.go's other helpers are not ported: `JoinRawArray`,
//! `SetRawArrayItems`, `NewRawArrayItems` and the `AppendSSEEvent*` family
//! only save allocations when building JSON and frames as bytes, and
//! `SetStringWithoutHTMLEscape` writes a string as serde_json always does,
//! leaving `<`, `>` and `&` unescaped (Go still escapes U+2028 and U+2029,
//! which read back the same).
//!
//! Deviations from upstream: none.

use serde_json::Value;

/// `SSEEventData`: appends one frame, `event: <event>`, then
/// `data: <payload>`, then a blank line, so frames written one after another
/// stay apart.
pub(crate) fn push_frame(out: &mut String, event: &str, payload: &str) {
    out.push_str("event: ");
    out.push_str(event);
    out.push_str("\ndata: ");
    out.push_str(payload);
    out.push_str("\n\n");
}

/// [`push_frame`] with `data` written as compact JSON.
pub(crate) fn push_event(out: &mut String, event: &str, data: &Value) {
    push_frame(out, event, &data.to_string());
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // Not upstream's: upstream has no test of SSEEventData of its own.
    #[test]
    fn frames_end_with_a_blank_line() {
        let mut out = String::new();
        push_event(&mut out, "step.delta", &json!({ "a": "<b>" }));
        push_frame(&mut out, "done", "[DONE]");
        assert_eq!(
            out,
            "event: step.delta\ndata: {\"a\":\"<b>\"}\n\nevent: done\ndata: [DONE]\n\n"
        );
    }
}
