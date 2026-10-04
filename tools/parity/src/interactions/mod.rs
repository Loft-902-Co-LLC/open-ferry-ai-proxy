//! The Gemini Interactions translators' suites (P4), one family for each
//! group of upstream translator packages, in a module of its own that lists
//! its suites:
//! - [`claude`]: Claude and Interactions (WP4-A);
//! - [`chat`]: Chat Completions and Interactions (WP4-B);
//! - [`responses`]: OpenAI Responses and Interactions (WP4-C, or WP4-C1 and
//!   WP4-C2);
//! - [`codex`]: Interactions to Codex (WP4-D);
//! - [`gemini`]: Gemini and Interactions, and the passthrough (WP4-E).
//!
//! Each suite is a [`Translator::Interactions`] of its family's kind, whose
//! [`Family`] methods the translator's own delegate to, and runs after the
//! other suites. Its Go side is an entry in the Interactions harness
//! (`go/interactions/main.go`), added by the family's
//! `go/interactions/parity_<family>.go`. Its key has `interactions` as its
//! package or format, which sends it to that harness.
//!
//! The registry suites run a family's translators too, once the family
//! registers its pairs, in `go/parity_registry_<family>.go` and the Rust
//! registry. [`Kind::native`] maps each pair to the family's suite, whose
//! output a registry case is read as, and the `registry_*` functions give
//! each pair's cases, which run after the others.
//!
//! Hand-written cases live in each family's module (or a directory under
//! it), and random ones in `crate::generate::interactions`, whose shared
//! generators make Interactions requests, event streams and responses.
//! [`mask_volatile`] masks the IDs and times upstream's translators read
//! from the clock.

mod chat;
mod claude;
mod codex;
mod gemini;
mod responses;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::cases::Case;
use crate::compare::{self, Deviation, JsonAt};
use crate::translator::{CREATED_NOW, Translator};

/// A suite as `main` runs it: the translator, its hand-written cases and
/// its random ones.
pub type Suite = (Translator, Vec<Case>, Vec<Case>);

/// A pair of formats in the registry: for a request, the client's format
/// and the provider's; for a response, the provider's and the client's.
pub type Pair = (&'static str, &'static str);

/// A registry pair's random stream cases and as many non-streaming ones.
pub type ResponseCases = (Pair, (Vec<Case>, Vec<Case>));

/// Which of the registry's translations a registry case runs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// `registry/request`.
    Request,
    /// `registry/response`.
    Stream,
    /// `registry/response-non-stream`.
    NonStream,
}

/// An Interactions suite: its family, and the suite within it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Claude(claude::Kind),
    Chat(chat::Kind),
    Responses(responses::Kind),
    Codex(codex::Kind),
    Gemini(gemini::Kind),
}

/// What a family's suites do. Each family's kind implements it, and
/// [`Translator`]'s methods of the same names delegate to it.
pub trait Family: Copy {
    /// The Interactions harness's key, `<package>/<format>/<kind>`, with
    /// `interactions` as its package or format.
    fn key(self) -> &'static str;
    /// A short name for directories.
    fn slug(self) -> &'static str;
    /// The suite's heading in the report.
    fn title(self) -> &'static str;
    /// The hand-written cases.
    fn cases(self) -> Vec<Case>;
    /// `count` random cases for `seed`.
    fn generate(self, seed: u64, count: usize) -> Vec<Case>;
    /// Runs our port on `case`, returning its output in the form
    /// [`Self::read`] gives.
    fn run(self, case: &Case) -> Result<Value, String>;
    /// Reads the harness entry's raw output as JSON, or `None` if it isn't
    /// the kind of output the translator should produce. Clock readings are
    /// masked here (see [`mask_volatile`]).
    fn read(self, case: &Case, output: &[u8]) -> Option<Value>;
    /// Where the translator writes JSON it read compactly while upstream
    /// copies its text (see [`Translator::embedded_json`]).
    fn embedded_json(self, case: &Case) -> &'static [JsonAt];
    /// Takes out of upstream's output what we leave out on purpose (see
    /// [`Translator::drop_deliberate_omissions`]).
    fn drop_deliberate_omissions(self, case: &Case, go: &mut Value) -> Option<Deviation>;
    /// For a stream suite, whether its harness entry writes its chunks
    /// joined, as SSE text, rather than as a JSON array of them. A registry
    /// stream that runs the suite's translator has its chunks joined the
    /// same way before [`Self::read`] reads them.
    fn joins_stream(self) -> bool;
    /// The suite whose translator the registry runs for `stage` and the
    /// pair `from`, `to` (see [`Pair`]), if the family registers one.
    fn native(stage: Stage, from: &str, to: &str) -> Option<Self>;
}

/// Evaluates `$body` with `$kind` bound to the family's own kind.
macro_rules! dispatch {
    ($value:expr, $kind:ident => $body:expr) => {
        match $value {
            Kind::Claude($kind) => $body,
            Kind::Chat($kind) => $body,
            Kind::Responses($kind) => $body,
            Kind::Codex($kind) => $body,
            Kind::Gemini($kind) => $body,
        }
    };
}

/// Calls `$function` in every family's module, joining the lists they
/// return in family order.
macro_rules! each_family {
    ($function:ident($($arg:expr),*)) => {{
        let mut all = claude::$function($($arg),*);
        all.extend(chat::$function($($arg),*));
        all.extend(responses::$function($($arg),*));
        all.extend(codex::$function($($arg),*));
        all.extend(gemini::$function($($arg),*));
        all
    }};
}

impl Kind {
    pub fn key(self) -> &'static str {
        dispatch!(self, kind => kind.key())
    }

    pub fn slug(self) -> &'static str {
        dispatch!(self, kind => kind.slug())
    }

    pub fn title(self) -> &'static str {
        dispatch!(self, kind => kind.title())
    }

    pub fn run(self, case: &Case) -> Result<Value, String> {
        dispatch!(self, kind => kind.run(case))
    }

    pub fn read(self, case: &Case, output: &[u8]) -> Option<Value> {
        dispatch!(self, kind => kind.read(case, output))
    }

    pub fn embedded_json(self, case: &Case) -> &'static [JsonAt] {
        dispatch!(self, kind => kind.embedded_json(case))
    }

    pub fn drop_deliberate_omissions(self, case: &Case, go: &mut Value) -> Option<Deviation> {
        dispatch!(self, kind => kind.drop_deliberate_omissions(case, go))
    }

    pub fn joins_stream(self) -> bool {
        dispatch!(self, kind => kind.joins_stream())
    }

    /// The suite whose translator the registry runs for `stage` and the
    /// pair `from`, `to`, if a family registers one.
    pub fn native(stage: Stage, from: &str, to: &str) -> Option<Self> {
        claude::Kind::native(stage, from, to)
            .map(Self::Claude)
            .or_else(|| chat::Kind::native(stage, from, to).map(Self::Chat))
            .or_else(|| responses::Kind::native(stage, from, to).map(Self::Responses))
            .or_else(|| codex::Kind::native(stage, from, to).map(Self::Codex))
            .or_else(|| gemini::Kind::native(stage, from, to).map(Self::Gemini))
    }
}

/// Every family's suites with their cases, in the order they run.
pub fn suites(seed: u64, random: usize) -> Vec<Suite> {
    each_family!(suites(seed, random))
}

/// The families' hand-written registry request cases, each list with the
/// pair it is sent through.
pub fn registry_requests() -> Vec<(Pair, Vec<Case>)> {
    each_family!(registry_requests())
}

/// The families' hand-written registry stream cases, each list with its
/// pair.
pub fn registry_streams() -> Vec<(Pair, Vec<Case>)> {
    each_family!(registry_streams())
}

/// The families' hand-written registry non-streaming cases, each list with
/// its pair.
pub fn registry_finals() -> Vec<(Pair, Vec<Case>)> {
    each_family!(registry_finals())
}

/// The families' random registry request cases for `seed`: `count` for each
/// pair.
pub fn registry_request_cases(seed: u64, count: usize) -> Vec<(Pair, Vec<Case>)> {
    each_family!(registry_request_cases(seed, count))
}

/// The families' random registry response cases for `seed`: `count` stream
/// cases and as many non-streaming ones for each pair.
pub fn registry_response_cases(seed: u64, count: usize) -> Vec<ResponseCases> {
    each_family!(registry_response_cases(seed, count))
}

/// The prefixes of the IDs upstream's Interactions translators make up from
/// the clock, as `<prefix>_<unix nanoseconds>`.
const GENERATED_ID_PREFIXES: &[&str] = &["interaction", "step", "msg", "chatcmpl", "response"];

/// How far from now a clock reading may be and still be taken for one.
const NOW_WINDOW: Duration = Duration::from_secs(3600);

/// Masks what upstream's Interactions translators and ours each read from
/// the clock, anywhere in `value`:
/// - an ID made up as `<prefix>_<unix nanoseconds>` (see
///   [`GENERATED_ID_PREFIXES`]) within an hour of now becomes
///   `<prefix>_(generated-<n>)` for the `n`th distinct one, so where the
///   same ID recurs still shows;
/// - a `created` or `updated` time within an hour of now, as RFC 3339 text
///   or unix seconds, becomes [`CREATED_NOW`]. Upstream writes RFC 3339
///   times in UTC, as we do, but they are read as instants anyway.
#[allow(
    dead_code,
    reason = "the families' readers call it as they land (WP4-A to WP4-E)"
)]
pub fn mask_volatile(value: &mut Value) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    mask_volatile_at(value, now, &mut Vec::new());
}

/// [`mask_volatile`], with the time taken for now, and the IDs already
/// masked in `seen`.
fn mask_volatile_at(value: &mut Value, now: Duration, seen: &mut Vec<String>) {
    match value {
        Value::Array(items) => {
            for item in items {
                mask_volatile_at(item, now, seen);
            }
        }
        Value::Object(fields) => {
            for (key, field) in fields.iter_mut() {
                if matches!(key.as_str(), "created" | "updated") && is_now(field, now) {
                    *field = CREATED_NOW.into();
                } else {
                    mask_volatile_at(field, now, seen);
                }
            }
        }
        Value::String(text) => {
            if let Some(prefix) = generated_id_prefix(text, now) {
                let prefix = prefix.to_owned();
                let n = match seen.iter().position(|id| id == text) {
                    Some(index) => index + 1,
                    None => {
                        seen.push(text.clone());
                        seen.len()
                    }
                };
                *text = format!("{prefix}_(generated-{n})");
            }
        }
        _ => {}
    }
}

/// Whether `value`, RFC 3339 text or unix seconds, is within
/// [`NOW_WINDOW`] of `now`.
fn is_now(value: &Value, now: Duration) -> bool {
    let seconds = match value {
        Value::String(text) => compare::rfc3339_seconds(text),
        Value::Number(number) => number.as_i64().map(i128::from),
        _ => None,
    };
    let now = i128::from(now.as_secs());
    seconds.is_some_and(|seconds| seconds.abs_diff(now) < u128::from(NOW_WINDOW.as_secs()))
}

/// `text`'s prefix, if `text` is an ID made up from a clock reading within
/// [`NOW_WINDOW`] of `now`.
fn generated_id_prefix(text: &str, now: Duration) -> Option<&str> {
    let (prefix, nanos) = text.split_once('_')?;
    if !GENERATED_ID_PREFIXES.contains(&prefix)
        || nanos.is_empty()
        || !nanos.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let nanos: u128 = nanos.parse().ok()?;
    (nanos.abs_diff(now.as_nanos()) < NOW_WINDOW.as_nanos()).then_some(prefix)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// 2026-10-04T12:00:00Z.
    const NOW: Duration = Duration::from_secs(1_791_115_200);

    #[test]
    fn generated_ids_are_numbered_in_order() {
        let nanos = NOW.as_nanos();
        let (a, b) = (format!("step_{}", nanos - 5), format!("step_{}", nanos + 9));
        let mut value = json!([
            { "id": format!("interaction_{nanos}"), "step": { "id": a } },
            { "call_id": b, "again": a, "msg": format!("chatcmpl_{nanos}") },
        ]);
        mask_volatile_at(&mut value, NOW, &mut Vec::new());
        assert_eq!(
            value,
            json!([
                { "id": "interaction_(generated-1)", "step": { "id": "step_(generated-2)" } },
                {
                    "call_id": "step_(generated-3)",
                    "again": "step_(generated-2)",
                    "msg": "chatcmpl_(generated-4)",
                },
            ])
        );
    }

    #[test]
    fn other_ids_are_left_alone() {
        let nanos = NOW.as_nanos();
        let mut value = json!([
            "interaction_1700000000000000000",
            format!("toolu_{nanos}"),
            format!("msg_{nanos}x"),
            "msg_",
            format!("msg {nanos}"),
            nanos.to_string(),
        ]);
        let original = value.clone();
        mask_volatile_at(&mut value, NOW, &mut Vec::new());
        assert_eq!(value, original);
    }

    #[test]
    fn only_current_creation_and_update_times_are_masked() {
        let now = NOW.as_secs();
        let mut value = json!({
            "interaction": {
                "created": "2026-10-04T11:59:58Z",
                "updated": "2026-10-04T14:00:00+02:00",
            },
            "created": now - 5,
            "old": { "created": "2025-01-01T00:00:00Z", "updated": 1_700_000_000 },
            "other": { "created_at": now, "time": "2026-10-04T12:00:00Z" },
        });
        mask_volatile_at(&mut value, NOW, &mut Vec::new());
        assert_eq!(
            value,
            json!({
                "interaction": { "created": CREATED_NOW, "updated": CREATED_NOW },
                "created": CREATED_NOW,
                "old": { "created": "2025-01-01T00:00:00Z", "updated": 1_700_000_000 },
                "other": { "created_at": now, "time": "2026-10-04T12:00:00Z" },
            })
        );
    }
}
