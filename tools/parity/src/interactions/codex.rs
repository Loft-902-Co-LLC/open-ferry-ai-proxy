//! The Interactions to Codex translators' suites (P4 WP4-D), all in
//! tr/codex/interactions, for Interactions clients to a Codex upstream:
//! - `codex/interactions/request`: the client's request as Codex's
//!   ([`Kind::Request`]);
//! - `codex/interactions/response`: Codex's event stream as the client's
//!   ([`Kind::Stream`]);
//! - `codex/interactions/response-non-stream`: Codex's final event as the
//!   client's whole response ([`Kind::NonStream`]).
//!
//! Their Go entries are in `go/interactions/parity_codex.go`. Hand-written
//! cases are in [`cases`], and random ones come from
//! `crate::generate::interactions::codex`. [`Family::native`] maps the
//! registry's request stage of `interactions` → `codex`, and its response
//! stages of `codex` → `interactions`, to these suites, and the `registry_*`
//! functions give their cases. The registry stream suite also sends every
//! fifth hand-written stream case of the other pairs from `codex` to
//! `interactions` (see `crate::cases::registry::streams`), so those run this
//! family's stream translator too.
//!
//! Outputs are read with what the translators take from the clock masked
//! (see [`mask_volatile`]): the IDs they make up and the times they write.
//!
//! Upstream copies the generation config's settings to a request walking a
//! Go map, whose order changes from run to run, so the fields they add come
//! out in an order that changes too. The port adds them in the order
//! upstream lists them, and upstream's request is read with them in that
//! order (see [`settings_in_listed_order`]). Which of two settings for the
//! same field wins (or whether `verbosity` lands in `text`) changes as well:
//! the random cases never have two, and the hand-written ones that do are
//! known differences. So are those with the port's other deviations, which
//! its module docs list: a repeated key, an integer out of `i64`'s range, a
//! stream line that isn't JSON, and call arguments that start as a JSON
//! object but aren't one, which upstream writes into its response as they
//! are.

mod cases;

use open_ferry_translate::codex::interactions::{
    CodexToInteractionsStream, convert_codex_response_to_interactions_non_stream,
    convert_interactions_request_to_codex,
};
use open_ferry_translate::json::exact;
use serde_json::Value;

use super::{Family, Pair, ResponseCases, Stage, Suite, mask_volatile};
use crate::cases::Case;
use crate::compare::{Deviation, JsonAt, JsonForm};
use crate::generate::interactions::codex as generate;
use crate::translator::{Translator, sse_frames_as_written};

/// The family's suites, a variant each.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `codex/interactions/request`.
    Request,
    /// `codex/interactions/response`.
    Stream,
    /// `codex/interactions/response-non-stream`.
    NonStream,
}

/// Every suite, in the order they run.
pub const KINDS: &[Kind] = &[Kind::Request, Kind::Stream, Kind::NonStream];

/// The registry's request pair: the client's format, then the provider's.
const REQUEST_PAIR: Pair = ("interactions", "codex");

/// The registry's response pair: the provider's format, then the client's.
const RESPONSE_PAIR: Pair = ("codex", "interactions");

/// The suites, with their cases.
pub fn suites(seed: u64, random: usize) -> Vec<Suite> {
    KINDS
        .iter()
        .map(|&kind| {
            let translator = Translator::Interactions(super::Kind::Codex(kind));
            (translator, kind.cases(), kind.generate(seed, random))
        })
        .collect()
}

impl Family for Kind {
    fn key(self) -> &'static str {
        match self {
            Self::Request => "codex/interactions/request",
            Self::Stream => "codex/interactions/response",
            Self::NonStream => "codex/interactions/response-non-stream",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Self::Request => "interactions-to-codex-request",
            Self::Stream => "codex-to-interactions-stream",
            Self::NonStream => "codex-to-interactions-non-stream",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Request => "Interactions -> Codex request",
            Self::Stream => "Codex -> Interactions stream",
            Self::NonStream => "Codex -> Interactions non-stream",
        }
    }

    fn cases(self) -> Vec<Case> {
        match self {
            Self::Request => cases::requests(),
            Self::Stream => cases::streams(),
            Self::NonStream => cases::finals(),
        }
    }

    fn generate(self, seed: u64, count: usize) -> Vec<Case> {
        match self {
            Self::Request => generate::request_cases(seed, count),
            Self::Stream => generate::stream_cases(seed, count),
            Self::NonStream => generate::final_cases(seed, count),
        }
    }

    fn run(self, case: &Case) -> Result<Value, String> {
        let output = match self {
            Self::Request => {
                let request = exact::from_str(&case.request)
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                // As the harness's streamOption reads it.
                let stream = case.options["stream"].as_bool().unwrap_or(false);
                convert_interactions_request_to_codex(&case.model, &request, stream).to_string()
            }
            // One event at a time, the chunks joined, as the harness writes them.
            Self::Stream => {
                let mut stream = CodexToInteractionsStream::new(&case.model);
                case.events
                    .iter()
                    .map(|event| stream.translate_line(event.as_bytes()))
                    .collect()
            }
            Self::NonStream => {
                let event = case
                    .events
                    .first()
                    .and_then(|event| exact::from_str(event).ok())
                    .unwrap_or(Value::Null);
                convert_codex_response_to_interactions_non_stream(&case.model, &event).to_string()
            }
        };
        self.read(case, output.as_bytes())
            .ok_or_else(|| "output is not JSON".to_owned())
    }

    /// Reads the output as JSON, a stream as its frames (see
    /// [`sse_frames_as_written`]), each number kept as written, with a
    /// request's settings in upstream's order (see
    /// [`settings_in_listed_order`]) and the clock's readings masked.
    fn read(self, case: &Case, output: &[u8]) -> Option<Value> {
        let text = String::from_utf8_lossy(output);
        let mut value = match self {
            Self::Request => {
                let mut request = exact::from_str(&text).ok()?;
                settings_in_listed_order(case, &mut request);
                request
            }
            Self::Stream => sse_frames_as_written(&text),
            Self::NonStream => exact::from_str(&text).ok()?,
        };
        mask_volatile(&mut value);
        Some(value)
    }

    fn embedded_json(self, _case: &Case) -> &'static [JsonAt] {
        match self {
            Self::Request => REQUEST_JSON,
            Self::Stream => STREAM_JSON,
            Self::NonStream => NON_STREAM_JSON,
        }
    }

    fn drop_deliberate_omissions(self, _case: &Case, _go: &mut Value) -> Option<Deviation> {
        None
    }

    fn joins_stream(self) -> bool {
        self == Self::Stream
    }

    fn native(stage: Stage, from: &str, to: &str) -> Option<Self> {
        match stage {
            Stage::Request => (REQUEST_PAIR == (from, to)).then_some(Self::Request),
            Stage::Stream => (RESPONSE_PAIR == (from, to)).then_some(Self::Stream),
            Stage::NonStream => (RESPONSE_PAIR == (from, to)).then_some(Self::NonStream),
        }
    }
}

/// The generation config's settings upstream copies, each with the field it
/// sets, in the order upstream lists them.
const SETTINGS: &[(&str, &str)] = &[
    ("max_output_tokens", "max_output_tokens"),
    ("maxOutputTokens", "max_output_tokens"),
    ("max_tokens", "max_output_tokens"),
    ("temperature", "temperature"),
    ("top_p", "top_p"),
    ("topP", "top_p"),
    ("presence_penalty", "presence_penalty"),
    ("presencePenalty", "presence_penalty"),
    ("frequency_penalty", "frequency_penalty"),
    ("frequencyPenalty", "frequency_penalty"),
    ("parallel_tool_calls", "parallel_tool_calls"),
    ("parallelToolCalls", "parallel_tool_calls"),
    ("response_format", "response_format"),
    ("responseFormat", "response_format"),
    ("text", "text"),
    ("verbosity", "text"),
    ("truncation", "truncation"),
    ("tool_choice", "tool_choice"),
    ("toolChoice", "tool_choice"),
    ("service_tier", "service_tier"),
    ("serviceTier", "service_tier"),
];

/// Puts the fields of `request`, a translated request, that the case's
/// generation config set in the order upstream lists the settings, each
/// among the places those fields hold. None of them is in the request
/// before the settings are copied, so upstream adds each where its map
/// walk first reaches it, and the port in that order. Fields set later
/// (`tools`, `tool_choice`, `service_tier` and those that pass through)
/// stay where the settings put them, or go after them.
fn settings_in_listed_order(case: &Case, request: &mut Value) {
    let Ok(body) = serde_json::from_str::<Value>(&case.request) else {
        return;
    };
    let Some(config) = body
        .get("generation_config")
        .or_else(|| body.get("generationConfig"))
    else {
        return;
    };
    let Value::Object(fields) = request else {
        return;
    };
    let set: Vec<&str> = SETTINGS
        .iter()
        .filter(|(source, _)| config.get(*source).is_some())
        .map(|&(_, field)| field)
        .collect();
    let rank = |key: &str| SETTINGS.iter().position(|&(_, field)| field == key);
    let keys: Vec<String> = fields.keys().cloned().collect();
    let places: Vec<usize> = (0..keys.len())
        .filter(|&index| set.contains(&keys[index].as_str()))
        .collect();
    let mut sorted: Vec<&String> = places.iter().map(|&index| &keys[index]).collect();
    sorted.sort_by_key(|key| rank(key));
    let mut order = keys.clone();
    for (&place, key) in places.iter().zip(sorted) {
        order[place].clone_from(key);
    }
    let mut old = std::mem::take(fields);
    for key in order {
        if let Some(value) = old.shift_remove(&key) {
            fields.insert(key, value);
        }
    }
}

/// Where the request translator writes compact JSON for a value given where
/// a string belongs, which upstream copies as written: names, IDs, texts,
/// media fields, a call's arguments and a result's output, and tool
/// descriptions. The instructions, a reasoning item's content and a data
/// URL can hold it within other text: parts' texts joined, or a MIME type
/// and data put together.
const REQUEST_JSON: &[JsonAt] = &[
    ("$.instructions", JsonForm::InText),
    ("$.input[*].content", JsonForm::InText),
    ("$.input[*].content[*].text", JsonForm::Whole),
    ("$.input[*].content[*].image_url", JsonForm::InText),
    ("$.input[*].content[*].file_data", JsonForm::Whole),
    ("$.input[*].content[*].file_url", JsonForm::Whole),
    ("$.input[*].content[*].filename", JsonForm::Whole),
    ("$.input[*].content[*].input_audio.data", JsonForm::Whole),
    ("$.input[*].id", JsonForm::Whole),
    ("$.input[*].name", JsonForm::Whole),
    ("$.input[*].call_id", JsonForm::Whole),
    ("$.input[*].arguments", JsonForm::Whole),
    ("$.input[*].output", JsonForm::Whole),
    ("$.tools[*].name", JsonForm::Whole),
    ("$.tools[*].description", JsonForm::Whole),
];

/// The same for the stream translator: the response's ID, model and status,
/// a call's name, ID and arguments, deltas, and an image's data and type. A
/// reasoning item's text can hold it within other text, its parts' texts
/// joined.
const STREAM_JSON: &[JsonAt] = &[
    ("$[*].data.interaction.id", JsonForm::Whole),
    ("$[*].data.interaction.model", JsonForm::Whole),
    ("$[*].data.interaction.status", JsonForm::Whole),
    ("$[*].data.interaction_id", JsonForm::Whole),
    ("$[*].data.step.id", JsonForm::Whole),
    ("$[*].data.step.call_id", JsonForm::Whole),
    ("$[*].data.step.name", JsonForm::Whole),
    ("$[*].data.delta.text", JsonForm::Whole),
    ("$[*].data.delta.arguments", JsonForm::Whole),
    ("$[*].data.delta.content.text", JsonForm::InText),
    ("$[*].data.delta.content.data", JsonForm::Whole),
    ("$[*].data.delta.content.mime_type", JsonForm::Whole),
];

/// The same for the non-streaming translator.
const NON_STREAM_JSON: &[JsonAt] = &[
    ("$.id", JsonForm::Whole),
    ("$.model", JsonForm::Whole),
    ("$.status", JsonForm::Whole),
    ("$.steps[*].name", JsonForm::Whole),
    ("$.steps[*].call_id", JsonForm::Whole),
    ("$.steps[*].content[*].text", JsonForm::InText),
    ("$.steps[*].content[*].data", JsonForm::Whole),
    ("$.steps[*].content[*].mime_type", JsonForm::Whole),
];

/// The suite's hand-written cases but the known differences.
fn registry_cases(kind: Kind) -> Vec<Case> {
    kind.cases()
        .into_iter()
        .filter(|case| case.known_difference.is_none())
        .collect()
}

/// The hand-written registry request cases for the family's pairs, each list
/// with its pair.
pub fn registry_requests() -> Vec<(Pair, Vec<Case>)> {
    vec![(REQUEST_PAIR, registry_cases(Kind::Request))]
}

/// `count` random registry request cases for each of the family's pairs.
pub fn registry_request_cases(seed: u64, count: usize) -> Vec<(Pair, Vec<Case>)> {
    vec![(REQUEST_PAIR, generate::request_cases(seed, count))]
}

/// The hand-written registry stream cases for the family's pairs, each list
/// with its pair.
pub fn registry_streams() -> Vec<(Pair, Vec<Case>)> {
    vec![(RESPONSE_PAIR, registry_cases(Kind::Stream))]
}

/// The hand-written registry non-streaming cases for the family's pairs, each
/// list with its pair.
pub fn registry_finals() -> Vec<(Pair, Vec<Case>)> {
    vec![(RESPONSE_PAIR, registry_cases(Kind::NonStream))]
}

/// `count` random registry stream cases, and as many non-streaming ones,
/// for each of the family's pairs.
pub fn registry_response_cases(seed: u64, count: usize) -> Vec<ResponseCases> {
    let cases = (
        generate::stream_cases(seed, count),
        generate::final_cases(seed, count),
    );
    vec![(RESPONSE_PAIR, cases)]
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn settings_are_read_in_listed_order() {
        let case = Case::new(
            "order",
            "gpt-5",
            json!({ "generationConfig": { "toolChoice": "auto", "topP": 1, "max_tokens": 5 } })
                .to_string(),
        );
        let mut go: Value = serde_json::from_str(
            r#"{"model":"gpt-5","input":[],"tool_choice":"auto","max_output_tokens":5,"top_p":1,"tools":[]}"#,
        )
        .unwrap();
        settings_in_listed_order(&case, &mut go);
        let keys: Vec<&String> = go.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "model",
                "input",
                "max_output_tokens",
                "top_p",
                "tool_choice",
                "tools"
            ]
        );
    }

    #[test]
    fn the_pairs_map_to_the_suites() {
        assert!(Kind::native(Stage::Request, "interactions", "codex") == Some(Kind::Request));
        assert!(Kind::native(Stage::Stream, "codex", "interactions") == Some(Kind::Stream));
        assert!(Kind::native(Stage::NonStream, "codex", "interactions") == Some(Kind::NonStream));
        assert!(Kind::native(Stage::Request, "codex", "interactions").is_none());
        assert!(Kind::native(Stage::Stream, "interactions", "codex").is_none());
    }
}
