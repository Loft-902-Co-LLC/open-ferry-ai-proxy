//! The Gemini and Interactions translators' suites (P4 WP4-E), whose Go
//! entries are in `go/interactions/parity_gemini.go`. All run
//! tr/gemini/interactions:
//! - `gemini/interactions/request`, `.../response` and
//!   `.../response-non-stream`: Interactions clients to a Gemini upstream;
//! - `interactions/gemini/request`, `.../response` and
//!   `.../response-non-stream`: Gemini clients to an Interactions upstream;
//! - `interactions/interactions/request`, `.../response` and
//!   `.../response-non-stream`: Interactions passed through.
//!
//! The hand-written cases are in [`cases`], and random ones in
//! `crate::generate::interactions::gemini`. The registry runs the same
//! translators for its pairs `interactions` → `gemini`, `gemini` →
//! `interactions` and `interactions` → `interactions`, with the same cases.
//!
//! A stream to an Interactions client is read as its SSE frames, the others
//! as a JSON array of their chunks, each read as JSON if it is. The IDs and
//! times upstream reads from the clock are masked (see
//! [`super::mask_volatile`]), step IDs all alike: Go's clock on Windows can
//! give two steps the same one, where ours doesn't.

mod cases;

use open_ferry_translate::gemini::interactions::{
    GeminiToInteractionsStream, InteractionsToGeminiStream, convert_gemini_request_to_interactions,
    convert_gemini_response_to_interactions_non_stream, convert_interactions_request_to_gemini,
    convert_interactions_request_to_interactions, convert_interactions_response_passthrough,
    convert_interactions_response_passthrough_non_stream,
    convert_interactions_response_to_gemini_non_stream,
};
use open_ferry_translate::json::exact;
use serde_json::Value;

use super::{Family, Pair, ResponseCases, Stage, Suite, mask_volatile};
use crate::cases::Case;
use crate::compare::{Deviation, JsonAt, JsonForm};
use crate::generate::interactions::gemini as generate;
use crate::translator::{Translator, sse_frames};

/// The family's suites, a variant each.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Interactions request → Gemini request.
    InteractionsToGeminiRequest,
    /// Gemini stream → Interactions events.
    GeminiToInteractionsStream,
    /// Gemini response → Interactions response.
    GeminiToInteractionsNonStream,
    /// Gemini request → Interactions request.
    GeminiToInteractionsRequest,
    /// Interactions events → Gemini stream.
    InteractionsToGeminiStream,
    /// Interactions response → Gemini response.
    InteractionsToGeminiNonStream,
    /// An Interactions request passed through.
    PassthroughRequest,
    /// Interactions events passed through.
    PassthroughStream,
    /// An Interactions response passed through.
    PassthroughNonStream,
}

/// Every suite, in the order they run.
pub const KINDS: &[Kind] = &[
    Kind::InteractionsToGeminiRequest,
    Kind::GeminiToInteractionsStream,
    Kind::GeminiToInteractionsNonStream,
    Kind::GeminiToInteractionsRequest,
    Kind::InteractionsToGeminiStream,
    Kind::InteractionsToGeminiNonStream,
    Kind::PassthroughRequest,
    Kind::PassthroughStream,
    Kind::PassthroughNonStream,
];

/// The suites, with their cases.
pub fn suites(seed: u64, random: usize) -> Vec<Suite> {
    KINDS
        .iter()
        .map(|&kind| {
            let translator = Translator::Interactions(super::Kind::Gemini(kind));
            (translator, kind.cases(), kind.generate(seed, random))
        })
        .collect()
}

/// Where the Interactions to Gemini request translator writes text it read
/// as a string, which may be an object's JSON.
const TO_GEMINI_REQUEST_JSON: &[JsonAt] = &[
    ("$.contents[*].parts[*].text", JsonForm::Whole),
    ("$.contents[*].parts[*].thoughtSignature", JsonForm::Whole),
    ("$.contents[*].parts[*].functionCall.name", JsonForm::Whole),
    ("$.contents[*].parts[*].functionCall.id", JsonForm::Whole),
    (
        "$.contents[*].parts[*].functionResponse.name",
        JsonForm::Whole,
    ),
    (
        "$.contents[*].parts[*].functionResponse.id",
        JsonForm::Whole,
    ),
    (
        "$.contents[*].parts[*].functionResponse.response.result",
        JsonForm::Whole,
    ),
    (
        "$.contents[*].parts[*].inlineData.mimeType",
        JsonForm::Whole,
    ),
    ("$.contents[*].parts[*].inlineData.data", JsonForm::Whole),
    ("$.contents[*].parts[*].fileData.mimeType", JsonForm::Whole),
    ("$.contents[*].parts[*].fileData.fileUri", JsonForm::Whole),
    ("$.systemInstruction.parts[*].text", JsonForm::Whole),
    ("$.tools[*].functionDeclarations[*].name", JsonForm::Whole),
    (
        "$.tools[*].functionDeclarations[*].description",
        JsonForm::Whole,
    ),
    (
        "$.toolConfig.functionCallingConfig.allowedFunctionNames[*]",
        JsonForm::Whole,
    ),
];

/// Where the Gemini to Interactions request translator writes text it read
/// as a string.
const TO_INTERACTIONS_REQUEST_JSON: &[JsonAt] = &[
    ("$.system_instruction", JsonForm::InText),
    ("$.generation_config.thinking_level", JsonForm::Whole),
    ("$.input[*].content[*].text", JsonForm::Whole),
    ("$.input[*].content[*].mime_type", JsonForm::Whole),
    ("$.input[*].content[*].data", JsonForm::Whole),
    ("$.input[*].name", JsonForm::Whole),
    ("$.input[*].call_id", JsonForm::Whole),
    ("$.input[*].signature", JsonForm::Whole),
    ("$.tools[*].name", JsonForm::Whole),
    ("$.tools[*].description", JsonForm::Whole),
];

/// Where the Gemini to Interactions stream translator writes text it read as
/// a string.
const TO_INTERACTIONS_STREAM_JSON: &[JsonAt] = &[
    ("$[*].data.delta.arguments", JsonForm::Whole),
    ("$[*].data.delta.text", JsonForm::Whole),
    ("$[*].data.delta.content.text", JsonForm::Whole),
    ("$[*].data.delta.signature", JsonForm::Whole),
    ("$[*].data.delta.name", JsonForm::Whole),
    ("$[*].data.step.id", JsonForm::Whole),
    ("$[*].data.step.name", JsonForm::Whole),
];

/// Where the Gemini to Interactions response translator writes text it read
/// as a string.
const TO_INTERACTIONS_RESPONSE_JSON: &[JsonAt] = &[
    ("$.id", JsonForm::Whole),
    ("$.steps[*].content[*].text", JsonForm::Whole),
    ("$.steps[*].content[*].mime_type", JsonForm::Whole),
    ("$.steps[*].content[*].data", JsonForm::Whole),
    ("$.steps[*].signature", JsonForm::Whole),
    ("$.steps[*].name", JsonForm::Whole),
    ("$.steps[*].call_id", JsonForm::Whole),
];

/// Where the Interactions to Gemini stream translator writes text it read as
/// a string.
const TO_GEMINI_STREAM_JSON: &[JsonAt] = &[
    ("$[*].candidates[*].content.parts[*].text", JsonForm::Whole),
    (
        "$[*].candidates[*].content.parts[*].thoughtSignature",
        JsonForm::Whole,
    ),
    (
        "$[*].candidates[*].content.parts[*].functionCall.name",
        JsonForm::Whole,
    ),
    (
        "$[*].candidates[*].content.parts[*].functionCall.id",
        JsonForm::Whole,
    ),
    (
        "$[*].candidates[*].content.parts[*].functionResponse.name",
        JsonForm::Whole,
    ),
    (
        "$[*].candidates[*].content.parts[*].functionResponse.id",
        JsonForm::Whole,
    ),
    (
        "$[*].candidates[*].content.parts[*].functionResponse.response.result",
        JsonForm::Whole,
    ),
    (
        "$[*].candidates[*].content.parts[*].inlineData.mimeType",
        JsonForm::Whole,
    ),
    (
        "$[*].candidates[*].content.parts[*].inlineData.data",
        JsonForm::Whole,
    ),
    (
        "$[*].candidates[*].content.parts[*].fileData.mimeType",
        JsonForm::Whole,
    ),
    (
        "$[*].candidates[*].content.parts[*].fileData.fileUri",
        JsonForm::Whole,
    ),
    ("$[*].modelVersion", JsonForm::Whole),
    ("$[*].responseId", JsonForm::Whole),
    ("$[*].usageMetadata.serviceTier", JsonForm::Whole),
    ("$[*].error.message", JsonForm::Whole),
];

/// Where the Interactions to Gemini response translator writes text it read
/// as a string.
const TO_GEMINI_RESPONSE_JSON: &[JsonAt] = &[
    ("$.candidates[*].content.parts[*].text", JsonForm::Whole),
    (
        "$.candidates[*].content.parts[*].thoughtSignature",
        JsonForm::Whole,
    ),
    (
        "$.candidates[*].content.parts[*].functionCall.name",
        JsonForm::Whole,
    ),
    (
        "$.candidates[*].content.parts[*].functionCall.id",
        JsonForm::Whole,
    ),
    (
        "$.candidates[*].content.parts[*].functionResponse.name",
        JsonForm::Whole,
    ),
    (
        "$.candidates[*].content.parts[*].functionResponse.id",
        JsonForm::Whole,
    ),
    (
        "$.candidates[*].content.parts[*].functionResponse.response.result",
        JsonForm::Whole,
    ),
    (
        "$.candidates[*].content.parts[*].inlineData.mimeType",
        JsonForm::Whole,
    ),
    (
        "$.candidates[*].content.parts[*].inlineData.data",
        JsonForm::Whole,
    ),
    (
        "$.candidates[*].content.parts[*].fileData.mimeType",
        JsonForm::Whole,
    ),
    (
        "$.candidates[*].content.parts[*].fileData.fileUri",
        JsonForm::Whole,
    ),
    ("$.modelVersion", JsonForm::Whole),
    ("$.responseId", JsonForm::Whole),
    ("$.usageMetadata.serviceTier", JsonForm::Whole),
    ("$.error.message", JsonForm::Whole),
];

impl Family for Kind {
    fn key(self) -> &'static str {
        match self {
            Self::InteractionsToGeminiRequest => "gemini/interactions/request",
            Self::GeminiToInteractionsStream => "gemini/interactions/response",
            Self::GeminiToInteractionsNonStream => "gemini/interactions/response-non-stream",
            Self::GeminiToInteractionsRequest => "interactions/gemini/request",
            Self::InteractionsToGeminiStream => "interactions/gemini/response",
            Self::InteractionsToGeminiNonStream => "interactions/gemini/response-non-stream",
            Self::PassthroughRequest => "interactions/interactions/request",
            Self::PassthroughStream => "interactions/interactions/response",
            Self::PassthroughNonStream => "interactions/interactions/response-non-stream",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Self::InteractionsToGeminiRequest => "interactions-to-gemini-request",
            Self::GeminiToInteractionsStream => "gemini-to-interactions-stream",
            Self::GeminiToInteractionsNonStream => "gemini-to-interactions-non-stream",
            Self::GeminiToInteractionsRequest => "gemini-to-interactions-request",
            Self::InteractionsToGeminiStream => "interactions-to-gemini-stream",
            Self::InteractionsToGeminiNonStream => "interactions-to-gemini-non-stream",
            Self::PassthroughRequest => "interactions-passthrough-request",
            Self::PassthroughStream => "interactions-passthrough-stream",
            Self::PassthroughNonStream => "interactions-passthrough-non-stream",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::InteractionsToGeminiRequest => "Interactions -> Gemini request",
            Self::GeminiToInteractionsStream => "Gemini -> Interactions response, streaming",
            Self::GeminiToInteractionsNonStream => "Gemini -> Interactions response, non-streaming",
            Self::GeminiToInteractionsRequest => "Gemini -> Interactions request",
            Self::InteractionsToGeminiStream => "Interactions -> Gemini response, streaming",
            Self::InteractionsToGeminiNonStream => "Interactions -> Gemini response, non-streaming",
            Self::PassthroughRequest => "Interactions passthrough request",
            Self::PassthroughStream => "Interactions passthrough response, streaming",
            Self::PassthroughNonStream => "Interactions passthrough response, non-streaming",
        }
    }

    fn cases(self) -> Vec<Case> {
        match self {
            Self::InteractionsToGeminiRequest => cases::interactions_requests(),
            Self::GeminiToInteractionsStream => cases::gemini_streams(),
            Self::GeminiToInteractionsNonStream => cases::gemini_responses(),
            Self::GeminiToInteractionsRequest => cases::gemini_requests(),
            Self::InteractionsToGeminiStream => cases::interactions_streams(),
            Self::InteractionsToGeminiNonStream => cases::interactions_responses(),
            Self::PassthroughRequest => cases::passthrough_requests(),
            Self::PassthroughStream => cases::passthrough_streams(),
            Self::PassthroughNonStream => cases::passthrough_responses(),
        }
    }

    fn generate(self, seed: u64, count: usize) -> Vec<Case> {
        match self {
            Self::InteractionsToGeminiRequest => generate::interactions_request_cases(seed, count),
            Self::GeminiToInteractionsStream => generate::gemini_event_cases(seed, count).0,
            Self::GeminiToInteractionsNonStream => generate::gemini_event_cases(seed, count).1,
            Self::GeminiToInteractionsRequest => generate::gemini_request_cases(seed, count),
            Self::InteractionsToGeminiStream => generate::interactions_event_cases(seed, count).0,
            Self::InteractionsToGeminiNonStream => {
                generate::interactions_event_cases(seed, count).1
            }
            Self::PassthroughRequest => generate::passthrough_request_cases(seed, count),
            Self::PassthroughStream => generate::passthrough_event_cases(seed, count).0,
            Self::PassthroughNonStream => generate::passthrough_event_cases(seed, count).1,
        }
    }

    fn run(self, case: &Case) -> Result<Value, String> {
        let request = || -> Result<Value, String> {
            exact::from_str(&case.request)
                .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))
        };
        let stream = case.options["stream"].as_bool().unwrap_or(false);
        let body = || {
            case.events
                .first()
                .map(String::as_bytes)
                .unwrap_or_default()
        };
        let output = match self {
            Self::InteractionsToGeminiRequest => {
                return Ok(convert_interactions_request_to_gemini(
                    &case.model,
                    &request()?,
                    stream,
                ));
            }
            Self::GeminiToInteractionsRequest => {
                return Ok(convert_gemini_request_to_interactions(
                    &case.model,
                    &request()?,
                    stream,
                ));
            }
            Self::PassthroughRequest => {
                return Ok(convert_interactions_request_to_interactions(
                    &case.model,
                    request()?,
                    stream,
                ));
            }
            Self::GeminiToInteractionsStream => {
                let mut translator = GeminiToInteractionsStream::new(&case.model);
                case.events
                    .iter()
                    .flat_map(|event| translator.translate(event.as_bytes()))
                    .collect::<String>()
            }
            Self::InteractionsToGeminiStream => {
                let mut translator = InteractionsToGeminiStream::new(&case.model);
                let chunks: Vec<String> = case
                    .events
                    .iter()
                    .filter_map(|event| translator.translate(event.as_bytes()))
                    .map(|chunk| chunk.to_string())
                    .collect();
                serde_json::to_string(&chunks).expect("strings serialize")
            }
            Self::PassthroughStream => {
                let chunks: Vec<String> = case
                    .events
                    .iter()
                    .filter_map(|event| convert_interactions_response_passthrough(event.as_bytes()))
                    .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
                    .collect();
                serde_json::to_string(&chunks).expect("strings serialize")
            }
            Self::GeminiToInteractionsNonStream => {
                convert_gemini_response_to_interactions_non_stream(&case.model, body()).to_string()
            }
            Self::InteractionsToGeminiNonStream => {
                convert_interactions_response_to_gemini_non_stream(&case.model, body()).to_string()
            }
            Self::PassthroughNonStream => String::from_utf8_lossy(
                convert_interactions_response_passthrough_non_stream(body()),
            )
            .into_owned(),
        };
        self.read(case, output.as_bytes())
            .ok_or_else(|| "output is not JSON".to_owned())
    }

    fn read(self, _case: &Case, output: &[u8]) -> Option<Value> {
        let text = String::from_utf8_lossy(output);
        let mut value = match self {
            Self::InteractionsToGeminiRequest
            | Self::GeminiToInteractionsRequest
            | Self::PassthroughRequest => return exact::from_str(&text).ok(),
            Self::PassthroughNonStream => return Some(text.into_owned().into()),
            Self::PassthroughStream => {
                let chunks: Vec<String> = serde_json::from_str(&text).ok()?;
                return Some(chunks.into_iter().map(Value::String).collect());
            }
            Self::GeminiToInteractionsStream => sse_frames(&text),
            Self::InteractionsToGeminiStream => {
                let chunks: Vec<String> = serde_json::from_str(&text).ok()?;
                chunks
                    .into_iter()
                    .map(|chunk| exact::from_str(&chunk).unwrap_or(Value::String(chunk)))
                    .collect()
            }
            Self::GeminiToInteractionsNonStream | Self::InteractionsToGeminiNonStream => {
                exact::from_str(&text).ok()?
            }
        };
        mask_step_ids(&mut value);
        mask_volatile(&mut value);
        Some(value)
    }

    fn embedded_json(self, _case: &Case) -> &'static [JsonAt] {
        match self {
            Self::InteractionsToGeminiRequest => TO_GEMINI_REQUEST_JSON,
            Self::GeminiToInteractionsRequest => TO_INTERACTIONS_REQUEST_JSON,
            Self::GeminiToInteractionsStream => TO_INTERACTIONS_STREAM_JSON,
            Self::GeminiToInteractionsNonStream => TO_INTERACTIONS_RESPONSE_JSON,
            Self::InteractionsToGeminiStream => TO_GEMINI_STREAM_JSON,
            Self::InteractionsToGeminiNonStream => TO_GEMINI_RESPONSE_JSON,
            Self::PassthroughRequest | Self::PassthroughStream | Self::PassthroughNonStream => &[],
        }
    }

    fn drop_deliberate_omissions(self, _case: &Case, _go: &mut Value) -> Option<Deviation> {
        None
    }

    fn joins_stream(self) -> bool {
        self == Self::GeminiToInteractionsStream
    }

    fn native(stage: Stage, from: &str, to: &str) -> Option<Self> {
        match (stage, from, to) {
            (Stage::Request, "interactions", "gemini") => Some(Self::InteractionsToGeminiRequest),
            (Stage::Request, "gemini", "interactions") => Some(Self::GeminiToInteractionsRequest),
            (Stage::Request, "interactions", "interactions") => Some(Self::PassthroughRequest),
            (Stage::Stream, "gemini", "interactions") => Some(Self::GeminiToInteractionsStream),
            (Stage::Stream, "interactions", "gemini") => Some(Self::InteractionsToGeminiStream),
            (Stage::Stream, "interactions", "interactions") => Some(Self::PassthroughStream),
            (Stage::NonStream, "gemini", "interactions") => {
                Some(Self::GeminiToInteractionsNonStream)
            }
            (Stage::NonStream, "interactions", "gemini") => {
                Some(Self::InteractionsToGeminiNonStream)
            }
            (Stage::NonStream, "interactions", "interactions") => Some(Self::PassthroughNonStream),
            _ => None,
        }
    }
}

/// How many digits a Unix time in nanoseconds has, from 2001 to 2286.
const NANOS_DIGITS: usize = 19;

/// Replaces every step ID made up from the clock, `step_` and the time in
/// nanoseconds, with `step_(generated)`. Go's clock on Windows only moves
/// every so often, so two steps can get the same ID from it where ours
/// differ; [`mask_volatile`] would number them apart.
fn mask_step_ids(value: &mut Value) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(mask_step_ids),
        Value::Object(fields) => fields.values_mut().for_each(mask_step_ids),
        Value::String(text) => {
            let generated = text.strip_prefix("step_").is_some_and(|nanos| {
                nanos.len() == NANOS_DIGITS && nanos.bytes().all(|b| b.is_ascii_digit())
            });
            if generated {
                *text = "step_(generated)".to_owned();
            }
        }
        _ => {}
    }
}

/// The pairs of the registry requests run with each kind's cases.
const REQUEST_PAIRS: [(Pair, Kind); 3] = [
    (
        ("interactions", "gemini"),
        Kind::InteractionsToGeminiRequest,
    ),
    (
        ("gemini", "interactions"),
        Kind::GeminiToInteractionsRequest,
    ),
    (("interactions", "interactions"), Kind::PassthroughRequest),
];

/// The pairs of the registry streams run with each kind's cases.
const STREAM_PAIRS: [(Pair, Kind); 3] = [
    (("gemini", "interactions"), Kind::GeminiToInteractionsStream),
    (("interactions", "gemini"), Kind::InteractionsToGeminiStream),
    (("interactions", "interactions"), Kind::PassthroughStream),
];

/// The pairs of the registry non-streaming responses run with each kind's
/// cases.
const FINAL_PAIRS: [(Pair, Kind); 3] = [
    (
        ("gemini", "interactions"),
        Kind::GeminiToInteractionsNonStream,
    ),
    (
        ("interactions", "gemini"),
        Kind::InteractionsToGeminiNonStream,
    ),
    (("interactions", "interactions"), Kind::PassthroughNonStream),
];

/// Each pair with its kind's hand-written cases. The registry translates
/// requests that are JSON only.
fn hand_written(pairs: [(Pair, Kind); 3]) -> Vec<(Pair, Vec<Case>)> {
    pairs
        .into_iter()
        .map(|(pair, kind)| {
            let cases = kind
                .cases()
                .into_iter()
                .filter(|case| serde_json::from_str::<Value>(&case.request).is_ok())
                .collect();
            (pair, cases)
        })
        .collect()
}

/// The hand-written registry request cases for the family's pairs, each list
/// with its pair.
pub fn registry_requests() -> Vec<(Pair, Vec<Case>)> {
    hand_written(REQUEST_PAIRS)
}

/// `count` random registry request cases for each of the family's pairs.
pub fn registry_request_cases(seed: u64, count: usize) -> Vec<(Pair, Vec<Case>)> {
    REQUEST_PAIRS
        .into_iter()
        .map(|(pair, kind)| (pair, kind.generate(seed, count)))
        .collect()
}

/// The hand-written registry stream cases for the family's pairs, each list
/// with its pair.
pub fn registry_streams() -> Vec<(Pair, Vec<Case>)> {
    hand_written(STREAM_PAIRS)
}

/// The hand-written registry non-streaming cases for the family's pairs, each
/// list with its pair.
pub fn registry_finals() -> Vec<(Pair, Vec<Case>)> {
    hand_written(FINAL_PAIRS)
}

/// `count` random registry stream cases, and as many non-streaming ones,
/// for each of the family's pairs.
pub fn registry_response_cases(seed: u64, count: usize) -> Vec<ResponseCases> {
    vec![
        (
            ("gemini", "interactions"),
            generate::gemini_event_cases(seed, count),
        ),
        (
            ("interactions", "gemini"),
            generate::interactions_event_cases(seed, count),
        ),
        (
            ("interactions", "interactions"),
            generate::passthrough_event_cases(seed, count),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn step_ids_from_the_clock_are_masked_alike() {
        let mut value = json!({
            "a": "step_1791115200000000000",
            "b": ["step_1791115200000000001", "step_179111520000000000", "step_x"],
        });
        mask_step_ids(&mut value);
        assert_eq!(
            value,
            json!({
                "a": "step_(generated)",
                "b": ["step_(generated)", "step_179111520000000000", "step_x"],
            })
        );
    }

    #[test]
    fn every_pair_runs_its_own_suite() {
        for (pairs, stage) in [
            (REQUEST_PAIRS, Stage::Request),
            (STREAM_PAIRS, Stage::Stream),
            (FINAL_PAIRS, Stage::NonStream),
        ] {
            for ((from, to), kind) in pairs {
                assert!(
                    Kind::native(stage, from, to) == Some(kind),
                    "{}",
                    kind.key()
                );
            }
        }
    }
}
