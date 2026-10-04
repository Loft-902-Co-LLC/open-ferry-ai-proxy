//! The Claude and Interactions translators' suites (P4 WP4-A), whose Go
//! entries are in `go/interactions/parity_claude.go`:
//! - `interactions/claude/request`, `.../request-compat`, `.../response`
//!   and `.../response-non-stream`: Claude clients to an Interactions
//!   upstream (tr/interactions/claude);
//! - `claude/interactions/request`, `.../response` and
//!   `.../response-non-stream`: Interactions clients to a Claude upstream
//!   (tr/claude/interactions).
//!
//! The request suites take the `stream` option. A stream suite's output is
//! the list of chunks, each read as its SSE frame (see [`frame`]); a
//! non-streaming suite's is the body. Hand-written cases are in
//! [`cases`], and random ones from `crate::generate::interactions::claude`.
//!
//! The registry runs these translators for `claude` to `interactions` and
//! `interactions` to `claude` (see [`Family::native`]); the `registry_*`
//! functions send the same hand-written and random cases through it.

mod cases;

use serde_json::{Value, json};

use open_ferry_translate::claude::interactions::{
    ClaudeToInteractionsStream, convert_claude_response_to_interactions_non_stream,
    convert_interactions_request_to_claude,
};
use open_ferry_translate::interactions::claude::{
    InteractionsToClaudeStream, convert_claude_request_to_interactions,
    convert_claude_request_to_interactions_with_compat,
    convert_interactions_response_to_claude_non_stream,
};

use super::{Family, Pair, ResponseCases, Stage, Suite};
use crate::cases::Case;
use crate::compare::{Deviation, JsonAt, JsonForm};
use crate::generate::interactions::claude as generate;
use crate::translator::Translator;

/// The family's suites, a variant each, named after the upstream package
/// they run (`interactions/claude` or `claude/interactions`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `interactions/claude/request`: a Claude request to Interactions.
    InteractionsRequest,
    /// The same in compatibility mode, which keeps empty thinking blocks.
    InteractionsRequestCompat,
    /// `interactions/claude/response`: an Interactions stream to Claude.
    InteractionsStream,
    /// `interactions/claude/response-non-stream`: an interaction to a
    /// Claude message.
    InteractionsNonStream,
    /// `claude/interactions/request`: an Interactions request to Claude.
    ClaudeRequest,
    /// `claude/interactions/response`: a Claude stream to Interactions.
    ClaudeStream,
    /// `claude/interactions/response-non-stream`: a Claude message, or a
    /// whole Claude stream, to an interaction.
    ClaudeNonStream,
}

/// Every suite, in the order they run.
pub const KINDS: &[Kind] = &[
    Kind::InteractionsRequest,
    Kind::InteractionsRequestCompat,
    Kind::InteractionsStream,
    Kind::InteractionsNonStream,
    Kind::ClaudeRequest,
    Kind::ClaudeStream,
    Kind::ClaudeNonStream,
];

/// The suites, with their cases.
pub fn suites(seed: u64, random: usize) -> Vec<Suite> {
    KINDS
        .iter()
        .map(|&kind| {
            let translator = Translator::Interactions(super::Kind::Claude(kind));
            (translator, kind.cases(), kind.generate(seed, random))
        })
        .collect()
}

impl Kind {
    /// How each of a stream suite's frames ends: upstream's translator to
    /// Claude ends each with two blank lines.
    fn frame_end(self) -> Option<&'static str> {
        match self {
            Self::InteractionsStream => Some("\n\n\n"),
            Self::ClaudeStream => Some("\n\n"),
            _ => None,
        }
    }
}

impl Family for Kind {
    fn key(self) -> &'static str {
        match self {
            Self::InteractionsRequest => "interactions/claude/request",
            Self::InteractionsRequestCompat => "interactions/claude/request-compat",
            Self::InteractionsStream => "interactions/claude/response",
            Self::InteractionsNonStream => "interactions/claude/response-non-stream",
            Self::ClaudeRequest => "claude/interactions/request",
            Self::ClaudeStream => "claude/interactions/response",
            Self::ClaudeNonStream => "claude/interactions/response-non-stream",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Self::InteractionsRequest => "claude-to-interactions-request",
            Self::InteractionsRequestCompat => "claude-to-interactions-request-compat",
            Self::InteractionsStream => "interactions-to-claude-stream",
            Self::InteractionsNonStream => "interactions-to-claude-non-stream",
            Self::ClaudeRequest => "interactions-to-claude-request",
            Self::ClaudeStream => "claude-to-interactions-stream",
            Self::ClaudeNonStream => "claude-to-interactions-non-stream",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::InteractionsRequest => "Claude -> Interactions request",
            Self::InteractionsRequestCompat => "Claude -> Interactions request, compatibility mode",
            Self::InteractionsStream => "Interactions -> Claude response, streaming",
            Self::InteractionsNonStream => "Interactions -> Claude response, non-streaming",
            Self::ClaudeRequest => "Interactions -> Claude request",
            Self::ClaudeStream => "Claude -> Interactions response, streaming",
            Self::ClaudeNonStream => "Claude -> Interactions response, non-streaming",
        }
    }

    fn cases(self) -> Vec<Case> {
        match self {
            Self::InteractionsRequest | Self::InteractionsRequestCompat => cases::claude_requests(),
            Self::InteractionsStream => cases::interactions_streams(),
            Self::InteractionsNonStream => cases::interactions_finals(),
            Self::ClaudeRequest => cases::interactions_requests(),
            Self::ClaudeStream => cases::claude_streams(),
            Self::ClaudeNonStream => cases::claude_finals(),
        }
    }

    fn generate(self, seed: u64, count: usize) -> Vec<Case> {
        match self {
            Self::InteractionsRequest => generate::claude_request_cases(seed, count),
            Self::InteractionsRequestCompat => {
                generate::claude_request_cases(seed.rotate_left(7), count)
            }
            Self::InteractionsStream => generate::interactions_event_cases(seed, count).0,
            Self::InteractionsNonStream => generate::interactions_event_cases(seed, count).1,
            Self::ClaudeRequest => generate::interactions_request_cases(seed, count),
            Self::ClaudeStream => generate::claude_event_cases(seed, count).0,
            Self::ClaudeNonStream => generate::claude_event_cases(seed, count).1,
        }
    }

    fn run(self, case: &Case) -> Result<Value, String> {
        let request = || {
            serde_json::from_str::<Value>(&case.request)
                .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))
        };
        let stream = case.options["stream"].as_bool().unwrap_or(false);
        let body = case.events.first().map_or(&b""[..], |body| body.as_bytes());
        let output = match self {
            Self::InteractionsRequest => {
                convert_claude_request_to_interactions(&case.model, &request()?, stream)
            }
            Self::InteractionsRequestCompat => {
                convert_claude_request_to_interactions_with_compat(&case.model, &request()?, stream)
            }
            Self::InteractionsStream => {
                let mut translator = InteractionsToClaudeStream::new(&case.model);
                let chunks: Vec<String> = case
                    .events
                    .iter()
                    .flat_map(|event| translator.translate(event.as_bytes()))
                    .collect();
                chunks.into()
            }
            Self::InteractionsNonStream => {
                convert_interactions_response_to_claude_non_stream(&case.model, body)
            }
            Self::ClaudeRequest => {
                convert_interactions_request_to_claude(&case.model, &request()?, stream)
            }
            Self::ClaudeStream => {
                let mut translator = ClaudeToInteractionsStream::new(&case.model);
                let chunks: Vec<String> = case
                    .events
                    .iter()
                    .flat_map(|event| translator.translate(event.as_bytes()))
                    .collect();
                chunks.into()
            }
            Self::ClaudeNonStream => {
                convert_claude_response_to_interactions_non_stream(&case.model, body)
            }
        };
        self.read(case, output.to_string().as_bytes())
            .ok_or_else(|| "output is not JSON".to_owned())
    }

    /// Reads a request or a body as JSON, and a stream's list of chunks as
    /// a list of frames (see [`frame`]), then masks the IDs and times read
    /// from the clock.
    fn read(self, case: &Case, output: &[u8]) -> Option<Value> {
        let _ = case;
        let mut value = match self.frame_end() {
            Some(end) => {
                let chunks: Vec<String> = serde_json::from_slice(output).ok()?;
                chunks.iter().map(|chunk| frame(chunk, end)).collect()
            }
            None => serde_json::from_slice(output).ok()?,
        };
        super::mask_volatile(&mut value);
        Some(value)
    }

    /// Where upstream copies JSON's text into a string: a Claude request's
    /// text that isn't a string, as text to Interactions; a call's arguments
    /// given as an object, as a Claude `partial_json`; and a result that
    /// isn't text, as a Claude tool result's content.
    fn embedded_json(self, case: &Case) -> &'static [JsonAt] {
        let _ = case;
        match self {
            Self::InteractionsRequest | Self::InteractionsRequestCompat => &[
                ("$.system_instruction", JsonForm::InText),
                ("$.input[*].content[*].text", JsonForm::InText),
                ("$.input[*].result[*].text", JsonForm::InText),
            ],
            Self::InteractionsStream => &[("$[*].data.delta.partial_json", JsonForm::Whole)],
            Self::ClaudeRequest => &[("$.messages[*].content[*].content", JsonForm::Whole)],
            Self::InteractionsNonStream | Self::ClaudeStream | Self::ClaudeNonStream => &[],
        }
    }

    fn drop_deliberate_omissions(self, case: &Case, go: &mut Value) -> Option<Deviation> {
        let _ = (case, go);
        None
    }

    fn joins_stream(self) -> bool {
        false
    }

    fn native(stage: Stage, from: &str, to: &str) -> Option<Self> {
        match (stage, from, to) {
            (Stage::Request, "claude", "interactions") => Some(Self::InteractionsRequest),
            (Stage::Request, "interactions", "claude") => Some(Self::ClaudeRequest),
            (Stage::Stream, "interactions", "claude") => Some(Self::InteractionsStream),
            (Stage::Stream, "claude", "interactions") => Some(Self::ClaudeStream),
            (Stage::NonStream, "interactions", "claude") => Some(Self::InteractionsNonStream),
            (Stage::NonStream, "claude", "interactions") => Some(Self::ClaudeNonStream),
            _ => None,
        }
    }
}

/// A chunk as `{"event", "data"}`, if it is one SSE frame of an `event:`
/// line and a `data:` line, ending in `end`, with its data read as JSON or
/// kept as text (such as `[DONE]`); otherwise `{"unparsed": chunk}`.
fn frame(chunk: &str, end: &str) -> Value {
    let parts = chunk
        .strip_prefix("event: ")
        .and_then(|rest| rest.strip_suffix(end))
        .and_then(|rest| rest.split_once("\ndata: "))
        .filter(|(event, data)| !event.contains('\n') && !data.contains('\n'));
    match parts {
        Some((event, data)) => {
            let data = serde_json::from_str(data).unwrap_or_else(|_| Value::from(data));
            json!({ "event": event, "data": data })
        }
        None => json!({ "unparsed": chunk }),
    }
}

/// The hand-written registry request cases for the family's pairs, each list
/// with its pair.
pub fn registry_requests() -> Vec<(Pair, Vec<Case>)> {
    vec![
        (("claude", "interactions"), cases::claude_requests()),
        (("interactions", "claude"), cases::interactions_requests()),
    ]
}

/// `count` random registry request cases for each of the family's pairs.
pub fn registry_request_cases(seed: u64, count: usize) -> Vec<(Pair, Vec<Case>)> {
    vec![
        (
            ("claude", "interactions"),
            generate::claude_request_cases(seed, count),
        ),
        (
            ("interactions", "claude"),
            generate::interactions_request_cases(seed, count),
        ),
    ]
}

/// The hand-written registry stream cases for the family's pairs, each list
/// with its pair.
pub fn registry_streams() -> Vec<(Pair, Vec<Case>)> {
    vec![
        (("interactions", "claude"), cases::interactions_streams()),
        (("claude", "interactions"), cases::claude_streams()),
    ]
}

/// The hand-written registry non-streaming cases for the family's pairs, each
/// list with its pair.
pub fn registry_finals() -> Vec<(Pair, Vec<Case>)> {
    vec![
        (("interactions", "claude"), cases::interactions_finals()),
        (("claude", "interactions"), cases::claude_finals()),
    ]
}

/// `count` random registry stream cases, and as many non-streaming ones,
/// for each of the family's pairs.
pub fn registry_response_cases(seed: u64, count: usize) -> Vec<ResponseCases> {
    vec![
        (
            ("interactions", "claude"),
            generate::interactions_event_cases(seed, count),
        ),
        (
            ("claude", "interactions"),
            generate::claude_event_cases(seed, count),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_read_by_their_parts() {
        assert_eq!(
            frame("event: done\ndata: [DONE]\n\n", "\n\n"),
            json!({ "event": "done", "data": "[DONE]" })
        );
        assert_eq!(
            frame("event: x\ndata: {\"a\":1}\n\n\n", "\n\n\n"),
            json!({ "event": "x", "data": { "a": 1 } })
        );
        for chunk in [
            "event: x\ndata: 1\n\n",
            "data: 1\n\n\n",
            "event: x\n\ndata: 1\n\n\n",
        ] {
            assert_eq!(frame(chunk, "\n\n\n"), json!({ "unparsed": chunk }));
        }
    }

    #[test]
    fn hand_written_requests_are_json() {
        for kind in [Kind::InteractionsRequest, Kind::ClaudeRequest] {
            for case in kind.cases() {
                serde_json::from_str::<Value>(&case.request).expect(&case.name);
            }
        }
    }
}
