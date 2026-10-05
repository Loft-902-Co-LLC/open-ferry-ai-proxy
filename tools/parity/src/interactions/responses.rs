//! The OpenAI Responses and Interactions translators' suites (P4 WP4-C),
//! whose Go entries go in `go/interactions/parity_responses.go`: the request
//! suites in [`request`] and the response suites in [`response`], so that
//! if WP4-C is split, WP4-C1 and WP4-C2 each own one. This module only joins
//! them, and needs no change.

mod request;
mod response;

use serde_json::Value;

use super::{Family, Pair, ResponseCases, Stage, Suite};
use crate::cases::Case;
use crate::compare::{Deviation, JsonAt};
use crate::translator::Translator;

/// A request suite or a response suite.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Request(request::Kind),
    Response(response::Kind),
}

/// The request suites then the response suites, with their cases.
pub fn suites(seed: u64, random: usize) -> Vec<Suite> {
    let requests = request::KINDS.iter().copied().map(Kind::Request);
    let responses = response::KINDS.iter().copied().map(Kind::Response);
    requests
        .chain(responses)
        .map(|kind| {
            let translator = Translator::Interactions(super::Kind::Responses(kind));
            (translator, kind.cases(), kind.generate(seed, random))
        })
        .collect()
}

/// Evaluates `$body` with `$kind` bound to the request or response kind.
macro_rules! dispatch {
    ($value:expr, $kind:ident => $body:expr) => {
        match $value {
            Kind::Request($kind) => $body,
            Kind::Response($kind) => $body,
        }
    };
}

impl Family for Kind {
    fn key(self) -> &'static str {
        dispatch!(self, kind => kind.key())
    }

    fn slug(self) -> &'static str {
        dispatch!(self, kind => kind.slug())
    }

    fn title(self) -> &'static str {
        dispatch!(self, kind => kind.title())
    }

    fn cases(self) -> Vec<Case> {
        dispatch!(self, kind => kind.cases())
    }

    fn generate(self, seed: u64, count: usize) -> Vec<Case> {
        dispatch!(self, kind => kind.generate(seed, count))
    }

    fn run(self, case: &Case) -> Result<Value, String> {
        dispatch!(self, kind => kind.run(case))
    }

    fn read(self, case: &Case, output: &[u8]) -> Option<Value> {
        dispatch!(self, kind => kind.read(case, output))
    }

    fn embedded_json(self, case: &Case) -> &'static [JsonAt] {
        dispatch!(self, kind => kind.embedded_json(case))
    }

    fn drop_deliberate_omissions(self, case: &Case, go: &mut Value) -> Option<Deviation> {
        dispatch!(self, kind => kind.drop_deliberate_omissions(case, go))
    }

    fn joins_stream(self) -> bool {
        dispatch!(self, kind => kind.joins_stream())
    }

    fn native(stage: Stage, from: &str, to: &str) -> Option<Self> {
        request::Kind::native(stage, from, to)
            .map(Self::Request)
            .or_else(|| response::Kind::native(stage, from, to).map(Self::Response))
    }
}

/// The request suites' hand-written registry cases (see
/// [`request::registry_requests`]).
pub fn registry_requests() -> Vec<(Pair, Vec<Case>)> {
    request::registry_requests()
}

/// The request suites' random registry cases (see
/// [`request::registry_request_cases`]).
pub fn registry_request_cases(seed: u64, count: usize) -> Vec<(Pair, Vec<Case>)> {
    request::registry_request_cases(seed, count)
}

/// The response suites' hand-written registry stream cases (see
/// [`response::registry_streams`]).
pub fn registry_streams() -> Vec<(Pair, Vec<Case>)> {
    response::registry_streams()
}

/// The response suites' hand-written registry non-streaming cases (see
/// [`response::registry_finals`]).
pub fn registry_finals() -> Vec<(Pair, Vec<Case>)> {
    response::registry_finals()
}

/// The response suites' random registry cases (see
/// [`response::registry_response_cases`]).
pub fn registry_response_cases(seed: u64, count: usize) -> Vec<ResponseCases> {
    response::registry_response_cases(seed, count)
}
