//! The translators under test, run on our side, and how to read each one's
//! output as JSON so ours and upstream's can be compared.

use std::time::{SystemTime, UNIX_EPOCH};

use open_ferry_translate::codex::claude::{
    CodexToClaudeStream, convert_claude_request_to_codex,
    convert_claude_request_to_codex_with_compat, convert_codex_response_to_claude_non_stream,
};
use open_ferry_translate::codex::openai::chat_completions::{
    CodexToOpenAIChatCompletionsStream,
    convert_codex_response_to_openai_chat_completions_non_stream,
    convert_openai_chat_completions_request_to_codex,
};
use open_ferry_translate::codex::openai::responses::{
    CodexToOpenAIResponsesStream, convert_codex_response_to_openai_responses_non_stream,
    convert_openai_responses_request_to_codex,
};
use serde_json::{Value, json};

use crate::cases::Case;
use crate::signature;

/// What a generated tool ID is replaced with before comparing.
const GENERATED_TOOL_ID: &str = "toolu_(generated)";

/// How an empty non-streaming output reads, unlike any JSON a response holds.
const NO_OUTPUT: &str = "(no output)";

/// How the Responses stream translator's harness writes a line it returned unchanged.
const UNCHANGED: &str = "=";

/// What a Chat Completions response's `created` is replaced with when it is
/// the current time, which upstream and we each read from the clock.
const CREATED_NOW: &str = "(now)";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Translator {
    /// Claude Messages request → Codex request.
    Request,
    /// The same in compatibility mode, which keeps more thinking blocks.
    RequestCompat,
    /// Codex event stream → Claude SSE events.
    Stream,
    /// The final Codex event → one Claude message.
    NonStream,
    /// OpenAI Responses request → Codex request.
    ResponsesRequest,
    /// Codex event stream → Responses events.
    ResponsesStream,
    /// The final Codex event → one Responses response.
    ResponsesNonStream,
    /// OpenAI Chat Completions request → Codex request.
    ChatRequest,
    /// Codex event stream → Chat Completions chunks.
    ChatStream,
    /// The final Codex event → one Chat Completions response.
    ChatNonStream,
    /// One reasoning signature → every check and replay decision on it.
    SignatureInspect,
    /// A Claude Messages request → its signed history stripped and sanitized.
    ClaudeMessagesSignatures,
    /// A Gemini request → its thought signatures sanitized and validated.
    GeminiSignatures,
}

impl Translator {
    /// The harness's name for the translator (see `go/main.go`).
    pub fn key(self) -> &'static str {
        match self {
            Self::Request => "codex/claude/request",
            Self::RequestCompat => "codex/claude/request-compat",
            Self::Stream => "codex/claude/response",
            Self::NonStream => "codex/claude/response-non-stream",
            Self::ResponsesRequest => "codex/openai-responses/request",
            Self::ResponsesStream => "codex/openai-responses/response",
            Self::ResponsesNonStream => "codex/openai-responses/response-non-stream",
            Self::ChatRequest => "codex/openai-chat/request",
            Self::ChatStream => "codex/openai-chat/response",
            Self::ChatNonStream => "codex/openai-chat/response-non-stream",
            Self::SignatureInspect => "signature/inspect",
            Self::ClaudeMessagesSignatures => "signature/claude-messages",
            Self::GeminiSignatures => "signature/gemini",
        }
    }

    /// A short name for directories.
    pub fn slug(self) -> &'static str {
        match self {
            Self::Request => "claude-request",
            Self::RequestCompat => "claude-request-compat",
            Self::Stream => "claude-stream",
            Self::NonStream => "claude-non-stream",
            Self::ResponsesRequest => "responses-request",
            Self::ResponsesStream => "responses-stream",
            Self::ResponsesNonStream => "responses-non-stream",
            Self::ChatRequest => "chat-request",
            Self::ChatStream => "chat-stream",
            Self::ChatNonStream => "chat-non-stream",
            Self::SignatureInspect => "signature-inspect",
            Self::ClaudeMessagesSignatures => "signature-claude-messages",
            Self::GeminiSignatures => "signature-gemini",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Request => "Claude -> Codex request",
            Self::RequestCompat => "Claude -> Codex request, compatibility mode",
            Self::Stream => "Codex -> Claude response, streaming",
            Self::NonStream => "Codex -> Claude response, non-streaming",
            Self::ResponsesRequest => "Responses -> Codex request",
            Self::ResponsesStream => "Codex -> Responses response, streaming",
            Self::ResponsesNonStream => "Codex -> Responses response, non-streaming",
            Self::ChatRequest => "Chat Completions -> Codex request",
            Self::ChatStream => "Codex -> Chat Completions response, streaming",
            Self::ChatNonStream => "Codex -> Chat Completions response, non-streaming",
            Self::SignatureInspect => "Signature checks and replay decisions",
            Self::ClaudeMessagesSignatures => "Claude Messages signature sanitizers",
            Self::GeminiSignatures => "Gemini thought signature sanitizer and validators",
        }
    }

    /// Runs our port on `case`, returning its output in the form [`Self::read`] gives.
    pub fn run_rust(self, case: &Case) -> Result<Value, String> {
        let request = serde_json::from_str::<Value>(&case.request);
        let final_event = || {
            case.events
                .first()
                .and_then(|event| serde_json::from_str(event).ok())
                .unwrap_or_default()
        };
        match self {
            Self::Request => {
                let request = request
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                Ok(convert_claude_request_to_codex(&case.model, &request))
            }
            Self::RequestCompat => {
                let request = request
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                Ok(convert_claude_request_to_codex_with_compat(
                    &case.model,
                    &request,
                ))
            }
            Self::SignatureInspect => Ok(signature::inspect(
                &case.model,
                &case.request,
                &case.options,
            )),
            Self::ClaudeMessagesSignatures => {
                let request = request
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                Ok(signature::claude_messages(
                    &case.model,
                    &request,
                    &case.options,
                ))
            }
            Self::GeminiSignatures => {
                let request = request
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                Ok(signature::gemini(&request, &case.options))
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
                let output = convert_codex_response_to_claude_non_stream(
                    &request.unwrap_or_default(),
                    &final_event(),
                );
                let output = output.map(|value| value.to_string()).unwrap_or_default();
                self.read(output.as_bytes())
                    .ok_or_else(|| "output is not JSON".to_owned())
            }
            Self::ResponsesRequest => {
                let request = request
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                Ok(convert_openai_responses_request_to_codex(
                    &case.model,
                    request,
                ))
            }
            Self::ResponsesStream => {
                let translated = serde_json::from_str(&case.translated_request).unwrap_or_default();
                let stream = CodexToOpenAIResponsesStream::new(
                    &case.model,
                    &request.unwrap_or_default(),
                    &translated,
                );
                // Written as the harness writes upstream's output.
                let lines: Vec<String> = case
                    .events
                    .iter()
                    .map(|line| {
                        let output = stream.translate_line(line.as_bytes());
                        if *output == *line.as_bytes() {
                            UNCHANGED.to_owned()
                        } else {
                            String::from_utf8_lossy(&output).into_owned()
                        }
                    })
                    .collect();
                let output = serde_json::to_vec(&lines).expect("strings serialize");
                Ok(self.read(&output).expect("streams always read"))
            }
            Self::ResponsesNonStream => {
                let output = convert_codex_response_to_openai_responses_non_stream(final_event());
                let output = output.map(|value| value.to_string()).unwrap_or_default();
                self.read(output.as_bytes())
                    .ok_or_else(|| "output is not JSON".to_owned())
            }
            Self::ChatRequest => {
                let request = request
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                Ok(convert_openai_chat_completions_request_to_codex(
                    &case.model,
                    &request,
                    true,
                ))
            }
            Self::ChatStream => {
                let mut stream = CodexToOpenAIChatCompletionsStream::new(
                    &case.model,
                    &request.unwrap_or_default(),
                );
                // Written as the harness writes upstream's output.
                let chunks: Vec<String> = case
                    .events
                    .iter()
                    .filter_map(|line| stream.translate_line(line.as_bytes()))
                    .map(|chunk| chunk.to_string())
                    .collect();
                let output = serde_json::to_vec(&chunks).expect("strings serialize");
                Ok(self.read(&output).expect("streams always read"))
            }
            Self::ChatNonStream => {
                let output = convert_codex_response_to_openai_chat_completions_non_stream(
                    &request.unwrap_or_default(),
                    &final_event(),
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
    /// A Claude stream becomes an array of `{"event", "data"}` frames, and a
    /// Responses or Chat Completions stream an array with an entry per line
    /// (see [`read_lines`]). An empty non-streaming output (no response) reads
    /// as [`NO_OUTPUT`]. In Claude responses, tool IDs generated for calls
    /// without one are masked, since they hold a timestamp. So is a Chat
    /// Completions response's `created` when it is the current time.
    pub fn read(self, output: &[u8]) -> Option<Value> {
        let text = String::from_utf8_lossy(output);
        let mut value = match self {
            Self::Request
            | Self::RequestCompat
            | Self::ResponsesRequest
            | Self::SignatureInspect
            | Self::ClaudeMessagesSignatures
            | Self::GeminiSignatures
            | Self::ChatRequest => return serde_json::from_str(&text).ok(),
            Self::ResponsesStream | Self::ChatStream => return read_lines(&text),
            Self::NonStream | Self::ResponsesNonStream | Self::ChatNonStream if text.is_empty() => {
                return Some(NO_OUTPUT.into());
            }
            Self::ResponsesNonStream => return serde_json::from_str(&text).ok(),
            Self::ChatNonStream => {
                let mut value: Value = serde_json::from_str(&text).ok()?;
                mask_created_now(&mut value);
                return Some(value);
            }
            Self::Stream => sse_frames(&text),
            Self::NonStream => serde_json::from_str(&text).ok()?,
        };
        mask_generated_tool_ids(&mut value);
        Some(value)
    }
}

/// Reads the Responses stream translator's output: a JSON array with a
/// string per output line, or `=` for a line returned unchanged. A changed
/// line becomes `{"data": …}` for an SSE data line, `{"json": …}` for a bare
/// JSON line, and `{"line": text}` for anything else.
fn read_lines(text: &str) -> Option<Value> {
    let lines: Vec<String> = serde_json::from_str(text).ok()?;
    let json = |text: &str| serde_json::from_str::<Value>(text).ok();
    let lines = lines
        .into_iter()
        .map(|line| {
            if line == UNCHANGED {
                Value::String(line)
            } else if let Some(data) = line.strip_prefix("data: ").and_then(json) {
                json!({ "data": data })
            } else if let Some(value) = json(&line) {
                json!({ "json": value })
            } else {
                json!({ "line": line })
            }
        })
        .collect();
    Some(Value::Array(lines))
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

/// Replaces a response's `created` time if it is within an hour of now.
fn mask_created_now(value: &mut Value) {
    let Some(created) = value.get_mut("created") else {
        return;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    if created
        .as_u64()
        .is_some_and(|created| created.abs_diff(now) < 3600)
    {
        *created = CREATED_NOW.into();
    }
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
    fn responses_lines_read_by_kind() {
        let text = json!(["=", "data: {\"x\":1}", "{\"y\":2}", "data: [DONE]"]).to_string();
        assert_eq!(
            read_lines(&text),
            Some(json!([
                "=",
                { "data": { "x": 1 } },
                { "json": { "y": 2 } },
                { "line": "data: [DONE]" }
            ]))
        );
        assert_eq!(read_lines("not json"), None);
    }

    #[test]
    fn only_a_current_created_time_is_masked() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut current = json!({ "created": now - 5 });
        mask_created_now(&mut current);
        assert_eq!(current["created"], CREATED_NOW);
        let mut past = json!({ "created": 1_700_000_000 });
        mask_created_now(&mut past);
        assert_eq!(past["created"], 1_700_000_000);
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
