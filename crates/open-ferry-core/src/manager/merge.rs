// Ported from CLIProxyAPI sdk/cliproxy/auth/metadata_merge.go
// (MergeRefreshedAuth, mergeAuthContent, IsAuthTokenPayloadKey) and
// sdk/cliproxy/auth/metadata_keys.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Folding a refreshed credential back into the live one.
//!
//! A refresh starts from a snapshot (`base`) and returns `updated`; by then
//! the live credential (`current`) may have changed. Each field takes the
//! refresh's change unless the live copy changed it too, in which case the
//! live change wins, except that new tokens always land.
//!
//! Deviations from upstream:
//! - An empty metadata, attribute or model-state map counts as Go's nil
//!   map: it means "no change", not "remove everything".
//! - The registration-epoch check is the caller's: the manager refuses a
//!   refresh from an earlier registration before merging.
//! - Metadata values compare as JSON values, where upstream compares the
//!   decoded Go values.

use serde_json::{Map, Value};

use super::text::{equal_fold, go_lower};
use crate::auth::{Auth, Status, Timestamp};

/// The snake_case name for a legacy credential metadata key (upstream's
/// `CanonicalCredentialMetadataKey`).
pub(crate) fn canonical_credential_metadata_key(key: &str) -> &str {
    match key {
        "api-key" => "api_key",
        "base-url" => "base_url",
        "disable-cooling" => "disable_cooling",
        "excluded-models" => "excluded_models",
        "fingerprint-profile" => "fingerprint_profile",
        "model-aliases" => "model_aliases",
        "proxy-url" => "proxy_url",
        "request-retry" => "request_retry",
        "request-scoped-errors" => "request_scoped_errors",
        "tool-prefix-disabled" => "tool_prefix_disabled",
        other => other,
    }
}

/// Renames legacy metadata keys to snake_case; a canonical key already
/// present wins (upstream's `NormalizeCredentialMetadata`).
pub(crate) fn normalize_credential_metadata(metadata: &mut Map<String, Value>) {
    let legacy: Vec<String> = metadata
        .keys()
        .filter(|key| canonical_credential_metadata_key(key) != key.as_str())
        .cloned()
        .collect();
    for key in legacy {
        let Some(value) = metadata.remove(&key) else {
            continue;
        };
        let canonical = canonical_credential_metadata_key(&key).to_owned();
        if !metadata.contains_key(&canonical) {
            metadata.insert(canonical, value);
        }
    }
}

/// Whether a metadata key holds a token or its lifecycle, which a refresh
/// always writes (upstream's `IsAuthTokenPayloadKey`).
pub(crate) fn is_auth_token_payload_key(key: &str) -> bool {
    matches!(
        go_lower(key.trim()).as_str(),
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

fn meta_proxy(metadata: &Map<String, Value>) -> String {
    match metadata.get("proxy_url") {
        Some(Value::String(s)) => s.trim().to_owned(),
        _ => String::new(),
    }
}

/// Merges the content of a refreshed credential: metadata, proxy URL,
/// prefix and attributes (upstream's `mergeAuthContent`).
pub(crate) fn merge_auth_content(base: &Auth, current: &Auth, updated: &Auth) -> Auth {
    let mut merged = current.clone();

    if !updated.metadata.is_empty() {
        for (key, value) in &updated.metadata {
            if equal_fold(key.trim(), "proxy_url") {
                continue;
            }
            let base_value = base.metadata.get(key);
            let current_value = current.metadata.get(key);
            let changed_by_executor = base_value != Some(value);
            let changed_by_user = base_value.is_some() != current_value.is_some()
                || (base_value.is_some() && base_value != current_value);
            if changed_by_executor && (!changed_by_user || is_auth_token_payload_key(key)) {
                merged.metadata.insert(key.clone(), value.clone());
            }
        }
        for (key, base_value) in &base.metadata {
            if equal_fold(key.trim(), "proxy_url") || updated.metadata.contains_key(key) {
                continue;
            }
            if current.metadata.get(key) == Some(base_value) {
                merged.metadata.remove(key);
            }
        }
    }

    let base_struct = base.proxy_url.trim();
    let current_struct = current.proxy_url.trim();
    let updated_struct = updated.proxy_url.trim();
    let base_meta = meta_proxy(&base.metadata);
    let current_meta = meta_proxy(&current.metadata);
    let updated_meta = meta_proxy(&updated.metadata);
    let user_changed_struct = current_struct != base_struct;
    let user_changed_meta = current_meta != base_meta;
    let exec_changed_struct = updated_struct != base_struct;
    let exec_changed_meta = updated_meta != base_meta;

    let mut final_proxy = current_struct;
    if !current_meta.is_empty() && current_struct.is_empty() && !user_changed_struct {
        final_proxy = &current_meta;
    }
    if user_changed_struct || user_changed_meta {
        final_proxy = if user_changed_struct && !user_changed_meta {
            current_struct
        } else if user_changed_meta && !user_changed_struct {
            &current_meta
        } else if !current_struct.is_empty() {
            current_struct
        } else {
            &current_meta
        };
    } else if exec_changed_struct || exec_changed_meta {
        final_proxy = if exec_changed_struct && !exec_changed_meta {
            updated_struct
        } else if exec_changed_meta && !exec_changed_struct {
            &updated_meta
        } else if !updated_struct.is_empty() {
            updated_struct
        } else {
            &updated_meta
        };
    }
    if final_proxy.is_empty() {
        merged.proxy_url.clear();
        merged.metadata.remove("proxy_url");
    } else {
        merged.proxy_url = final_proxy.to_owned();
        merged
            .metadata
            .insert("proxy_url".into(), Value::String(final_proxy.to_owned()));
    }

    let base_prefix = base.prefix.trim();
    let current_prefix = current.prefix.trim();
    let updated_prefix = updated.prefix.trim();
    merged.prefix = if updated_prefix != base_prefix && current_prefix == base_prefix {
        updated_prefix.to_owned()
    } else {
        current_prefix.to_owned()
    };

    if !updated.attributes.is_empty() {
        for (key, value) in &updated.attributes {
            let base_value = base.attributes.get(key);
            let current_value = current.attributes.get(key);
            let changed_by_executor = base_value != Some(value);
            let changed_by_user = base_value.is_some() != current_value.is_some()
                || (base_value.is_some() && base_value != current_value);
            if changed_by_executor && !changed_by_user {
                merged.attributes.insert(key.clone(), value.clone());
            }
        }
        for (key, base_value) in &base.attributes {
            if updated.attributes.contains_key(key) {
                continue;
            }
            if current.attributes.get(key) == Some(base_value) {
                merged.attributes.remove(key);
            }
        }
    }
    merged
}

/// Folds a refresh result into the live credential, keeping concurrent
/// changes and active cooldowns (upstream's `MergeRefreshedAuth`).
pub(crate) fn merge_refreshed_auth(
    base: &Auth,
    current: &Auth,
    updated: &Auth,
    now: Timestamp,
) -> Auth {
    let mut merged = merge_auth_content(base, current, updated);
    merged
        .rejected_access_token
        .clone_from(&updated.rejected_access_token);

    if updated.last_refreshed_at.is_some() {
        merged.last_refreshed_at = updated.last_refreshed_at;
    }
    if updated.next_refresh_after.is_some() || base.next_refresh_after.is_some() {
        merged.next_refresh_after = updated.next_refresh_after;
    }

    let base_err = base.last_error.as_ref().map_or("", |e| e.message.as_str());
    let current_err = current
        .last_error
        .as_ref()
        .map_or("", |e| e.message.as_str());
    let has_new_concurrent_error = !current_err.is_empty() && current_err != base_err;

    let base_disabled = base.disabled || base.status == Status::Disabled;
    let current_disabled = current.disabled || current.status == Status::Disabled;
    let updated_disabled = updated.disabled || updated.status == Status::Disabled;
    let changed_by_executor = updated_disabled != base_disabled;
    let changed_by_user = current_disabled != base_disabled;
    let final_disabled = if changed_by_executor && !changed_by_user {
        updated_disabled
    } else {
        current_disabled
    };

    if final_disabled {
        merged.disabled = true;
        merged.status = Status::Disabled;
        merged.metadata.insert("disabled".into(), Value::Bool(true));
    } else {
        merged.disabled = false;
        if merged.status == Status::Disabled {
            merged.status = Status::Active;
        }
        merged
            .metadata
            .insert("disabled".into(), Value::Bool(false));
        let quota = &current.quota;
        if has_new_concurrent_error {
            merged.last_error = current.last_error.clone();
            merged.status = current.status;
            merged.unavailable = current.unavailable;
            merged.status_message = current.status_message.clone();
        } else if (quota.exceeded
            && quota.reason == "credential_quota"
            && quota.next_recover_at.is_some_and(|t| t > now))
            || (current.unavailable && current.next_retry_after.is_some_and(|t| t > now))
        {
            // An active credential quota or cooldown stays.
            merged.unavailable = current.unavailable;
            merged.status = current.status;
            merged.status_message = current.status_message.clone();
        } else if matches!(updated.status, Status::Active | Status::Unknown) {
            merged.status = Status::Active;
            merged.unavailable = false;
            merged.status_message.clear();
            merged.last_error = None;
        }
    }

    if !updated.model_states.is_empty() {
        for (model, updated_state) in &updated.model_states {
            let base_state = base.model_states.get(model);
            let current_state = current.model_states.get(model);
            let changed_by_executor = base_state != Some(updated_state);
            let changed_by_user = base_state != current_state;
            if changed_by_executor && !changed_by_user {
                merged
                    .model_states
                    .insert(model.clone(), updated_state.clone());
            }
        }
        for (model, base_state) in &base.model_states {
            if updated.model_states.contains_key(model) {
                continue;
            }
            if current.model_states.get(model) == Some(base_state) {
                merged.model_states.remove(model);
            }
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::auth::AuthError;

    fn now() -> Timestamp {
        chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap_or_default()
    }

    fn with_meta(meta: Value) -> Auth {
        Auth {
            id: "a".into(),
            metadata: meta.as_object().cloned().unwrap_or_default(),
            ..Auth::default()
        }
    }

    #[test]
    fn normalize_renames_legacy_keys_and_keeps_canonical_values() {
        let mut meta = json!({"api-key": "old", "api_key": "new", "proxy-url": "p"})
            .as_object()
            .cloned()
            .unwrap_or_default();
        normalize_credential_metadata(&mut meta);
        assert_eq!(meta.get("api_key"), Some(&json!("new")));
        assert_eq!(meta.get("proxy_url"), Some(&json!("p")));
        assert!(!meta.contains_key("api-key"));
        assert!(!meta.contains_key("proxy-url"));
    }

    #[test]
    fn tokens_land_even_when_the_user_changed_them() {
        let base = with_meta(json!({"access_token": "t0", "note": "n0"}));
        let current = with_meta(json!({"access_token": "t-user", "note": "n-user"}));
        let updated = with_meta(json!({"access_token": "t1", "note": "n-exec"}));
        let merged = merge_refreshed_auth(&base, &current, &updated, now());
        assert_eq!(merged.metadata.get("access_token"), Some(&json!("t1")));
        assert_eq!(merged.metadata.get("note"), Some(&json!("n-user")));
        assert_eq!(merged.metadata.get("disabled"), Some(&json!(false)));
    }

    #[test]
    fn executor_deletions_apply_only_to_untouched_keys() {
        let base = with_meta(json!({"a": 1, "b": 2}));
        let current = with_meta(json!({"a": 1, "b": 3}));
        let updated = with_meta(json!({"x": 0}));
        let merged = merge_auth_content(&base, &current, &updated);
        assert!(!merged.metadata.contains_key("a"));
        assert_eq!(merged.metadata.get("b"), Some(&json!(3)));
        assert_eq!(merged.metadata.get("x"), Some(&json!(0)));
    }

    #[test]
    fn proxy_url_follows_the_user_over_the_executor() {
        let base = Auth {
            proxy_url: "http://a".into(),
            ..Auth::default()
        };
        let current = Auth {
            proxy_url: "http://user".into(),
            ..Auth::default()
        };
        let updated = Auth {
            proxy_url: "http://exec".into(),
            ..Auth::default()
        };
        let merged = merge_auth_content(&base, &current, &updated);
        assert_eq!(merged.proxy_url, "http://user");
        assert_eq!(
            merged.metadata.get("proxy_url"),
            Some(&json!("http://user"))
        );
        let merged = merge_auth_content(&base, &base, &updated);
        assert_eq!(merged.proxy_url, "http://exec");
    }

    #[test]
    fn a_concurrent_error_survives_the_refresh() {
        let base = Auth::default();
        let current = Auth {
            status: Status::Error,
            unavailable: true,
            last_error: Some(AuthError {
                message: "boom".into(),
                ..AuthError::default()
            }),
            ..Auth::default()
        };
        let updated = Auth {
            status: Status::Active,
            ..Auth::default()
        };
        let merged = merge_refreshed_auth(&base, &current, &updated, now());
        assert_eq!(merged.status, Status::Error);
        assert!(merged.unavailable);
        let merged = merge_refreshed_auth(&base, &base, &updated, now());
        assert_eq!(merged.status, Status::Active);
    }
}
