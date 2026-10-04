// Ported from CLIProxyAPI internal/runtime/executor/helps/payload_helpers.go
// (payloadRawValue, and the value encoding setPayloadValueIfDifferentTracked
// and sjson.SetBytesOptions give each kind of value) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The config's payload rules compiled once per config load: names and
//! protocols trimmed, `match` values decoded as Go compares them, and each
//! param's value turned into the JSON the rule writes.
//!
//! A value is written as upstream writes it: a string as a JSON string, a
//! YAML float as Go's `strconv.FormatFloat(f, 'f', -1, 64)` gives it (so
//! `1.0` is written `1` and `1e21` in full), a sequence or mapping as Go's
//! `encoding/json` encodes it, and a raw rule's string as the JSON it
//! holds.
//!
//! Deviations from upstream:
//! - A value that can't be written is dropped at load with a warning naming
//!   the section, the rule's 1-based index and the param's path, never the
//!   value. Upstream finds out on each request and skips it, except for a
//!   NaN or infinite float, which it writes as `NaN` or `+Inf`, leaving
//!   the body invalid JSON. A raw string the config check let through
//!   (Go's `json.Valid`) that serde_json won't read, such as one with a
//!   lone UTF-16 surrogate escape, or one nested more than 128 deep, is
//!   dropped the same way.

use std::collections::BTreeMap;

use open_ferry_core::config::{
    AnyValue, Config, DisableImageGeneration, PayloadModelRule, PayloadRule,
};
use open_ferry_translate::go::{format_float, json_float};
use serde_json::{Map, Number, Value};

use super::matchers::{ModelRule, Norm, norm_any, normalize_from_protocol};

/// The config's payload rules, compiled (upstream's `cfg.Payload` and
/// `cfg.DisableImageGeneration`).
#[derive(Debug, Default)]
pub struct Rules {
    pub(super) image: DisableImageGeneration,
    pub(super) default: Vec<Rule>,
    pub(super) default_raw: Vec<Rule>,
    pub(super) overrides: Vec<Rule>,
    pub(super) override_raw: Vec<Rule>,
    pub(super) filter: Vec<FilterRule>,
}

/// A rule that writes values.
#[derive(Debug)]
pub(super) struct Rule {
    pub(super) models: Vec<ModelRule>,
    /// Each path, as configured, and the JSON written there.
    pub(super) params: Vec<(String, Value)>,
}

/// A rule that removes paths.
#[derive(Debug)]
pub(super) struct FilterRule {
    pub(super) models: Vec<ModelRule>,
    pub(super) params: Vec<String>,
}

impl Rules {
    /// Compiles `config`'s rules, warning about each value dropped.
    pub fn compile(config: &Config) -> Self {
        Self::build(config, true)
    }

    /// Compiles `config`'s rules; `warn` says whether to warn about the
    /// values dropped.
    pub(super) fn build(config: &Config, warn: bool) -> Self {
        let payload = &config.payload;
        let writes = |section: &str, rules: &[PayloadRule], raw: bool| {
            rules
                .iter()
                .enumerate()
                .filter_map(|(index, rule)| {
                    let models = compile_models(&rule.models);
                    let params = rule
                        .params
                        .iter()
                        .filter_map(|(path, value)| {
                            let encoded = if raw {
                                encode_raw(value)
                            } else {
                                encode_value(value).map_err(|()| true)
                            };
                            match encoded {
                                Ok(json) => Some((path.clone(), json)),
                                Err(report) => {
                                    if warn && report {
                                        tracing::warn!(
                                            section,
                                            rule_index = index + 1,
                                            param = %path,
                                            "payload rule value dropped: it can't be written as JSON"
                                        );
                                    }
                                    None
                                }
                            }
                        })
                        .collect::<Vec<_>>();
                    (!models.is_empty() && !params.is_empty()).then_some(Rule { models, params })
                })
                .collect()
        };
        Self {
            image: config.disable_image_generation,
            default: writes("default", &payload.default, false),
            default_raw: writes("default-raw", &payload.default_raw, true),
            overrides: writes("override", &payload.r#override, false),
            override_raw: writes("override-raw", &payload.override_raw, true),
            filter: payload
                .filter
                .iter()
                .filter_map(|rule| {
                    let models = compile_models(&rule.models);
                    (!models.is_empty() && !rule.params.is_empty()).then(|| FilterRule {
                        models,
                        params: rule.params.clone(),
                    })
                })
                .collect(),
        }
    }

    /// Whether any rule writes or removes a path (`hasPayloadRules`, of
    /// the rules that can apply).
    pub(super) fn has_rules(&self) -> bool {
        !(self.default.is_empty()
            && self.default_raw.is_empty()
            && self.overrides.is_empty()
            && self.override_raw.is_empty()
            && self.filter.is_empty())
    }

    /// Whether a default rule can apply, which reads the client's request.
    pub(super) fn has_defaults(&self) -> bool {
        !(self.default.is_empty() && self.default_raw.is_empty())
    }
}

/// The model entries with a name, compiled.
fn compile_models(models: &[PayloadModelRule]) -> Vec<ModelRule> {
    models
        .iter()
        .filter(|entry| !entry.name.trim().is_empty())
        .map(|entry| ModelRule {
            name: entry.name.trim().to_owned(),
            protocol: entry.protocol.trim().to_owned(),
            from_protocol: normalize_from_protocol(&entry.from_protocol),
            headers: entry
                .headers
                .iter()
                .filter(|(key, _)| !key.trim().is_empty())
                .map(|(key, pattern)| (key.trim().to_owned(), pattern.clone()))
                .collect(),
            matches: conditions(&entry.r#match),
            not_matches: conditions(&entry.not_match),
            exist: paths(&entry.exist),
            not_exist: paths(&entry.not_exist),
        })
        .collect()
}

fn conditions(maps: &[BTreeMap<String, AnyValue>]) -> Vec<(String, Option<Norm>)> {
    maps.iter()
        .flatten()
        .filter(|(path, _)| !path.trim().is_empty())
        .map(|(path, value)| (path.clone(), norm_any(value)))
        .collect()
}

fn paths(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .filter(|path| !path.trim().is_empty())
        .cloned()
        .collect()
}

/// JSON from Go-encoded `text`, which is always valid.
fn number(text: &str) -> Value {
    serde_json::from_str::<Number>(text).map_or(Value::Null, Value::Number)
}

/// What an `override` or `default` rule writes for `value`
/// (`sjson.SetBytes`): `Err` where Go's encoder refuses it, or, for a NaN
/// or infinite float, where the result wouldn't be JSON.
fn encode_value(value: &AnyValue) -> Result<Value, ()> {
    match value {
        AnyValue::Null => Ok(Value::Null),
        AnyValue::Bool(b) => Ok(Value::Bool(*b)),
        AnyValue::Int(n) => Ok(Value::Number((*n).into())),
        AnyValue::Uint(n) => Ok(Value::Number((*n).into())),
        AnyValue::Float(f) if f.is_finite() => Ok(number(&format_float(*f))),
        AnyValue::Str(s) => Ok(Value::String(s.clone())),
        _ => marshal(value),
    }
}

/// What a raw rule writes for `value` (`payloadRawValue`): `Ok`, or
/// `Err(true)` to warn and `Err(false)` to drop quietly, as upstream skips a
/// null.
fn encode_raw(value: &AnyValue) -> Result<Value, bool> {
    match value {
        AnyValue::Null => Err(false),
        AnyValue::Str(raw) => serde_json::from_str(raw).map_err(|_| true),
        _ => marshal(value).map_err(|()| true),
    }
}

/// Go's `json.Marshal` of `value` (`Err` where it refuses it).
fn marshal(value: &AnyValue) -> Result<Value, ()> {
    match value {
        AnyValue::Null => Ok(Value::Null),
        AnyValue::Bool(b) => Ok(Value::Bool(*b)),
        AnyValue::Int(n) => Ok(Value::Number((*n).into())),
        AnyValue::Uint(n) => Ok(Value::Number((*n).into())),
        AnyValue::Float(f) if f.is_finite() => Ok(number(&json_float(*f))),
        AnyValue::Str(s) | AnyValue::Time(Some(s)) => Ok(Value::String(s.clone())),
        AnyValue::Seq(items) => items
            .iter()
            .map(marshal)
            .collect::<Result<_, _>>()
            .map(Value::Array),
        AnyValue::Map(map) => map
            .iter()
            .map(|(key, value)| Ok((key.clone(), marshal(value)?)))
            .collect::<Result<Map<_, _>, ()>>()
            .map(Value::Object),
        AnyValue::Float(_) | AnyValue::Time(None) | AnyValue::AnyMap => Err(()),
    }
}
