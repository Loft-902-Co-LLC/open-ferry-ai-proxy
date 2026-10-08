// Ported from CLIProxyAPI sdk/cliproxy/auth/metadata_keys.go, priority.go,
// custom_headers.go, MergeExistingAuthMetadata and IsAuthTokenPayloadKey in
// metadata_merge.go, and the metadata overrides in types.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Settings a credential file carries next to its tokens: legacy key names,
//! priority, extra request headers, cooling and retry overrides, and how a
//! fresh login keeps the settings of the file it replaces.
//!
//! Deviations from upstream:
//! - `fingerprint-profile` isn't renamed to `fingerprint_profile`; the
//!   project doesn't port client fingerprint profiles, so the key is left as
//!   it is, like any other unknown key.
//! - The merge's special case for `meta` credentials isn't ported, as `meta`
//!   isn't.
//! - Numbers are read as the float64 Go decodes them to, and converted to
//!   integers saturating where Go's conversion is undefined.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::Auth;
use super::go::{atoi, equal_fold, number_to_i64, parse_bool};

/// Marks a priority attribute that came from the credential's own file.
pub const ATTRIBUTE_FILE_PRIORITY: &str = "file_priority";

/// The snake_case name for a metadata key that used to have a config-style
/// spelling too, or `key` itself.
pub fn canonical_credential_metadata_key(key: &str) -> &str {
    match key {
        "api-key" => "api_key",
        "base-url" => "base_url",
        "disable-cooling" => "disable_cooling",
        "excluded-models" => "excluded_models",
        "model-aliases" => "model_aliases",
        "proxy-url" => "proxy_url",
        "request-retry" => "request_retry",
        "request-scoped-errors" => "request_scoped_errors",
        "tool-prefix-disabled" => "tool_prefix_disabled",
        _ => key,
    }
}

/// Renames legacy metadata keys to their snake_case names. Where both
/// spellings are present, the snake_case value wins and the legacy key goes.
pub fn normalize_credential_metadata(metadata: &mut Map<String, Value>) {
    let legacy: Vec<String> = metadata
        .keys()
        .filter(|key| canonical_credential_metadata_key(key) != key.as_str())
        .cloned()
        .collect();
    for key in legacy {
        let canonical = canonical_credential_metadata_key(&key).to_owned();
        if let Some(value) = metadata.shift_remove(&key)
            && !metadata.contains_key(&canonical)
        {
            metadata.insert(canonical, value);
        }
    }
}

/// Whether `key` holds a token or its lifetime, which a fresh login must not
/// take from an older file.
pub fn is_auth_token_payload_key(key: &str) -> bool {
    matches!(
        open_ferry_translate::go::to_lower(key.trim()).as_str(),
        "access_token"
            | "refresh_token"
            | "id_token"
            | "session_id"
            | "expired"
            | "last_refresh"
            | "expires_in"
            | "timestamp"
            | "token_type"
            | "user_code"
            | "verification_uri"
            | "verification_uri_complete"
    )
}

/// Copies settings from an existing credential file into a fresh login's
/// record: every key the record doesn't have, except tokens, and for Meta
/// the old login's API key and DCA token. The file's `disabled` flag carries
/// over unless the record sets its own.
pub fn merge_existing_auth_metadata(target: &mut Auth, existing: &Map<String, Value>) {
    if existing.is_empty() {
        return;
    }
    if !target.metadata.contains_key("disabled")
        && let Some(Value::Bool(disabled)) = existing.get("disabled")
    {
        target.disabled = *disabled;
    }
    let meta = equal_fold(target.provider.trim(), "meta");
    for (key, value) in existing {
        if is_auth_token_payload_key(key) || target.metadata.contains_key(key) {
            continue;
        }
        if meta
            && matches!(
                canonical_credential_metadata_key(key),
                "api_key" | "dca_token" | "dca_expired" | "dca_expires_at"
            )
        {
            continue;
        }
        target.metadata.insert(key.clone(), value.clone());
    }
}

/// Sets `auth`'s priority from a credential file's `priority`, a number or
/// a string holding an integer, and marks it as the file's. A missing or
/// invalid value leaves any other priority alone but drops the mark.
pub fn apply_auth_priority_metadata(auth: &mut Auth, metadata: &Map<String, Value>) {
    auth.attributes.remove(ATTRIBUTE_FILE_PRIORITY);
    let Some(raw) = metadata.get("priority") else {
        return;
    };
    let priority = match raw {
        Value::Number(number) => match number.as_f64() {
            Some(value) => (value as i64).to_string(),
            None => return,
        },
        Value::String(text) => {
            let trimmed = text.trim();
            if atoi(trimmed).is_none() {
                return;
            }
            trimmed.to_owned()
        }
        _ => return,
    };
    auth.metadata.insert("priority".to_owned(), raw.clone());
    auth.attributes.insert("priority".to_owned(), priority);
    auth.attributes
        .insert(ATTRIBUTE_FILE_PRIORITY.to_owned(), "true".to_owned());
}

/// The extra request headers in a credential's `headers` object: names and
/// values trimmed, entries with an empty name or a value that is empty or
/// not a string left out.
pub fn extract_custom_headers_from_metadata(
    metadata: &Map<String, Value>,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(Value::Object(headers)) = metadata.get("headers") else {
        return out;
    };
    for (name, value) in headers {
        let name = name.trim();
        let Some(value) = value.as_str().map(str::trim) else {
            continue;
        };
        if name.is_empty() || value.is_empty() {
            continue;
        }
        out.insert(name.to_owned(), value.to_owned());
    }
    out
}

/// Sets a `header:<name>` attribute for each extra request header in
/// `auth`'s metadata.
pub fn apply_custom_headers_from_metadata(auth: &mut Auth) {
    for (name, value) in extract_custom_headers_from_metadata(&auth.metadata) {
        auth.attributes.insert(format!("header:{name}"), value);
    }
}

impl Auth {
    /// The credential's own `disable_cooling` setting (or legacy
    /// `disable-cooling`), when it has one.
    pub fn disable_cooling_override(&self) -> Option<bool> {
        ["disable_cooling", "disable-cooling"]
            .into_iter()
            .find_map(|key| self.metadata.get(key).and_then(parse_bool_any))
    }

    /// Whether the credential's `tool_prefix_disabled` (or
    /// `tool-prefix-disabled`) setting is on. Only read here; nothing in
    /// this project adds a tool name prefix.
    pub fn tool_prefix_disabled(&self) -> bool {
        ["tool_prefix_disabled", "tool-prefix-disabled"]
            .into_iter()
            .find_map(|key| self.metadata.get(key).and_then(parse_bool_any))
            .unwrap_or(false)
    }

    /// The credential's own `request_retry` setting (or legacy
    /// `request-retry`). A negative value means none, so the global
    /// setting applies.
    pub fn request_retry_override(&self) -> Option<i64> {
        let retry = ["request_retry", "request-retry"]
            .into_iter()
            .find_map(|key| self.metadata.get(key).and_then(parse_int_any))?;
        (retry >= 0).then_some(retry)
    }
}

/// Upstream's `parseBoolAny`: a bool, a string Go's `ParseBool` accepts, or
/// a number (true unless zero).
pub(crate) fn parse_bool_any(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                parse_bool(trimmed)
            }
        }
        Value::Number(number) => number.as_f64().map(|n| n != 0.0),
        _ => None,
    }
}

/// Upstream's `parseIntAny`: a number, truncated toward zero, or a string
/// holding an integer.
pub(crate) fn parse_int_any(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => Some(number_to_i64(number)),
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                atoi(trimmed)
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => panic!("not an object"),
        }
    }

    fn with_metadata(value: Value) -> Auth {
        Auth {
            metadata: map(value),
            ..Auth::default()
        }
    }

    #[test]
    fn normalize_credential_metadata_renames_legacy_keys() {
        let mut metadata = map(json!({
            "api-key": "legacy-key",
            "base-url": "https://legacy.example",
            "disable-cooling": true,
            "excluded-models": ["legacy-model"],
            "fingerprint-profile": "left-alone",
            "model-aliases": [{"name": "upstream", "alias": "public"}],
            "proxy-url": "http://legacy-proxy.example",
            "request-retry": 3,
            "request_retry": 0,
            "request-scoped-errors": [{"status": 429}],
            "tool-prefix-disabled": true,
            "provider_field": "preserved",
        }));
        normalize_credential_metadata(&mut metadata);
        let want = map(json!({
            "api_key": "legacy-key",
            "base_url": "https://legacy.example",
            "disable_cooling": true,
            "excluded_models": ["legacy-model"],
            "fingerprint-profile": "left-alone",
            "model_aliases": [{"name": "upstream", "alias": "public"}],
            "proxy_url": "http://legacy-proxy.example",
            "request_retry": 0,
            "request_scoped_errors": [{"status": 429}],
            "tool_prefix_disabled": true,
            "provider_field": "preserved",
        }));
        assert_eq!(Value::Object(metadata), Value::Object(want));
    }

    #[test]
    fn canonical_key_preserves_unknown_keys() {
        assert_eq!(
            canonical_credential_metadata_key("provider-specific-key"),
            "provider-specific-key"
        );
    }

    #[test]
    fn merge_preserves_disabled_state() {
        let mut target = with_metadata(json!({"type": "claude"}));
        merge_existing_auth_metadata(
            &mut target,
            &map(json!({"disabled": true, "prefix": "team"})),
        );
        assert!(target.disabled);
        assert_eq!(target.metadata.get("disabled"), Some(&json!(true)));
        assert_eq!(target.metadata_str("prefix"), Some("team"));
    }

    #[test]
    fn merge_keeps_explicit_disabled_state() {
        let mut target = with_metadata(json!({"disabled": false}));
        merge_existing_auth_metadata(&mut target, &map(json!({"disabled": true})));
        assert!(!target.disabled);
        assert_eq!(target.metadata.get("disabled"), Some(&json!(false)));
    }

    #[test]
    fn merge_skips_tokens_and_keeps_unknown_keys() {
        let mut target = with_metadata(json!({"type": "claude", "access_token": "new"}));
        let existing = map(json!({
            "access_token": "old",
            "Refresh_Token ": "old",
            "claude_device_ids": {"opaque": [1, 2]},
            "type": "other",
        }));
        merge_existing_auth_metadata(&mut target, &existing);
        assert_eq!(target.metadata_str("access_token"), Some("new"));
        assert_eq!(target.metadata_str("type"), Some("claude"));
        assert!(!target.metadata.contains_key("Refresh_Token "));
        assert_eq!(
            target.metadata.get("claude_device_ids"),
            Some(&json!({"opaque": [1, 2]}))
        );
    }

    #[test]
    fn merge_meta_does_not_restore_old_key() {
        // A new device login can succeed while API key minting fails; its
        // DCA credential must not inherit the previous login's API key.
        let mut target = with_metadata(json!({"access_token": "dca:new", "dca_token": "dca:new"}));
        target.provider = "meta".into();
        let existing = map(json!({
            "api_key": "LLM|old",
            "dca_expired": "old expiry",
            "dca_expires_at": 42,
            "priority": 3,
        }));
        merge_existing_auth_metadata(&mut target, &existing);
        for key in ["api_key", "dca_expired", "dca_expires_at"] {
            assert!(!target.metadata.contains_key(key), "restored {key}");
        }
        assert_eq!(target.metadata.get("priority"), Some(&json!(3)));
        assert_eq!(target.metadata_str("dca_token"), Some("dca:new"));
    }

    #[test]
    fn priority_clears_untrusted_file_marker() {
        for metadata in [json!({}), json!({"priority": "bad"})] {
            let mut auth = Auth {
                attributes: [
                    ("priority".to_owned(), "7".to_owned()),
                    (ATTRIBUTE_FILE_PRIORITY.to_owned(), "true".to_owned()),
                ]
                .into(),
                metadata: map(json!({"priority": 7.0})),
                ..Auth::default()
            };
            apply_auth_priority_metadata(&mut auth, &map(metadata));
            assert!(auth.attribute(ATTRIBUTE_FILE_PRIORITY).is_none());
            assert_eq!(auth.attribute("priority"), Some("7"));
            assert_eq!(auth.metadata.get("priority"), Some(&json!(7.0)));
        }
    }

    #[test]
    fn priority_truncates_numbers_and_keeps_the_original() {
        let mut auth = Auth::default();
        apply_auth_priority_metadata(&mut auth, &map(json!({"priority": 1.5})));
        assert_eq!(auth.attribute("priority"), Some("1"));
        assert_eq!(auth.metadata.get("priority"), Some(&json!(1.5)));
        assert_eq!(auth.attribute(ATTRIBUTE_FILE_PRIORITY), Some("true"));

        apply_auth_priority_metadata(&mut auth, &map(json!({"priority": " 10 "})));
        assert_eq!(auth.attribute("priority"), Some("10"));
        assert_eq!(auth.metadata.get("priority"), Some(&json!(" 10 ")));
    }

    #[test]
    fn extract_custom_headers() {
        let metadata = map(json!({"headers": {
            " X-Test ": " value ",
            "": "ignored",
            "X-Empty": "   ",
            "X-Num": 1,
        }}));
        let want: BTreeMap<String, String> = [("X-Test".to_owned(), "value".to_owned())].into();
        assert_eq!(extract_custom_headers_from_metadata(&metadata), want);
        assert!(extract_custom_headers_from_metadata(&map(json!({"headers": "x"}))).is_empty());
    }

    #[test]
    fn apply_custom_headers() {
        let mut auth = Auth {
            metadata: map(json!({"headers": {"X-Test": "new", "X-Empty": "   "}})),
            attributes: [
                ("header:X-Test".to_owned(), "old".to_owned()),
                ("keep".to_owned(), "1".to_owned()),
            ]
            .into(),
            ..Auth::default()
        };
        apply_custom_headers_from_metadata(&mut auth);
        assert_eq!(auth.attribute("header:X-Test"), Some("new"));
        assert!(auth.attribute("header:X-Empty").is_none());
        assert_eq!(auth.attribute("keep"), Some("1"));
    }

    #[test]
    fn apply_custom_headers_keeps_placeholders_as_text() {
        let mut auth = with_metadata(json!({"headers": {"X-CPA-Session": "$CPA-SESSION-ID"}}));
        apply_custom_headers_from_metadata(&mut auth);
        assert_eq!(
            auth.attribute("header:X-CPA-Session"),
            Some("$CPA-SESSION-ID")
        );
    }

    #[test]
    fn request_retry_override() {
        let cases = [
            (json!({}), None),
            (json!({"request_retry": 0}), Some(0)),
            (json!({"request_retry": 3}), Some(3)),
            (json!({"request_retry": -1}), None),
            (json!({"request-retry": 2}), Some(2)),
            (json!({"request-retry": -2}), None),
            (json!({"request_retry": 0, "request-retry": 2}), Some(0)),
            (json!({"request_retry": "0"}), Some(0)),
            (json!({"request_retry": -1, "request-retry": 2}), None),
            (json!({"request_retry": "x", "request-retry": 2}), Some(2)),
        ];
        for (metadata, want) in cases {
            let auth = with_metadata(metadata.clone());
            assert_eq!(auth.request_retry_override(), want, "{metadata}");
        }
    }

    #[test]
    fn tool_prefix_disabled() {
        assert!(!Auth::default().tool_prefix_disabled());
        assert!(with_metadata(json!({"tool_prefix_disabled": true})).tool_prefix_disabled());
        assert!(with_metadata(json!({"tool_prefix_disabled": "true"})).tool_prefix_disabled());
        assert!(with_metadata(json!({"tool-prefix-disabled": true})).tool_prefix_disabled());
        assert!(!with_metadata(json!({"tool_prefix_disabled": false})).tool_prefix_disabled());
    }

    #[test]
    fn disable_cooling_override_supports_explicit_false() {
        let cases = [
            (json!({}), None),
            (json!({"disable_cooling": true}), Some(true)),
            (json!({"disable_cooling": false}), Some(false)),
            (json!({"disable-cooling": false}), Some(false)),
            (json!({"disable_cooling": "false"}), Some(false)),
            (json!({"disable_cooling": "invalid"}), None),
        ];
        for (metadata, want) in cases {
            let auth = with_metadata(metadata.clone());
            assert_eq!(auth.disable_cooling_override(), want, "{metadata}");
        }
    }
}
