//! The OpenAI Responses and Interactions response translators' suites (P4
//! WP4-C, or WP4-C2 if it is split).
//!
//! Not ported yet: there are none. WP4-C adds a [`Kind`] variant for each,
//! lists it in [`KINDS`] and fills in the methods below:
//! - `interactions/openai-responses/response`, `.../response-non-stream`
//!   and `.../tool-input-error` (FinalizeToolInput): Interactions responses
//!   for OpenAI Responses clients;
//! - `openai-responses/interactions/response` and
//!   `.../response-non-stream`: Responses responses for Interactions
//!   clients.
//!
//! All are tr/openai/interactions/responses. Hand-written cases go here or
//! in a `response/` directory, and random ones in
//! `crate::generate::interactions::responses::response`. Once the pairs are
//! registered, [`Family::native`] maps the response stages of
//! `interactions` → `openai-response` and `openai-response` →
//! `interactions` to these suites, and the `registry_*` functions give
//! their cases.

use serde_json::Value;

use super::super::{Family, Pair, ResponseCases, Stage};
use crate::cases::Case;
use crate::compare::{Deviation, JsonAt};

/// The suites, a variant each.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {}

/// Every suite, in the order they run.
pub const KINDS: &[Kind] = &[];

impl Family for Kind {
    fn key(self) -> &'static str {
        match self {}
    }

    fn slug(self) -> &'static str {
        match self {}
    }

    fn title(self) -> &'static str {
        match self {}
    }

    fn cases(self) -> Vec<Case> {
        match self {}
    }

    fn generate(self, seed: u64, count: usize) -> Vec<Case> {
        let _ = (seed, count);
        match self {}
    }

    fn run(self, case: &Case) -> Result<Value, String> {
        let _ = case;
        match self {}
    }

    fn read(self, case: &Case, output: &[u8]) -> Option<Value> {
        let _ = (case, output);
        match self {}
    }

    fn embedded_json(self, case: &Case) -> &'static [JsonAt] {
        let _ = case;
        match self {}
    }

    fn drop_deliberate_omissions(self, case: &Case, go: &mut Value) -> Option<Deviation> {
        let _ = (case, go);
        match self {}
    }

    fn joins_stream(self) -> bool {
        match self {}
    }

    fn native(stage: Stage, from: &str, to: &str) -> Option<Self> {
        let _ = (stage, from, to);
        None
    }
}

/// The hand-written registry stream cases for the pairs, each list
/// with its pair.
pub fn registry_streams() -> Vec<(Pair, Vec<Case>)> {
    Vec::new()
}

/// The hand-written registry non-streaming cases for the pairs, each
/// list with its pair.
pub fn registry_finals() -> Vec<(Pair, Vec<Case>)> {
    Vec::new()
}

/// `count` random registry stream cases, and as many non-streaming ones,
/// for each of the pairs.
pub fn registry_response_cases(seed: u64, count: usize) -> Vec<ResponseCases> {
    let _ = (seed, count);
    Vec::new()
}
