// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_request_scoped_errors.go
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Operator rules for upstream errors: a rule names a status and text to
//! match, and whether the request stops or moves on to the next credential,
//! with or without cooling the credential down.
//!
//! Deviations from upstream:
//! - Rules are compiled from their regular expressions each time an error
//!   is checked, as upstream does, but with Rust's `regex` syntax, which is
//!   close to Go's RE2.

use regex::Regex;
use serde_json::Value;

use super::classify::{CODE_FORCE_COOLDOWN, CODE_REQUEST_SCOPED, ErrView};
use super::credential::{KIND_OAUTH, auth_kind};
use super::models::{config_index, resolve_openai_compat_config_for_auth};
use super::settings::{RequestScopedErrorRule, Settings};
use super::text::go_lower;
use crate::auth::json::{decode_field, remarshaled_fold_values as go_values};
use crate::auth::{Auth, AuthError};
use crate::exec::ExecError;

/// What a matching rule asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScopedAction {
    /// End the request with this error, without cooling the credential.
    Stop,
    /// End the request, and cool the credential down.
    StopAndCooldown,
    /// Try the next credential, without cooling this one.
    Continue,
    /// Try the next credential, and cool this one down.
    ContinueAndCooldown,
}

impl ScopedAction {
    fn parse(action: &str) -> Option<Self> {
        match go_lower(action.trim()).as_str() {
            "stop" => Some(Self::Stop),
            "stop-and-cooldown" => Some(Self::StopAndCooldown),
            "continue" => Some(Self::Continue),
            "continue-and-cooldown" => Some(Self::ContinueAndCooldown),
            _ => None,
        }
    }

    /// Whether the request ends here (upstream's `isRequestScopedStop`).
    pub(crate) fn is_stop(self) -> bool {
        matches!(self, Self::Stop | Self::StopAndCooldown)
    }

    /// Recodes the recorded failure for the action (upstream's
    /// `applyRequestScopedActionToResult`).
    pub(crate) fn apply_to_result(self, result: &mut AuthError) {
        result.code = match self {
            Self::Stop | Self::Continue => CODE_REQUEST_SCOPED,
            Self::StopAndCooldown | Self::ContinueAndCooldown => CODE_FORCE_COOLDOWN,
        }
        .into();
    }
}

/// The rules in a credential's `request_scoped_errors` metadata, decoded as
/// Go decodes them after marshaling them again (each key matching a field
/// in sorted order, see [`decode_field`]): a type mismatch anywhere drops
/// them all.
fn rules_from_metadata(raw: &Value) -> Option<Vec<RequestScopedErrorRule>> {
    let Value::Array(items) = raw else {
        return None;
    };
    let mut rules = Vec::with_capacity(items.len());
    for item in items {
        match item {
            Value::Null => rules.push(RequestScopedErrorRule::default()),
            Value::Object(object) => rules.push(RequestScopedErrorRule {
                status: decode_field(go_values(object, "status"), 0, status_value)?,
                matches: decode_strings(go_values(object, "match"))?,
                match_regex: decode_strings(go_values(object, "match-regexr"))?,
                action: decode_field(go_values(object, "action"), String::new(), |value| {
                    value.as_str().map(str::to_owned)
                })?,
            }),
            _ => return None,
        }
    }
    (!rules.is_empty()).then_some(rules)
}

/// An `int` field's value: a whole number, as a float64 round trip leaves
/// it. Go writes the float64 back out and reads it as an `int64`, so a
/// fraction or a value outside `int64` (written with an exponent from 1e21)
/// fails and drops the list. One outside the range of statuses never
/// matches.
fn status_value(value: &Value) -> Option<u16> {
    const INT64: std::ops::Range<f64> = -9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0;
    let Value::Number(number) = value else {
        return None;
    };
    let n = number.as_f64()?;
    if n.fract() != 0.0 || !INT64.contains(&n) {
        return None;
    }
    Some(if (1.0..=f64::from(u16::MAX)).contains(&n) {
        n as u16
    } else {
        0
    })
}

/// A `[]string` field from the values Go meets for it, in turn: a `null`
/// empties it, as Go sets a slice to nil, and anything but a list of strings
/// (a `null` among them reading as an empty one) fails.
fn decode_strings<'a>(values: impl IntoIterator<Item = &'a Value>) -> Option<Vec<String>> {
    let mut field = Vec::new();
    for value in values {
        field = match value {
            Value::Null => Vec::new(),
            Value::Array(items) => items
                .iter()
                .map(|item| match item {
                    Value::String(text) => Some(text.clone()),
                    Value::Null => Some(String::new()),
                    _ => None,
                })
                .collect::<Option<_>>()?,
            _ => return None,
        };
    }
    Some(field)
}

/// The rules that apply to a credential (upstream's
/// `extractRequestScopedErrorRules`): its own, else its provider's OAuth
/// rules, else its API key's or OpenAI-compatible provider's.
pub(crate) fn extract_request_scoped_error_rules(
    auth: &Auth,
    settings: &Settings,
) -> Vec<RequestScopedErrorRule> {
    let raw = auth
        .metadata
        .get("request_scoped_errors")
        .or_else(|| auth.metadata.get("request-scoped-errors"));
    if let Some(raw) = raw
        && let Some(rules) = rules_from_metadata(raw)
    {
        return rules;
    }

    let provider = go_lower(auth.provider.trim());
    if auth_kind(auth) == KIND_OAUTH {
        return settings
            .oauth_request_scoped_errors
            .get(&provider)
            .filter(|rules| !rules.is_empty())
            .cloned()
            .unwrap_or_default();
    }

    let provider_key = auth
        .attributes
        .get("provider_key")
        .cloned()
        .unwrap_or_default();
    let mut compat_name = auth
        .attributes
        .get("compat_name")
        .cloned()
        .unwrap_or_default();
    if compat_name.is_empty() {
        if let Some(rest) = provider.strip_prefix("openai-compatible-") {
            compat_name = rest.to_owned();
        } else if let Some(rest) = provider.strip_prefix("openai-compatibility:") {
            compat_name = rest.to_owned();
        }
    }
    if (!compat_name.is_empty()
        || !provider_key.is_empty()
        || provider == "openai-compatibility"
        || provider.starts_with("openai-compatibility:")
        || provider.starts_with("openai-compatible"))
        && let Some(entry) =
            resolve_openai_compat_config_for_auth(settings, auth, &provider_key, &compat_name)
    {
        return entry.request_scoped_errors.clone();
    }

    let list = match provider.as_str() {
        "claude" | "codex" | "xai" | "meta" | "gemini" => provider.as_str(),
        "interactions" | "gemini-interactions" => "gemini-interactions",
        _ => return Vec::new(),
    };
    config_index(auth)
        .and_then(|index| settings.api_key_entries(list).get(index))
        .map(|entry| entry.request_scoped_errors.clone())
        .unwrap_or_default()
}

/// The text rules match against (upstream's `extractErrorBody`): the
/// provider's body, else the error's text.
fn extract_error_body(err: &ExecError) -> String {
    if err.message.is_empty() {
        err.to_string()
    } else {
        err.message.clone()
    }
}

/// The action of the first rule that matches the error (upstream's
/// `matchRequestScopedErrorAction`).
pub(crate) fn match_request_scoped_error_action(
    auth: &Auth,
    err: &ExecError,
    settings: &Settings,
) -> Option<ScopedAction> {
    let rules = extract_request_scoped_error_rules(auth, settings);
    if rules.is_empty() {
        return None;
    }
    let status = ErrView::Exec(err).status();
    let body = extract_error_body(err);
    for rule in &rules {
        if rule.status == 0 || rule.status != status {
            continue;
        }
        if rule.matches.is_empty() && rule.match_regex.is_empty() {
            continue;
        }
        let matched = rule
            .matches
            .iter()
            .any(|substr| !substr.is_empty() && body.contains(substr.as_str()))
            || rule.match_regex.iter().any(|pattern| {
                !pattern.is_empty() && Regex::new(pattern).is_ok_and(|re| re.is_match(&body))
            });
        if !matched {
            continue;
        }
        if let Some(action) = ScopedAction::parse(&rule.action) {
            return Some(action);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::settings::{ApiKeyEntry, OpenAiCompat};
    use serde_json::json;

    fn rule(status: u16, matches: &[&str], regex: &[&str], action: &str) -> RequestScopedErrorRule {
        RequestScopedErrorRule {
            status,
            matches: matches.iter().map(|s| (*s).to_owned()).collect(),
            match_regex: regex.iter().map(|s| (*s).to_owned()).collect(),
            action: action.into(),
        }
    }

    #[test]
    fn metadata_rules_win_and_decode_like_go() {
        let mut auth = Auth {
            provider: "codex".into(),
            ..Auth::default()
        };
        auth.metadata.insert("email".into(), json!("a@b"));
        auth.metadata.insert(
            "request-scoped-errors".into(),
            json!([{"Status": 400.0, "match": ["quota"], "action": " Stop "}]),
        );
        let err = ExecError::upstream(400, "over quota");
        let settings = Settings::default();
        assert_eq!(
            match_request_scoped_error_action(&auth, &err, &settings),
            Some(ScopedAction::Stop)
        );
        auth.metadata.insert(
            "request-scoped-errors".into(),
            json!([{"status": "400", "match": ["quota"], "action": "stop"}]),
        );
        assert_eq!(
            match_request_scoped_error_action(&auth, &err, &settings),
            None
        );
    }

    #[test]
    fn metadata_statuses_decode_as_go_ints() {
        let status = |value: Value| {
            rules_from_metadata(&json!([{"status": value, "match": ["busy"]}]))
                .map(|rules| rules[0].status)
        };
        // Past int64, or not whole: Go's decoding fails.
        for value in [
            json!(1e100),
            json!(1e20),
            json!(9.3e18),
            json!(u64::MAX),
            json!(i64::MAX),
            json!(-1e19),
            json!(1.5),
        ] {
            assert_eq!(status(value.clone()), None, "{value}");
        }
        // Go marshals the rules again, sorting keys, and decodes each key in
        // turn: "status" comes after "Status", a null leaves the status as it
        // was, and a value of the wrong type under either key fails.
        for (rules, want) in [
            (json!([{"status": 400, "Status": 500}]), Some(400)),
            (json!([{"Status": 500, "status": 400}]), Some(400)),
            (json!([{"Status": 500, "status": null}]), Some(500)),
            (json!([{"Status": "500", "status": 400}]), None),
        ] {
            assert_eq!(
                rules_from_metadata(&rules).map(|r| r[0].status),
                want,
                "{rules}"
            );
        }
        // A null list empties the one before it.
        let matches = |rules: Value| rules_from_metadata(&rules).map(|r| r[0].matches.clone());
        assert_eq!(
            matches(json!([{"MATCH": ["a"], "match": null}])),
            Some(vec![])
        );
        assert_eq!(
            matches(json!([{"match": ["a"], "Match": null}])),
            Some(vec!["a".to_owned()])
        );
        assert_eq!(
            matches(json!([{"Match": ["a"], "match": ["b", null]}])),
            Some(vec!["b".to_owned(), String::new()])
        );
        // Within int64 but not a status: kept, and never matches.
        assert_eq!(status(json!(9.2e18)), Some(0));
        assert_eq!(status(json!(-5)), Some(0));
        assert_eq!(status(json!(500.0)), Some(500));
        assert_eq!(status(json!(null)), Some(0));
    }

    #[test]
    fn config_rules_by_kind() {
        let mut settings = Settings::default();
        settings.oauth_request_scoped_errors.insert(
            "codex".into(),
            vec![rule(429, &[], &["^usage.*limit$"], "continue-and-cooldown")],
        );
        settings.api_keys.insert(
            "claude".into(),
            vec![ApiKeyEntry {
                request_scoped_errors: vec![rule(400, &["x"], &[], "continue")],
                ..ApiKeyEntry::default()
            }],
        );
        settings.openai_compatibility.push(OpenAiCompat {
            name: "acme".into(),
            request_scoped_errors: vec![
                rule(409, &["dup"], &[], "bogus"),
                rule(409, &["dup"], &[], "stop-and-cooldown"),
            ],
            ..OpenAiCompat::default()
        });

        let mut oauth = Auth {
            provider: "Codex".into(),
            ..Auth::default()
        };
        oauth.metadata.insert("refresh_token".into(), json!("r"));
        let err = ExecError::upstream(429, "usage over limit");
        assert_eq!(
            match_request_scoped_error_action(&oauth, &err, &settings),
            Some(ScopedAction::ContinueAndCooldown)
        );

        let mut key = Auth {
            provider: "claude".into(),
            ..Auth::default()
        };
        key.attributes.insert("api_key".into(), "k".into());
        key.attributes.insert("config_index".into(), "0".into());
        let err = ExecError::upstream(400, "xyz");
        assert_eq!(
            match_request_scoped_error_action(&key, &err, &settings),
            Some(ScopedAction::Continue)
        );

        let mut compat = Auth {
            provider: "openai-compatible-acme".into(),
            ..Auth::default()
        };
        compat.attributes.insert("api_key".into(), "k".into());
        let err = ExecError::upstream(409, "dup id");
        let action = match_request_scoped_error_action(&compat, &err, &settings);
        assert_eq!(action, Some(ScopedAction::StopAndCooldown));
        assert!(action.is_some_and(ScopedAction::is_stop));
        let mut result = AuthError::default();
        ScopedAction::StopAndCooldown.apply_to_result(&mut result);
        assert_eq!(result.code, CODE_FORCE_COOLDOWN);
    }
}
