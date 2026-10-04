//! The OpenAI Responses and Interactions response translators' suites (P4
//! WP4-C2), all tr/openai/interactions/responses:
//! - `interactions/openai-responses/response`, `.../response-non-stream`
//!   and `.../tool-input-error`: Interactions responses for OpenAI
//!   Responses clients. The last runs the stream, then FinalizeToolInput,
//!   and reports `{"events", "finalize", "failed"}`: the stream's SSE text,
//!   what FinalizeToolInput added, and whether a tool input error is set.
//!   Errors are compared only by whether there is one, since the port's
//!   messages are its own.
//! - `openai-responses/interactions/response` and
//!   `.../response-non-stream`: Responses responses for Interactions
//!   clients.
//!
//! Streams are read as SSE frames (see `translator::sse_frames`), and a
//! whole response as JSON, or [`NO_OUTPUT`] when there is none. The IDs and
//! times upstream reads from the clock are masked (see [`mask_volatile`]).
//! A call's arguments object in a whole Interactions response is written
//! compactly by the port where upstream copies its text (see the port's
//! module docs), so the non-streaming suite allows that in the output's
//! `arguments` and `input`.
//!
//! Hand-written cases are in [`cases`], and random ones in
//! `crate::generate::interactions::responses::response`.
//!
//! The pairs are not registered yet (WP4-C1's request translators are not on
//! main), so [`Family::native`] maps nothing and the `registry_*` functions
//! give no cases. Once they are, `native` maps the response stages of
//! `interactions` → `openai-response` and `openai-response` →
//! `interactions` to these suites, and the `registry_*` functions give
//! their cases.

mod cases;

use open_ferry_translate::openai::interactions::responses::{
    InteractionsToOpenAIResponsesStream, OpenAIResponsesToInteractionsStream,
    convert_interactions_response_to_openai_responses_non_stream,
    convert_openai_responses_response_to_interactions_non_stream,
};
use serde_json::{Value, json};

use super::super::{Family, Pair, ResponseCases, Stage, mask_volatile};
use crate::cases::Case;
use crate::compare::{Deviation, JsonAt, JsonForm};
use crate::generate::interactions::responses::response as generate;
use crate::translator::{NO_OUTPUT, sse_frames};

/// The suites, a variant each.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `interactions/openai-responses/response`.
    ToResponsesStream,
    /// `interactions/openai-responses/response-non-stream`.
    ToResponsesNonStream,
    /// `interactions/openai-responses/tool-input-error`.
    ToolInputError,
    /// `openai-responses/interactions/response`.
    ToInteractionsStream,
    /// `openai-responses/interactions/response-non-stream`.
    ToInteractionsNonStream,
}

/// Every suite, in the order they run.
pub const KINDS: &[Kind] = &[
    Kind::ToResponsesStream,
    Kind::ToResponsesNonStream,
    Kind::ToolInputError,
    Kind::ToInteractionsStream,
    Kind::ToInteractionsNonStream,
];

/// Where the Interactions → Responses non-streaming translator writes a
/// call's arguments compactly, where upstream copies their text.
const TO_RESPONSES_NON_STREAM_JSON: &[JsonAt] = &[
    ("$.output[*].arguments", JsonForm::Whole),
    ("$.output[*].input", JsonForm::Whole),
];

/// A JSON request, or `Null` for one that is empty or isn't JSON, which
/// the port counts as absent.
fn request_json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_default()
}

impl Family for Kind {
    fn key(self) -> &'static str {
        match self {
            Self::ToResponsesStream => "interactions/openai-responses/response",
            Self::ToResponsesNonStream => "interactions/openai-responses/response-non-stream",
            Self::ToolInputError => "interactions/openai-responses/tool-input-error",
            Self::ToInteractionsStream => "openai-responses/interactions/response",
            Self::ToInteractionsNonStream => "openai-responses/interactions/response-non-stream",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Self::ToResponsesStream => "interactions-to-responses-stream",
            Self::ToResponsesNonStream => "interactions-to-responses-non-stream",
            Self::ToolInputError => "interactions-to-responses-tool-input-error",
            Self::ToInteractionsStream => "responses-to-interactions-stream",
            Self::ToInteractionsNonStream => "responses-to-interactions-non-stream",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::ToResponsesStream => "Interactions -> Responses response, streaming",
            Self::ToResponsesNonStream => "Interactions -> Responses response, non-streaming",
            Self::ToolInputError => {
                "Interactions -> Responses tool input error (FinalizeToolInput)"
            }
            Self::ToInteractionsStream => "Responses -> Interactions response, streaming",
            Self::ToInteractionsNonStream => "Responses -> Interactions response, non-streaming",
        }
    }

    fn cases(self) -> Vec<Case> {
        match self {
            Self::ToResponsesStream | Self::ToolInputError => cases::to_responses_streams(),
            Self::ToResponsesNonStream => cases::to_responses_finals(),
            Self::ToInteractionsStream => cases::to_interactions_streams(),
            Self::ToInteractionsNonStream => cases::to_interactions_finals(),
        }
    }

    fn generate(self, seed: u64, count: usize) -> Vec<Case> {
        match self {
            Self::ToResponsesStream => generate::to_responses_cases(seed, count).0,
            Self::ToResponsesNonStream => generate::to_responses_cases(seed, count).1,
            Self::ToolInputError => generate::tool_input_cases(seed, count),
            Self::ToInteractionsStream => generate::to_interactions_cases(seed, count).0,
            Self::ToInteractionsNonStream => generate::to_interactions_cases(seed, count).1,
        }
    }

    fn run(self, case: &Case) -> Result<Value, String> {
        let body = case.events.first().map_or(&b""[..], |body| body.as_bytes());
        let output = match self {
            Self::ToResponsesStream | Self::ToolInputError => {
                let mut stream = InteractionsToOpenAIResponsesStream::new(
                    &case.model,
                    &request_json(&case.request),
                    &request_json(&case.translated_request),
                );
                let events: String = case
                    .events
                    .iter()
                    .map(|line| stream.translate(line.as_bytes()))
                    .collect();
                if self == Self::ToResponsesStream {
                    events
                } else if case.events.is_empty() {
                    // Upstream's stream state only exists once an event has
                    // come, so the harness has nothing to finalize.
                    json!({ "events": "", "finalize": "", "failed": false }).to_string()
                } else {
                    let finalize = stream.finalize_tool_input();
                    let failed = stream.tool_input_error().is_some();
                    json!({ "events": events, "finalize": finalize, "failed": failed }).to_string()
                }
            }
            Self::ToResponsesNonStream => {
                convert_interactions_response_to_openai_responses_non_stream(
                    &case.model,
                    &request_json(&case.request),
                    &request_json(&case.translated_request),
                    body,
                )
                .map(|response| response.to_string())
                .unwrap_or_default()
            }
            Self::ToInteractionsStream => {
                let mut stream = OpenAIResponsesToInteractionsStream::new(&case.model);
                case.events
                    .iter()
                    .map(|line| stream.translate(line.as_bytes()))
                    .collect()
            }
            Self::ToInteractionsNonStream => {
                convert_openai_responses_response_to_interactions_non_stream(&case.model, body)
                    .to_string()
            }
        };
        self.read(case, output.as_bytes())
            .ok_or_else(|| "output is not of the expected kind".to_owned())
    }

    fn read(self, case: &Case, output: &[u8]) -> Option<Value> {
        let _ = case;
        let text = String::from_utf8_lossy(output);
        let mut value = match self {
            Self::ToResponsesStream | Self::ToInteractionsStream => sse_frames(&text),
            Self::ToolInputError => {
                let report: Value = serde_json::from_str(&text).ok()?;
                json!({
                    "events": sse_frames(report.get("events")?.as_str()?),
                    "finalize": sse_frames(report.get("finalize")?.as_str()?),
                    "failed": report.get("failed")?.as_bool()?,
                })
            }
            Self::ToResponsesNonStream | Self::ToInteractionsNonStream => {
                if text.is_empty() {
                    return Some(NO_OUTPUT.into());
                }
                serde_json::from_str(&text).ok()?
            }
        };
        mask_volatile(&mut value);
        Some(value)
    }

    fn embedded_json(self, case: &Case) -> &'static [JsonAt] {
        let _ = case;
        match self {
            Self::ToResponsesNonStream => TO_RESPONSES_NON_STREAM_JSON,
            _ => &[],
        }
    }

    fn drop_deliberate_omissions(self, case: &Case, go: &mut Value) -> Option<Deviation> {
        let _ = (self, case, go);
        None
    }

    fn joins_stream(self) -> bool {
        matches!(self, Self::ToResponsesStream | Self::ToInteractionsStream)
    }

    fn native(stage: Stage, from: &str, to: &str) -> Option<Self> {
        let _ = (stage, from, to);
        None
    }
}

/// The hand-written registry stream cases for the pairs, each list
/// with its pair. None until the pairs are registered.
pub fn registry_streams() -> Vec<(Pair, Vec<Case>)> {
    Vec::new()
}

/// The hand-written registry non-streaming cases for the pairs, each
/// list with its pair. None until the pairs are registered.
pub fn registry_finals() -> Vec<(Pair, Vec<Case>)> {
    Vec::new()
}

/// `count` random registry stream cases, and as many non-streaming ones,
/// for each of the pairs. None until the pairs are registered.
pub fn registry_response_cases(seed: u64, count: usize) -> Vec<ResponseCases> {
    let _ = (seed, count);
    Vec::new()
}
