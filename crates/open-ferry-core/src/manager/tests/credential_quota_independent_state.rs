// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_cooldown.go (v8.0.32, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A model's own cooldown is kept beneath a credential-wide quota cooldown,
//! and comes back when that quota ends.
//!
//! Upstream added this with its Devin quota tracking (b946d54e), and its
//! tests use Devin's executor and error messages. These are the
//! provider-neutral parts, written on `claude`: no Devin test is ported
//! line by line, so each test says what it follows.
//!
//! Deviations from upstream:
//! - Upstream's `isDevinQuotaExhaustionMessage` checks are treated as
//!   false, so only a failure marked `credential_scope` wraps the siblings.

use std::sync::Arc;
use std::time::Duration;

use chrono::TimeDelta;

use super::cooldown_state_store::{RecordingStore, temp_dir};
use super::support::*;
use crate::auth::{AuthError, ModelState, QuotaState, Status, Timestamp};
use crate::manager::cooldown_store::{
    FileStore, Record, StateStore, flush, install_store, restore_now, set_debounce,
};
use crate::manager::{CallResult, Settings};

const PROVIDER: &str = "claude";
const OWN: &str = "model-own";
const OTHER: &str = "model-other";

fn error(status: u16, message: &str) -> AuthError {
    AuthError {
        message: message.into(),
        retryable: true,
        http_status: status,
        ..AuthError::default()
    }
}

/// A 429 that holds every model of the credential for `minutes`.
fn credential_quota(id: &str, model: &str, minutes: u64) -> CallResult {
    CallResult {
        auth_id: id.into(),
        provider: PROVIDER.into(),
        model: model.into(),
        success: false,
        retry_after: Some(Duration::from_secs(minutes * 60)),
        credential_scope: true,
        error: Some(error(429, "credential quota")),
        ..CallResult::default()
    }
}

/// A 401 on one model, which holds it for about 30 minutes.
fn own_failure(id: &str, model: &str) -> CallResult {
    CallResult {
        auth_id: id.into(),
        provider: PROVIDER.into(),
        model: model.into(),
        success: false,
        error: Some(error(401, "own failure")),
        ..CallResult::default()
    }
}

fn harness_with(id: &str) -> Harness {
    let h = Harness::new(Settings::default());
    let mut candidate = auth(id, PROVIDER);
    candidate.status = Status::Active;
    h.manager
        .register_unsaved(candidate)
        .expect("register the credential");
    h
}

fn model_state(h: &Harness, id: &str, model: &str) -> ModelState {
    h.get(id)
        .model_states
        .get(model)
        .cloned()
        .unwrap_or_else(|| panic!("expected model state for {model}"))
}

/// Follows upstream's Devin test of a credential quota over a sibling that
/// holds its own cooldown: the first wrap keeps the sibling's live block.
#[tokio::test(start_paused = true)]
async fn first_wrap_keeps_the_siblings_own_cooldown() {
    let id = "wrap-keeps-own";
    let h = harness_with(id);
    h.manager.mark_result(&own_failure(id, OWN));
    let before = h.now();

    h.manager.mark_result(&credential_quota(id, OTHER, 5));

    let state = model_state(&h, id, OWN);
    assert!(state.credential_quota_propagated, "{state:?}");
    assert_eq!(state.quota.reason, "credential_quota", "{state:?}");
    let shadow = state
        .credential_quota_shadow
        .as_deref()
        .unwrap_or_else(|| panic!("expected the own cooldown to be kept: {state:?}"));
    assert!(
        shadow
            .next_retry_after
            .is_some_and(|t| t >= before + TimeDelta::minutes(25)),
        "{shadow:?}"
    );
    // Only the credential-wide window holds the model on its surface.
    let other = model_state(&h, id, OTHER);
    assert!(other.credential_quota_shadow.is_none(), "{other:?}");
}

/// Follows upstream's Devin test of a model's own failure during a
/// credential quota: it lands in the independent layer.
#[tokio::test(start_paused = true)]
async fn own_failure_under_a_wrap_updates_the_independent_layer() {
    let id = "own-under-wrap";
    let h = harness_with(id);
    h.manager.mark_result(&credential_quota(id, OTHER, 5));
    let before = h.now();
    assert!(model_state(&h, id, OTHER).credential_quota_shadow.is_none());

    h.manager.mark_result(&own_failure(id, OTHER));

    let state = model_state(&h, id, OTHER);
    assert_eq!(state.quota.reason, "credential_quota", "{state:?}");
    assert!(state.credential_quota_propagated, "{state:?}");
    let shadow = state
        .credential_quota_shadow
        .as_deref()
        .unwrap_or_else(|| panic!("expected the own failure to be kept: {state:?}"));
    assert!(
        shadow
            .next_retry_after
            .is_some_and(|t| t >= before + TimeDelta::minutes(25)),
        "{shadow:?}"
    );
}

/// Follows upstream's Devin test of a wrap that ends: a later credential
/// quota finds the expired wrap unwrapped to the model's own cooldown.
#[tokio::test(start_paused = true)]
async fn a_new_wrap_after_the_old_one_ends_keeps_the_own_cooldown() {
    let id = "wrap-ends";
    let h = harness_with(id);
    h.manager.mark_result(&own_failure(id, OWN));
    let first = h.now();
    h.manager.mark_result(&credential_quota(id, OTHER, 5));

    h.clock.advance(Duration::from_secs(6 * 60));
    h.manager.mark_result(&credential_quota(id, OTHER, 5));

    let state = model_state(&h, id, OWN);
    let shadow = state
        .credential_quota_shadow
        .as_deref()
        .unwrap_or_else(|| panic!("expected the own cooldown to survive: {state:?}"));
    assert!(
        shadow
            .next_retry_after
            .is_some_and(|t| t >= first + TimeDelta::minutes(25)),
        "{shadow:?}"
    );
}

/// Not upstream's: the saved record carries the own layer and the mark, and
/// a restart brings both back. Follows the Devin restore tests.
#[tokio::test(start_paused = true)]
async fn the_independent_layer_is_saved_and_restored() {
    let id = "save-restore";
    let h = harness_with(id);
    set_debounce(&h.manager, Duration::from_secs(3600));
    let store = Arc::new(RecordingStore::default());
    install_store(&h.manager, store.clone());
    restore_now(&h.manager);
    h.manager.mark_result(&own_failure(id, OWN));
    h.manager.mark_result(&credential_quota(id, OTHER, 5));
    flush(&h.manager);

    let saved = store.saved();
    let record = saved
        .iter()
        .find(|r| r.model == OWN)
        .unwrap_or_else(|| panic!("expected a record for {OWN}: {saved:?}"));
    assert!(record.credential_quota_propagated, "{record:?}");
    assert!(record.independent_state.is_some(), "{record:?}");

    // A new process restores the records into a fresh credential.
    let restarted = harness_with(id);
    set_debounce(&restarted.manager, Duration::from_secs(3600));
    install_store(&restarted.manager, RecordingStore::with_load(saved));
    restore_now(&restarted.manager);

    let state = model_state(&restarted, id, OWN);
    assert!(state.credential_quota_propagated, "{state:?}");
    assert!(
        state.credential_quota_shadow.is_some(),
        "expected the own layer back: {state:?}"
    );
}

/// Not upstream's: the file written for an own layer reads back the same,
/// and a record from before the layer existed reads as having none.
#[test]
fn the_independent_layer_round_trips_through_the_file() {
    let dir = temp_dir();
    let store = FileStore::new(dir.path().to_path_buf());
    let now = chrono::Utc::now();
    let until = now + TimeDelta::minutes(30);
    let record = Record {
        provider: PROVIDER.into(),
        auth_id: "round-trip".into(),
        model: OWN.into(),
        status: "cooling".into(),
        next_retry_after: Some(until),
        reason: "quota".into(),
        independent_state: Some(ModelState {
            status: Status::Error,
            status_message: "own failure".into(),
            unavailable: true,
            next_retry_after: Some(until),
            last_error: Some(error(401, "own failure")),
            updated_at: Some(now),
            ..ModelState::default()
        }),
        credential_quota_propagated: true,
        updated_at: Some(now),
        ..Record::default()
    };
    let legacy = Record {
        model: OTHER.into(),
        independent_state: None,
        credential_quota_propagated: false,
        ..record.clone()
    };

    store
        .save(&[record.clone(), legacy.clone()], now)
        .expect("save");
    let loaded = store.load().expect("load");

    let read = |model: &str| {
        loaded
            .iter()
            .find(|r| r.model == model)
            .unwrap_or_else(|| panic!("expected a record for {model}: {loaded:?}"))
    };
    let own = read(OWN);
    assert!(own.credential_quota_propagated, "{own:?}");
    let layer = own.independent_state.as_ref().expect("independent state");
    assert_eq!(layer.status, Status::Error);
    assert_eq!(layer.status_message, "own failure");
    assert_eq!(layer.next_retry_after, Some(until));
    assert_eq!(layer.last_error.as_ref().map(|e| e.http_status), Some(401));
    let plain = read(OTHER);
    assert!(!plain.credential_quota_propagated, "{plain:?}");
    assert!(plain.independent_state.is_none(), "{plain:?}");
}

/// Follows upstream's reset and clear rules: a success under a live
/// credential quota changes nothing, and one after it ends clears the
/// model's own layer along with the cooldown.
#[tokio::test(start_paused = true)]
async fn a_success_after_the_quota_drops_the_independent_layer() {
    let id = "clear-drops";
    let h = harness_with(id);
    h.manager.mark_result(&own_failure(id, OWN));
    h.manager.mark_result(&credential_quota(id, OTHER, 5));
    let success = CallResult {
        auth_id: id.into(),
        provider: PROVIDER.into(),
        model: OWN.into(),
        success: true,
        ..CallResult::default()
    };

    h.manager.mark_result(&success);
    assert!(
        model_state(&h, id, OWN).credential_quota_shadow.is_some(),
        "a live credential quota stays"
    );

    h.clock.advance(Duration::from_secs(6 * 60));
    h.manager.mark_result(&success);
    let state = model_state(&h, id, OWN);
    assert!(state.credential_quota_shadow.is_none(), "{state:?}");
    assert!(!state.credential_quota_propagated, "{state:?}");
}

fn hours(h: i64) -> TimeDelta {
    TimeDelta::hours(h)
}

/// A credential quota block on a model that has lasted until `until`.
fn wrapped_state(until: Timestamp, now: Timestamp) -> ModelState {
    ModelState {
        status: Status::Error,
        unavailable: true,
        next_retry_after: Some(until),
        quota: QuotaState {
            exceeded: true,
            reason: "credential_quota".into(),
            next_recover_at: Some(until),
            ..QuotaState::default()
        },
        updated_at: Some(now),
        ..ModelState::default()
    }
}

/// A saved cooldown of `OWN` as the store gives it back.
fn saved(until: Timestamp, written: Timestamp, reason: &str) -> Record {
    Record {
        provider: PROVIDER.into(),
        auth_id: "restore".into(),
        model: OWN.into(),
        status: "cooling".into(),
        next_retry_after: Some(until),
        reason: reason.into(),
        quota: QuotaState {
            exceeded: true,
            reason: reason.into(),
            next_recover_at: Some(until),
            ..QuotaState::default()
        },
        updated_at: Some(written),
        ..Record::default()
    }
}

fn restore(h: &Harness, records: Vec<Record>) {
    set_debounce(&h.manager, Duration::from_secs(3600));
    install_store(&h.manager, RecordingStore::with_load(records));
    restore_now(&h.manager);
}

/// Ported from upstream's TestDevinRestoreDoesNotClobberNewerShadow, on
/// `claude`: a stale record's own layer doesn't replace a newer live one.
#[tokio::test(start_paused = true)]
async fn restore_does_not_clobber_a_newer_shadow() {
    let h = Harness::new(Settings::default());
    let now = h.now();
    let mut live = auth("restore", PROVIDER);
    live.status = Status::Active;
    let newer = ModelState {
        status: Status::Error,
        unavailable: true,
        next_retry_after: Some(now + hours(12)),
        updated_at: Some(now),
        ..ModelState::default()
    };
    live.model_states.insert(
        OWN.into(),
        ModelState {
            credential_quota_shadow: Some(Box::new(newer)),
            credential_quota_propagated: true,
            ..wrapped_state(now + hours(72), now)
        },
    );
    h.manager.register_unsaved(live).expect("register");

    let older = ModelState {
        status: Status::Error,
        unavailable: true,
        next_retry_after: Some(now + hours(1)),
        updated_at: Some(now - hours(2)),
        ..ModelState::default()
    };
    restore(
        &h,
        vec![Record {
            independent_state: Some(older),
            ..saved(now + hours(72), now - hours(2), "credential_quota")
        }],
    );

    let state = model_state(&h, "restore", OWN);
    let shadow = state.credential_quota_shadow.as_deref();
    assert!(
        shadow.is_some_and(|s| s.next_retry_after.is_some_and(|t| t > now + hours(6))),
        "newer live shadow clobbered by the stale record: {state:?}"
    );
}

/// Ported from upstream's TestDevinRestorePreservesLiveUnwrappedIndependent,
/// on `claude`: a stale wrapped record applied over a live cooldown of the
/// model's own keeps that cooldown as the layer beneath.
#[tokio::test(start_paused = true)]
async fn restore_keeps_a_live_unwrapped_cooldown_as_the_shadow() {
    let h = Harness::new(Settings::default());
    let now = h.now();
    let mut live = auth("restore", PROVIDER);
    live.status = Status::Active;
    let own = ModelState {
        status: Status::Error,
        unavailable: true,
        next_retry_after: Some(now + hours(12)),
        last_error: Some(error(404, "missing")),
        updated_at: Some(now),
        ..ModelState::default()
    };
    live.unavailable = true;
    live.next_retry_after = own.next_retry_after;
    live.model_states.insert(OWN.into(), own);
    h.manager.register_unsaved(live).expect("register");

    restore(
        &h,
        vec![Record {
            credential_quota_propagated: true,
            ..saved(now + hours(72), now - hours(2), "credential_quota")
        }],
    );

    let state = model_state(&h, "restore", OWN);
    let shadow = state
        .credential_quota_shadow
        .as_deref()
        .unwrap_or_else(|| panic!("the live cooldown must be kept as the shadow: {state:?}"));
    let deadline = shadow.next_retry_after.expect("shadow deadline");
    assert!(
        deadline > now + hours(6) && deadline < now + hours(24),
        "captured shadow deadline = {deadline}, want the ~12h block"
    );
}

/// Ported from upstream's TestDevinRestoreIndependentRecordUnderLiveWrapPreserved
/// (its restore assertions; the refresh that follows is Devin's): a record
/// of a model's own cooldown that arrives late, under a live credential
/// quota, is kept as the model's layer.
#[tokio::test(start_paused = true)]
async fn restore_keeps_an_independent_record_under_a_live_wrap() {
    let h = harness_with("restore");
    h.manager
        .mark_result(&credential_quota("restore", OTHER, 72 * 60));
    let now = h.now();
    assert!(
        h.get("restore")
            .model_states
            .get(OWN)
            .is_none_or(|s| s.credential_quota_shadow.is_none())
    );

    restore(
        &h,
        vec![Record {
            last_error: Some(error(429, "model rate limit")),
            ..saved(now + hours(12), now - TimeDelta::minutes(1), "quota")
        }],
    );

    let state = model_state(&h, "restore", OWN);
    let shadow = state
        .credential_quota_shadow
        .as_deref()
        .unwrap_or_else(|| panic!("the record must be kept beneath the wrap: {state:?}"));
    assert!(
        shadow
            .next_retry_after
            .is_some_and(|t| t > now + hours(6) && t < now + hours(24)),
        "{shadow:?}"
    );
    assert!(state.credential_quota_propagated, "{state:?}");
}
