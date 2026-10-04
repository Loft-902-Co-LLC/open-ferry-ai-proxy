//! Tests of the payload rules, ported from upstream's
//! internal/runtime/executor/helps/payload_helpers_codex_integer_test.go,
//! payload_helpers_disable_image_generation_test.go and
//! payload_mutations_test.go, and from the executors' payload tests.
//!
//! Upstream tests not ported:
//! - `TestSetStringIfDifferentReusesCanonicalValue`,
//!   `TestSetBoolIfDifferentReusesCanonicalValue` and
//!   `TestSetRawIfDifferentReusesIdenticalRawValue` check that a byte
//!   slice isn't copied; a parsed body is edited in place, so there is no
//!   copy to avoid. `TestSetRawIfDifferentUpdatesDifferentRawValue` tests
//!   `SetRawIfDifferent`, which nothing ported calls.
//! - `TestApplyPayloadConfigNormalizesByteSliceOverride` writes a Go
//!   `[]byte`, and `TestSetPayloadValueIfDifferentUsesSJSONNumberEncoding`
//!   a Go `float32`, neither of which a YAML config can hold.
//! - `TestSetPayloadValueIfDifferentCallsMarshalerOnce` counts calls of a
//!   Go `json.Marshaler`; a rule's value is encoded once, at load.
//! - `BenchmarkSetStringIfDifferentLargeCanonicalPayload` is a benchmark.
//! - `TestConfigExampleDocumentsCodexAdditionalToolsPayloadFilter` reads
//!   upstream's config.example.yaml, which this repository doesn't ship;
//!   `codex_additional_tools_filter` tests the path it documents.
//! - The executors' payload tests for features not ported: cloaking,
//!   billing headers, usage reporting, sensitive words, Fable, the Gemini
//!   Interactions API, AI Studio, Antigravity and xAI.

mod codex_integer;
mod disable_image_generation;
mod engine;
mod executors;
mod payload_mutations;

use http::HeaderMap;
use http::header::{HeaderName, HeaderValue};
use open_ferry_core::config::Config;
use serde_json::Value;

use super::{Call, Rules, Touched, apply_call};

/// The payload rules of a config document.
fn rules(yaml: &str) -> Rules {
    let config = Config::parse(yaml).expect("config parses");
    Rules::build(&config, false)
}

/// A JSON document.
fn json(text: &str) -> Value {
    serde_json::from_str(text).expect("valid JSON")
}

/// Headers from name and value pairs.
fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.append(
            HeaderName::from_bytes(name.as_bytes()).expect("header name"),
            HeaderValue::from_str(value).expect("header value"),
        );
    }
    map
}

/// Upstream's arguments to `ApplyPayloadConfigWithTrackedPathsForExecutor`.
#[derive(Default)]
struct Args<'a> {
    executor: &'a str,
    model: &'a str,
    protocol: &'a str,
    from: &'a str,
    root: &'a str,
    original: Option<&'a str>,
    requested: &'a str,
    request_path: &'a str,
    headers: HeaderMap,
    tracked: &'a [&'a str],
}

impl<'a> Args<'a> {
    /// `ApplyPayloadConfigWithRoot(cfg, model, protocol, root, payload,
    /// nil, "", "")`.
    fn with_root(model: &'a str, protocol: &'a str, root: &'a str) -> Self {
        Args {
            model,
            protocol,
            root,
            ..Args::default()
        }
    }

    /// The body `body` with `rules` applied, and the tracked paths touched.
    fn apply(&self, rules: Option<&Rules>, body: &str) -> (Value, Touched) {
        let mut body = json(body);
        let call = Call {
            executor: self.executor,
            protocol: self.protocol,
            from: self.from,
            model: self.model,
            requested_model: self.requested,
            request_path: self.request_path,
            root: self.root,
            headers: &self.headers,
            tracked: self.tracked,
        };
        let touched = apply_call(rules, &call, || self.original.map(json), &mut body);
        (body, touched)
    }

    /// The body `body` with the rules of the config document `yaml`
    /// applied.
    fn run(&self, yaml: &str, body: &str) -> Value {
        self.apply(Some(&rules(yaml)), body).0
    }
}
