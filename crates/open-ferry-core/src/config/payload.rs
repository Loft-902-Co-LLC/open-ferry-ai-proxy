// Ported from CLIProxyAPI internal/config/config_types.go (PayloadConfig,
// PayloadFilterRule, PayloadRule, PayloadModelRule) and config_validation.go
// (SanitizePayloadRules, sanitizePayloadRawRules, payloadRawString)
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The `payload` section: rules that set or remove JSON paths in the
//! bodies sent upstream, for the models, protocols and request headers
//! they match.
//!
//! The providers' payload module applies them; this module only types and
//! checks them. A rule writes exactly the value the operator configured,
//! the same on every request. That includes identity-shaped fields such as
//! `metadata.user_id`, `user`, `safety_identifier` or `prompt_cache_key`
//! when the operator writes them; open-ferry never generates a value for
//! them.
//!
//! `Debug` shows the shape of a value and the names of the headers a rule
//! matches, never their contents.
//!
//! Deviations from upstream:
//! - A rule's `params` keep the order the file gives them, where Go's map
//!   iterates in random order. So the param a dropped raw rule is reported
//!   for is the first bad one in the file.

use std::collections::BTreeMap;
use std::fmt;

use open_ferry_translate::go::{json_valid, trim_space};
use serde::Deserialize;
use serde::de::{Deserializer, MapAccess, Visitor};

use super::layout::AnyValue;
use super::types::RedactedMap;

/// Default and override rules for provider payloads (upstream's
/// `PayloadConfig`).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default, rename = "config.PayloadConfig", rename_all = "kebab-case")]
pub struct PayloadConfig {
    /// Rules that set a path only when the request doesn't have it.
    pub default: Vec<PayloadRule>,
    /// As `default`, with string values used as raw JSON.
    pub default_raw: Vec<PayloadRule>,
    /// Rules that always set a path.
    pub r#override: Vec<PayloadRule>,
    /// As `override`, with string values used as raw JSON.
    pub override_raw: Vec<PayloadRule>,
    /// Rules that remove paths.
    pub filter: Vec<PayloadFilterRule>,
}

/// Paths removed from the payloads of matching models (upstream's
/// `PayloadFilterRule`).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(
    default,
    rename = "config.PayloadFilterRule",
    rename_all = "kebab-case"
)]
pub struct PayloadFilterRule {
    /// The models the rule applies to.
    pub models: Vec<PayloadModelRule>,
    /// gjson/sjson paths to remove.
    pub params: Vec<String>,
}

/// Values written into the payloads of matching models (upstream's
/// `PayloadRule`).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default, rename = "config.PayloadRule", rename_all = "kebab-case")]
pub struct PayloadRule {
    /// The models the rule applies to.
    pub models: Vec<PayloadModelRule>,
    /// gjson/sjson paths and the values written to them, in file order.
    #[serde(deserialize_with = "ordered_params")]
    pub params: Vec<(String, AnyValue)>,
}

/// Which requests a rule applies to (upstream's `PayloadModelRule`).
#[derive(Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename = "config.PayloadModelRule", rename_all = "kebab-case")]
pub struct PayloadModelRule {
    /// A model name or wildcard pattern such as `gpt-*`.
    pub name: String,
    /// The upstream format the rule is limited to, such as `gemini`.
    pub protocol: String,
    /// Wildcard patterns the request's headers must all match. They are
    /// only read, never sent.
    pub headers: BTreeMap<String, String>,
    /// The client format the rule is limited to.
    pub from_protocol: String,
    /// Paths that must equal the given values.
    pub r#match: Vec<BTreeMap<String, AnyValue>>,
    /// Paths that must not equal the given values.
    pub not_match: Vec<BTreeMap<String, AnyValue>>,
    /// Paths that must be present and not null.
    pub exist: Vec<String>,
    /// Paths that must be missing or null.
    pub not_exist: Vec<String>,
}

impl fmt::Debug for PayloadModelRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PayloadModelRule")
            .field("name", &self.name)
            .field("protocol", &self.protocol)
            .field("headers", &RedactedMap(&self.headers))
            .field("from_protocol", &self.from_protocol)
            .field("match", &self.r#match)
            .field("not_match", &self.not_match)
            .field("exist", &self.exist)
            .field("not_exist", &self.not_exist)
            .finish()
    }
}

/// Decodes a `map[string]any`, keeping the entries in file order.
fn ordered_params<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<(String, AnyValue)>, D::Error> {
    struct Params;

    impl<'de> Visitor<'de> for Params {
        type Value = Vec<(String, AnyValue)>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a mapping")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut params = Vec::new();
            while let Some(entry) = map.next_entry::<String, AnyValue>()? {
                params.push(entry);
            }
            Ok(params)
        }
    }

    deserializer.deserialize_map(Params)
}

/// Drops the raw rules with a value that isn't JSON (upstream's
/// `SanitizePayloadRules`).
pub(crate) fn sanitize_payload_rules(payload: &mut PayloadConfig) {
    sanitize_raw_rules(&mut payload.default_raw, "default-raw");
    sanitize_raw_rules(&mut payload.override_raw, "override-raw");
}

/// Drops the rules with no params, and those with a string value that
/// isn't JSON once trimmed, with a warning naming the first such param
/// (upstream's `sanitizePayloadRawRules`).
fn sanitize_raw_rules(rules: &mut Vec<PayloadRule>, section: &str) {
    let mut index = 0;
    rules.retain(|rule| {
        index += 1;
        if rule.params.is_empty() {
            return false;
        }
        let invalid = rule.params.iter().find(|(_, value)| match value {
            AnyValue::Str(raw) => {
                let trimmed = trim_space(raw.as_bytes());
                trimmed.is_empty() || !json_valid(trimmed)
            }
            _ => false,
        });
        if let Some((path, _)) = invalid {
            tracing::warn!(
                section,
                rule_index = index,
                param = %path,
                "payload rule dropped: invalid raw JSON"
            );
            return false;
        }
        true
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(params: &[(&str, AnyValue)]) -> PayloadRule {
        PayloadRule {
            models: Vec::new(),
            params: params
                .iter()
                .map(|(path, value)| ((*path).to_owned(), value.clone()))
                .collect(),
        }
    }

    fn text(value: &str) -> AnyValue {
        AnyValue::Str(value.to_owned())
    }

    // Not upstream's: sanitizePayloadRawRules has no test of its own.
    #[test]
    fn raw_rules_without_json_are_dropped() {
        let mut payload = PayloadConfig {
            default_raw: vec![
                rule(&[("a", text(" {\"x\":1} "))]),
                rule(&[]),
                rule(&[("b", AnyValue::Int(1)), ("c", text("{"))]),
                rule(&[("d", text("  "))]),
                rule(&[("e", AnyValue::Bool(true))]),
            ],
            r#override: vec![rule(&[]), rule(&[("f", text("{"))])],
            override_raw: vec![rule(&[("g", text("[1, 2]"))])],
            ..PayloadConfig::default()
        };
        sanitize_payload_rules(&mut payload);
        assert_eq!(
            payload.default_raw,
            [
                rule(&[("a", text(" {\"x\":1} "))]),
                rule(&[("e", AnyValue::Bool(true))]),
            ]
        );
        // Only the raw sections are checked.
        assert_eq!(payload.r#override.len(), 2);
        assert_eq!(payload.override_raw.len(), 1);
    }

    // Not upstream's: Debug keeps header patterns and values out.
    #[test]
    fn debug_hides_values_and_header_patterns() {
        let model = PayloadModelRule {
            name: "gpt-*".into(),
            headers: BTreeMap::from([("X-Team".into(), "SECRET-PATTERN".into())]),
            r#match: vec![BTreeMap::from([("user".into(), text("SECRET-VALUE"))])],
            ..PayloadModelRule::default()
        };
        let payload = PayloadConfig {
            r#override: vec![PayloadRule {
                models: vec![model],
                params: vec![("metadata.user_id".into(), text("SECRET-VALUE"))],
            }],
            ..PayloadConfig::default()
        };
        let shown = format!("{payload:?}");
        assert!(!shown.contains("SECRET"), "{shown}");
        assert!(shown.contains("X-Team"), "{shown}");
    }
}
