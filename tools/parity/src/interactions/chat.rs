//! The Chat Completions and Interactions translators' suites (P4 WP4-B),
//! whose Go entries go in `go/interactions/parity_chat.go`.
//!
//! Not ported yet: there are none. WP4-B adds a [`Kind`] variant for each,
//! lists it in [`KINDS`] and fills in the methods below:
//! - `interactions/openai-chat/request`, `.../response` and
//!   `.../response-non-stream`: Chat Completions clients to an Interactions
//!   upstream;
//! - `openai/interactions/request`, `.../response` and
//!   `.../response-non-stream`: Interactions clients to a Chat Completions
//!   upstream.
//!
//! Both are tr/openai/interactions/chat-completions. Hand-written cases go
//! here or in a `chat/` directory, and random ones in
//! `crate::generate::interactions::chat`. Once WP4-B registers its pairs,
//! [`Family::native`] maps them to these suites (requests `openai` →
//! `interactions` and `interactions` → `openai`, and responses the same two
//! ways), and the `registry_*` functions give their cases.

use serde_json::Value;

use super::{Family, Pair, ResponseCases, Stage, Suite};
use crate::cases::Case;
use crate::compare::{Deviation, JsonAt};
use crate::translator::Translator;

/// The family's suites, a variant each.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {}

/// Every suite, in the order they run.
pub const KINDS: &[Kind] = &[];

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

/// The hand-written registry request cases for the family's pairs, each list
/// with its pair.
pub fn registry_requests() -> Vec<(Pair, Vec<Case>)> {
    Vec::new()
}

/// `count` random registry request cases for each of the family's pairs.
pub fn registry_request_cases(seed: u64, count: usize) -> Vec<(Pair, Vec<Case>)> {
    let _ = (seed, count);
    Vec::new()
}

/// The hand-written registry stream cases for the family's pairs, each list
/// with its pair.
pub fn registry_streams() -> Vec<(Pair, Vec<Case>)> {
    Vec::new()
}

/// The hand-written registry non-streaming cases for the family's pairs, each
/// list with its pair.
pub fn registry_finals() -> Vec<(Pair, Vec<Case>)> {
    Vec::new()
}

/// `count` random registry stream cases, and as many non-streaming ones,
/// for each of the family's pairs.
pub fn registry_response_cases(seed: u64, count: usize) -> Vec<ResponseCases> {
    let _ = (seed, count);
    Vec::new()
}
