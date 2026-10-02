//! The translators under test, run on our side, and how to read each one's
//! output as JSON so ours and upstream's can be compared.

use open_ferry_translate::codex::claude::{
    CodexToClaudeStream, convert_claude_request_to_codex,
    convert_codex_response_to_claude_non_stream,
};
use serde_json::{Value, json};

use crate::cases::Case;

/// What a generated tool ID is replaced with before comparing.
const GENERATED_TOOL_ID: &str = "toolu_(generated)";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Translator {
    /// Claude Messages request → Codex request.
    Request,
    /// Codex event stream → Claude SSE events.
    Stream,
    /// The final Codex event → one Claude message.
    NonStream,
}

impl Translator {
    /// The harness's name for the translator (see `go/main.go`).
    pub fn key(self) -> &'static str {
        match self {
            Self::Request => "codex/claude/request",
            Self::Stream => "codex/claude/response",
            Self::NonStream => "codex/claude/response-non-stream",
        }
    }

    /// A short name for directories.
    pub fn slug(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Stream => "stream",
            Self::NonStream => "non-stream",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Request => "Claude -> Codex request",
            Self::Stream => "Codex -> Claude response, streaming",
            Self::NonStream => "Codex -> Claude response, non-streaming",
        }
    }

    /// Runs our port on `case`, returning its output in the form [`Self::read`] gives.
    pub fn run_rust(self, case: &Case) -> Result<Value, String> {
        let request = serde_json::from_str::<Value>(&case.request);
        match self {
            Self::Request => {
                let request = request
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                Ok(convert_claude_request_to_codex(&case.model, &request))
            }
            Self::Stream => {
                let mut stream = CodexToClaudeStream::new(&request.unwrap_or_default());
                let output: String = case
                    .events
                    .iter()
                    .map(|line| stream.translate_line(line.as_bytes()))
                    .collect();
                Ok(self.read(output.as_bytes()).expect("streams always read"))
            }
            Self::NonStream => {
                let event = case
                    .events
                    .first()
                    .and_then(|event| serde_json::from_str(event).ok())
                    .unwrap_or_default();
                let output = convert_codex_response_to_claude_non_stream(
                    &request.unwrap_or_default(),
                    &event,
                );
                let output = output.map(|value| value.to_string()).unwrap_or_default();
                self.read(output.as_bytes())
                    .ok_or_else(|| "output is not JSON".to_owned())
            }
        }
    }

    /// Reads a translator's raw output as JSON, or `None` if it isn't the
    /// kind of output the translator should produce.
    ///
    /// A stream becomes an array of `{"event", "data"}` frames, and an empty
    /// non-streaming output (no message) becomes null. In responses, tool IDs
    /// generated for calls without one are masked, since they hold a timestamp.
    pub fn read(self, output: &[u8]) -> Option<Value> {
        let text = String::from_utf8_lossy(output);
        let mut value = match self {
            Self::Request => return serde_json::from_str(&text).ok(),
            Self::Stream => sse_frames(&text),
            Self::NonStream if text.is_empty() => Value::Null,
            Self::NonStream => serde_json::from_str(&text).ok()?,
        };
        mask_generated_tool_ids(&mut value);
        Some(value)
    }
}

/// Splits SSE text into `{"event": …, "data": …}` frames. Anything that isn't
/// an `event: …\ndata: <JSON>\n\n` frame is kept as `{"unparsed": text}`, so
/// it shows up as a difference.
fn sse_frames(text: &str) -> Value {
    let frames = text
        .split_terminator("\n\n")
        .map(|frame| {
            frame
                .strip_prefix("event: ")
                .and_then(|rest| rest.split_once("\ndata: "))
                .and_then(|(event, data)| {
                    let data: Value = serde_json::from_str(data).ok()?;
                    Some(json!({ "event": event, "data": data }))
                })
                .unwrap_or_else(|| json!({ "unparsed": frame }))
        })
        .collect();
    Value::Array(frames)
}

/// Replaces `toolu_<unix nanos>_<counter>`, the ID upstream and we generate for
/// a call that has none.
fn mask_generated_tool_ids(value: &mut Value) {
    match value {
        Value::String(text) if is_generated_tool_id(text) => *text = GENERATED_TOOL_ID.into(),
        Value::Array(items) => items.iter_mut().for_each(mask_generated_tool_ids),
        Value::Object(fields) => fields.values_mut().for_each(mask_generated_tool_ids),
        _ => {}
    }
}

fn is_generated_tool_id(text: &str) -> bool {
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    text.strip_prefix("toolu_")
        .and_then(|rest| rest.split_once('_'))
        .is_some_and(|(nanos, counter)| digits(nanos) && digits(counter))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_text_becomes_frames() {
        let text = "event: a\ndata: {\"x\":1}\n\nevent: b\ndata: not json\n\n";
        assert_eq!(
            sse_frames(text),
            json!([
                { "event": "a", "data": { "x": 1 } },
                { "unparsed": "event: b\ndata: not json" }
            ])
        );
        assert_eq!(sse_frames(""), json!([]));
    }

    #[test]
    fn only_generated_tool_ids_are_masked() {
        let mut value =
            json!({ "ids": ["toolu_1759400000000000000_3", "toolu_01ABC", "toolu_1_x"] });
        mask_generated_tool_ids(&mut value);
        assert_eq!(
            value["ids"],
            json!([GENERATED_TOOL_ID, "toolu_01ABC", "toolu_1_x"])
        );
    }
}
