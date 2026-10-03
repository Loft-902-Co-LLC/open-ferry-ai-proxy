// Ported from CLIProxyAPI sdk/cliproxy/auth/types.go (the expiry and override
// readers), sdk/cliproxy/auth/classification.go, internal/credentialweight/
// weight.go, the auth readers in sdk/cliproxy/auth/selector.go and the token
// readers in sdk/cliproxy/auth/conductor_refresh.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! What the manager reads off a credential: its kind and source, priority,
//! weight, overrides, tokens, and when its access token expires.
//!
//! Expiry times come from [`Auth::expiration_time`] and
//! [`Auth::access_token_expiration_time`].
//!
//! Deviations from upstream:
//! - Go's zero time, which upstream's expiry readers can return as a valid
//!   expiry (for a non-positive Unix time), is [`go_zero`].
//! - A JWT's `exp` claim is read by its exact name first, then by any case,
//!   where Go's decoder takes the last key matching in any case.

use serde_json::Value;

use std::time::Duration;

use super::text::{
    atoi, equal_fold, parse_bool, parse_bool_any, parse_duration_string, parse_int_any,
    seconds_to_duration,
};
use crate::auth::{Auth, AuthKind, AuthSource, Timestamp, parse_time_value};

pub(crate) use crate::auth::classification::{
    AUTH_KIND_API_KEY as KIND_API_KEY, AUTH_KIND_OAUTH as KIND_OAUTH,
    AUTH_SOURCE_CONFIG as SOURCE_CONFIG,
};
pub(crate) use crate::auth::zero_time as go_zero;

/// Whether `time` is Go's zero time, or none.
pub(crate) fn is_zero(time: Option<Timestamp>) -> bool {
    time.is_none_or(|t| t == go_zero())
}

/// A trimmed attribute, or empty (upstream's `authAttribute`).
pub(crate) fn attribute(auth: &Auth, key: &str) -> String {
    auth.attributes
        .get(key)
        .map(|v| v.trim().to_owned())
        .unwrap_or_default()
}

/// A trimmed metadata string, or empty (upstream's `authMetadataString`).
pub(crate) fn metadata_string(auth: &Auth, key: &str) -> String {
    match auth.metadata.get(key) {
        Some(Value::String(v)) => v.trim().to_owned(),
        _ => String::new(),
    }
}

/// `apikey`, `oauth`, or empty (upstream's `Auth.AuthKind`).
pub(crate) fn auth_kind(auth: &Auth) -> &'static str {
    auth.auth_kind().map_or("", AuthKind::as_str)
}

/// Where the credential came from, such as `config` or `file` (upstream's
/// `Auth.AuthSourceKind`).
pub(crate) fn auth_source_kind(auth: &Auth) -> &'static str {
    auth.auth_source_kind().map_or("", AuthSource::as_str)
}

/// The `priority` attribute, or 0 (upstream's `authPriority`).
pub(crate) fn priority(auth: &Auth) -> i64 {
    atoi(&attribute(auth, "priority")).unwrap_or(0)
}

/// The most weight a credential may have.
const MAX_WEIGHT: i64 = 1_000_000;

/// The credential's weight for weighted routing: 1 by default, 0 when set
/// to a non-positive or invalid value (upstream's `authWeight`).
pub(crate) fn weight(auth: &Auth) -> i64 {
    if let Some(raw) = auth.attributes.get("weight")
        && !raw.trim().is_empty()
    {
        return parse_weight_str(raw).unwrap_or(0);
    }
    if let Some(raw) = auth.metadata.get("weight") {
        return parse_weight_value(raw).unwrap_or(0);
    }
    1
}

/// Checks every weight the credential sets (upstream's
/// `ValidateAuthWeight`), with upstream's error text.
pub(crate) fn validate_weight(auth: &Auth) -> Result<(), String> {
    if let Some(raw) = auth.attributes.get("weight") {
        parse_weight_str(raw).map_err(|err| format!("invalid attributes weight: {err}"))?;
    }
    if let Some(raw) = auth.metadata.get("weight") {
        parse_weight_value(raw).map_err(|err| format!("invalid metadata weight: {err}"))?;
    }
    Ok(())
}

fn too_heavy() -> String {
    format!("weight must not exceed {MAX_WEIGHT}")
}

fn normalize_weight(weight: i64) -> Result<i64, String> {
    if weight <= 0 {
        Ok(0)
    } else if weight > MAX_WEIGHT {
        Err(too_heavy())
    } else {
        Ok(weight)
    }
}

/// Upstream's `credentialweight.ParseString`.
fn parse_weight_str(raw: &str) -> Result<i64, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(1);
    }
    match atoi(raw) {
        Some(weight) => normalize_weight(weight),
        None => {
            let digits = raw.strip_prefix(['+', '-']).unwrap_or(raw);
            let reason = if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                "value out of range"
            } else {
                "invalid syntax"
            };
            Err(format!(
                "weight must be an integer: strconv.ParseInt: parsing {}: {reason}",
                go_quote(raw)
            ))
        }
    }
}

/// Upstream's `credentialweight.ParseValue` over a decoded JSON value, whose
/// numbers Go reads as float64.
fn parse_weight_value(value: &Value) -> Result<i64, String> {
    match value {
        Value::Number(number) => {
            let f = number.as_f64().unwrap_or(f64::NAN);
            if !f.is_finite() || f.trunc() != f {
                return Err("weight must be an integer".into());
            }
            if f <= 0.0 {
                return Ok(0);
            }
            if f > MAX_WEIGHT as f64 {
                return Err(too_heavy());
            }
            Ok(f as i64)
        }
        Value::String(text) => parse_weight_str(text),
        _ => Err("weight must be an integer".into()),
    }
}

/// Go's `strconv.Quote`, for the ASCII escapes; other characters as they are.
fn go_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Whether the credential has Responses WebSockets on (upstream's
/// `authWebsocketsEnabled`).
pub(crate) fn websockets_enabled(auth: &Auth) -> bool {
    if let Some(raw) = auth.attributes.get("websockets")
        && let Some(parsed) = parse_bool(raw.trim())
    {
        return parsed;
    }
    match auth.metadata.get("websockets") {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => parse_bool(text.trim()).unwrap_or(false),
        _ => false,
    }
}

/// The credential's own `request_retry` (upstream's `RequestRetryOverride`).
pub(crate) fn request_retry_override(auth: &Auth) -> Option<i64> {
    for key in ["request_retry", "request-retry"] {
        if let Some(parsed) = auth.metadata.get(key).and_then(parse_int_any) {
            return (parsed >= 0).then_some(parsed);
        }
    }
    None
}

/// The credential's own `disable_cooling` (upstream's
/// `DisableCoolingOverride`).
pub(crate) fn disable_cooling_override(auth: &Auth) -> Option<bool> {
    ["disable_cooling", "disable-cooling"]
        .iter()
        .find_map(|key| auth.metadata.get(*key).and_then(parse_bool_any))
}

/// The access token, or empty (upstream's `authAccessToken`).
pub(crate) fn access_token(auth: &Auth) -> String {
    let token = metadata_string(auth, "access_token");
    if !token.is_empty() {
        return token;
    }
    metadata_string(auth, "accessToken")
}

/// The refresh token, or empty (upstream's `authRefreshToken`).
pub(crate) fn refresh_token(auth: &Auth) -> String {
    let token = metadata_string(auth, "refresh_token");
    if !token.is_empty() {
        return token;
    }
    metadata_string(auth, "refreshToken")
}

/// The ID token, or empty.
fn id_token(auth: &Auth) -> String {
    let token = metadata_string(auth, "id_token");
    if !token.is_empty() {
        return token;
    }
    metadata_string(auth, "idToken")
}

/// The API key: the `api_key` attribute as it is, else the metadata's.
fn api_key(auth: &Auth) -> String {
    match auth.attributes.get("api_key") {
        Some(key) if !key.is_empty() => key.clone(),
        _ => metadata_string(auth, "api_key"),
    }
}

/// Whether the credential can refresh (upstream's
/// `authHasRefreshCredential`): a refresh token, or Meta's device token.
pub(crate) fn has_refresh_credential(auth: &Auth) -> bool {
    if !metadata_string(auth, "refresh_token").is_empty()
        || !metadata_string(auth, "refreshToken").is_empty()
    {
        return true;
    }
    equal_fold(auth.provider.trim(), "meta")
        && (!metadata_string(auth, "dca_token").is_empty()
            || !attribute(auth, "dca_token").is_empty())
}

/// Whether the tokens or API key differ (upstream's `CredentialsChanged`).
pub(crate) fn credentials_changed(existing: &Auth, incoming: &Auth) -> bool {
    access_token(existing) != access_token(incoming)
        || refresh_token(existing) != refresh_token(incoming)
        || id_token(existing) != id_token(incoming)
        || api_key(existing) != api_key(incoming)
}

const LAST_REFRESH_KEYS: [&str; 4] = [
    "last_refresh",
    "lastRefresh",
    "last_refreshed_at",
    "lastRefreshedAt",
];

/// When the credential last refreshed, by its metadata or attributes
/// (upstream's `authLastRefreshTimestamp`).
pub(crate) fn last_refresh_timestamp(auth: &Auth) -> Option<Timestamp> {
    for key in LAST_REFRESH_KEYS {
        if let Some(ts) = auth.metadata.get(key).and_then(parse_time_value) {
            return Some(ts);
        }
    }
    for key in LAST_REFRESH_KEYS {
        let value = attribute(auth, key);
        if !value.is_empty()
            && let Some(ts) = parse_time_value(&Value::String(value))
        {
            return Some(ts);
        }
    }
    None
}

const REFRESH_INTERVAL_KEYS: [&str; 4] = [
    "refresh_interval_seconds",
    "refreshIntervalSeconds",
    "refresh_interval",
    "refreshInterval",
];

/// How often the credential asks to refresh, or none (upstream's
/// `authPreferredInterval`): a number of seconds or a duration such as
/// `90m`, in its metadata or attributes.
pub(crate) fn preferred_interval(auth: &Auth) -> Option<Duration> {
    for key in REFRESH_INTERVAL_KEYS {
        if let Some(interval) = auth.metadata.get(key).and_then(duration_value) {
            return Some(interval);
        }
    }
    for key in REFRESH_INTERVAL_KEYS {
        if let Some(interval) = auth
            .attributes
            .get(key)
            .and_then(|raw| parse_duration_string(raw))
        {
            return Some(interval);
        }
    }
    None
}

/// A positive duration in metadata: a number of seconds, or a duration
/// string (upstream's `parseDurationValue`).
fn duration_value(value: &Value) -> Option<Duration> {
    match value {
        Value::Number(number) => {
            let seconds = number.as_f64()?;
            seconds_to_duration(seconds)
        }
        Value::String(text) => parse_duration_string(text),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn auth_with(metadata: Value) -> Auth {
        let Value::Object(metadata) = metadata else {
            unreachable!()
        };
        Auth {
            metadata,
            ..Auth::default()
        }
    }

    #[test]
    fn readers_match_upstream() {
        let mut auth = auth_with(json!({"weight": 2.0, "request_retry": "x", "request-retry": 3}));
        assert_eq!(weight(&auth), 2);
        assert_eq!(request_retry_override(&auth), Some(3));
        auth.metadata.insert("weight".into(), json!(1.5));
        assert_eq!(weight(&auth), 0);
        auth.attributes.insert("weight".into(), " 7 ".into());
        assert_eq!(weight(&auth), 7);
        auth.attributes.insert("weight".into(), "2000000".into());
        assert_eq!(weight(&auth), 0);
        auth.attributes.insert("priority".into(), " 5".into());
        assert_eq!(priority(&auth), 5);
        auth.metadata.insert("request_retry".into(), json!(-1));
        assert_eq!(request_retry_override(&auth), None);
        auth.metadata
            .insert("disable-cooling".into(), json!("true"));
        assert_eq!(disable_cooling_override(&auth), Some(true));
        assert_eq!(auth_kind(&auth), "");
        auth.metadata.insert("email".into(), json!("a@b"));
        assert_eq!(auth_kind(&auth), KIND_OAUTH);
        auth.attributes.insert("api_key".into(), "k".into());
        assert_eq!(auth_kind(&auth), KIND_API_KEY);
        auth.attributes
            .insert("source".into(), "config:openai[0]".into());
        assert_eq!(auth_source_kind(&auth), SOURCE_CONFIG);
        auth.attributes.insert("websockets".into(), "1".into());
        assert!(websockets_enabled(&auth));
    }
}
