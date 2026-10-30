// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_issue6819_test.go
// (v8.0.38, MIT), with registration and partition concurrency regressions.
// https://github.com/router-for-me/CLIProxyAPI

use super::siwc::{at, fixture};
use super::support::*;
use crate::auth::codex_refresh::{CodexPersistIntent, CredentialRefreshPatch};
use crate::auth::{Auth, AuthError, AuthStore as _};
use crate::exec::{Dispatcher, ExecError, Request};
use crate::manager::CallResult;
use futures_util::StreamExt;
use serde_json::json;

fn valid() -> Auth {
    let mut auth = fixture();
    auth.metadata["siwc"]["expires_at"] = json!("2099-01-01T00:00:00Z");
    auth
}

fn rejected(auth: &Auth) -> CallResult {
    CallResult {
        auth_id: auth.id.clone(),
        model: "gpt-test".into(),
        access_token: auth.access_token().into(),
        credential_version: auth.credential_version,
        registration_epoch: auth.registration_epoch,
        error: Some(AuthError {
            http_status: 401,
            message: "unauthorized".into(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn local_status_expiry_sources_and_rejection() {
    let now = at("2026-10-21T12:00:00Z");
    for (fields, expected) in [
        (
            json!({"expires_at":"2026-10-21T12:00:01Z", "expired":"2000-01-01T00:00:00Z"}),
            "valid",
        ),
        (json!({"expires_at":"2026-10-21T12:00:00Z"}), "invalid"),
        (json!({"expiresAt":"2026-10-21T11:59:59Z"}), "invalid"),
        (
            json!({"expires_in":3600, "last_refresh":"2026-10-21T11:30:00Z"}),
            "valid",
        ),
        (
            json!({"expiresIn":"3600", "issued_at":"2026-10-21T10:00:00Z"}),
            "invalid",
        ),
        (
            json!({"expires_in":1, "timestamp":"2026-10-21T12:00:00Z"}),
            "valid",
        ),
        (
            json!({"access_token":"e30.eyJleHAiOjQwNzA5MDg4MDB9.sig"}),
            "valid",
        ),
        (json!({"access_token":"e30.eyJleHAiOjF9.sig"}), "invalid"),
        (json!({"expired":"2099-01-01T00:00:00Z"}), "invalid"),
        (json!({}), "invalid"),
    ] {
        let mut auth = valid();
        auth.metadata["siwc"] = json!({"access_token":"dummy-nested"});
        auth.metadata["siwc"]
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        assert_eq!(auth.siwc_status(now), expected);
        let token = auth.access_token().to_owned();
        auth.record_rejected_token(&token);
        assert_eq!(auth.siwc_status(now), "invalid");
        auth.metadata.insert("use_siwc".into(), json!(false));
        assert_eq!(auth.siwc_status(now), "off");
    }
    let mut auth = valid();
    auth.metadata["siwc"]
        .as_object_mut()
        .unwrap()
        .remove("access_token");
    assert_eq!(auth.siwc_status(now), "invalid");
}

#[test]
fn rejection_survives_outer_login_toggle_and_same_token_bind() {
    let manager = Harness::new(Default::default()).manager;
    manager.register(valid()).unwrap();
    let initial = manager.get("codex.json").unwrap();
    manager.mark_result(&rejected(&initial));
    for change in ["outer", "off", "on", "bind", "disable", "enable"] {
        let mut auth = Auth::clone(&manager.get("codex.json").unwrap());
        match change {
            "outer" => {
                auth.metadata
                    .insert("access_token".into(), json!("outer-login"));
            }
            "off" => {
                auth.metadata.insert("use_siwc".into(), json!(false));
            }
            "on" => {
                auth.metadata.insert("use_siwc".into(), json!(true));
            }
            "disable" => {
                auth.disabled = true;
            }
            "enable" => {
                auth.disabled = false;
            }
            _ => {
                auth.codex_persist_intent = CodexPersistIntent::SiwcBind(
                    auth.metadata["siwc"].as_object().unwrap().clone(),
                );
            }
        }
        manager.update(auth).unwrap();
        let live = manager.get("codex.json").unwrap();
        assert_eq!(live.rejected_siwc_access_token, initial.access_token());
        assert_eq!(
            live.siwc_status(at("2026-10-21T12:00:00Z")),
            if change == "off" { "off" } else { "invalid" }
        );
        assert!(!format!("{live:?}").contains(initial.access_token()));
    }
    let mut changed = Auth::clone(&manager.get("codex.json").unwrap());
    changed.metadata["siwc"]["access_token"] = json!("different-nested");
    manager.update(changed).unwrap();
    assert_eq!(
        manager
            .get("codex.json")
            .unwrap()
            .siwc_status(at("2026-10-21T12:00:00Z")),
        "valid"
    );
}

#[tokio::test]
async fn same_token_refresh_is_not_retried_and_retry_401_is_recorded() {
    let manager = Harness::new(Default::default()).manager;
    let executor = FakeExecutor::new("codex");
    executor.set_refresh(|before| {
        let mut after = before.clone();
        after.metadata["siwc"]["expires_at"] = json!("2099-02-01T00:00:00Z");
        after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
            &before.metadata,
            &after.metadata,
            false,
            true,
        ));
        Ok(after)
    });
    manager.register_executor(executor.clone());
    manager.register(valid()).unwrap();
    let initial = manager.get("codex.json").unwrap();
    assert!(
        manager
            .try_refresh_after_unauthorized(
                &initial,
                &ExecError::upstream(401, "unauthorized"),
                false
            )
            .await
            .is_none()
    );
    assert_eq!(
        manager
            .get("codex.json")
            .unwrap()
            .rejected_siwc_access_token,
        initial.access_token()
    );
    let mut changed = Auth::clone(&manager.get("codex.json").unwrap());
    changed.metadata["siwc"]["access_token"] = json!("retry-token");
    manager.update(changed).unwrap();
    let retry = manager.get("codex.json").unwrap();
    assert!(
        manager
            .try_refresh_after_unauthorized(&retry, &ExecError::upstream(401, "unauthorized"), true)
            .await
            .is_none()
    );
    assert_eq!(
        manager
            .get("codex.json")
            .unwrap()
            .rejected_siwc_access_token,
        "retry-token"
    );
    // A result from a removed registration cannot reject a replacement.
    manager.remove("codex.json");
    manager.register(valid()).unwrap();
    assert!(
        manager
            .try_refresh_after_unauthorized(
                &initial,
                &ExecError::upstream(401, "unauthorized"),
                true
            )
            .await
            .is_none()
    );
    assert!(
        manager
            .get("codex.json")
            .unwrap()
            .rejected_siwc_access_token
            .is_empty()
    );
}

#[tokio::test]
async fn stream_401_captures_nested_token_across_outer_change_and_toggle() {
    let h = Harness::new(Default::default());
    h.models.register("codex.json", &["gpt-test"]);
    let executor = FakeExecutor::with("codex", |_| {
        Reply::chunks(vec![
            Ok(bytes::Bytes::from_static(b"first")),
            Err(ExecError::upstream(401, "unauthorized")),
        ])
    });
    h.manager.register_executor(executor);
    h.manager.register(valid()).unwrap();
    let mut stream = h
        .manager
        .execute_stream(
            &["codex".into()],
            Request {
                model: "gpt-test".into(),
                payload: bytes::Bytes::from_static(b"{}"),
            },
            options(),
        )
        .await
        .unwrap();
    assert!(stream.chunks.next().await.unwrap().is_ok());
    let mut edit = Auth::clone(&h.manager.get("codex.json").unwrap());
    edit.metadata
        .insert("access_token".into(), json!("other-outer"));
    edit.metadata.insert("use_siwc".into(), json!(false));
    h.manager.update(edit).unwrap();
    assert!(stream.chunks.next().await.unwrap().is_err());
    while stream.chunks.next().await.is_some() {}
    let live = h.manager.get("codex.json").unwrap();
    assert_eq!(live.rejected_siwc_access_token, "siwc-old-token");
    assert_ne!(live.rejected_access_token, "other-outer");
}

#[test]
fn concurrent_refresh_merge_keeps_same_token_rebind_rejection() {
    let base = valid();
    let mut refreshed = base.clone();
    refreshed
        .metadata
        .insert("access_token".into(), json!("rotated-outer"));
    refreshed.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
        &base.metadata,
        &refreshed.metadata,
        true,
        false,
    ));
    let mut bound = base.clone();
    bound.record_rejected_token("siwc-old-token");
    bound.metadata["siwc"]["last_refresh"] = json!("2026-10-21T12:00:00Z");
    let merged = crate::manager::merge::merge_refreshed_auth(
        &base,
        &bound,
        &refreshed,
        at("2026-10-21T12:00:00Z"),
    );
    assert_eq!(merged.rejected_siwc_access_token, "siwc-old-token");
    assert_eq!(merged.siwc_status(at("2026-10-21T12:00:00Z")), "invalid");
    assert!(!format!("{:?}", rejected(&merged)).contains("siwc-old-token"));
}

#[tokio::test]
async fn outer_refresh_races_same_token_bind_without_losing_rejection() {
    use super::siwc::{ControlledExecutor, disk_manager_unleased};
    let (_dir, store, manager) = disk_manager_unleased();
    let executor = ControlledExecutor::new(true, false, None, true);
    manager.register_executor(executor.clone());
    let worker = manager.clone();
    let refresh = tokio::spawn(async move { worker.force_refresh("codex.json").await });
    executor.entered.notified().await;
    manager.mark_result(&rejected(&manager.get("codex.json").unwrap()));
    let mut siwc = fixture().metadata["siwc"].as_object().unwrap().clone();
    siwc.insert("expires_at".into(), json!("2099-01-01T00:00:00Z"));
    let bound = store.bind_siwc(fixture(), siwc).await.unwrap();
    manager.update_unsaved(bound).unwrap();
    executor.release.add_permits(1);
    refresh.await.unwrap().unwrap();
    let live = manager.get("codex.json").unwrap();
    assert_eq!(live.outer_access_token(), "outer-rotated-token");
    assert_eq!(live.rejected_siwc_access_token, "siwc-old-token");
    assert_eq!(live.siwc_status(at("2026-10-21T12:00:00Z")), "invalid");
    let raw = serde_json::to_string(&store.list().unwrap()[0].metadata).unwrap();
    assert!(!raw.contains("rejected_siwc"));
}

#[tokio::test]
async fn non_auth_errors_and_refresh_failure_do_not_reject_unexpired_bearer() {
    let manager = Harness::new(Default::default()).manager;
    let executor = FakeExecutor::new("codex");
    executor.set_refresh(|_| Err(ExecError::upstream(401, "invalid_grant")));
    manager.register_executor(executor);
    manager.register(valid()).unwrap();
    for status in [429, 503] {
        let mut result = rejected(&manager.get("codex.json").unwrap());
        result.error.as_mut().unwrap().http_status = status;
        result.error.as_mut().unwrap().message = "transient".into();
        manager.mark_result(&result);
        assert_eq!(
            manager
                .get("codex.json")
                .unwrap()
                .siwc_status(at("2026-10-21T12:00:00Z")),
            "valid"
        );
    }
    assert!(manager.force_refresh("codex.json").await.is_err());
    let live = manager.get("codex.json").unwrap();
    assert_eq!(live.siwc_status(at("2026-10-21T12:00:00Z")), "valid");
    assert_eq!(live.siwc_status(at("2099-01-01T00:00:00Z")), "invalid");
}

#[tokio::test]
async fn execute_retry_401_survives_concurrent_outer_edit_and_same_token_bind() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let h = Harness::new(Default::default());
    h.models.register("codex.json", &["gpt-test"]);
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let manager = h.manager.clone();
    let executor = FakeExecutor::with("codex", move |_| {
        if counter.fetch_add(1, Ordering::SeqCst) == 1 {
            let mut bound = Auth::clone(&manager.get("codex.json").unwrap());
            bound
                .metadata
                .insert("access_token".into(), json!("concurrent-outer"));
            bound.codex_persist_intent =
                CodexPersistIntent::SiwcBind(bound.metadata["siwc"].as_object().unwrap().clone());
            manager.update_unsaved(bound).unwrap();
        }
        Reply::status(401, "unauthorized")
    });
    executor.set_refresh(|before| {
        let mut after = before.clone();
        after.metadata["siwc"]["access_token"] = json!("retry-nested");
        after.metadata["siwc"]["refresh_token"] = json!("retry-refresh");
        after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
            &before.metadata,
            &after.metadata,
            false,
            true,
        ));
        Ok(after)
    });
    h.manager.register_executor(executor);
    h.manager.register(valid()).unwrap();
    let error = h
        .manager
        .execute(
            &["codex".into()],
            Request {
                model: "gpt-test".into(),
                payload: bytes::Bytes::from_static(b"{}"),
            },
            options(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.http_status(), 401);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let live = h.manager.get("codex.json").unwrap();
    assert_eq!(live.outer_access_token(), "concurrent-outer");
    assert_eq!(live.rejected_siwc_access_token, "retry-nested");
    assert_eq!(live.siwc_status(h.now()), "invalid");
}

#[test]
fn local_status_legacy_access_alias() {
    let mut auth = valid();
    auth.metadata["siwc"] = json!({"access":"e30.eyJleHAiOjQwNzA5MDg4MDB9.sig"});
    assert_eq!(auth.siwc_status(at("2026-10-21T12:00:00Z")), "valid");
    assert_eq!(auth.access_token(), auth.siwc_access_token());
}

#[test]
fn repeated_and_stale_rejection_preserve_snapshot_identity() {
    let h = Harness::new(Default::default());
    h.manager.register(valid()).unwrap();
    h.manager
        .mark_rejected_access_token("codex.json", "siwc-old-token");
    let before = h.manager.get("codex.json").unwrap();
    for token in ["siwc-old-token", "rotated-away-token", " "] {
        h.manager.mark_rejected_access_token("codex.json", token);
        let after = h.manager.get("codex.json").unwrap();
        assert!(std::sync::Arc::ptr_eq(&before, &after));
        assert_eq!(before.generation, after.generation);
        assert_eq!(before.updated_at, after.updated_at);
    }
}

#[test]
fn local_status_future_sources_and_precedence() {
    let now = at("2026-10-21T12:00:00Z");
    let future_jwt = "e30.eyJleHAiOjQwNzA5MDg4MDB9.sig";
    let past_jwt = "e30.eyJleHAiOjF9.sig";
    let mut cases = vec![
        (json!({"expires_at":"2026-10-21T12:00:01Z"}), "valid"),
        (json!({"expiresAt":"2026-10-21T12:00:01Z"}), "valid"),
        (json!({"access_token":future_jwt}), "valid"),
        (
            json!({"expires_at":"2000-01-01T00:00:00Z", "access_token":future_jwt}),
            "invalid",
        ),
        (
            json!({"expires_at":"2099-01-01T00:00:00Z", "access_token":past_jwt}),
            "valid",
        ),
        (
            json!({"expires_in":1, "issued_at":"2000-01-01T00:00:00Z", "access_token":future_jwt}),
            "invalid",
        ),
        (
            json!({"expires_in":3600, "issued_at":"2026-10-21T11:30:00Z", "access_token":past_jwt}),
            "valid",
        ),
        (
            json!({"expires_at":"2000-01-01T00:00:00Z", "expires_in":3600, "issued_at":"2026-10-21T11:30:00Z"}),
            "invalid",
        ),
        (
            json!({"expires_at":"2099-01-01T00:00:00Z", "expires_in":1, "issued_at":"2000-01-01T00:00:00Z"}),
            "valid",
        ),
    ];
    for lifetime in ["expires_in", "expiresIn"] {
        for issuance in [
            "last_refresh",
            "lastRefresh",
            "timestamp",
            "issued_at",
            "issuedAt",
        ] {
            let mut fields = serde_json::Map::new();
            fields.insert(lifetime.into(), json!(3600));
            fields.insert(issuance.into(), json!("2026-10-21T11:30:00Z"));
            cases.push((serde_json::Value::Object(fields), "valid"));
        }
    }
    for (fields, expected) in cases {
        let mut auth = valid();
        auth.metadata["siwc"] = json!({"access_token":"dummy-nested"});
        auth.metadata["siwc"]
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        assert_eq!(auth.siwc_status(now), expected, "source: {fields}");
    }
}

#[test]
fn round2_rejection_partitions_survive_both_orders_and_sibling_refreshes() {
    use crate::manager::merge::merge_refreshed_auth;
    let now = at("2026-10-21T12:00:00Z");
    for outer_first in [false, true] {
        for refresh_between in [false, true] {
            let mut auth = valid();
            let first = if outer_first {
                auth.outer_access_token()
            } else {
                auth.siwc_access_token()
            }
            .to_owned();
            auth.record_rejected_token(&first);
            if refresh_between {
                let base = auth.clone();
                let mut updated = base.clone();
                if outer_first {
                    updated.metadata["siwc"]["access_token"] = json!("dummy-new-siwc");
                } else {
                    updated
                        .metadata
                        .insert("access_token".into(), json!("dummy-new-outer"));
                }
                updated.codex_persist_intent =
                    CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
                        &base.metadata,
                        &updated.metadata,
                        !outer_first,
                        outer_first,
                    ));
                auth = merge_refreshed_auth(&base, &auth, &updated, now);
            }
            let second = if outer_first {
                auth.siwc_access_token()
            } else {
                auth.outer_access_token()
            }
            .to_owned();
            auth.record_rejected_token(&second);
            assert_eq!(auth.rejected_access_token, auth.outer_access_token());
            assert_eq!(auth.rejected_siwc_access_token, auth.siwc_access_token());
            assert_eq!(
                auth.outer_expiration_time(),
                Some(chrono::DateTime::UNIX_EPOCH)
            );
            assert!(!auth.has_valid_access_token(now));
            for refreshing in [false, true] {
                let mut published = auth.clone();
                published.rejected_access_token.clear();
                published.rejected_siwc_access_token.clear();
                published.preserve_rejected_tokens_from(&auth, refreshing);
                assert_eq!(published.rejected_access_token, auth.outer_access_token());
                assert_eq!(
                    published.rejected_siwc_access_token,
                    auth.siwc_access_token()
                );
            }
            for rotate_outer in [false, true] {
                let base = auth.clone();
                let mut updated = base.clone();
                if rotate_outer {
                    updated
                        .metadata
                        .insert("access_token".into(), json!("dummy-rotated-outer"));
                } else {
                    updated.metadata["siwc"]["access_token"] = json!("dummy-rotated-siwc");
                }
                updated.codex_persist_intent =
                    CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
                        &base.metadata,
                        &updated.metadata,
                        rotate_outer,
                        !rotate_outer,
                    ));
                let merged = merge_refreshed_auth(&base, &auth, &updated, now);
                assert_eq!(merged.rejected_access_token.is_empty(), rotate_outer);
                assert_eq!(merged.rejected_siwc_access_token.is_empty(), !rotate_outer);
            }
        }
    }
}
