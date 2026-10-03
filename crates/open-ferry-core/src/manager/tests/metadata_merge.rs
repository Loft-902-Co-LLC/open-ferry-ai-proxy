// Ported from CLIProxyAPI sdk/cliproxy/auth/metadata_merge_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Folding a refreshed credential into the live one: new tokens land,
//! concurrent edits to notes, attributes, prefix, proxy and status survive,
//! and a refresh from an earlier registration is refused.
//!
//! Deviations from upstream:
//! - `TestMergeRefreshedAuth`'s subtests are one test each, named
//!   `merge_refreshed_auth_<case>`.
//! - Dropped subtests: "nil current returns clone of updated or base" and
//!   "nil updated returns clone of current" (Go nil pointers; the manager
//!   always merges three credentials), and "propagates storage and runtime"
//!   (`Auth` has no `Storage` or `Runtime`).
//! - "stale registration epoch returns clone of current" goes through the
//!   manager: the epoch check is the caller's (merge.rs), so a refresh from
//!   epoch 1 after a second registration is refused with upstream's
//!   `UpdateRefreshedAuth` error and the live credential stays as it was.
//! - "MergePreparedAuth does not modify lifecycle fields" calls
//!   `merge_auth_content`, which `MergePreparedAuth` wraps upstream.
//! - The three `MergeExistingAuthMetadata` tests are in `auth/metadata.rs`,
//!   with the helper.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use super::support::*;
use crate::auth::{Auth, AuthError, Status, Timestamp};
use crate::manager::Settings;
use crate::manager::merge::{merge_auth_content, merge_refreshed_auth};

fn now() -> Timestamp {
    chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("timestamp")
}

fn meta(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => unreachable!(),
    }
}

fn attrs(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

fn with_proxy(id: &str, proxy_url: &str, metadata: Value) -> Auth {
    Auth {
        id: id.into(),
        proxy_url: proxy_url.into(),
        metadata: meta(metadata),
        ..Auth::default()
    }
}

fn str_meta<'a>(auth: &'a Auth, key: &str) -> &'a str {
    auth.metadata.get(key).and_then(Value::as_str).unwrap_or("")
}

#[test]
fn merge_refreshed_auth_preserves_concurrent_proxy_url_and_merges_refreshed_token() {
    let base = with_proxy(
        "auth1",
        "",
        json!({"access_token": "token-old", "type": "antigravity"}),
    );
    let current = with_proxy(
        "auth1",
        "http://127.0.0.1:8080",
        json!({
            "access_token": "token-old",
            "type": "antigravity",
            "proxy_url": "http://127.0.0.1:8080",
            "user_note": "important",
        }),
    );
    let updated = with_proxy(
        "auth1",
        "",
        json!({
            "access_token": "token-new",
            "type": "antigravity",
            "expired": "2030-01-01T00:00:00Z",
        }),
    );

    let merged = merge_refreshed_auth(&base, &current, &updated, now());
    assert_eq!(merged.proxy_url, "http://127.0.0.1:8080");
    assert_eq!(str_meta(&merged, "proxy_url"), "http://127.0.0.1:8080");
    assert_eq!(str_meta(&merged, "user_note"), "important");
    assert_eq!(str_meta(&merged, "access_token"), "token-new");
    assert_eq!(str_meta(&merged, "expired"), "2030-01-01T00:00:00Z");
}

#[test]
fn merge_refreshed_auth_preserves_concurrent_note_edit_while_executor_adds_project_id() {
    let base = with_proxy(
        "auth2",
        "",
        json!({"access_token": "tok1", "note": "original-note"}),
    );
    let current = with_proxy(
        "auth2",
        "",
        json!({"access_token": "tok1", "note": "concurrently-updated-note"}),
    );
    let updated = with_proxy(
        "auth2",
        "",
        json!({
            "access_token": "tok2",
            "note": "original-note",
            "project_id": "discovered-proj-123",
        }),
    );

    let merged = merge_refreshed_auth(&base, &current, &updated, now());
    assert_eq!(str_meta(&merged, "note"), "concurrently-updated-note");
    assert_eq!(str_meta(&merged, "project_id"), "discovered-proj-123");
    assert_eq!(str_meta(&merged, "access_token"), "tok2");
}

#[test]
fn merge_refreshed_auth_merges_attributes_and_prefix() {
    let base = Auth {
        id: "auth4".into(),
        prefix: "old-prefix".into(),
        attributes: attrs(&[("shared", "base"), ("removed_by_exec", "true")]),
        ..Auth::default()
    };
    let current = Auth {
        id: "auth4".into(),
        prefix: "concurrent-prefix".into(),
        attributes: attrs(&[("shared", "base"), ("added_by_user", "custom")]),
        ..Auth::default()
    };
    let updated = Auth {
        id: "auth4".into(),
        prefix: "old-prefix".into(),
        attributes: attrs(&[("shared", "updated-by-exec")]),
        ..Auth::default()
    };

    let merged = merge_refreshed_auth(&base, &current, &updated, now());
    assert_eq!(merged.prefix, "concurrent-prefix");
    assert_eq!(
        merged.attributes,
        attrs(&[("shared", "updated-by-exec"), ("added_by_user", "custom")])
    );
}

#[tokio::test(start_paused = true)]
async fn merge_refreshed_auth_stale_registration_epoch_returns_clone_of_current() {
    let h = Harness::new(Settings::default());
    let base = h
        .manager
        .register(auth_with_metadata(
            "auth5",
            "claude",
            json!({"access_token": "old-epoch-1-token"}),
        ))
        .expect("register epoch 1");
    assert_eq!(h.versions("auth5").0, 1);
    h.manager
        .register(auth_with_metadata(
            "auth5",
            "claude",
            json!({"access_token": "new-epoch-2-token"}),
        ))
        .expect("register epoch 2");

    let updated = auth_with_metadata(
        "auth5",
        "claude",
        json!({"access_token": "refreshed-epoch-1-token"}),
    );
    let err = h
        .manager
        .update_refreshed(&base, 1, updated)
        .expect_err("stale refresh");
    assert_eq!(
        err.to_string(),
        "update auth auth5: stale registration epoch 1 != 2"
    );

    assert_eq!(h.versions("auth5").0, 2, "RegistrationEpoch");
    assert_eq!(
        str_meta(&h.get("auth5"), "access_token"),
        "new-epoch-2-token"
    );
}

#[test]
fn merge_refreshed_auth_concurrently_cleared_proxy_url_is_not_resurrected() {
    let base = with_proxy(
        "auth6",
        "http://proxy.old:8080",
        json!({"access_token": "tok-old", "proxy_url": "http://proxy.old:8080"}),
    );
    let current = with_proxy("auth6", "", json!({"access_token": "tok-old"}));
    // The executor didn't touch the proxy; it carried base's over.
    let updated = with_proxy(
        "auth6",
        "http://proxy.old:8080",
        json!({"access_token": "tok-new", "proxy_url": "http://proxy.old:8080"}),
    );

    let merged = merge_refreshed_auth(&base, &current, &updated, now());
    assert_eq!(merged.proxy_url, "", "should not resurrect");
    assert!(
        !merged.metadata.contains_key("proxy_url"),
        "Metadata[proxy_url] should not exist, got {:?}",
        merged.metadata.get("proxy_url")
    );
    assert_eq!(str_meta(&merged, "access_token"), "tok-new");
}

#[test]
fn merge_refreshed_auth_proxy_arbitration_only_struct_field_modified() {
    let old = json!({"proxy_url": "http://old:8080"});
    let base = with_proxy("auth7a", "http://old:8080", old.clone());
    let current = with_proxy("auth7a", "http://user-new:8080", old.clone());
    let updated = with_proxy("auth7a", "http://old:8080", old);

    let merged = merge_refreshed_auth(&base, &current, &updated, now());
    assert_eq!(merged.proxy_url, "http://user-new:8080");
    assert_eq!(str_meta(&merged, "proxy_url"), "http://user-new:8080");
}

#[test]
fn merge_refreshed_auth_proxy_arbitration_only_metadata_modified() {
    let old = json!({"proxy_url": "http://old:8080"});
    let base = with_proxy("auth8a", "http://old:8080", old.clone());
    let current = with_proxy(
        "auth8a",
        "http://old:8080",
        json!({"proxy_url": "http://user-new:8080"}),
    );
    let updated = with_proxy("auth8a", "http://old:8080", old);

    let merged = merge_refreshed_auth(&base, &current, &updated, now());
    assert_eq!(merged.proxy_url, "http://user-new:8080");
    assert_eq!(str_meta(&merged, "proxy_url"), "http://user-new:8080");
}

#[test]
fn merge_refreshed_auth_proxy_arbitration_struct_cleared() {
    let old = json!({"proxy_url": "http://old:8080"});
    let base = with_proxy("auth7b", "http://old:8080", old.clone());
    let current = with_proxy("auth7b", "", old.clone());
    let updated = with_proxy("auth7b", "http://old:8080", old);

    let merged = merge_refreshed_auth(&base, &current, &updated, now());
    assert_eq!(merged.proxy_url, "");
    assert!(
        !merged.metadata.contains_key("proxy_url"),
        "Metadata[proxy_url] should not exist, got {:?}",
        merged.metadata.get("proxy_url")
    );
}

#[test]
fn merge_refreshed_auth_proxy_arbitration_metadata_cleared() {
    let old = json!({"proxy_url": "http://old:8080"});
    let base = with_proxy("auth8b", "http://old:8080", old.clone());
    let current = with_proxy("auth8b", "http://old:8080", json!({}));
    let updated = with_proxy("auth8b", "http://old:8080", old);

    let merged = merge_refreshed_auth(&base, &current, &updated, now());
    assert_eq!(merged.proxy_url, "");
    assert!(
        !merged.metadata.contains_key("proxy_url"),
        "Metadata[proxy_url] should not exist, got {:?}",
        merged.metadata.get("proxy_url")
    );
}

#[test]
fn merge_refreshed_auth_merges_executor_disabled_status() {
    let active = Auth {
        id: "auth-dis".into(),
        status: Status::Active,
        ..Auth::default()
    };
    let updated = Auth {
        status: Status::Disabled,
        disabled: true,
        ..active.clone()
    };

    let merged = merge_refreshed_auth(&active, &active, &updated, now());
    assert!(
        merged.disabled,
        "Disabled = false, want true (executor disabled)"
    );
    assert_eq!(merged.status, Status::Disabled);
    assert_eq!(merged.metadata.get("disabled"), Some(&json!(true)));
}

fn upstream_503(id: &str) -> Auth {
    Auth {
        id: id.into(),
        status: Status::Error,
        unavailable: true,
        status_message: "upstream 503".into(),
        last_error: Some(AuthError {
            message: "upstream 503".into(),
            ..AuthError::default()
        }),
        ..Auth::default()
    }
}

#[test]
fn merge_refreshed_auth_executor_disabled_takes_precedence_over_concurrent_503_error() {
    let base = Auth {
        id: "auth-dis-503".into(),
        status: Status::Active,
        ..Auth::default()
    };
    let current = upstream_503("auth-dis-503");
    let updated = Auth {
        id: "auth-dis-503".into(),
        status: Status::Disabled,
        disabled: true,
        ..Auth::default()
    };

    let merged = merge_refreshed_auth(&base, &current, &updated, now());
    assert!(merged.disabled, "Disabled = false, want true");
    assert_eq!(
        merged.status,
        Status::Disabled,
        "disabled must take precedence over concurrent 503"
    );
    assert_eq!(merged.metadata.get("disabled"), Some(&json!(true)));
}

#[test]
fn merge_refreshed_auth_preserves_new_concurrent_503_error_on_current_during_refresh() {
    let base = Auth {
        id: "auth9".into(),
        status: Status::Active,
        ..Auth::default()
    };
    let current = upstream_503("auth9");
    let updated = base.clone();

    let merged = merge_refreshed_auth(&base, &current, &updated, now());
    assert_eq!(
        merged.status,
        Status::Error,
        "concurrent 503 must be preserved"
    );
    assert!(merged.unavailable, "Unavailable = false, want true");
    assert_eq!(merged.status_message, "upstream 503");
}

#[test]
fn merge_refreshed_auth_merge_prepared_auth_does_not_modify_lifecycle_fields() {
    let base = Auth {
        id: "auth10".into(),
        status: Status::Error,
        unavailable: true,
        status_message: "cooling_503".into(),
        last_error: Some(AuthError {
            message: "cooling_503".into(),
            ..AuthError::default()
        }),
        ..Auth::default()
    };
    let current = base.clone();
    let updated = Auth {
        id: "auth10".into(),
        status: Status::Active,
        metadata: meta(json!({"project_id": "discovered-project"})),
        ..Auth::default()
    };

    let merged = merge_auth_content(&base, &current, &updated);
    assert_eq!(merged.status, Status::Error);
    assert!(merged.unavailable, "Unavailable = false, want true");
    assert_eq!(
        merged.last_error.as_ref().map(|err| err.message.as_str()),
        Some("cooling_503")
    );
    assert_eq!(str_meta(&merged, "project_id"), "discovered-project");
}
