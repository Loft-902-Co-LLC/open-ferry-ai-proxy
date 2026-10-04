//! The OpenAI Responses and Interactions request translators' suites (P4
//! WP4-C, or WP4-C1 if it is split).
//!
//! Not ported yet: there are none. WP4-C adds a [`Kind`] variant for each,
//! lists it in [`KINDS`] and fills in the methods below:
//! `interactions/openai-responses/request`, OpenAI Responses clients to an
//! Interactions upstream, and `openai-responses/interactions/request`,
//! Interactions clients to a Responses upstream (both
//! tr/openai/interactions/responses).
//!
//! Hand-written cases go here or in a `request/` directory, and random ones
//! in `crate::generate::interactions::responses::request`. Once the pairs
//! are registered, [`Family::native`] maps the request stage of
//! `openai-response` → `interactions` and `interactions` →
//! `openai-response` to these suites, and the `registry_*` functions give
//! their cases.

use serde_json::Value;

use super::super::{Family, Pair, Stage};
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

/// The hand-written registry request cases for the pairs, each list
/// with its pair.
pub fn registry_requests() -> Vec<(Pair, Vec<Case>)> {
    Vec::new()
}

/// `count` random registry request cases for each of the pairs.
pub fn registry_request_cases(seed: u64, count: usize) -> Vec<(Pair, Vec<Case>)> {
    let _ = (seed, count);
    Vec::new()
}
