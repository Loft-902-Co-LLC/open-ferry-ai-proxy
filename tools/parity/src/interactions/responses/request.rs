//! The OpenAI Responses and Interactions request translators' suites (P4
//! WP4-C1), both in tr/openai/interactions/responses:
//! - `interactions/openai-responses/request`: OpenAI Responses clients to an
//!   Interactions upstream ([`Kind::ResponsesToInteractions`]);
//! - `openai-responses/interactions/request`: Interactions clients to a
//!   Responses upstream ([`Kind::InteractionsToResponses`]).
//!
//! Their Go entries are in `go/interactions/parity_responses_request.go`.
//! Hand-written cases are in [`cases`], and random ones come from
//! `crate::generate::interactions::responses::request`. [`Family::native`]
//! maps the request stage of `openai-response` → `interactions` and
//! `interactions` → `openai-response` to these suites, and the `registry_*`
//! functions give their cases.

mod cases;

use open_ferry_translate::json::exact;
use open_ferry_translate::openai::interactions::responses::{
    convert_interactions_request_to_openai_responses,
    convert_openai_responses_request_to_interactions,
};
use serde_json::Value;

use super::super::{Family, Pair, Stage};
use crate::cases::Case;
use crate::compare::{Deviation, JsonAt, JsonForm};
use crate::generate::interactions::responses::request as generate;

/// The suites, a variant each.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `interactions/openai-responses/request`.
    ResponsesToInteractions,
    /// `openai-responses/interactions/request`.
    InteractionsToResponses,
}

/// Every suite, in the order they run.
pub const KINDS: &[Kind] = &[Kind::ResponsesToInteractions, Kind::InteractionsToResponses];

impl Kind {
    /// The registry pair whose request translator the suite runs: the
    /// client's format, then the provider's.
    fn pair(self) -> Pair {
        match self {
            Self::ResponsesToInteractions => ("openai-response", "interactions"),
            Self::InteractionsToResponses => ("interactions", "openai-response"),
        }
    }
}

impl Family for Kind {
    fn key(self) -> &'static str {
        match self {
            Self::ResponsesToInteractions => "interactions/openai-responses/request",
            Self::InteractionsToResponses => "openai-responses/interactions/request",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Self::ResponsesToInteractions => "responses-to-interactions-request",
            Self::InteractionsToResponses => "interactions-to-responses-request",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::ResponsesToInteractions => "Responses -> Interactions request",
            Self::InteractionsToResponses => "Interactions -> Responses request",
        }
    }

    fn cases(self) -> Vec<Case> {
        match self {
            Self::ResponsesToInteractions => cases::responses_requests(),
            Self::InteractionsToResponses => cases::interactions_requests(),
        }
    }

    fn generate(self, seed: u64, count: usize) -> Vec<Case> {
        match self {
            Self::ResponsesToInteractions => generate::responses_cases(seed, count),
            Self::InteractionsToResponses => generate::interactions_cases(seed, count),
        }
    }

    fn run(self, case: &Case) -> Result<Value, String> {
        let request = exact::from_str(&case.request)
            .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
        // As the harness's streamOption reads it.
        let stream = case.options["stream"].as_bool().unwrap_or(false);
        Ok(match self {
            Self::ResponsesToInteractions => {
                convert_openai_responses_request_to_interactions(&case.model, &request, stream)
            }
            Self::InteractionsToResponses => {
                convert_interactions_request_to_openai_responses(&case.model, &request, stream)
            }
        })
    }

    /// The request as JSON, each number kept as written.
    fn read(self, _case: &Case, output: &[u8]) -> Option<Value> {
        exact::from_slice(output).ok()
    }

    fn embedded_json(self, _case: &Case) -> &'static [JsonAt] {
        match self {
            Self::ResponsesToInteractions => RESPONSES_TO_INTERACTIONS_JSON,
            Self::InteractionsToResponses => INTERACTIONS_TO_RESPONSES_JSON,
        }
    }

    fn drop_deliberate_omissions(self, _case: &Case, _go: &mut Value) -> Option<Deviation> {
        None
    }

    fn joins_stream(self) -> bool {
        false
    }

    fn native(stage: Stage, from: &str, to: &str) -> Option<Self> {
        if stage != Stage::Request {
            return None;
        }
        KINDS.iter().copied().find(|kind| kind.pair() == (from, to))
    }
}

/// Where the Responses → Interactions translator writes the JSON text of a
/// value given where a string belongs, which upstream copies as written and
/// we write compactly: names, IDs, texts, image fields, a custom tool call's
/// input and tool descriptions. A qualified name or an instruction can hold
/// it within other text: a namespace and its tool's name joined, or the
/// texts of instruction parts run together.
const RESPONSES_TO_INTERACTIONS_JSON: &[JsonAt] = &[
    ("$.model", JsonForm::Whole),
    ("$.system_instruction", JsonForm::InText),
    ("$.previous_interaction_id", JsonForm::Whole),
    ("$.environment_id", JsonForm::Whole),
    ("$.input[*].content[*].text", JsonForm::Whole),
    ("$.input[*].content[*].image_url", JsonForm::Whole),
    ("$.input[*].content[*].data", JsonForm::Whole),
    ("$.input[*].content[*].mime_type", JsonForm::Whole),
    ("$.input[*].name", JsonForm::InText),
    ("$.input[*].call_id", JsonForm::Whole),
    ("$.input[*].arguments.input", JsonForm::Whole),
    ("$.tools[*].name", JsonForm::InText),
    ("$.tools[*].description", JsonForm::Whole),
    ("$.generation_config.tool_choice.name", JsonForm::InText),
    (
        "$.generation_config.tool_choice.function.name",
        JsonForm::InText,
    ),
    (
        "$.generation_config.tool_choice.custom.name",
        JsonForm::InText,
    ),
];

/// The same for the Interactions → Responses translator: names, IDs and
/// texts, and a call's arguments or a result that isn't a string. A media
/// part's URL can hold it within a data URL, and an audio part's note
/// within its text.
const INTERACTIONS_TO_RESPONSES_JSON: &[JsonAt] = &[
    ("$.model", JsonForm::Whole),
    ("$.instructions", JsonForm::InText),
    ("$.previous_response_id", JsonForm::Whole),
    ("$.environment_id", JsonForm::Whole),
    ("$.input[*].content[*].text", JsonForm::InText),
    ("$.input[*].content[*].image_url", JsonForm::InText),
    ("$.input[*].content[*].file_data", JsonForm::InText),
    ("$.input[*].content[*].filename", JsonForm::Whole),
    ("$.input[*].summary[*].text", JsonForm::Whole),
    ("$.input[*].name", JsonForm::Whole),
    ("$.input[*].call_id", JsonForm::Whole),
    ("$.input[*].arguments", JsonForm::Whole),
    ("$.input[*].output", JsonForm::Whole),
    ("$.tools[*].name", JsonForm::Whole),
    ("$.tools[*].description", JsonForm::Whole),
];

/// The hand-written registry request cases for the pairs, each list
/// with its pair: the suites' own, but the known differences.
pub fn registry_requests() -> Vec<(Pair, Vec<Case>)> {
    KINDS
        .iter()
        .map(|&kind| {
            let cases = kind
                .cases()
                .into_iter()
                .filter(|case| case.known_difference.is_none())
                .collect();
            (kind.pair(), cases)
        })
        .collect()
}

/// `count` random registry request cases for each of the pairs.
pub fn registry_request_cases(seed: u64, count: usize) -> Vec<(Pair, Vec<Case>)> {
    KINDS
        .iter()
        .map(|&kind| (kind.pair(), kind.generate(seed, count)))
        .collect()
}
