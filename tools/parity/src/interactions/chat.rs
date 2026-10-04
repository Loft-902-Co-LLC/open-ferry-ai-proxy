//! The Chat Completions and Interactions translators' suites (P4 WP4-B),
//! whose Go entries go in `go/interactions/parity_chat.go`. All six run
//! tr/openai/interactions/chat-completions:
//! - `interactions/openai-chat/request`, `.../response` and
//!   `.../response-non-stream`: Chat Completions clients to an Interactions
//!   upstream;
//! - `openai/interactions/request`, `.../response` and
//!   `.../response-non-stream`: Interactions clients to a Chat Completions
//!   upstream.
//!
//! Hand-written cases are in `chat/cases.rs`, and random ones in
//! `crate::generate::interactions::chat`. A request case's `stream` option is
//! the translator's `stream` argument. The Interactions to Chat Completions
//! stream's entry writes its chunks as a JSON array of them, and the Chat
//! Completions to Interactions one its SSE frames joined.
//!
//! The registry runs them for the pairs `openai` → `interactions` and
//! `interactions` → `openai` (see [`Family::native`]), with the same cases.

mod cases;

use open_ferry_translate::openai::interactions::chat_completions::{
    InteractionsToOpenAIStream, OpenAIToInteractionsStream, convert_interactions_request_to_openai,
    convert_interactions_response_to_openai_non_stream, convert_openai_request_to_interactions,
    convert_openai_response_to_interactions_non_stream,
};
use serde_json::{Value, json};

use super::{Family, Pair, ResponseCases, Stage, Suite, mask_volatile};
use crate::cases::Case;
use crate::compare::{Deviation, JsonAt, JsonForm};
use crate::generate::interactions::chat as generate;
use crate::translator::{Translator, sse_frames};

/// The family's suites, a variant each.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `interactions/openai-chat/request`: a Chat Completions request as an
    /// Interactions one.
    ChatRequest,
    /// `interactions/openai-chat/response`: an Interactions event stream as
    /// Chat Completions chunks.
    ChatStream,
    /// `interactions/openai-chat/response-non-stream`: an Interactions
    /// response as a Chat Completions one.
    ChatNonStream,
    /// `openai/interactions/request`: an Interactions request as a Chat
    /// Completions one.
    InteractionsRequest,
    /// `openai/interactions/response`: a Chat Completions stream as
    /// Interactions events.
    InteractionsStream,
    /// `openai/interactions/response-non-stream`: a Chat Completions
    /// response as an Interactions one.
    InteractionsNonStream,
}

/// Every suite, in the order they run.
pub const KINDS: &[Kind] = &[
    Kind::ChatRequest,
    Kind::ChatStream,
    Kind::ChatNonStream,
    Kind::InteractionsRequest,
    Kind::InteractionsStream,
    Kind::InteractionsNonStream,
];

/// The suites, with their cases.
pub fn suites(seed: u64, random: usize) -> Vec<Suite> {
    KINDS
        .iter()
        .map(|&kind| {
            let translator = Translator::Interactions(super::Kind::Chat(kind));
            (translator, kind.cases(), kind.generate(seed, random))
        })
        .collect()
}

/// Where the translators to Chat Completions write a value's JSON text
/// compactly: a call's arguments, and a tool result that isn't text.
const TO_CHAT_REQUEST_JSON: &[JsonAt] = &[
    (
        "$.messages[*].tool_calls[*].function.arguments",
        JsonForm::Whole,
    ),
    ("$.messages[*].content", JsonForm::Whole),
];

const TO_CHAT_STREAM_JSON: &[JsonAt] = &[(
    "$[*].choices[*].delta.tool_calls[*].function.arguments",
    JsonForm::Whole,
)];

const TO_CHAT_NON_STREAM_JSON: &[JsonAt] = &[(
    "$.choices[*].message.tool_calls[*].function.arguments",
    JsonForm::Whole,
)];

/// Where the translator to Interactions writes a value's JSON text
/// compactly: an image URL, a tool's description or a text part that isn't
/// a string.
const TO_INTERACTIONS_REQUEST_JSON: &[JsonAt] = &[
    ("$.input[*].content[*].image_url", JsonForm::Whole),
    ("$.input[*].content[*].text", JsonForm::Whole),
    ("$.tools[*].description", JsonForm::Whole),
];

/// The same in a whole response: content that isn't a string.
const TO_INTERACTIONS_NON_STREAM_JSON: &[JsonAt] =
    &[("$.steps[*].content[*].text", JsonForm::Whole)];

impl Family for Kind {
    fn key(self) -> &'static str {
        match self {
            Self::ChatRequest => "interactions/openai-chat/request",
            Self::ChatStream => "interactions/openai-chat/response",
            Self::ChatNonStream => "interactions/openai-chat/response-non-stream",
            Self::InteractionsRequest => "openai/interactions/request",
            Self::InteractionsStream => "openai/interactions/response",
            Self::InteractionsNonStream => "openai/interactions/response-non-stream",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Self::ChatRequest => "chat-to-interactions-request",
            Self::ChatStream => "interactions-to-chat-stream",
            Self::ChatNonStream => "interactions-to-chat-non-stream",
            Self::InteractionsRequest => "interactions-to-chat-request",
            Self::InteractionsStream => "chat-to-interactions-stream",
            Self::InteractionsNonStream => "chat-to-interactions-non-stream",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::ChatRequest => "Chat Completions -> Interactions request",
            Self::ChatStream => "Interactions -> Chat Completions response, streaming",
            Self::ChatNonStream => "Interactions -> Chat Completions response, non-streaming",
            Self::InteractionsRequest => "Interactions -> Chat Completions request",
            Self::InteractionsStream => "Chat Completions -> Interactions response, streaming",
            Self::InteractionsNonStream => {
                "Chat Completions -> Interactions response, non-streaming"
            }
        }
    }

    fn cases(self) -> Vec<Case> {
        match self {
            Self::ChatRequest => cases::chat_requests(),
            Self::ChatStream => cases::interactions_streams(),
            Self::ChatNonStream => cases::interactions_finals(),
            Self::InteractionsRequest => cases::interactions_requests(),
            Self::InteractionsStream => cases::chat_streams(),
            Self::InteractionsNonStream => cases::chat_finals(),
        }
    }

    fn generate(self, seed: u64, count: usize) -> Vec<Case> {
        let cases = match self {
            Self::ChatRequest => generate::chat_request_cases(seed, count),
            Self::ChatStream => generate::event_cases(seed, count).0,
            Self::ChatNonStream => generate::event_cases(seed, count).1,
            Self::InteractionsRequest => generate::request_cases(seed, count),
            Self::InteractionsStream => generate::chunk_cases(seed, count).0,
            Self::InteractionsNonStream => generate::chunk_cases(seed, count).1,
        };
        cases
            .into_iter()
            .enumerate()
            .map(|(index, case)| match self {
                Self::ChatRequest | Self::InteractionsRequest => {
                    case.with_options(json!({ "stream": index % 2 == 1 }))
                }
                Self::InteractionsStream | Self::InteractionsNonStream if index % 3 != 0 => Case {
                    model: ["gpt-test", " "][index % 2].to_owned(),
                    ..case
                },
                _ => case,
            })
            .collect()
    }

    fn run(self, case: &Case) -> Result<Value, String> {
        let model = case.model.as_str();
        let stream = case.options["stream"].as_bool().unwrap_or(false);
        let body = || {
            let body = case.events.first().map_or("", String::as_str);
            parse(body)
        };
        let output = match self {
            Self::ChatRequest => {
                convert_openai_request_to_interactions(model, &parse(&case.request), stream)
                    .to_string()
            }
            Self::InteractionsRequest => {
                convert_interactions_request_to_openai(model, &parse(&case.request), stream)
                    .to_string()
            }
            Self::ChatStream => {
                let mut translator = InteractionsToOpenAIStream::new(model);
                let chunks: Vec<String> = case
                    .events
                    .iter()
                    .flat_map(|event| translator.translate(event.as_bytes()))
                    .map(|chunk| chunk.to_string())
                    .collect();
                serde_json::to_string(&chunks).expect("strings serialize")
            }
            Self::ChatNonStream => {
                convert_interactions_response_to_openai_non_stream(model, &body()).to_string()
            }
            Self::InteractionsStream => {
                let mut translator = OpenAIToInteractionsStream::new(model);
                case.events
                    .iter()
                    .map(|event| translator.translate(event.as_bytes()))
                    .collect()
            }
            Self::InteractionsNonStream => {
                convert_openai_response_to_interactions_non_stream(model, &body()).to_string()
            }
        };
        self.read(case, output.as_bytes())
            .ok_or_else(|| "output is not JSON".to_owned())
    }

    /// A request as JSON, a Chat Completions stream as an array of its
    /// chunks (each as JSON, or text if it isn't), an Interactions stream as
    /// its frames (see [`sse_frames`]), and a response as JSON, with the
    /// clock's readings masked (see [`mask_volatile`]). A Chat Completions
    /// chunk or response with no ID makes one up from the clock each time,
    /// so those are all masked alike.
    fn read(self, case: &Case, output: &[u8]) -> Option<Value> {
        let _ = case;
        let text = String::from_utf8_lossy(output);
        let mut value = match self {
            Self::ChatRequest | Self::InteractionsRequest => {
                return serde_json::from_str(&text).ok();
            }
            Self::ChatNonStream | Self::InteractionsNonStream => {
                serde_json::from_str(&text).ok()?
            }
            Self::ChatStream => {
                let chunks: Vec<String> = serde_json::from_str(&text).ok()?;
                chunks
                    .into_iter()
                    .map(|chunk| serde_json::from_str(&chunk).unwrap_or(Value::String(chunk)))
                    .collect()
            }
            Self::InteractionsStream => sse_frames(&text),
        };
        mask_volatile(&mut value);
        if matches!(self, Self::ChatStream | Self::ChatNonStream) {
            unnumber_chat_ids(&mut value);
        }
        Some(value)
    }

    fn embedded_json(self, case: &Case) -> &'static [JsonAt] {
        let _ = case;
        match self {
            Self::InteractionsRequest => TO_CHAT_REQUEST_JSON,
            Self::ChatStream => TO_CHAT_STREAM_JSON,
            Self::ChatNonStream => TO_CHAT_NON_STREAM_JSON,
            Self::ChatRequest => TO_INTERACTIONS_REQUEST_JSON,
            Self::InteractionsNonStream => TO_INTERACTIONS_NON_STREAM_JSON,
            Self::InteractionsStream => &[],
        }
    }

    fn drop_deliberate_omissions(self, case: &Case, go: &mut Value) -> Option<Deviation> {
        let _ = (case, go);
        None
    }

    fn joins_stream(self) -> bool {
        self == Self::InteractionsStream
    }

    fn native(stage: Stage, from: &str, to: &str) -> Option<Self> {
        match (stage, from, to) {
            (Stage::Request, OPENAI, INTERACTIONS) => Some(Self::ChatRequest),
            (Stage::Request, INTERACTIONS, OPENAI) => Some(Self::InteractionsRequest),
            (Stage::Stream, INTERACTIONS, OPENAI) => Some(Self::ChatStream),
            (Stage::NonStream, INTERACTIONS, OPENAI) => Some(Self::ChatNonStream),
            (Stage::Stream, OPENAI, INTERACTIONS) => Some(Self::InteractionsStream),
            (Stage::NonStream, OPENAI, INTERACTIONS) => Some(Self::InteractionsNonStream),
            _ => None,
        }
    }
}

/// The registry's names of the two formats.
const OPENAI: &str = "openai";
const INTERACTIONS: &str = "interactions";

/// A provider's body or event as JSON. One that isn't valid JSON reads as
/// having no fields, as the ports read it.
fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or(Value::Null)
}

/// Turns each masked Chat Completions ID, `chatcmpl_(generated-<n>)`, into
/// `chatcmpl_(generated)`: a stream with no ID makes one up for each chunk,
/// and how many readings of the clock fall on the same nanosecond varies.
fn unnumber_chat_ids(value: &mut Value) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(unnumber_chat_ids),
        Value::Object(fields) => fields.values_mut().for_each(unnumber_chat_ids),
        Value::String(text) if text.starts_with("chatcmpl_(generated-") => {
            *text = "chatcmpl_(generated)".to_owned();
        }
        _ => {}
    }
}

/// The hand-written registry request cases for the family's pairs, each list
/// with its pair.
pub fn registry_requests() -> Vec<(Pair, Vec<Case>)> {
    vec![
        ((OPENAI, INTERACTIONS), cases::chat_requests()),
        ((INTERACTIONS, OPENAI), cases::interactions_requests()),
    ]
}

/// `count` random registry request cases for each of the family's pairs.
pub fn registry_request_cases(seed: u64, count: usize) -> Vec<(Pair, Vec<Case>)> {
    vec![
        (
            (OPENAI, INTERACTIONS),
            generate::chat_request_cases(seed, count),
        ),
        ((INTERACTIONS, OPENAI), generate::request_cases(seed, count)),
    ]
}

/// The hand-written registry stream cases for the family's pairs, each list
/// with its pair.
pub fn registry_streams() -> Vec<(Pair, Vec<Case>)> {
    vec![
        ((INTERACTIONS, OPENAI), cases::interactions_streams()),
        ((OPENAI, INTERACTIONS), cases::chat_streams()),
    ]
}

/// The hand-written registry non-streaming cases for the family's pairs, each
/// list with its pair.
pub fn registry_finals() -> Vec<(Pair, Vec<Case>)> {
    vec![
        ((INTERACTIONS, OPENAI), cases::interactions_finals()),
        ((OPENAI, INTERACTIONS), cases::chat_finals()),
    ]
}

/// `count` random registry stream cases, and as many non-streaming ones,
/// for each of the family's pairs.
pub fn registry_response_cases(seed: u64, count: usize) -> Vec<ResponseCases> {
    vec![
        ((INTERACTIONS, OPENAI), generate::event_cases(seed, count)),
        ((OPENAI, INTERACTIONS), generate::chunk_cases(seed, count)),
    ]
}
