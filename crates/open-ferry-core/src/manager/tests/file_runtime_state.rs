// Ports upstream d5816d5e's runtime preservation; observations still read fresh bytes.
use super::support::*;
use crate::auth::{AdoptionRead, Auth, AuthError, ModelState};
use crate::manager::{AdoptionResult, CallResult};
use chrono::TimeDelta;
use serde_json::json;
use std::sync::Arc;

fn observed(h: &Harness, record: Auth, during_read: impl FnOnce()) {
    let hint = h.get("file");
    assert!(matches!(
        h.manager
            .adopt_observed(&hint, None, |_| {
                during_read();
                Ok(AdoptionRead::Record(record))
            })
            .unwrap(),
        AdoptionResult::Published { .. }
    ));
}

fn failure(h: &Harness, model: &str, status: u16) {
    h.manager.mark_result(&CallResult {
        auth_id: "file".into(),
        provider: "codex".into(),
        model: model.into(),
        error: Some(AuthError {
            http_status: status,
            message: "dummy failure".into(),
            ..Default::default()
        }),
        execution_seq: h.manager.next_execution_seq(),
        ..Default::default()
    });
}

#[tokio::test(start_paused = true)]
async fn file_observation_keeps_nonempty_live_states_refresh_backoff_and_sequence() {
    let h = Harness::new(Default::default());
    let initial = h.add(
        auth_with_metadata("file", "codex", json!({"access_token":"dummy"})),
        &["m"],
    );
    let mut stale = initial.as_ref().clone();
    stale.model_states.insert(
        "obsolete".into(),
        ModelState {
            unavailable: true,
            next_retry_after: Some(h.now() + TimeDelta::hours(9)),
            ..Default::default()
        },
    );
    stale.last_refreshed_at = Some(h.now() - TimeDelta::hours(5));
    stale.next_refresh_after = Some(h.now() + TimeDelta::hours(5));
    stale.created_at = None;
    stale.metadata.insert("prefix".into(), json!("new-setting"));
    failure(&h, "m", 429);
    {
        let mut state = h.manager.lock();
        let entry = state.auths.get_mut("file").unwrap();
        entry.refresh_failures = 3;
        let auth = Arc::make_mut(&mut entry.auth);
        auth.last_refreshed_at = Some(h.now() - TimeDelta::minutes(1));
        auth.next_refresh_after = Some(h.now() + TimeDelta::minutes(4));
    }
    // A result lands after observation captured its initial live handle.
    observed(&h, stale, || failure(&h, "m", 503));
    let live = h.get("file");
    assert!(!live.model_states.contains_key("obsolete"));
    assert_eq!(
        live.model_states["m"]
            .last_error
            .as_ref()
            .unwrap()
            .http_status,
        503
    );
    assert!(live.model_states["m"].cooldown_recorded_seq > 0);
    assert_eq!(live.created_at, initial.created_at);
    assert_eq!(
        live.last_refreshed_at,
        Some(h.now() - TimeDelta::minutes(1))
    );
    assert_eq!(
        live.next_refresh_after,
        Some(h.now() + TimeDelta::minutes(4))
    );
    assert_eq!(h.refresh_failures("file"), 3);
    assert_eq!(live.metadata["prefix"], "new-setting");
}

#[test]
fn file_observation_keeps_nonquota_credential_hold_and_dispatch_fence() {
    let h = Harness::new(Default::default());
    let initial = h.add(auth("file", "codex"), &["m"]);
    let dispatched = h.manager.next_execution_seq();
    failure(&h, "", 503);
    let before = h.get("file");
    observed(&h, initial.as_ref().clone(), || {});
    let live = h.get("file");
    assert_eq!(live.cooldown_recorded_seq, before.cooldown_recorded_seq);
    assert!(live.cooldown_recorded_seq > dispatched);
    assert_eq!(live.next_retry_after, before.next_retry_after);
    assert_eq!(live.status, before.status);
    assert_eq!(live.last_error.as_ref().unwrap().http_status, 503);
    h.manager.mark_result(&CallResult {
        auth_id: "file".into(),
        provider: "codex".into(),
        success: true,
        execution_seq: dispatched,
        ..Default::default()
    });
    assert_eq!(h.get("file").next_retry_after, before.next_retry_after);
}

#[test]
fn pending_observation_cannot_resurrect_cooldowns_cleared_by_reset() {
    let h = Harness::new(Default::default());
    h.add(auth("file", "codex"), &["m"]);
    failure(&h, "m", 429);
    let stale = h.get("file").as_ref().clone();
    observed(&h, stale, || {
        h.manager.reset_quota("file").unwrap();
    });
    let live = h.get("file");
    assert!(!live.unavailable);
    assert!(
        live.model_states
            .values()
            .all(|s| !s.unavailable && !s.quota.exceeded)
    );
    assert!(!live.quota.exceeded);
}

#[test]
fn pending_observation_cannot_publish_across_registration_epoch() {
    let h = Harness::new(Default::default());
    let initial = h.add(auth("file", "codex"), &["m"]);
    let result = h
        .manager
        .adopt_observed(&initial, None, |_| {
            h.manager.remove("file");
            h.add(auth("file", "codex"), &["m"]);
            Ok(AdoptionRead::Record(initial.as_ref().clone()))
        })
        .unwrap();
    assert!(matches!(result, AdoptionResult::RegistrationChanged));
    assert!(h.get("file").registration_epoch > initial.registration_epoch);
    assert_eq!(h.get("file").status, initial.status);
}

#[test]
fn credential_quota_carry_keeps_its_cooldown_watermark() {
    let h = Harness::new(Default::default());
    let initial = h.add(auth("file", "codex"), &["m"]);
    let dispatched = h.manager.next_execution_seq();
    h.manager.mark_result(&CallResult {
        auth_id: "file".into(),
        provider: "codex".into(),
        model: "m".into(),
        credential_scope: true,
        retry_after: Some(std::time::Duration::from_secs(600)),
        execution_seq: h.manager.next_execution_seq(),
        error: Some(AuthError {
            http_status: 429,
            message: "quota".into(),
            ..Default::default()
        }),
        ..Default::default()
    });
    let before = h.get("file");
    h.manager.update_unsaved(initial.as_ref().clone()).unwrap();
    let live = h.get("file");
    assert_eq!(live.cooldown_recorded_seq, before.cooldown_recorded_seq);
    assert!(live.cooldown_recorded_seq > dispatched);
    h.manager.mark_result(&CallResult {
        auth_id: "file".into(),
        provider: "codex".into(),
        model: "m".into(),
        success: true,
        execution_seq: dispatched,
        ..Default::default()
    });
    assert!(h.get("file").quota.exceeded);
}
