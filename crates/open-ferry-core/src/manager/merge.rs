// Ported from CLIProxyAPI sdk/cliproxy/auth/metadata_merge.go
// (MergeRefreshedAuth, mergeAuthContent, IsAuthTokenPayloadKey) and
// sdk/cliproxy/auth/metadata_keys.go (v8.0.38, MIT).
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

use super::quota_signals::merge_quota_observation;
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

/// Reconciles a successful partition save after another live mutation. Valid
/// rotations still land; concurrent settings and newer token identities win.
pub(crate) fn reconcile_persisted_codex(snapshot: &Auth, current: &Auth, disk: &Auth) -> Auth {
    use crate::auth::codex_refresh::{
        CodexPersistIntent, CredentialRefreshPatch, OUTER_KEYS, SIWC_KEYS, same_version,
    };
    let mut current = current.clone();
    let mut baseline = snapshot.metadata.clone();
    if let CodexPersistIntent::Refresh(patch) = &snapshot.codex_persist_intent {
        current.metadata = patch.apply(&current.metadata).0;
        baseline = patch.apply(&baseline).0;
    }
    let mut updated = disk.clone();
    updated.attributes = snapshot.attributes.clone();
    updated
        .attributes
        .retain(|key, _| !key.starts_with("header:") || disk.attributes.contains_key(key));
    updated.attributes.extend(disk.attributes.clone());
    updated.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
        &snapshot.metadata,
        &disk.metadata,
        true,
        snapshot.metadata.contains_key("siwc") || disk.metadata.contains_key("siwc"),
    ));
    let mut merged = merge_auth_content(snapshot, &current, &updated);
    if current.metadata.get("use_siwc") == snapshot.metadata.get("use_siwc") {
        match disk.metadata.get("use_siwc") {
            Some(value) => {
                merged.metadata.insert("use_siwc".into(), value.clone());
            }
            None => {
                merged.metadata.remove("use_siwc");
            }
        }
    }
    // Disk is authoritative for unchanged identities, including absent keys.
    // Upstream assigns the merged disk map directly (sdk/cliproxy/auth/codex_persist.go:63-68).
    if same_version(&baseline, &current.metadata) {
        for key in OUTER_KEYS {
            match disk.metadata.get(*key) {
                Some(value) => {
                    merged.metadata.insert((*key).into(), value.clone());
                }
                None => {
                    merged.metadata.remove(*key);
                }
            }
        }
    }
    let empty = Map::new();
    let baseline_siwc = baseline
        .get("siwc")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let current_siwc = current
        .metadata
        .get("siwc")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    if same_version(baseline_siwc, current_siwc) {
        if let Some(Value::Object(disk_siwc)) = disk.metadata.get("siwc") {
            let target = merged
                .metadata
                .entry("siwc")
                .or_insert_with(|| Value::Object(Map::new()));
            if !target.is_object() {
                *target = Value::Object(Map::new());
            }
            let target = target.as_object_mut().unwrap();
            for key in SIWC_KEYS {
                match disk_siwc.get(*key) {
                    Some(value) => {
                        target.insert((*key).into(), value.clone());
                    }
                    None => {
                        target.remove(*key);
                    }
                }
            }
        } else if snapshot.metadata.contains_key("siwc") {
            merged.metadata.remove("siwc");
        }
    }
    let base = snapshot
        .metadata
        .get("siwc")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let live = current
        .metadata
        .get("siwc")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let disk = disk
        .metadata
        .get("siwc")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    if let Some(Value::Object(target)) = merged.metadata.get_mut("siwc") {
        for key in base.keys().chain(disk.keys()) {
            if !SIWC_KEYS.contains(&key.as_str()) && live.get(key) == base.get(key) {
                match disk.get(key) {
                    Some(value) => {
                        target.insert(key.clone(), value.clone());
                    }
                    None => {
                        target.remove(key);
                    }
                }
            }
        }
    }
    merged.disabled = merged
        .metadata
        .get("disabled")
        .and_then(Value::as_bool)
        .unwrap_or(current.disabled);
    merged
}

/// Whether a response's credentials are merged into the live record or
/// refused: under `Refused` the policy forbade persisting the received
/// rotation, so no credential patch is taken or synthesized from the response
/// (an explicit `Refresh` intent on it is ignored too) and its credential keys
/// never reach the merged record. `current`'s intent is kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResponseCredentials {
    Merge,
    Refused,
}

/// Merges the content of a refreshed credential: metadata, proxy URL,
/// prefix and attributes (upstream's `mergeAuthContent`).
pub(crate) fn merge_auth_content(base: &Auth, current: &Auth, updated: &Auth) -> Auth {
    merge_auth_content_with(base, current, updated, ResponseCredentials::Merge)
}

/// [`merge_auth_content`] with the response's credentials merged or refused.
pub(crate) fn merge_auth_content_with(
    base: &Auth,
    current: &Auth,
    updated: &Auth,
    credentials: ResponseCredentials,
) -> Auth {
    let refused = credentials == ResponseCredentials::Refused;
    let mut merged = current.clone();
    let patch = match &updated.codex_persist_intent {
        // A refused response takes no patch, not even an explicit one.
        _ if refused => None,
        crate::auth::codex_refresh::CodexPersistIntent::Refresh(patch) => Some(patch.clone()),
        _ if crate::auth::codex_refresh::is_codex_auth(updated) => {
            Some(crate::auth::codex_refresh::CredentialRefreshPatch::new(
                &base.metadata,
                &updated.metadata,
                crate::auth::codex_refresh::OUTER_KEYS
                    .iter()
                    .any(|key| base.metadata.get(*key) != updated.metadata.get(*key)),
                base.metadata.get("siwc") != updated.metadata.get("siwc")
                    && updated.metadata.contains_key("siwc"),
            ))
        }
        _ => None,
    };

    if !updated.metadata.is_empty() {
        for (key, value) in &updated.metadata {
            if equal_fold(key.trim(), "proxy_url") {
                continue;
            }
            if (patch.is_some() || refused) && crate::auth::codex_refresh::is_credential_key(key) {
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
            if equal_fold(key.trim(), "proxy_url")
                || updated.metadata.contains_key(key)
                || matches!(key.as_str(), "siwc" | "use_siwc")
                || ((patch.is_some() || refused)
                    && crate::auth::codex_refresh::is_credential_key(key))
            {
                continue;
            }
            if current.metadata.get(key) == Some(base_value) {
                merged.metadata.remove(key);
            }
        }
    }

    let unattempted_failure = current
        .codex_refresh_failure
        .as_ref()
        .filter(|failure| {
            patch
                .as_ref()
                .is_some_and(|patch| !patch.includes_partition(failure.partition))
        })
        .cloned();
    if let Some(patch) = patch {
        merged.metadata = patch.apply(&merged.metadata).0;
        merged.codex_persist_intent =
            crate::auth::codex_refresh::CodexPersistIntent::Refresh(patch);
    }
    merged.codex_refresh_failure = updated
        .codex_refresh_failure
        .clone()
        .or(unattempted_failure);

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
#[cfg(test)]
pub(crate) fn merge_refreshed_auth(
    base: &Auth,
    current: &Auth,
    updated: &Auth,
    now: Timestamp,
) -> Auth {
    merge_refreshed_auth_with(base, current, updated, now, ResponseCredentials::Merge)
}

/// Folds a refresh result into the live credential, as upstream's
/// `MergeRefreshedAuth` does, with the response's credentials merged or
/// refused.
pub(crate) fn merge_refreshed_auth_with(
    base: &Auth,
    current: &Auth,
    updated: &Auth,
    now: Timestamp,
    credentials: ResponseCredentials,
) -> Auth {
    let mut merged = merge_auth_content_with(base, current, updated, credentials);
    merged.preserve_rejected_tokens_from(current, true);

    if updated.last_refreshed_at.is_some() {
        merged.last_refreshed_at = updated.last_refreshed_at;
    }
    if updated.next_refresh_after.is_some() || base.next_refresh_after.is_some() {
        merged.next_refresh_after =
            if current
                .codex_refresh_failure
                .as_ref()
                .is_some_and(|failure| {
                    matches!(&merged.codex_persist_intent,
                crate::auth::codex_refresh::CodexPersistIntent::Refresh(patch)
                    if !patch.includes_partition(failure.partition))
                })
            {
                current.next_refresh_after.max(updated.next_refresh_after)
            } else {
                updated.next_refresh_after
            };
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
        } else if matches!(updated.status, Status::Active | Status::Unknown)
            && (merged.rejected_access_token.is_empty()
                || merged.access_token() != merged.rejected_access_token)
        {
            merged.status = Status::Active;
            merged.unavailable = false;
            merged.status_message.clear();
            merged.last_error = None;
        }
    }

    // The refreshed snapshot of the quota replaces an older one; the
    // cooldown fields stay with the live credential, so a failure that came
    // meanwhile isn't lost.
    merged.quota = merge_quota_observation(std::mem::take(&mut merged.quota), &updated.quota);

    // A model's own layer under a credential-wide quota cooldown, and the
    // mark of the wrap, follow the live credential's: a refresh derives
    // from the snapshot it started from and never moves them.
    for (model, merged_state) in &mut merged.model_states {
        if let Some(current_state) = current.model_states.get(model) {
            if let Some(shadow) = &current_state.credential_quota_shadow
                && merged_state.credential_quota_shadow.is_none()
            {
                merged_state.credential_quota_shadow = Some(Box::new(shadow.clone_shallow()));
            }
            merged_state.credential_quota_propagated |= current_state.credential_quota_propagated;
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

    fn codex(meta: Value) -> Auth {
        Auth {
            provider: "codex".into(),
            ..with_meta(meta)
        }
    }

    use crate::auth::codex_refresh::CodexPersistIntent;

    #[test]
    fn a_merged_codex_response_synthesizes_a_refresh_patch() {
        let base = codex(json!({"access_token": "t0", "refresh_token": "r0"}));
        let updated = codex(json!({"access_token": "t1", "refresh_token": "r1"}));
        let merged = merge_auth_content_with(&base, &base, &updated, ResponseCredentials::Merge);
        assert_eq!(merged.metadata.get("access_token"), Some(&json!("t1")));
        assert!(matches!(
            merged.codex_persist_intent,
            CodexPersistIntent::Refresh(_)
        ));
    }

    #[test]
    fn a_refused_response_leaves_the_credentials_and_makes_no_patch() {
        let base = codex(json!({"access_token": "t0", "refresh_token": "r0"}));
        let current = codex(json!({"access_token": "t-live", "refresh_token": "r0"}));
        let updated = codex(json!({"access_token": "t1", "refresh_token": "r1"}));
        let merged =
            merge_auth_content_with(&base, &current, &updated, ResponseCredentials::Refused);
        assert_eq!(merged.metadata.get("access_token"), Some(&json!("t-live")));
        assert_eq!(merged.metadata.get("refresh_token"), Some(&json!("r0")));
        assert_eq!(
            merged.codex_persist_intent,
            CodexPersistIntent::Configuration
        );
    }

    #[test]
    fn a_refused_response_still_merges_its_other_changes() {
        let base = codex(json!({"access_token": "t0", "note": "n0", "gone": 1}));
        let current = codex(json!({"access_token": "t0", "note": "n0", "gone": 1}));
        let updated = codex(json!({"access_token": "t1", "note": "n1"}));
        let merged =
            merge_auth_content_with(&base, &current, &updated, ResponseCredentials::Refused);
        assert_eq!(merged.metadata.get("access_token"), Some(&json!("t0")));
        assert_eq!(merged.metadata.get("note"), Some(&json!("n1")));
        assert!(!merged.metadata.contains_key("gone"));
    }

    #[test]
    fn a_refused_response_keeps_credential_keys_it_omits_and_siwc() {
        let base = codex(json!({
            "access_token": "t0",
            "expired": "2099-01-01T00:00:00Z",
            "siwc": {"access_token": "s0"},
            "use_siwc": true
        }));
        let updated = codex(json!({"note": "n1"}));
        let merged = merge_auth_content_with(&base, &base, &updated, ResponseCredentials::Refused);
        assert_eq!(merged.metadata.get("access_token"), Some(&json!("t0")));
        assert_eq!(
            merged.metadata.get("expired"),
            Some(&json!("2099-01-01T00:00:00Z"))
        );
        assert_eq!(merged.metadata["siwc"]["access_token"], json!("s0"));
        assert_eq!(merged.metadata.get("use_siwc"), Some(&json!(true)));
    }

    #[test]
    fn a_refused_refresh_keeps_bookkeeping_merges_like_a_merged_one() {
        let base = Auth {
            status: Status::Active,
            ..codex(json!({"access_token": "t0"}))
        };
        let current = Auth {
            status: Status::Error,
            unavailable: true,
            last_error: Some(AuthError {
                message: "boom".into(),
                ..AuthError::default()
            }),
            ..codex(json!({"access_token": "t0"}))
        };
        let updated = Auth {
            status: Status::Active,
            ..codex(json!({"access_token": "t1"}))
        };
        let merged = merge_refreshed_auth_with(
            &base,
            &current,
            &updated,
            now(),
            ResponseCredentials::Refused,
        );
        assert_eq!(merged.status, Status::Error);
        assert!(merged.unavailable);
        assert_eq!(merged.metadata.get("access_token"), Some(&json!("t0")));
    }

    fn outer_patch(before: &Auth, after: &Auth) -> CodexPersistIntent {
        CodexPersistIntent::Refresh(crate::auth::codex_refresh::CredentialRefreshPatch::new(
            &before.metadata,
            &after.metadata,
            true,
            false,
        ))
    }

    fn failure(
        partition: crate::auth::CodexCredentialPartition,
        message: &str,
    ) -> crate::auth::CodexRefreshFailure {
        crate::auth::CodexRefreshFailure {
            error: crate::exec::ExecError::new(crate::exec::ErrorKind::Upstream, message),
            partition,
        }
    }

    #[test]
    fn a_refused_response_ignores_an_explicit_refresh_intent() {
        let base = codex(json!({"access_token": "t-base", "refresh_token": "r-base"}));
        let current = Auth {
            codex_persist_intent: CodexPersistIntent::OuterLogin,
            ..codex(json!({"access_token": "t-cur", "refresh_token": "r-cur"}))
        };
        let mut updated = codex(json!({"access_token": "t-upd", "refresh_token": "r-upd"}));
        updated.codex_persist_intent = outer_patch(&base, &updated);

        let merged =
            merge_auth_content_with(&base, &current, &updated, ResponseCredentials::Refused);
        assert_eq!(merged.metadata.get("access_token"), Some(&json!("t-cur")));
        assert_eq!(merged.metadata.get("refresh_token"), Some(&json!("r-cur")));
        assert_eq!(merged.codex_persist_intent, CodexPersistIntent::OuterLogin);

        // Under `Merge` the patch is taken: its intent lands, and the patch
        // writes a partition only where `current` still holds `base`'s tokens.
        let merged = merge_auth_content_with(&base, &current, &updated, ResponseCredentials::Merge);
        assert!(matches!(
            merged.codex_persist_intent,
            CodexPersistIntent::Refresh(_)
        ));
        let merged = merge_auth_content_with(&base, &base, &updated, ResponseCredentials::Merge);
        assert_eq!(merged.metadata.get("access_token"), Some(&json!("t-upd")));
        assert_eq!(merged.metadata.get("refresh_token"), Some(&json!("r-upd")));
        assert!(matches!(
            merged.codex_persist_intent,
            CodexPersistIntent::Refresh(_)
        ));
    }

    #[test]
    fn a_refused_response_merges_the_refresh_failure_without_a_patch() {
        use crate::auth::CodexCredentialPartition::{Outer, Siwc};
        let base = codex(json!({"access_token": "t-base"}));
        let mut updated = codex(json!({"access_token": "t-upd"}));
        updated.codex_persist_intent = outer_patch(&base, &updated);
        updated.codex_refresh_failure = Some(failure(Siwc, "updated failure"));
        let current = Auth {
            codex_refresh_failure: Some(failure(Outer, "current failure")),
            ..codex(json!({"access_token": "t-cur"}))
        };

        // The response's failure lands, and `current`'s credentials stay.
        let merged =
            merge_auth_content_with(&base, &current, &updated, ResponseCredentials::Refused);
        let landed = merged.codex_refresh_failure.as_ref();
        assert_eq!(landed.map(|f| f.partition), Some(Siwc));
        assert_eq!(
            landed.map(|f| f.error.message.as_str()),
            Some("updated failure")
        );
        assert_eq!(merged.metadata.get("access_token"), Some(&json!("t-cur")));

        // With no patch, no `current` failure is carried over as unattempted.
        updated.codex_refresh_failure = None;
        let merged =
            merge_auth_content_with(&base, &current, &updated, ResponseCredentials::Refused);
        assert!(merged.codex_refresh_failure.is_none());

        // A merged response with an outer-only patch keeps an unattempted
        // failure of the other partition.
        let current = Auth {
            codex_refresh_failure: Some(failure(Siwc, "current failure")),
            ..current
        };
        let merged = merge_auth_content_with(&base, &current, &updated, ResponseCredentials::Merge);
        assert_eq!(
            merged.codex_refresh_failure.as_ref().map(|f| f.partition),
            Some(Siwc)
        );
    }

    fn secs(n: i64) -> Timestamp {
        now() + chrono::Duration::seconds(n)
    }

    fn lasting_error(message: &str) -> Option<AuthError> {
        Some(AuthError {
            message: message.into(),
            ..AuthError::default()
        })
    }

    /// Merges a refused refresh shaped as `refresh_owned` leaves `updated`
    /// (refreshed now, no error, available, Active), against a `current` the
    /// closure shapes. Every input differs in timestamps and credential keys.
    fn refused_refresh_against(shape: impl FnOnce(&mut Auth)) -> (Auth, Auth) {
        let base = Auth {
            status: Status::Active,
            last_refreshed_at: Some(secs(-3600)),
            next_refresh_after: Some(secs(-1800)),
            last_error: lasting_error("old"),
            ..codex(json!({"access_token": "t-base", "refresh_token": "r-base"}))
        };
        let mut current = Auth {
            status: Status::Error,
            unavailable: true,
            status_message: "held".into(),
            last_error: lasting_error("old"),
            last_refreshed_at: Some(secs(-1200)),
            next_refresh_after: Some(secs(-900)),
            ..codex(json!({"access_token": "t-cur", "refresh_token": "r-cur"}))
        };
        shape(&mut current);
        let mut updated = Auth {
            status: Status::Active,
            last_refreshed_at: Some(now()),
            next_refresh_after: Some(secs(300)),
            ..codex(json!({"access_token": "t-upd", "refresh_token": "r-upd"}))
        };
        updated.codex_persist_intent = outer_patch(&base, &updated);
        let merged = merge_refreshed_auth_with(
            &base,
            &current,
            &updated,
            now(),
            ResponseCredentials::Refused,
        );
        (merged, current)
    }

    fn assert_refused_refresh_bookkeeping(merged: &Auth, current: &Auth) {
        assert_eq!(merged.last_refreshed_at, Some(now()));
        assert_eq!(merged.next_refresh_after, Some(secs(300)));
        assert_eq!(merged.metadata.get("access_token"), Some(&json!("t-cur")));
        assert_eq!(merged.metadata.get("refresh_token"), Some(&json!("r-cur")));
        assert_eq!(merged.codex_persist_intent, current.codex_persist_intent);
    }

    #[test]
    fn a_refused_refresh_keeps_a_new_concurrent_error() {
        let (merged, current) = refused_refresh_against(|current| {
            current.last_error = lasting_error("new concurrent");
            current.next_retry_after = None;
        });
        assert_refused_refresh_bookkeeping(&merged, &current);
        assert_eq!(
            merged.last_error.as_ref().map(|e| e.message.as_str()),
            Some("new concurrent")
        );
        assert_eq!(merged.status, Status::Error);
        assert!(merged.unavailable);
        assert_eq!(merged.status_message, "held");
    }

    #[test]
    fn a_refused_refresh_keeps_an_active_cooldown() {
        let (merged, current) = refused_refresh_against(|current| {
            current.next_retry_after = Some(secs(60));
        });
        assert_refused_refresh_bookkeeping(&merged, &current);
        assert!(merged.unavailable);
        assert_eq!(merged.status, Status::Error);
        assert_eq!(merged.status_message, "held");
    }

    #[test]
    fn a_refused_refresh_keeps_an_active_credential_quota() {
        let (merged, current) = refused_refresh_against(|current| {
            current.next_retry_after = Some(secs(-60));
            current.quota.exceeded = true;
            current.quota.reason = "credential_quota".into();
            current.quota.next_recover_at = Some(secs(60));
        });
        assert_refused_refresh_bookkeeping(&merged, &current);
        assert!(merged.unavailable);
        assert_eq!(merged.status, Status::Error);
        assert_eq!(merged.status_message, "held");
    }

    #[test]
    fn a_refused_refresh_clears_a_stale_error_and_cooldown() {
        let (merged, current) = refused_refresh_against(|current| {
            current.next_retry_after = Some(secs(-60));
            current.quota.exceeded = true;
            current.quota.reason = "credential_quota".into();
            current.quota.next_recover_at = Some(secs(-60));
        });
        assert_refused_refresh_bookkeeping(&merged, &current);
        assert_eq!(merged.status, Status::Active);
        assert!(!merged.unavailable);
        assert!(merged.status_message.is_empty());
        assert!(merged.last_error.is_none());
    }

    /// Ported from upstream's `MergeRefreshedAuth` rule for the independent
    /// layers (the model-states loop in `metadata_merge.go`, which its Devin
    /// refresh tests reach): a refresh derives from the snapshot it started
    /// from, so a layer, a mark and a watermark the live credential gained
    /// meanwhile stay.
    #[test]
    fn a_refresh_keeps_the_live_shadow_the_mark_and_the_watermark() {
        use crate::auth::ModelState;

        let own = ModelState {
            status: Status::Error,
            unavailable: true,
            next_retry_after: Some(secs(3600)),
            ..ModelState::default()
        };
        let mut base = Auth::default();
        base.model_states.insert(
            "m".into(),
            ModelState {
                status: Status::Active,
                ..ModelState::default()
            },
        );
        let updated = base.clone();
        for refresh_changed_the_model in [false, true] {
            let mut current = base.clone();
            current.model_states.insert(
                "m".into(),
                ModelState {
                    status: Status::Error,
                    unavailable: true,
                    next_retry_after: Some(secs(7200)),
                    cooldown_recorded_seq: 7,
                    credential_quota_shadow: Some(Box::new(own.clone())),
                    credential_quota_propagated: true,
                    ..ModelState::default()
                },
            );
            let mut updated = updated.clone();
            if refresh_changed_the_model && let Some(state) = updated.model_states.get_mut("m") {
                state.status_message = "refreshed".into();
            }

            let merged = merge_refreshed_auth(&base, &current, &updated, now());

            let state = &merged.model_states["m"];
            assert_eq!(
                state.cooldown_recorded_seq, 7,
                "{refresh_changed_the_model}"
            );
            assert!(
                state.credential_quota_propagated,
                "{refresh_changed_the_model}"
            );
            assert_eq!(
                state
                    .credential_quota_shadow
                    .as_deref()
                    .map(|s| s.next_retry_after),
                Some(Some(secs(3600))),
                "{refresh_changed_the_model}"
            );
            assert_eq!(state.next_retry_after, Some(secs(7200)));
        }
    }
}
