// Ported from CLIProxyAPI internal/runtime/executor/helps/payload_helpers.go
// (payloadModelRulesMatch, payloadModelRuleConditionsMatch,
// payloadMatchConditionsMatch, payloadNotMatchConditionsMatch,
// payloadExistConditionsMatch, payloadNotExistConditionsMatch,
// payloadPathMatchesValue, payloadPathExists, payloadResultEquals,
// normalizedPayloadResult, normalizedPayloadValue, normalizedPayloadJSON,
// payloadFromProtocolMatches, normalizePayloadFromProtocol,
// payloadHeadersMatch, payloadHeaderValues, payloadModelCandidates,
// matchModelPattern) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Which rules apply to a call: model names with `*` wildcards, the
//! upstream and client protocols, request headers, and conditions on the
//! body as it is when the rule comes up.
//!
//! A `match` condition compares a path's value with the configured one as
//! upstream does, both decoded as Go's `encoding/json` decodes into `any`:
//! numbers as float64, so `1` equals `1.0`, and objects without their key
//! order.
//!
//! Headers are only read here. No rule writes one.
//!
//! Deviations from upstream: none.

use std::collections::BTreeMap;

use http::HeaderMap;
use open_ferry_core::config::AnyValue;
use open_ferry_translate::go::{equal_fold, parse_float, to_lower, trim_space};
use serde_json::Value;

use super::gjson::{self, Found};
use super::path::{build_path, resolve};
use crate::thinking::parse_suffix;

/// A JSON value as Go's `encoding/json` decodes it into `any`, which
/// `reflect.DeepEqual` compares.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Norm {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Norm>),
    Obj(BTreeMap<String, Norm>),
}

/// `value` decoded, or `None` where Go's decode fails: a number too large
/// for a float64.
pub(super) fn norm_json(value: &Value) -> Option<Norm> {
    Some(match value {
        Value::Null => Norm::Null,
        Value::Bool(b) => Norm::Bool(*b),
        Value::Number(number) => {
            let float = parse_float(number.as_str());
            if !float.is_finite() {
                return None;
            }
            Norm::Num(float)
        }
        Value::String(s) => Norm::Str(s.clone()),
        Value::Array(items) => Norm::Arr(items.iter().map(norm_json).collect::<Option<_>>()?),
        Value::Object(map) => Norm::Obj(
            map.iter()
                .map(|(key, value)| Some((key.clone(), norm_json(value)?)))
                .collect::<Option<_>>()?,
        ),
    })
}

/// A configured value as Go encodes it to JSON and decodes it again, or
/// `None` where the encoder refuses it.
pub(super) fn norm_any(value: &AnyValue) -> Option<Norm> {
    // An integer decodes as the float64 nearest it, which a cast gives.
    #[allow(clippy::cast_precision_loss)]
    Some(match value {
        AnyValue::Null => Norm::Null,
        AnyValue::Bool(b) => Norm::Bool(*b),
        AnyValue::Int(n) => Norm::Num(*n as f64),
        AnyValue::Uint(n) => Norm::Num(*n as f64),
        AnyValue::Float(f) if f.is_finite() => Norm::Num(*f),
        AnyValue::Str(s) => Norm::Str(s.clone()),
        AnyValue::Time(Some(text)) => Norm::Str(text.clone()),
        AnyValue::Seq(items) => Norm::Arr(items.iter().map(norm_any).collect::<Option<_>>()?),
        AnyValue::Map(map) => Norm::Obj(
            map.iter()
                .map(|(key, value)| Some((key.clone(), norm_any(value)?)))
                .collect::<Option<_>>()?,
        ),
        AnyValue::Float(_) | AnyValue::Time(None) | AnyValue::AnyMap => return None,
    })
}

/// What a path found, decoded (`normalizedPayloadResult`).
fn norm_found(found: &Found<'_>) -> Option<Norm> {
    match found {
        Found::At(value, _) => norm_json(value),
        _ => norm_json(&found.to_value()),
    }
}

/// When a rule applies: a model name or pattern and what the call and its
/// body must have (upstream's `PayloadModelRule`, its strings trimmed).
#[derive(Debug)]
pub(super) struct ModelRule {
    /// The model name or `*` pattern; never empty.
    pub(super) name: String,
    /// The upstream protocol, or empty for any.
    pub(super) protocol: String,
    /// The client protocol, normalized, or empty for any.
    pub(super) from_protocol: String,
    /// Header names, trimmed and not empty, and the patterns a value of
    /// theirs must match.
    pub(super) headers: Vec<(String, String)>,
    /// Paths that must have the value, `None` for one Go can't encode.
    pub(super) matches: Vec<(String, Option<Norm>)>,
    /// Paths that must not have the value.
    pub(super) not_matches: Vec<(String, Option<Norm>)>,
    /// Paths that must be there and not null.
    pub(super) exist: Vec<String>,
    /// Paths that must be missing or null.
    pub(super) not_exist: Vec<String>,
}

/// What a call brings to the rules' checks.
pub(super) struct Context<'a> {
    pub(super) protocol: &'a str,
    pub(super) from: &'a str,
    pub(super) headers: &'a HeaderMap,
    pub(super) root: &'a str,
    pub(super) candidates: &'a [String],
}

/// Whether one of `models` applies to the call and `body`
/// (`payloadModelRulesMatch`).
pub(super) fn rules_match(models: &[ModelRule], context: &Context<'_>, body: &Value) -> bool {
    if models.is_empty() || context.candidates.is_empty() {
        return false;
    }
    context.candidates.iter().any(|model| {
        models.iter().any(|entry| {
            (entry.protocol.is_empty()
                || context.protocol.is_empty()
                || equal_fold(&entry.protocol, context.protocol))
                && from_protocol_matches(&entry.from_protocol, context.from)
                && headers_match(context.headers, &entry.headers)
                && match_model_pattern(entry.name.as_bytes(), model.as_bytes())
                && conditions_match(entry, context.root, body)
        })
    })
}

/// `payloadModelRuleConditionsMatch`.
fn conditions_match(rule: &ModelRule, root: &str, body: &Value) -> bool {
    rule.matches
        .iter()
        .all(|(path, value)| path_matches_value(body, &build_path(root, path), value.as_ref()))
        && !rule
            .not_matches
            .iter()
            .any(|(path, value)| path_matches_value(body, &build_path(root, path), value.as_ref()))
        && rule
            .exist
            .iter()
            .all(|path| path_exists(body, &build_path(root, path)))
        && !rule
            .not_exist
            .iter()
            .any(|path| path_exists(body, &build_path(root, path)))
}

/// Whether a path `path` stands for has `expected` (`payloadPathMatchesValue`).
fn path_matches_value(body: &Value, path: &str, expected: Option<&Norm>) -> bool {
    let Some(expected) = expected else {
        return false;
    };
    resolve(body, path).iter().any(|resolved| {
        gjson::get(body, resolved)
            .is_some_and(|found| norm_found(&found).as_ref() == Some(expected))
    })
}

/// Whether a path `path` stands for has a value that isn't null
/// (`payloadPathExists`).
fn path_exists(body: &Value, path: &str) -> bool {
    resolve(body, path).iter().any(|resolved| {
        gjson::get(body, resolved).is_some_and(|found| !matches!(found, Found::At(Value::Null, _)))
    })
}

/// `normalizePayloadFromProtocol`: lower case and trimmed, with the
/// Responses spellings as `responses`.
pub(super) fn normalize_from_protocol(protocol: &str) -> String {
    let protocol = to_lower(protocol.trim());
    match protocol.as_str() {
        "openai-response" | "openai-responses" | "response" => "responses".to_owned(),
        _ => protocol,
    }
}

/// Whether the client protocol `from` is the normalized `pattern`, which
/// any matches when empty (`payloadFromProtocolMatches`).
fn from_protocol_matches(pattern: &str, from: &str) -> bool {
    if pattern.is_empty() {
        return true;
    }
    let from = normalize_from_protocol(from);
    !from.is_empty() && equal_fold(pattern, &from)
}

/// Whether, for each rule, a value of the headers named as it is, in any
/// case, matches its pattern (`payloadHeadersMatch`).
fn headers_match(headers: &HeaderMap, rules: &[(String, String)]) -> bool {
    rules.iter().all(|(key, pattern)| {
        headers
            .iter()
            .filter(|(name, _)| equal_fold(name.as_str(), key))
            .any(|(_, value)| match_model_pattern(pattern.as_bytes(), value.as_bytes()))
    })
}

/// The model names the rules are matched against: the model sent
/// upstream, then the client's without and with its thinking suffix, each
/// once regardless of case (`payloadModelCandidates`).
pub(super) fn candidates(model: &str, requested: &str) -> Vec<String> {
    let model = model.trim();
    let requested = requested.trim();
    let mut out: Vec<String> = Vec::with_capacity(3);
    let mut add = |value: &str| {
        let value = value.trim();
        if value.is_empty() {
            return;
        }
        let key = to_lower(value);
        if out.iter().any(|seen| to_lower(seen) == key) {
            return;
        }
        out.push(value.to_owned());
    };
    add(model);
    if !requested.is_empty() {
        let (base, suffix) = parse_suffix(requested);
        add(base);
        if suffix.is_some() {
            add(requested);
        }
    }
    out
}

/// Whether `model` matches `pattern`, where `*` stands for any bytes, both
/// trimmed; an empty pattern matches nothing (`matchModelPattern`).
pub(super) fn match_model_pattern(pattern: &[u8], model: &[u8]) -> bool {
    let pattern = trim_space(pattern);
    let model = trim_space(model);
    if pattern.is_empty() {
        return false;
    }
    if pattern == b"*" {
        return true;
    }
    let (mut pi, mut si) = (0, 0);
    let mut star: Option<usize> = None;
    let mut matched = 0;
    while let Some(&c) = model.get(si) {
        let p = pattern.get(pi).copied();
        if p == Some(c) {
            pi += 1;
            si += 1;
            continue;
        }
        if p == Some(b'*') {
            star = Some(pi);
            matched = si;
            pi += 1;
            continue;
        }
        if let Some(at) = star {
            pi = at + 1;
            matched += 1;
            si = matched;
            continue;
        }
        return false;
    }
    while pattern.get(pi) == Some(&b'*') {
        pi += 1;
    }
    pi == pattern.len()
}
