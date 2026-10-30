//! Tests of the Codex refresh lease: the owed save, read-back marks and the
//! send fence (B7b fills this file).
//!
//! Covered: the commit outcomes that leave a partition marked or confirmed
//! (B-32), the parked save (B-34), the pass's own tail and the owed save
//! (B-36, B-37), the read-back marks as the send fence sees them (B-38) and
//! the send fence itself (B-39). Every test runs under `bounded`, orders its
//! steps with hooks and never sleeps. Logs and assertions name ids and
//! partition names only.

use super::siwc_persistence::{
    LeaseReadHook, Probe, REFRESH_SOURCE, Rig, bounded, code_only, expiry_executor, fixture,
    fn_bodies, mutated, observed, partition, pending_b, rig, token_count,
};
use super::support::FakeExecutor;
use crate::auth::codex_refresh::{CodexPersistIntent, CredentialRefreshPatch};
use crate::auth::{Auth, CodexOrigin, SaveDeferral};
use crate::exec::ExecError;
use crate::manager::lifecycle::Save;
use crate::manager::{
    EditError, HeldPartitions, Manager, ManagerError, MarkCause, Persisted, RefreshHold, lock,
};
use serde_json::{Map, Value, json};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const ID: &str = "codex.json";

// ---- shared setup ----

/// A peer process's raw write of one partition's tokens into the credential
/// file. A raw write takes no credential lock, so it works during a lease.
fn peer_write(path: &Path, outer: bool, name: &str) {
    let mut file: Map<String, Value> =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let target = if outer {
        &mut file
    } else {
        file.get_mut("siwc")
            .and_then(Value::as_object_mut)
            .expect("the file has a SIWC partition")
    };
    target.insert("access_token".into(), json!(format!("{name}-access")));
    target.insert("refresh_token".into(), json!(format!("{name}-refresh")));
    std::fs::write(path, serde_json::to_vec(&file).unwrap()).unwrap();
}

/// Edits the credential file with a raw write, as a peer process does.
fn edit_file(path: &Path, change: impl FnOnce(&mut Map<String, Value>)) {
    let mut file: Map<String, Value> =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    change(&mut file);
    std::fs::write(path, serde_json::to_vec(&file).unwrap()).unwrap();
}

/// `before` with its outer partition rotated to `name`, as the provider
/// answers, carrying the rotation as the refresh intent.
fn rotated(before: &Auth, name: &str) -> Auth {
    let mut after = before.clone();
    after
        .metadata
        .insert("access_token".into(), json!(format!("{name}-access")));
    after
        .metadata
        .insert("refresh_token".into(), json!(format!("{name}-refresh")));
    after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
        &before.metadata,
        &after.metadata,
        true,
        false,
    ));
    after
}

/// `before` with both partitions rotated (outer to `outer`, SIWC to `siwc`).
fn rotated_both(before: &Auth, outer: &str, siwc: &str) -> Auth {
    let mut after = before.clone();
    after
        .metadata
        .insert("access_token".into(), json!(format!("{outer}-access")));
    after
        .metadata
        .insert("refresh_token".into(), json!(format!("{outer}-refresh")));
    after.metadata["siwc"]["access_token"] = json!(format!("{siwc}-access"));
    after.metadata["siwc"]["refresh_token"] = json!(format!("{siwc}-refresh"));
    after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
        &before.metadata,
        &after.metadata,
        true,
        true,
    ));
    after
}

/// The executor answers its first call with outer B and any later one with R.
fn b_then_r(probe: &Probe) {
    let calls = AtomicU32::new(0);
    probe.fake.set_refresh(move |before| {
        let name = if calls.fetch_add(1, Ordering::SeqCst) == 0 {
            "b"
        } else {
            "r"
        };
        Ok(rotated(before, name))
    });
}

/// Lets a rotation commit give up after about a millisecond.
fn quick_commit_retries(rig: &Rig) {
    *lock(&rig.manager.shared.lease_retry_window) = Some(Duration::from_millis(1));
}

/// Runs `act` with the pass's published snapshot, once the lease is released.
fn at_tail(rig: &Rig, act: impl Fn(&Auth) + Send + Sync + 'static) {
    *lock(&rig.manager.shared.lease_tail_hook) = Some(Arc::new(act));
}

/// Runs `act` the first time a lease commit starts.
fn at_first_commit(rig: &Rig, act: impl Fn() + Send + Sync + 'static) {
    let armed = AtomicBool::new(true);
    *lock(&rig.store.before_lease_commit_hook) = Some(Arc::new(move |_: &Auth| {
        if armed.swap(false, Ordering::SeqCst) {
            act();
        }
    }));
}

/// A partition's mark and the name of its cause.
fn mark(manager: &Manager, outer: bool) -> (bool, &'static str) {
    let state = manager.lock();
    let entry = &state.auths[ID];
    let (marked, cause) = if outer {
        (entry.undurable.outer, &entry.undurable_causes.outer)
    } else {
        (entry.undurable.siwc, &entry.undurable_causes.siwc)
    };
    let cause = match cause {
        None => "none",
        Some(MarkCause::ReadBack { .. }) => "read-back",
        Some(MarkCause::Different) => "different",
    };
    (marked, cause)
}

/// What a pass left in the live record, read in one look.
#[derive(Debug, PartialEq)]
struct View {
    outer: (bool, &'static str),
    siwc: (bool, &'static str),
    /// `is_written` of each pending operation.
    pending: Vec<bool>,
    /// `is_written` of the intent when it is a refresh.
    intent_written: Option<bool>,
}

fn view(manager: &Manager) -> View {
    let live = manager.get(ID).unwrap();
    View {
        outer: mark(manager, true),
        siwc: mark(manager, false),
        pending: live
            .codex_pending_refresh
            .iter()
            .map(CredentialRefreshPatch::is_written)
            .collect(),
        intent_written: match &live.codex_persist_intent {
            CodexPersistIntent::Refresh(patch) => Some(patch.is_written()),
            _ => None,
        },
    }
}

fn warned(manager: &Manager) -> (bool, bool) {
    let state = manager.lock();
    let flags = state.auths[ID].read_back_warned;
    (flags.outer, flags.siwc)
}

fn landing(manager: &Manager) -> (bool, bool) {
    let state = manager.lock();
    let flags = state.auths[ID].landing_unconfirmed;
    (flags.outer, flags.siwc)
}

fn owed(manager: &Manager) -> Option<(crate::auth::SaveDeferral, u64)> {
    manager.lock().auths[ID]
        .owed_save
        .map(|owed| (owed.reason, owed.version))
}

/// A sign-in or bind of one partition: capture the command, write the new
/// tokens into the file with a raw write, adopt the completed command.
fn token_command(rig: &Rig, outer: bool, name: &str) {
    let command = rig
        .manager
        .capture_token_command(ID, partition(outer))
        .unwrap();
    peer_write(&rig.path(), outer, name);
    let mut login = fixture();
    login.metadata = serde_json::from_slice(&std::fs::read(rig.path()).unwrap()).unwrap();
    rig.manager
        .adopt_completed_token_command(&login, command)
        .unwrap();
}

/// The wait the last deferral set, in seconds from now.
fn wait_seconds(rig: &Rig) -> i64 {
    let held = rig.live().next_refresh_after.expect("a wait was set");
    (held - chrono::Utc::now()).num_seconds()
}

fn assert_store_error(error: ManagerError, what: &str) {
    assert!(
        matches!(error, ManagerError::Store(_)),
        "{what}: a deferral is a store error, got {error}"
    );
}

/// The `final` rotation A -> B marks, as `lease_read_hook` leaves them for
/// B-32 (b): the hook puts sA's SIWC object into the map (c)'s read returns.
fn arm_read_with_original_siwc(rig: &Rig) {
    let store = Arc::downgrade(&rig.store);
    let original = fixture().metadata["siwc"].clone();
    at_first_commit(rig, move || {
        let original = original.clone();
        let hook: LeaseReadHook = Arc::new(move |map: &mut Map<String, Value>| {
            map.insert("siwc".into(), original.clone());
        });
        if let Some(store) = store.upgrade() {
            *lock(&store.lease_read_hook) = Some(hook);
        }
    });
}

// ---- B-32: the commit outcomes ----

#[tokio::test(flavor = "current_thread")]
async fn b32_a_a_read_that_confirms_the_landing_leaves_nothing_marked() {
    // B-32 (a): every commit attempt writes B and returns an error, so the
    // outcome is Exhausted, and (c)'s read shows B.
    // Mutation: publication marking the partition although (c)'s read confirmed B (the mark would block the next pass).
    bounded(async {
        let rig = rig();
        let probe = Probe::new("codex", false);
        b_then_r(&probe);
        rig.manager.register_executor(probe.clone());
        quick_commit_retries(&rig);
        rig.store
            .fail_after_lease_commit
            .store(u32::MAX, Ordering::SeqCst);
        let at_tail_view = Arc::new(Mutex::new(None));
        let (path, seen, manager) = (rig.path(), at_tail_view.clone(), rig.manager.clone());
        at_tail(&rig, move |_| {
            *lock(&seen) = Some(view(&manager));
            // A peer rotated the landed B.
            peer_write(&path, true, "c");
        });
        rig.manager.force_refresh(ID).await.unwrap();
        assert!(
            rig.store.lease_commit_attempts.load(Ordering::SeqCst) >= 1,
            "the commit ran"
        );
        // Step 1: no mark, no cause, `final` written, nothing pending.
        assert_eq!(
            lock(&at_tail_view).take().expect("the tail hook ran"),
            View {
                outer: (false, "none"),
                siwc: (false, "none"),
                pending: vec![],
                intent_written: Some(true),
            }
        );
        // Step 3: the ordinary save keeps the peer's C; nothing is marked.
        assert_eq!(rig.disk()["refresh_token"], "c-refresh");
        assert_eq!(
            rig.live().metadata["refresh_token"],
            "c-refresh",
            "live is unmarked, so the read-back took C"
        );
        assert_eq!(mark(&rig.manager, true), (false, "none"));
        assert_eq!(rig.manager.refresh_hold(ID), None);
        // The next pass sends C, never B.
        rig.store.fail_after_lease_commit.store(0, Ordering::SeqCst);
        *lock(&rig.manager.shared.lease_tail_hook) = None;
        rig.manager.force_refresh(ID).await.unwrap();
        assert_eq!(probe.sent(), 2);
        assert_eq!(probe.sent_refresh(0), "outer-refresh-old");
        assert_eq!(probe.sent_refresh(1), "c-refresh", "C was sent, never B");
        assert_eq!(probe.reused(), 0, "a spent token was presented again");
        assert_eq!(rig.disk()["refresh_token"], "r-refresh");
        assert_eq!(mark(&rig.manager, true), (false, "none"));
        assert_eq!(rig.manager.refresh_hold(ID), None);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b32_b_a_two_partition_operation_with_one_partition_confirmed() {
    // B-32 (b): the file holds both rotated partitions, but (c)'s read shows
    // sA, so only the outer partition is confirmed.
    // Mutation: confirming the whole operation from one partition (SIWC would be treated as landed).
    bounded(async {
        let rig = rig();
        let probe = Probe::new("codex", false);
        probe
            .fake
            .set_refresh(|before| Ok(rotated_both(before, "b", "sb")));
        rig.manager.register_executor(probe.clone());
        quick_commit_retries(&rig);
        rig.store
            .fail_after_lease_commit
            .store(u32::MAX, Ordering::SeqCst);
        arm_read_with_original_siwc(&rig);
        let at_tail_view = Arc::new(Mutex::new(None));
        let baselines = Arc::new(Mutex::new(None));
        let (seen, kept, manager) = (at_tail_view.clone(), baselines.clone(), rig.manager.clone());
        at_tail(&rig, move |_| {
            *lock(&seen) = Some(view(&manager));
            let state = manager.lock();
            let entry = &state.auths[ID];
            *lock(&kept) = Some((
                entry.undurable_baselines.outer.len(),
                entry
                    .undurable_baselines
                    .siwc
                    .iter()
                    .map(|baseline| baseline.get("refresh_token").cloned())
                    .collect::<Vec<_>>(),
            ));
        });
        rig.manager.force_refresh(ID).await.unwrap();
        *lock(&rig.store.before_lease_commit_hook) = None;
        // Step 1: SIWC is marked `Different` with final's baselines; the
        // operation is pending and not written.
        assert_eq!(
            lock(&at_tail_view).take().expect("the tail hook ran"),
            View {
                outer: (false, "none"),
                siwc: (true, "different"),
                pending: vec![false],
                intent_written: Some(false),
            }
        );
        let (outer_baselines, siwc_baselines) = lock(&baselines).take().unwrap();
        assert_eq!(
            outer_baselines, 0,
            "the confirmed partition has no baseline"
        );
        assert!(
            siwc_baselines.contains(&Some(json!("siwc-refresh-old"))),
            "the baselines include sA"
        );
        // Step 2: the ordinary save retries and clears it.
        let disk = rig.disk();
        assert_eq!(disk["refresh_token"], "b-refresh");
        assert_eq!(disk["siwc"]["refresh_token"], "sb-refresh");
        assert_eq!(
            view(&rig.manager),
            View {
                outer: (false, "none"),
                siwc: (false, "none"),
                pending: vec![],
                intent_written: None,
            }
        );
        assert_eq!(probe.sent(), 1, "the executor was entered once");
        assert_eq!(probe.reused(), 0, "a spent token was presented again");
    })
    .await;
}

/// B-32 (c) steps 1 to 3: B landed, the confirming read failed, and a peer
/// wrote C at the tail. Live holds B, outer is marked `ReadBack`, and the
/// file holds C.
async fn third_token_after_unconfirmed_landing() -> (Rig, Arc<Probe>) {
    let rig = rig();
    let probe = Probe::new("codex", false);
    b_then_r(&probe);
    rig.manager.register_executor(probe.clone());
    quick_commit_retries(&rig);
    rig.store
        .fail_after_lease_commit
        .store(u32::MAX, Ordering::SeqCst);
    let store = Arc::downgrade(&rig.store);
    at_first_commit(&rig, move || {
        if let Some(store) = store.upgrade() {
            store.fail_lease_read.store(1, Ordering::SeqCst);
        }
    });
    let at_tail_view = Arc::new(Mutex::new(None));
    let (path, seen, manager) = (rig.path(), at_tail_view.clone(), rig.manager.clone());
    at_tail(&rig, move |_| {
        *lock(&seen) = Some(view(&manager));
        peer_write(&path, true, "c");
    });
    rig.manager.force_refresh(ID).await.unwrap();
    *lock(&rig.store.before_lease_commit_hook) = None;
    *lock(&rig.manager.shared.lease_tail_hook) = None;
    rig.store.fail_after_lease_commit.store(0, Ordering::SeqCst);
    // Step 1: outer marked `ReadBack`, `final` pending and not yet written.
    assert_eq!(
        lock(&at_tail_view).take().expect("the tail hook ran"),
        View {
            outer: (true, "read-back"),
            siwc: (false, "none"),
            pending: vec![false],
            intent_written: Some(false),
        }
    );
    // Step 3: the save kept C and marked `final` written; the mark stays.
    assert_eq!(rig.disk()["refresh_token"], "c-refresh");
    assert_eq!(rig.live().metadata["refresh_token"], "b-refresh");
    assert_eq!(mark(&rig.manager, true), (true, "read-back"));
    assert_eq!(rig.manager.refresh_hold(ID), None, "no hold yet");
    assert_eq!(landing(&rig.manager), (false, false));
    let origin = CodexOrigin::of(&rig.path()).unwrap();
    assert!(
        matches!(
            &rig.manager.lock().auths[ID].undurable_causes.outer,
            Some(MarkCause::ReadBack { origin: marked }) if *marked == origin
        ),
        "the cause names the leased file"
    );
    (rig, probe)
}

#[tokio::test(flavor = "current_thread")]
async fn b32_c_a_third_token_holds_the_credential_until_a_token_command() {
    // B-32 (c) steps 4 to 6.
    // Mutation: the third token being treated as the landing (the credential would send B again, already rotated).
    bounded(async {
        let (rig, probe) = third_token_after_unconfirmed_landing().await;
        assert_eq!(probe.sent(), 1);
        // Step 4: step 4c finds C, neither live's B nor the baseline A.
        let started = chrono::Utc::now();
        let error = rig.manager.force_refresh(ID).await.unwrap_err();
        assert_store_error(error, "landing unconfirmed");
        assert_eq!(probe.sent(), 1, "the executor saw A only");
        assert_eq!(probe.sent_refresh(0), "outer-refresh-old");
        assert_eq!(
            rig.manager.refresh_hold(ID),
            Some((RefreshHold::LandingUnconfirmed, HeldPartitions::Outer))
        );
        assert_eq!(landing(&rig.manager), (true, false));
        assert_eq!(mark(&rig.manager, true), (true, "read-back"));
        assert_eq!(rig.live().metadata["refresh_token"], "b-refresh");
        assert_eq!(rig.disk()["refresh_token"], "c-refresh");
        let held = rig.live().next_refresh_after.expect("a wait was set");
        assert!(held >= started + chrono::Duration::seconds(60), "{held}");
        assert!(wait_seconds(&rig) <= 1800, "{}", wait_seconds(&rig));
        // Step 5: a second force and the 401 path stop at step 1.
        let leases = rig.store.lease_calls.load(Ordering::SeqCst);
        let error = rig.manager.force_refresh(ID).await.unwrap_err();
        assert_store_error(error, "second force");
        let unauthorized = ExecError::upstream(401, "unauthorized");
        assert!(
            rig.manager
                .try_refresh_after_unauthorized(&rig.live(), &unauthorized, false)
                .await
                .is_none(),
            "the 401 path yields no retry"
        );
        assert_eq!(
            rig.store.lease_calls.load(Ordering::SeqCst),
            leases,
            "both stopped before the lease"
        );
        assert_eq!(probe.sent(), 1);
        // Step 6: only a token command ends it.
        token_command(&rig, true, "n");
        assert_eq!(mark(&rig.manager, true), (false, "none"));
        assert_eq!(landing(&rig.manager), (false, false));
        assert_eq!(rig.manager.refresh_hold(ID), None);
        assert_eq!(rig.live().metadata["refresh_token"], "n-refresh");
        rig.manager.force_refresh(ID).await.unwrap();
        assert_eq!(probe.sent(), 2);
        assert_eq!(probe.sent_refresh(1), "n-refresh", "N was sent");
        assert_eq!(probe.reused(), 0, "a spent token was presented again");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b32_c_a_failed_read_defers_with_a_lease_error_and_keeps_the_mark() {
    // B-32 (c) variant: step 3's read fails, so the pass defers before 4c.
    // Mutation: a failed read clearing or relabelling the mark instead of deferring.
    bounded(async {
        let (rig, probe) = third_token_after_unconfirmed_landing().await;
        rig.store.fail_lease_read.store(1, Ordering::SeqCst);
        let error = rig.manager.force_refresh(ID).await.unwrap_err();
        let ManagerError::Store(error) = error else {
            panic!("a deferral is a store error");
        };
        assert!(
            error.to_string().contains("injected lease read failure"),
            "the read's own error is handed back: {error}"
        );
        assert_eq!(probe.sent(), 1, "nothing was sent");
        assert_eq!(mark(&rig.manager, true), (true, "read-back"));
        assert_eq!(landing(&rig.manager), (false, false));
        assert_eq!(rig.manager.refresh_hold(ID), None);
        // Step 4 then follows as written.
        let error = rig.manager.force_refresh(ID).await.unwrap_err();
        assert_store_error(error, "landing unconfirmed");
        assert_eq!(
            rig.manager.refresh_hold(ID),
            Some((RefreshHold::LandingUnconfirmed, HeldPartitions::Outer))
        );
        assert_eq!(probe.sent(), 1);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b32_d_a_write_that_did_not_land_is_committed_again_and_live_is_sent() {
    // B-32 (d): every attempt fails before it forwards and the confirming
    // read fails too, so outer is marked `ReadBack` though D/f stays A.
    // Mutation: an unlanded write being treated as landed (the mark would not be ReadBack and live B would not be sent).
    bounded(async {
        let rig = rig();
        let probe = Probe::new("codex", false);
        b_then_r(&probe);
        rig.manager.register_executor(probe.clone());
        quick_commit_retries(&rig);
        rig.store
            .fail_lease_commit
            .store(u32::MAX, Ordering::SeqCst);
        let store = Arc::downgrade(&rig.store);
        at_first_commit(&rig, move || {
            if let Some(store) = store.upgrade() {
                store.fail_lease_read.store(1, Ordering::SeqCst);
            }
        });
        let store = Arc::downgrade(&rig.store);
        at_tail(&rig, move |_| {
            if let Some(store) = store.upgrade() {
                store.fail_refresh.store(true, Ordering::SeqCst);
            }
        });
        rig.manager.force_refresh(ID).await.unwrap();
        *lock(&rig.store.before_lease_commit_hook) = None;
        *lock(&rig.manager.shared.lease_tail_hook) = None;
        assert_eq!(rig.disk()["refresh_token"], "outer-refresh-old");
        assert_eq!(rig.live().metadata["refresh_token"], "b-refresh");
        assert_eq!(mark(&rig.manager, true), (true, "read-back"));
        assert_eq!(rig.live().codex_pending_refresh.len(), 1, "B is owed");
        // Step 1: the next pass reads A (the baseline), commits A -> B, sends B.
        rig.store.fail_lease_commit.store(0, Ordering::SeqCst);
        rig.manager.force_refresh(ID).await.unwrap();
        assert_eq!(probe.sent(), 2);
        assert_eq!(probe.sent_refresh(0), "outer-refresh-old");
        assert_eq!(probe.sent_refresh(1), "b-refresh", "live B was sent");
        assert_eq!(probe.reused(), 0, "a spent token was presented again");
        // Step 2.
        assert_eq!(rig.disk()["refresh_token"], "r-refresh");
        assert_eq!(rig.live().metadata["refresh_token"], "r-refresh");
        assert_eq!(
            view(&rig.manager),
            View {
                outer: (false, "none"),
                siwc: (false, "none"),
                pending: vec![],
                intent_written: None,
            }
        );
        assert_eq!(rig.manager.refresh_hold(ID), None);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_exhausted_commit_that_kept_the_tokens_is_not_confirmed_by_identity() {
    // Every commit attempt fails before it writes, and the confirming read
    // succeeds. The rotation kept the tokens and changed only the expiry, so
    // the file that read returns has the rotation's identity but not its
    // expiry: outer did not land, and the tail's save writes it.
    // Mutation: the confirming read judged on identity alone (the rotation
    // would count as written with nothing marked or pending, and the tail's
    // Configuration save would keep the file's old expiry).
    bounded(async {
        let rig = rig();
        rig.manager.register_executor(expiry_executor());
        quick_commit_retries(&rig);
        rig.store
            .fail_lease_commit
            .store(u32::MAX, Ordering::SeqCst);
        let at_tail_view = Arc::new(Mutex::new(None));
        let (seen, manager) = (at_tail_view.clone(), rig.manager.clone());
        at_tail(&rig, move |_| *lock(&seen) = Some(view(&manager)));
        rig.manager.force_refresh(ID).await.unwrap();
        *lock(&rig.manager.shared.lease_tail_hook) = None;
        assert!(
            rig.store.lease_commit_attempts.load(Ordering::SeqCst) >= 1,
            "the commit ran"
        );
        assert_eq!(
            lock(&at_tail_view).take().expect("the tail hook ran"),
            View {
                outer: (true, "different"),
                siwc: (false, "none"),
                pending: vec![false],
                intent_written: Some(false),
            }
        );
        assert_eq!(
            rig.disk()["expired"],
            "2099-02-01T00:00:00Z",
            "the tail's save wrote the expiry"
        );
        assert_eq!(rig.disk()["refresh_token"], "outer-refresh-old");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_exhausted_commit_is_judged_against_the_locked_files_values_too() {
    // As above, but the rotation kept the tokens and the expiry and set
    // `last_refresh`, and the locked file already has that `last_refresh`
    // with another expiry. The commit would write live's expiry over the
    // file's, so the file the confirming read returns is not the result.
    // Mutation: the confirming read checking only the values the rotation
    // changed from live (outer would count as written, and the tail's
    // Configuration save would keep the file's expiry).
    bounded(async {
        let rig = rig();
        let executor = FakeExecutor::new("codex");
        executor.set_refresh(|before| {
            let mut after = before.clone();
            after
                .metadata
                .insert("last_refresh".into(), json!("2026-10-17T12:00:00Z"));
            after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
                &before.metadata,
                &after.metadata,
                true,
                false,
            ));
            Ok(after)
        });
        rig.manager.register_executor(executor);
        edit_file(&rig.path(), |file| {
            file.insert("last_refresh".into(), json!("2026-10-17T12:00:00Z"));
            file.insert("expired".into(), json!("2099-03-01T00:00:00Z"));
        });
        quick_commit_retries(&rig);
        rig.store
            .fail_lease_commit
            .store(u32::MAX, Ordering::SeqCst);
        let at_tail_view = Arc::new(Mutex::new(None));
        let (seen, manager) = (at_tail_view.clone(), rig.manager.clone());
        at_tail(&rig, move |_| *lock(&seen) = Some(view(&manager)));
        rig.manager.force_refresh(ID).await.unwrap();
        *lock(&rig.manager.shared.lease_tail_hook) = None;
        assert!(
            rig.store.lease_commit_attempts.load(Ordering::SeqCst) >= 1,
            "the commit ran"
        );
        assert_eq!(
            lock(&at_tail_view).take().expect("the tail hook ran"),
            View {
                outer: (true, "different"),
                siwc: (false, "none"),
                pending: vec![false],
                intent_written: Some(false),
            }
        );
        assert_eq!(
            rig.disk()["expired"],
            "2099-01-01T00:00:00Z",
            "the tail's save wrote live's expiry"
        );
        assert_eq!(rig.disk()["last_refresh"], "2026-10-17T12:00:00Z");
        assert_eq!(rig.disk()["refresh_token"], "outer-refresh-old");
    })
    .await;
}

// ---- B-36, B-37: the pass's tail and the owed save ----

/// An operator's change of the `note` setting, saved.
fn set_note(manager: &Manager, note: &str) -> Result<Option<Arc<Auth>>, ManagerError> {
    let mut changed = (*manager.get(ID).unwrap()).clone();
    changed.metadata.insert("note".into(), json!(note));
    manager.update(changed)
}

/// How many saves the store refused for a refresh in flight.
fn refused_count(rig: &Rig) -> usize {
    lock(&rig.store.guarded)
        .iter()
        .filter(|(_, deferral)| matches!(deferral, Some(SaveDeferral::RefreshInFlight)))
        .count()
}

/// The `note` the file holds.
fn disk_note(rig: &Rig) -> Value {
    rig.disk()["note"].clone()
}

/// Makes every Configuration save fail while the returned flag is set. A
/// leased pass's own tail saves a Configuration intent too (its tokens landed
/// at the commit), so this fails the tail's save and step 14's.
fn fail_configuration_saves(rig: &Rig) -> Arc<AtomicBool> {
    let failing = Arc::new(AtomicBool::new(false));
    let (flag, store) = (failing.clone(), Arc::downgrade(&rig.store));
    *lock(&rig.store.before_save_hook) = Some(Arc::new(move |auth: &Auth| {
        if matches!(auth.codex_persist_intent, CodexPersistIntent::Configuration)
            && flag.load(Ordering::SeqCst)
            && let Some(store) = store.upgrade()
        {
            *lock(&store.error_once) = Some("injected save failure".into());
        }
    }));
    failing
}

/// The change of an operator that makes the record a config-sourced API key
/// which still carries its token metadata: never saved, never refreshed.
fn make_ineligible(manager: &Manager, note: &str) {
    let mut changed = (*manager.get(ID).unwrap()).clone();
    changed.metadata.insert("note".into(), json!(note));
    // `use_siwc` makes a record OAuth whatever else it says.
    changed.metadata.insert("use_siwc".into(), json!(false));
    changed.metadata.insert("auth_kind".into(), json!("apikey"));
    changed
        .attributes
        .insert("source".into(), "config:openai[0]".into());
    // A `source_backend` attribute would classify it as a file credential.
    changed.attributes.remove("source_backend");
    manager.update(changed).unwrap();
}

/// A leased pass whose tail runs `act` with the lease released, the refresh
/// lock still held and the state lock free.
async fn leased_pass_with(rig: &Rig, probe: &Arc<Probe>, act: impl Fn() + Send + Sync + 'static) {
    rig.manager.register_executor(probe.clone());
    at_tail(rig, move |_| act());
    rig.manager.force_refresh(ID).await.unwrap();
    *lock(&rig.manager.shared.lease_tail_hook) = None;
}

#[tokio::test(flavor = "current_thread")]
async fn b36_d_a_save_refused_at_the_tail_is_owed_and_the_landed_tail_covers_it() {
    // B-36 (d) with the rotation landed: an update's save and its own flush
    // are both refused for the refresh in flight, and the pass's leased tail
    // then writes the note itself (a landed Refresh intent is saved as
    // Configuration) and clears the debt. Which later save covers it is not
    // pinned here; `b36_d_step_14_writes_a_change_the_exhausted_tail_left_owed`
    // isolates step 14.
    // Mutation: the leased tail dropping the owed change (D/f would keep the
    // old note).
    bounded(async {
        let rig = rig();
        let probe = Probe::new("codex", false).rotating("r");
        let refusals = Arc::new(Mutex::new(None));
        let (manager, seen) = (rig.manager.clone(), refusals.clone());
        let store = rig.store.clone();
        leased_pass_with(&rig, &probe, move || {
            set_note(&manager, "owed").unwrap();
            let state = owed(&manager);
            let refused = lock(&store.guarded)
                .iter()
                .filter(|(_, d)| matches!(d, Some(SaveDeferral::RefreshInFlight)))
                .count();
            *lock(&seen) = Some((state, refused));
        })
        .await;
        let (state, refused) = lock(&refusals).take().expect("the tail ran");
        assert!(
            matches!(state, Some((SaveDeferral::RefreshInFlight, _))),
            "the change is owed while the pass runs: {state:?}"
        );
        assert_eq!(refused, 2, "the save and its flush, with no loop");
        assert_eq!(refused_count(&rig), 2, "no later save was refused");
        assert_eq!(disk_note(&rig), "owed", "the change reached the file");
        assert_eq!(owed(&rig.manager), None, "a covering save cleared the flag");
        assert_eq!(rig.disk()["refresh_token"], "r-refresh");
        assert_eq!(rig.live().metadata["note"], "owed");
        assert_eq!(probe.reused(), 0);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b36_d_step_14_writes_a_change_the_exhausted_tail_left_owed() {
    // B-36 (d), isolating step 14: every commit attempt fails, so the leased
    // tail stays an unwritten Refresh intent, which writes the rotation and
    // leaves the owed note (and the flag) alone. The update's save and its own
    // flush are refused at the tail; only the pass's step 14 then writes the
    // note and clears the flag.
    // Mutation: removing the flush call in `refresh_with` (step 14) alone; the
    // other callers of `flush_owed_save` stay.
    bounded(async {
        let rig = rig();
        quick_commit_retries(&rig);
        rig.store
            .fail_lease_commit
            .store(u32::MAX, Ordering::SeqCst);
        let probe = Probe::new("codex", false).rotating("r");
        let seen = Arc::new(Mutex::new(None));
        let (manager, store, log) = (rig.manager.clone(), rig.store.clone(), seen.clone());
        leased_pass_with(&rig, &probe, move || {
            set_note(&manager, "owed").unwrap();
            let refused = lock(&store.guarded)
                .iter()
                .filter(|(_, d)| matches!(d, Some(SaveDeferral::RefreshInFlight)))
                .count();
            *lock(&log) = Some((owed(&manager), refused));
        })
        .await;
        let (state, refused) = lock(&seen).take().expect("the tail ran");
        assert!(
            matches!(state, Some((SaveDeferral::RefreshInFlight, _))),
            "the change is owed while the pass runs: {state:?}"
        );
        assert_eq!(refused, 2, "the update's save and its own flush");
        // Observed: the third refusal is the post-tail `finish_update` flush
        // deferring itself. The tail wrote R through `save_bound`, which
        // `guarded` does not record, so the one recorded write is step 14's
        // and carries the note.
        assert_eq!(
            deferrals(&rig),
            vec![
                Some(SaveDeferral::RefreshInFlight),
                Some(SaveDeferral::RefreshInFlight),
                Some(SaveDeferral::RefreshInFlight),
                None
            ],
            "the update, its flush and the post-tail flush refused; step 14 wrote"
        );
        assert!(
            rig.store.lease_commit_attempts.load(Ordering::SeqCst) >= 1,
            "the commit ran and never landed"
        );
        assert_eq!(rig.disk()["refresh_token"], "r-refresh", "the tail wrote R");
        assert_eq!(disk_note(&rig), "owed", "step 14 wrote the note");
        assert_eq!(owed(&rig.manager), None, "a covering save cleared the flag");
        assert_eq!(rig.live().metadata["note"], "owed");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b36_b_the_no_rotation_tail_writes_directly() {
    // B-36 (b) (decision 13): the executor returns live's tokens unchanged, so
    // the tail is the unleased one and saves without being refused.
    // Step 5's commit already put B on disk, so the tail's own save is what
    // is counted: one new guarded Written result and one new store write, and
    // no `save_bound` call.
    // Mutation: the tail skipping its save, or taking the leased path (its
    // save would be refused as in flight).
    bounded(async {
        let rig = rig();
        let probe = Probe::new("codex", false);
        probe.fake.set_refresh(|before| Ok(before.clone()));
        rig.manager.register_executor(probe.clone());
        rig.manager.register_unsaved(pending_b()).unwrap();
        let guarded_before = lock(&rig.store.guarded).len();
        let writes_before = lock(&rig.store.writes).len();
        let bound_before = rig.store.save_bound_calls.load(Ordering::SeqCst);
        rig.manager.force_refresh(ID).await.unwrap();
        let guarded = lock(&rig.store.guarded)[guarded_before..].to_vec();
        assert_eq!(
            guarded.iter().map(|(_, d)| *d).collect::<Vec<_>>(),
            vec![None],
            "exactly one guarded save, and it was written: {guarded:?}"
        );
        assert_eq!(
            lock(&rig.store.writes).len() - writes_before,
            1,
            "one store write"
        );
        assert_eq!(
            rig.store.save_bound_calls.load(Ordering::SeqCst),
            bound_before,
            "no bound save"
        );
        assert_eq!(probe.sent(), 1, "the executor was entered once");
        assert_eq!(refused_count(&rig), 0, "no refusal from its own tail");
        assert_eq!(owed(&rig.manager), None);
        let live = rig.live();
        let disk = rig.disk();
        for key in ["note", "refresh_token", "access_token", "use_siwc"] {
            assert_eq!(disk[key], live.metadata[key], "D/f holds live's {key}");
        }
        assert_eq!(rig.disk()["refresh_token"], "b-refresh");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b36_c_an_unsupported_store_tail_writes_its_rotation_directly() {
    // B-36 (c) (decision 13): with no lease the tail is the pass's own unleased
    // save, and D/f holds R when `force_refresh` returns.
    // Mutation: the unsupported-store tail not writing its rotation (D/f would keep the old token).
    bounded(async {
        let rig = rig();
        rig.store.lease_unsupported.store(true, Ordering::SeqCst);
        let probe = Probe::new("codex", false).rotating("r");
        rig.manager.register_executor(probe.clone());
        rig.manager.force_refresh(ID).await.unwrap();
        assert_eq!(probe.sent(), 1);
        assert_eq!(refused_count(&rig), 0, "no refusal from its own tail");
        assert_eq!(owed(&rig.manager), None);
        assert_eq!(rig.disk()["refresh_token"], "r-refresh");
        assert_eq!(rig.live().metadata["refresh_token"], "r-refresh");
    })
    .await;
}

/// A leased pass that rotates to R. Its tail runs `act` with the lease
/// released and the refresh lock still held; `act` is the owed change's
/// setup, and the pass's step 14 follows it.
async fn pass_with_tail(rig: &Rig, act: impl Fn() + Send + Sync + 'static) -> Arc<Probe> {
    let probe = Probe::new("codex", false).rotating("r");
    leased_pass_with(rig, &probe, act).await;
    probe
}

#[tokio::test(flavor = "current_thread")]
async fn b37_a_a_failed_flush_keeps_the_flag_and_the_next_one_clears_it() {
    // B-37 (a): every save after the change fails, step 14's included; the
    // flag stays, and the next flush writes and clears it.
    // Mutation: a failed flush clearing the flag (the change would never be retried).
    bounded(async {
        let rig = rig();
        let failing = fail_configuration_saves(&rig);
        let (manager, flag) = (rig.manager.clone(), failing.clone());
        pass_with_tail(&rig, move || {
            set_note(&manager, "owed").unwrap();
            flag.store(true, Ordering::SeqCst);
        })
        .await;
        assert!(
            owed(&rig.manager).is_some(),
            "the failed flush keeps the flag"
        );
        assert_ne!(disk_note(&rig), "owed", "nothing wrote the change");
        // The next flush writes and clears it.
        failing.store(false, Ordering::SeqCst);
        rig.manager
            .flush_owed_save(ID, rig.live().registration_epoch);
        assert_eq!(disk_note(&rig), "owed");
        assert_eq!(owed(&rig.manager), None);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_owed_use_siwc_change_survives_adopting_a_peer_rotation() {
    // An operator turns SIWC off at a pass's tail and every save of the
    // change fails, so it stays owed. A peer then rotates outer, and the next
    // pass adopts that rotation from a file that still has SIWC on: the owed
    // change keeps live's switch, and the pass's flush writes it.
    // Mutation: adoption taking `use_siwc` from the file while a change is
    // owed (live would turn SIWC back on, and the flush would write that and
    // clear the flag).
    bounded(async {
        let rig = rig();
        let failing = fail_configuration_saves(&rig);
        let (manager, flag) = (rig.manager.clone(), failing.clone());
        let probe = pass_with_tail(&rig, move || {
            let mut changed = (*manager.get(ID).unwrap()).clone();
            changed.metadata.insert("use_siwc".into(), json!(false));
            manager.update(changed).unwrap();
            flag.store(true, Ordering::SeqCst);
        })
        .await;
        assert!(
            owed(&rig.manager).is_some(),
            "the failed flush keeps the flag"
        );
        assert_eq!(rig.disk()["use_siwc"], true, "nothing wrote the change");
        // A peer rotates outer; the next pass adopts it, and its flush writes.
        failing.store(false, Ordering::SeqCst);
        peer_write(&rig.path(), true, "p");
        rig.manager.force_refresh(ID).await.unwrap();
        assert_eq!(probe.sent(), 2);
        assert_eq!(
            probe.sent_refresh(1),
            "p-refresh",
            "the pass adopted the peer's rotation"
        );
        assert_eq!(
            rig.live().metadata["use_siwc"],
            false,
            "live keeps the owed switch"
        );
        assert_eq!(rig.disk()["use_siwc"], false, "the flush wrote it");
        assert_eq!(owed(&rig.manager), None);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_refusal_of_a_generation_already_written_owes_nothing() {
    // A flush reads the flag, then waits for the save lock while a pass's
    // tail writes the same generation and clears it. Refused by check (vi)
    // while the pass still holds the refresh lock, the flush must not owe it
    // again: the pass may already have looked for the flag, and nothing else
    // would flush it. The test holds the refresh lock in the pass's place.
    // Mutation: recording every refusal (the flag stays set after the change
    // is on disk).
    bounded(async {
        let rig = rig();
        let refresh_lock = lock(&rig.manager.shared.refresh_locks)
            .entry(ID.to_owned())
            .or_default()
            .clone();
        let pass = refresh_lock.clone().try_lock_owned().unwrap();
        set_note(&rig.manager, "owed").unwrap();
        let stale = rig.live();
        assert_eq!(
            owed(&rig.manager),
            Some((SaveDeferral::RefreshInFlight, stale.generation)),
            "the edit is owed behind the pass"
        );
        drop(pass);
        rig.manager.flush_owed_save(ID, stale.registration_epoch);
        assert_eq!(disk_note(&rig), "owed");
        assert_eq!(owed(&rig.manager), None);
        assert_eq!(
            rig.live().generation,
            stale.generation,
            "the flush wrote the edit's generation"
        );
        // A pass starts, and the flush that read the flag reaches check (vi).
        let _pass = refresh_lock.try_lock_owned().unwrap();
        let discard_epoch = rig.manager.lock().auths[ID].discard_epoch;
        let refused = rig
            .manager
            .persist(
                &stale,
                stale.registration_epoch,
                stale.generation,
                discard_epoch,
                Save::Yes,
            )
            .unwrap();
        assert_eq!(
            refused,
            Persisted::Deferred(SaveDeferral::RefreshInFlight),
            "check (vi) refused the late flush"
        );
        assert_eq!(
            owed(&rig.manager),
            None,
            "a refusal of a written generation owes nothing"
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b37_b_a_skipped_flush_keeps_the_flag() {
    // B-37 (b): the record is disabled and its file is gone, so the save is
    // Skipped; Skipped writes nothing and keeps the flag.
    // Mutation: a skipped flush clearing the flag.
    bounded(async {
        let rig = rig();
        let (manager, path) = (rig.manager.clone(), rig.path());
        pass_with_tail(&rig, move || {
            let mut changed = (*manager.get(ID).unwrap()).clone();
            changed.metadata.insert("note".into(), json!("owed"));
            changed.disabled = true;
            manager.update(changed).unwrap();
            assert!(owed(&manager).is_some(), "refused while the pass runs");
            std::fs::remove_file(&path).unwrap();
        })
        .await;
        assert!(!rig.path().exists(), "a Skipped save creates no file");
        assert!(
            owed(&rig.manager).is_some(),
            "a Skipped save leaves the flag"
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b37_c_an_ineligible_flush_keeps_the_flag() {
    // B-37 (c): the record became a config-sourced API key that keeps its
    // token metadata, which is never saved; the flag stays.
    // Mutation: an ineligible flush clearing the flag.
    bounded(async {
        let rig = rig();
        let manager = rig.manager.clone();
        pass_with_tail(&rig, move || {
            set_note(&manager, "owed").unwrap();
            assert!(owed(&manager).is_some(), "refused while the pass runs");
            make_ineligible(&manager, "owed");
        })
        .await;
        assert_ne!(disk_note(&rig), "owed", "an ineligible record is not saved");
        assert!(owed(&rig.manager).is_some(), "the flag stays");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b37_d_only_a_covering_configuration_save_clears_the_flag() {
    // B-37 (d): the commit never lands, so the tail's save is a Written
    // Refresh-intent save, and it leaves the flag; with every Configuration
    // save failing, the flag survives the pass. A Configuration save of the
    // owed generation then clears it.
    // Mutation: a Refresh-intent save clearing the flag (the owed note would be lost).
    bounded(async {
        let rig = rig();
        quick_commit_retries(&rig);
        rig.store
            .fail_lease_commit
            .store(u32::MAX, Ordering::SeqCst);
        let failing = fail_configuration_saves(&rig);
        let flag = Arc::new(Mutex::new(None));
        let (manager, seen, store, fail) = (
            rig.manager.clone(),
            flag.clone(),
            rig.store.clone(),
            failing.clone(),
        );
        pass_with_tail(&rig, move || {
            set_note(&manager, "owed").unwrap();
            *lock(&seen) = Some(owed(&manager));
            store.fail_lease_commit.store(0, Ordering::SeqCst);
            fail.store(true, Ordering::SeqCst);
        })
        .await;
        let owed_before = lock(&flag).take().unwrap().expect("owed at the tail");
        // The tail's Refresh-intent save was Written, and step 14 failed.
        assert_eq!(rig.disk()["refresh_token"], "r-refresh");
        assert_eq!(
            owed(&rig.manager),
            Some(owed_before),
            "a Written Refresh-intent save leaves the flag"
        );
        assert_ne!(disk_note(&rig), "owed");
        failing.store(false, Ordering::SeqCst);
        rig.manager
            .flush_owed_save(ID, rig.live().registration_epoch);
        assert_eq!(owed(&rig.manager), None);
        assert_eq!(disk_note(&rig), "owed");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b37_e_the_tail_save_over_a_refused_change_is_covered_by_step_14() {
    // B-37 (e): the commit never lands (`fail_lease_commit`), so the tail
    // saves `final` as a Refresh intent, with the file's old `note`, while
    // live keeps "new"; step 14 then saves live's note.
    // Mutation: the tail save's read-back copying disk's note over live (the owed note would be lost).
    bounded(async {
        let rig = rig();
        quick_commit_retries(&rig);
        rig.store
            .fail_lease_commit
            .store(u32::MAX, Ordering::SeqCst);
        let saves = Arc::new(Mutex::new(Vec::new()));
        let (log, path) = (saves.clone(), rig.path());
        *lock(&rig.store.before_save_hook) = Some(Arc::new(move |auth: &Auth| {
            let refresh = matches!(auth.codex_persist_intent, CodexPersistIntent::Refresh(_));
            let on_disk: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            lock(&log).push((refresh, on_disk["note"].clone()));
        }));
        let (manager, store) = (rig.manager.clone(), rig.store.clone());
        let at_tail = Arc::new(Mutex::new(None));
        let seen = at_tail.clone();
        pass_with_tail(&rig, move || {
            set_note(&manager, "new").unwrap();
            store.fail_lease_commit.store(0, Ordering::SeqCst);
            *lock(&seen) = Some(owed(&manager));
        })
        .await;
        assert!(lock(&at_tail).take().unwrap().is_some(), "owed at the tail");
        let saves = lock(&saves).clone();
        // A Refresh save writes the rotation and leaves the file's other
        // fields alone, so the file still held the old note when step 14's
        // save began.
        assert!(
            saves.contains(&(true, json!("initial"))),
            "the tail's Refresh save met the old note: {saves:?}"
        );
        assert_eq!(
            saves.last(),
            Some(&(false, json!("initial"))),
            "the tail's save left the note for step 14: {saves:?}"
        );
        let disk = rig.disk();
        assert_eq!(
            disk["refresh_token"], "r-refresh",
            "the tail wrote its rotation"
        );
        assert_eq!(disk["note"], "new");
        assert_eq!(owed(&rig.manager), None);
        assert_eq!(rig.live().metadata["note"], "new");
    })
    .await;
}

// ---- B-34: a parked save ----

type SaveResult = Result<Option<Arc<Auth>>, ManagerError>;

/// An update whose save is parked on another directory's credential lock.
struct Parked {
    /// The other directory (E). Dropped last.
    other: tempfile::TempDir,
    /// The lock the parked save waits for.
    held: Arc<Mutex<Option<crate::auth::credential_lock::CredentialLock>>>,
    save: Mutex<Option<std::thread::JoinHandle<SaveResult>>>,
}

impl Parked {
    fn file(&self) -> std::path::PathBuf {
        self.other.path().join(ID)
    }

    /// Lets the parked save go and waits for it to finish.
    fn release(&self) -> SaveResult {
        lock(&self.held).take();
        lock(&self.save)
            .take()
            .expect("released once")
            .join()
            .expect("the parked update did not panic")
    }
}

/// Binds the store to a new directory E (with a copy of the credential file
/// when `copy`), takes E's credential lock, parks an update of `note` on it in
/// a thread, and binds the store back to D. Reaching the lock is ordered by
/// the lock's own "waiting" event, never by a sleep.
fn park_update(rig: &Rig, copy: bool, note: &str) -> Parked {
    park_save(rig, copy, note, false)
}

/// [`park_update`], or with `settings_only` a settings-only `edit` that
/// installs no explicit path: it reads no file first, so it waits on E's
/// credential lock even when E/f is missing (`update` would stop at its
/// adoption read).
fn park_save(rig: &Rig, copy: bool, note: &str, settings_only: bool) -> Parked {
    let other = tempfile::tempdir_in(rig.dir.path().parent().unwrap()).unwrap();
    if copy {
        std::fs::copy(rig.path(), other.path().join(ID)).unwrap();
    }
    let here = rig.dir.path().to_path_buf();
    rig.store.files.set_base_dir(other.path());
    let file = other.path().join(ID);
    let held = Arc::new(Mutex::new(Some(
        crate::auth::credential_lock::acquire_sync(&file).unwrap(),
    )));
    let (waiting, reached) = std::sync::mpsc::channel();
    let waiting = Mutex::new(waiting);
    crate::auth::credential_lock::set_test_hook(
        &file,
        Some(Arc::new(move |event: &str| {
            if event == "waiting" {
                let _ = lock(&waiting).send(());
            }
        })),
    );
    let manager = rig.manager.clone();
    let note = note.to_owned();
    let save = std::thread::spawn(move || {
        if settings_only {
            manager
                .edit(ID, |auth| {
                    auth.metadata.insert("note".into(), json!(note));
                    Ok::<bool, ()>(true)
                })
                .map_err(|error| match error {
                    EditError::Manager(error) => error,
                    EditError::Rejected(()) => unreachable!("the edit never refuses"),
                })
        } else {
            let mut changed = (*manager.get(ID).unwrap()).clone();
            changed.metadata.insert("note".into(), json!(note));
            manager.update(changed)
        }
    });
    reached
        .recv_timeout(Duration::from_secs(10))
        .expect("the save reached the held credential lock");
    crate::auth::credential_lock::set_test_hook(&file, None);
    rig.store.files.set_base_dir(&here);
    Parked {
        other,
        held,
        save: Mutex::new(Some(save)),
    }
}

/// The generations and deferrals the store's guard recorded, in order.
fn deferrals_with_generation(rig: &Rig) -> Vec<(u64, Option<SaveDeferral>)> {
    lock(&rig.store.guarded).clone()
}

/// The deferrals the store's guard recorded, in order.
fn deferrals(rig: &Rig) -> Vec<Option<SaveDeferral>> {
    lock(&rig.store.guarded).iter().map(|(_, d)| *d).collect()
}

#[tokio::test(flavor = "current_thread")]
async fn b34_a_a_parked_save_released_during_the_pass_cannot_reach_the_old_directory() {
    // B-34 step 3: a save parked on E's lock is released at the tail, while the
    // pass still holds the refresh lock. Check (vi) runs before the origin
    // check, so the guard records `RefreshInFlight` and not `OriginMoved`
    // (the later notes say the same of `Stale`); either way E is untouched and
    // step 14 writes the change to P.
    // Mutation: removing check (vi) so a parked save is admitted during the pass (E's bytes would change).
    bounded(async {
        let rig = rig();
        let probe = Probe::new("codex", false).rotating("r");
        rig.manager.register_executor(probe.clone());
        rig.manager.register_unsaved(pending_b()).unwrap();
        let parked = Arc::new(park_update(&rig, true, "parked"));
        let before = std::fs::read(parked.file()).unwrap();
        let saved = Arc::new(Mutex::new(None));
        let (release, seen) = (parked.clone(), saved.clone());
        at_tail(&rig, move |_| {
            // The lease is released: the parked save gets E's lock now.
            *lock(&seen) = Some(release.release());
        });
        rig.manager.force_refresh(ID).await.unwrap();
        *lock(&rig.manager.shared.lease_tail_hook) = None;
        let result = lock(&saved).take().expect("the parked save ran");
        assert!(result.is_ok(), "the update itself succeeds");
        let refused = deferrals(&rig);
        assert!(
            refused.contains(&Some(SaveDeferral::RefreshInFlight)),
            "the parked save was refused: {refused:?}"
        );
        assert!(
            refused
                .iter()
                .flatten()
                .all(|d| *d == SaveDeferral::RefreshInFlight),
            "(vi) answers first: {refused:?}"
        );
        assert_eq!(
            std::fs::read(parked.file()).unwrap(),
            before,
            "E's file is untouched, byte for byte"
        );
        assert_eq!(probe.sent(), 1);
        assert_eq!(probe.sent_refresh(0), "b-refresh");
        assert_eq!(rig.disk()["refresh_token"], "r-refresh");
        assert_eq!(disk_note(&rig), "parked", "P holds the change");
        assert_eq!(owed(&rig.manager), None);
        assert_eq!(mark(&rig.manager, true), (false, "none"));
        assert_eq!(rig.live().metadata["refresh_token"], "r-refresh");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b34_b_a_parked_save_with_no_pass_in_flight_is_refused_as_moved() {
    // B-34 step 3, with no pass in flight and E/f present: the guard's origin
    // check is the one that refuses a save whose destination is not where live
    // now resolves. E's file is untouched, and the owed change is written to
    // the file live names. The missing-E/f variant is `b34_i_*`.
    // Mutation: dropping the origin check (the save would write E).
    for copy in [true] {
        bounded(async move {
            let rig = rig();
            rig.manager.register_unsaved(pending_b()).unwrap();
            let parked = park_update(&rig, copy, "parked");
            let before = std::fs::read(parked.file()).ok();
            let result = parked.release();
            assert!(result.is_ok(), "the update itself succeeds");
            let refused = deferrals(&rig);
            assert!(
                refused.contains(&Some(SaveDeferral::OriginMoved)),
                "copy={copy}: {refused:?}"
            );
            assert_eq!(
                std::fs::read(parked.file()).ok(),
                before,
                "copy={copy}: E's file is as it was"
            );
            assert_eq!(parked.file().exists(), copy, "no E/f was created");
            assert_eq!(disk_note(&rig), "parked", "the owed save wrote P");
            assert_eq!(owed(&rig.manager), None);
        })
        .await;
    }
}

// ---- B-38 (h): the one-executor-call source audit ----

/// The production prefix of `refresh.rs`: the text before its first
/// column-zero `#[cfg(test)]` item, line endings normalized.
fn refresh_prefix(src: &str) -> String {
    let src = src.replace("\r\n", "\n");
    src.split("\n#[cfg(test)]\n")
        .next()
        .expect("a prefix")
        .to_owned()
}

/// The ways the executor's refresh is reached: it must be exactly one call,
/// inside `send_refresh`, after the fence.
fn executor_refresh_calls(prefix: &str) -> (usize, usize, bool) {
    let code = code_only(prefix);
    let total = token_count(&code, ".refresh_with_options(");
    let bodies = fn_bodies(&code, |name| name == "send_refresh");
    assert_eq!(bodies.len(), 1, "one send_refresh");
    let body = bodies[0].1;
    let in_send = token_count(body, ".refresh_with_options(");
    let fenced = match (
        body.find("fence_send_locked("),
        body.find(".refresh_with_options("),
    ) {
        (Some(fence), Some(send)) => fence < send,
        _ => false,
    };
    (total, in_send, fenced)
}

#[test]
fn b38_h_the_executor_is_called_in_one_place_behind_the_fence() {
    // B-38 (h): a scan of the production source of refresh.rs.
    // Mutation: a second executor call or a send before the fence (both are built below and must be seen).
    let prefix = refresh_prefix(REFRESH_SOURCE);
    assert_eq!(executor_refresh_calls(&prefix), (1, 1, true));
    // Negative: a second call outside `send_refresh` is seen.
    let extra = mutated(
        &prefix,
        "    fn defer_refresh(",
        "    fn sneaky(&self, e: &Arc<dyn ProviderExecutor>, a: Arc<Auth>) { let _ = e.refresh_with_options(a, Default::default()); }\n    fn defer_refresh(",
    );
    assert_eq!(executor_refresh_calls(&extra).0, 2);
    // Negative: a send before the fence is seen.
    let early = mutated(
        &prefix,
        "match self.locked(|s| self.fence_send_locked(",
        "let _ = executor.refresh_with_options(Arc::clone(auth), options.clone());\n        match self.locked(|s| self.fence_send_locked(",
    );
    let (total, in_send, fenced) = executor_refresh_calls(&early);
    assert_eq!((total, in_send), (2, 2));
    assert!(!fenced, "an early send is not behind the fence");
}

// ---- B-38 (a), (b), (d)

#[tokio::test(flavor = "current_thread")]
async fn b38_d_an_unsettled_read_back_defers_every_entry_before_the_send() {
    // B-38 (b) and (d): with a `ReadBack` mark that no read can settle (the
    // store answers `Unsupported`), `force_refresh`, `force_refresh_all` and
    // the 401 path each defer before the executor, warn once for the
    // partition, set a 60 s to 30 min wait, and leave no hold.
    // Mutation: removing the ReadBackUnsettled refusal from the fence (the executor would be entered).
    bounded(async {
        let (rig, probe) = third_token_after_unconfirmed_landing().await;
        rig.store.lease_unsupported.store(true, Ordering::SeqCst);
        assert_eq!(mark(&rig.manager, true), (true, "read-back"));
        let started = chrono::Utc::now();
        let error = rig.manager.force_refresh(ID).await.unwrap_err();
        assert_store_error(error, "force_refresh");
        assert_eq!(probe.sent(), 1, "the executor was not called");
        assert!(rig.live().next_refresh_after.unwrap() >= started + chrono::Duration::seconds(60));
        assert!(wait_seconds(&rig) <= 1800, "{}", wait_seconds(&rig));
        assert_eq!(warned(&rig.manager), (true, false), "one warn for outer");
        assert_eq!(rig.manager.refresh_hold(ID), None, "no hold");
        assert_eq!(mark(&rig.manager, true), (true, "read-back"));
        // The other entries defer the same way and do not warn again.
        let results = rig.manager.force_refresh_all().await;
        assert!(results.iter().all(|r| r.error.is_some()), "all deferred");
        let unauthorized = ExecError::upstream(401, "unauthorized");
        assert!(
            rig.manager
                .try_refresh_after_unauthorized(&rig.live(), &unauthorized, false)
                .await
                .is_none()
        );
        assert_eq!(probe.sent(), 1, "still no executor call");
        assert_eq!(warned(&rig.manager), (true, false));
        // The scheduler's entry defers the same way.
        let error = rig.manager.refresh_at_epoch(ID, "", 0).await.unwrap_err();
        assert_store_error(error, "refresh_at_epoch");
        assert_eq!(probe.sent(), 1, "the scheduler entry sent nothing");
        assert_eq!(warned(&rig.manager), (true, false), "no second warn");
        // A token command ends it, and the next pass sends N.
        token_command(&rig, true, "n");
        assert_eq!(mark(&rig.manager, true), (false, "none"));
        assert_eq!(
            warned(&rig.manager),
            (false, false),
            "the warn flag cleared"
        );
        rig.manager.force_refresh(ID).await.unwrap();
        assert_eq!(probe.sent_refresh(probe.sent() - 1), "n-refresh");
    })
    .await;
}

// ---- B-38 (a), (c), (e), (f), (g), (i) and the read-back settle case

/// B-32 (c) steps 1 and 2 with the ordinary save failing: live holds B, outer
/// is marked `ReadBack`, the file holds C, and `final` is still pending.
async fn third_token_with_pending_final() -> (Rig, Arc<Probe>) {
    let rig = rig();
    let probe = Probe::new("codex", false);
    b_then_r(&probe);
    rig.manager.register_executor(probe.clone());
    quick_commit_retries(&rig);
    rig.store
        .fail_after_lease_commit
        .store(u32::MAX, Ordering::SeqCst);
    let store = Arc::downgrade(&rig.store);
    at_first_commit(&rig, move || {
        if let Some(store) = store.upgrade() {
            store.fail_lease_read.store(1, Ordering::SeqCst);
        }
    });
    let (path, store) = (rig.path(), Arc::downgrade(&rig.store));
    at_tail(&rig, move |_| {
        peer_write(&path, true, "c");
        if let Some(store) = store.upgrade() {
            store.fail_refresh.store(true, Ordering::SeqCst);
        }
    });
    rig.manager.force_refresh(ID).await.unwrap();
    *lock(&rig.store.before_lease_commit_hook) = None;
    *lock(&rig.manager.shared.lease_tail_hook) = None;
    rig.store.fail_after_lease_commit.store(0, Ordering::SeqCst);
    rig.store.fail_refresh.store(false, Ordering::SeqCst);
    assert_eq!(rig.disk()["refresh_token"], "c-refresh");
    assert_eq!(rig.live().metadata["refresh_token"], "b-refresh");
    assert_eq!(mark(&rig.manager, true), (true, "read-back"));
    assert_eq!(rig.live().codex_pending_refresh.len(), 1, "final is owed");
    (rig, probe)
}

/// B-32 (a)'s state with a third outer token in (c)'s read: the read shows
/// something else, so outer is marked `Different`. A peer then writes C.
async fn different_cause_state() -> Rig {
    let rig = rig();
    let probe = Probe::new("codex", false);
    b_then_r(&probe);
    rig.manager.register_executor(probe.clone());
    quick_commit_retries(&rig);
    rig.store
        .fail_after_lease_commit
        .store(u32::MAX, Ordering::SeqCst);
    let store = Arc::downgrade(&rig.store);
    at_first_commit(&rig, move || {
        let hook: LeaseReadHook = Arc::new(|map: &mut Map<String, Value>| {
            map.insert("access_token".into(), json!("c-access"));
            map.insert("refresh_token".into(), json!("c-refresh"));
        });
        if let Some(store) = store.upgrade() {
            *lock(&store.lease_read_hook) = Some(hook);
        }
    });
    let path = rig.path();
    at_tail(&rig, move |_| peer_write(&path, true, "c"));
    rig.manager.force_refresh(ID).await.unwrap();
    *lock(&rig.store.before_lease_commit_hook) = None;
    *lock(&rig.manager.shared.lease_tail_hook) = None;
    *lock(&rig.store.lease_read_hook) = None;
    rig.store.fail_after_lease_commit.store(0, Ordering::SeqCst);
    assert_eq!(mark(&rig.manager, true), (true, "different"));
    rig
}

/// Gives the record its OAuth classification back, with live's tokens.
fn restore_oauth(manager: &Manager) {
    let mut changed = (*manager.get(ID).unwrap()).clone();
    changed.metadata.insert("auth_kind".into(), json!("oauth"));
    changed.attributes.remove("source");
    manager.update(changed).unwrap();
}

/// A settings save that reads no file first (`update` would adopt it).
fn edit_note(manager: &Manager, dir: &Path, note: &str) {
    manager
        .edit(ID, |auth| {
            // The save goes where the record's `path` says.
            auth.attributes
                .insert("path".into(), dir.join(ID).display().to_string());
            auth.metadata.insert("note".into(), json!(note));
            Ok::<bool, ()>(true)
        })
        .unwrap();
}

/// A directory beside the rig's, to bind the store to.
fn other_dir(rig: &Rig) -> tempfile::TempDir {
    tempfile::tempdir_in(rig.dir.path().parent().unwrap()).unwrap()
}

async fn expect_unsettled(rig: &Rig, probe: &Probe, what: &str) {
    let error = rig.manager.force_refresh(ID).await.unwrap_err();
    assert_store_error(error, what);
    assert_eq!(probe.sent(), 1, "{what}: the executor saw A only");
    assert_eq!(probe.sent_refresh(0), "outer-refresh-old");
    assert_eq!(warned(&rig.manager), (true, false), "{what}: one warn");
    assert_eq!(landing(&rig.manager), (false, false), "{what}: not 4c");
    assert_eq!(rig.manager.refresh_hold(ID), None, "{what}: no hold");
    assert_eq!(mark(&rig.manager, true), (true, "read-back"));
    assert!(wait_seconds(rig) >= 59 && wait_seconds(rig) <= 1800);
}

#[tokio::test(flavor = "current_thread")]
async fn b38_a_a_forced_ineligible_pass_is_refused_by_the_send_fence() {
    // B-38 (a): outer is marked `ReadBack` and the record then becomes a
    // config-sourced API key that keeps its tokens. The forced pass takes
    // step 1's ineligible branch, and the fence refuses with
    // `ReadBackUnsettled`; the 401 path gives the same. Mutation:
    // the fence skipping marks on an ineligible pass (it would send B, which
    // a peer already rotated), or the deferral not being the unsettled one
    // (`warned` is set only by it; `landing` only by 4c).
    bounded(async {
        let (rig, probe) = third_token_after_unconfirmed_landing().await;
        make_ineligible(&rig.manager, "ineligible");
        expect_unsettled(&rig, &probe, "force_refresh").await;
        let unauthorized = ExecError::upstream(401, "unauthorized");
        assert!(
            rig.manager
                .try_refresh_after_unauthorized(&rig.live(), &unauthorized, false)
                .await
                .is_none()
        );
        assert_eq!(probe.sent(), 1, "the 401 path sent nothing");
        assert_eq!(probe.reused(), 0);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b38_c_the_ineligibility_discard_keeps_the_cause_and_4c_then_holds() {
    // B-38 (c): the discard on becoming ineligible keeps the `ReadBack` cause
    // (v10 relabelled it `Different`); the fence refuses while ineligible; the
    // restored record's next leased pass finds C at 4c and holds. Mutation: the discard relabelling the cause (4c would skip the mark and
    // the pass would send B, already rotated by the peer to C).
    bounded(async {
        let (rig, probe) = third_token_with_pending_final().await;
        make_ineligible(&rig.manager, "ineligible");
        assert_eq!(
            mark(&rig.manager, true),
            (true, "read-back"),
            "the discard kept the cause"
        );
        assert!(
            rig.live().codex_pending_refresh.is_empty(),
            "the discard dropped the pending work"
        );
        expect_unsettled(&rig, &probe, "ineligible").await;
        restore_oauth(&rig.manager);
        assert_eq!(mark(&rig.manager, true), (true, "read-back"));
        let error = rig.manager.force_refresh(ID).await.unwrap_err();
        assert_store_error(error, "landing unconfirmed");
        assert_eq!(landing(&rig.manager), (true, false));
        assert_eq!(
            rig.manager.refresh_hold(ID),
            Some((RefreshHold::LandingUnconfirmed, HeldPartitions::Outer))
        );
        assert_eq!(probe.sent(), 1, "A only: B was never sent");
        assert_eq!(probe.reused(), 0);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b38_e_a_different_proof_at_4c_keeps_the_cause_and_holds() {
    // B-38 (e): the store is bound to a copy of the file, so 4c's proof of the
    // mark's origin is `Different`. The cause stays `ReadBack`, the landing
    // hold is set and nothing is sent. Mutation: a `Different` proof
    // relabelling the cause (the next eligible pass would send B).
    bounded(async {
        let (rig, probe) = third_token_after_unconfirmed_landing().await;
        let other = other_dir(&rig);
        std::fs::copy(rig.path(), other.path().join(ID)).unwrap();
        rig.store.files.set_base_dir(other.path());
        let error = rig.manager.force_refresh(ID).await.unwrap_err();
        assert_store_error(error, "landing unconfirmed");
        assert_eq!(mark(&rig.manager, true), (true, "read-back"));
        assert_eq!(landing(&rig.manager), (true, false));
        assert_eq!(probe.sent(), 1, "A only");
        assert_eq!(probe.reused(), 0);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b38_f_the_confirmation_read_is_the_only_one_after_the_last_attempt() {
    // B-38 (f): exactly one `codex_lease_read` follows the last commit
    // attempt (the confirming read) and publication makes none; publication's
    // result is B-32 (a) step 1's. That `Outcome::Exhausted`'s map equals that
    // read is not observable at manager level; a second read would be.
    // Mutation: publication (or the tail) reading the file again.
    bounded(async {
        let rig = rig();
        let probe = Probe::new("codex", false);
        b_then_r(&probe);
        rig.manager.register_executor(probe.clone());
        quick_commit_retries(&rig);
        rig.store
            .fail_after_lease_commit
            .store(u32::MAX, Ordering::SeqCst);
        let reads = Arc::new(Mutex::new(Vec::new()));
        let (log, store) = (reads.clone(), Arc::downgrade(&rig.store));
        let hook: LeaseReadHook = Arc::new(move |_: &mut Map<String, Value>| {
            if let Some(store) = store.upgrade() {
                lock(&log).push(store.lease_commit_attempts.load(Ordering::SeqCst));
            }
        });
        *lock(&rig.store.lease_read_hook) = Some(hook);
        let seen = Arc::new(Mutex::new(None));
        let (log, manager) = (seen.clone(), rig.manager.clone());
        at_tail(&rig, move |_| {
            *lock(&log) = Some(view(&manager));
        });
        rig.manager.force_refresh(ID).await.unwrap();
        let attempts = rig.store.lease_commit_attempts.load(Ordering::SeqCst);
        let reads = lock(&reads).clone();
        assert!(attempts >= 1);
        assert_eq!(
            reads.iter().filter(|at| **at == attempts).count(),
            1,
            "one read after the last attempt: {reads:?}"
        );
        assert_eq!(reads.len(), 2, "step 3's read and the confirming read");
        assert_eq!(
            lock(&seen).take().expect("the tail hook ran"),
            View {
                outer: (false, "none"),
                siwc: (false, "none"),
                pending: vec![],
                intent_written: Some(true),
            }
        );
        assert_eq!(rig.disk()["refresh_token"], "b-refresh");
    })
    .await;
}

/// From B-32 (a)'s `Different` mark on outer, a later pass whose commit lands
/// (a peer put B back in the file just before it) and reports an error on
/// every attempt, so the confirming read decides what
/// publication clears. `read_hook` shapes the maps `codex_lease_read` returns
/// and is given the commit attempts made by this pass (setup's own commits
/// are not counted). Returns outer's mark as the tail
/// sees it.
async fn exhausted_pass_over_a_different_mark(
    read_hook: impl Fn(&mut Map<String, Value>, u32) + Send + Sync + 'static,
) -> (bool, &'static str) {
    let rig = different_cause_state().await;
    rig.store
        .fail_after_lease_commit
        .store(u32::MAX, Ordering::SeqCst);
    let store = Arc::downgrade(&rig.store);
    let base = rig.store.lease_commit_attempts.load(Ordering::SeqCst);
    let hook: LeaseReadHook = Arc::new(move |map: &mut Map<String, Value>| {
        if let Some(store) = store.upgrade() {
            let attempts = store.lease_commit_attempts.load(Ordering::SeqCst) - base;
            read_hook(map, attempts);
        }
    });
    *lock(&rig.store.lease_read_hook) = Some(hook);
    // The peer's C is replaced by B just before the commit, so the commit's
    // baseline (B) holds and R lands.
    let path = rig.path();
    at_first_commit(&rig, move || peer_write(&path, true, "b"));
    let seen = Arc::new(Mutex::new(None));
    let (log, manager) = (seen.clone(), rig.manager.clone());
    at_tail(&rig, move |_| *lock(&log) = Some(mark(&manager, true)));
    rig.manager.force_refresh(ID).await.unwrap();
    lock(&seen).take().expect("the tail hook ran")
}

#[tokio::test(flavor = "current_thread")]
async fn b38_f_the_confirming_map_clears_a_mark_the_exhausted_commit_landed_over() {
    // B-38 (f): outer is marked `Different`; the next pass's commit lands R
    // but errors on every attempt, so the outcome is Exhausted and the
    // confirming read's map is all publication has. It shows R, so it
    // verifies outer and the tail sees the mark cleared.
    // Mutation: `Outcome::Exhausted`'s map replaced by `None` (publication
    // would have no disk to verify against and the mark would stay).
    bounded(async {
        let mark = exhausted_pass_over_a_different_mark(|_, _| {}).await;
        assert_eq!(mark, (false, "none"), "the confirming map cleared it");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b38_f_a_confirming_map_with_another_field_unequal_keeps_the_mark() {
    // B-38 (f) control: only the confirming read (once this pass has made a
    // commit attempt; setup's reads are not touched) shows R but one more
    // token field unequal to the pass's final metadata, so the partition is
    // not verified and keeps its mark. Mutation: verification against the
    // pass's own final metadata instead of the map (the mark would clear).
    bounded(async {
        let mark = exhausted_pass_over_a_different_mark(|map, attempts| {
            if attempts >= 1 {
                map.insert("expired".into(), json!("2098-01-01T00:00:00Z"));
            }
        })
        .await;
        assert_eq!(mark, (true, "different"), "the unequal field kept it");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b38_g_a_replace_observation_preserves_owed_policy_and_adopts_other_fields() {
    // Round 3 design B: the unsaved operator note remains owed while the
    // observation adopts the file's other fields. Step 14 persists the merged
    // record, preserving PR129's refresh and refusal bookkeeping.
    bounded(async {
        let rig = rig();
        let (manager, path, files) = (rig.manager.clone(), rig.path(), rig.store.files.clone());
        pass_with_tail(&rig, move || {
            set_note(&manager, "owed").unwrap();
            assert!(owed(&manager).is_some(), "refused while the pass runs");
            edit_file(&path, |file| {
                file.insert("note".into(), json!("file"));
            });
            let hint = (*manager.get(ID).unwrap()).clone();
            observed(&manager, &files, &hint);
        })
        .await;
        assert_eq!(rig.live().metadata["note"], "owed");
        assert_eq!(
            disk_note(&rig),
            "owed",
            "step 14 wrote the owed operator note"
        );
        assert_eq!(owed(&rig.manager), None, "the flush cleared the flag");
        assert_eq!(rig.disk()["refresh_token"], "r-refresh");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b38_i_a_read_back_partition_is_withheld_from_saves() {
    // B-38 (i): with outer marked `ReadBack` a settings save keeps the
    // destination's outer keys, and defers when the destination is missing;
    // with the cause `Different` it creates the file from live as today.
    // Mutation: dropping the `ReadBack` partition from the withheld set
    // (the missing file would be created holding B, E's outer would become B).
    bounded(async {
        // No E/f: MissingFile, nothing created, the change is owed.
        let (rig, _probe) = third_token_after_unconfirmed_landing().await;
        let other = other_dir(&rig);
        rig.store.files.set_base_dir(other.path());
        edit_note(&rig.manager, other.path(), "changed");
        assert_eq!(
            deferrals(&rig).last(),
            Some(&Some(SaveDeferral::MissingFile))
        );
        assert!(!other.path().join(ID).exists(), "no E/f was created");
        assert!(owed(&rig.manager).is_some(), "the change is owed");
        // E/f holds outer A: written, and E's outer keys are A's exactly.
        std::fs::copy(rig.path(), other.path().join(ID)).unwrap();
        edit_file(&other.path().join(ID), |file| {
            file.insert("access_token".into(), json!("outer-old"));
            file.insert("refresh_token".into(), json!("outer-refresh-old"));
        });
        edit_note(&rig.manager, other.path(), "changed-again");
        assert_eq!(deferrals(&rig).last(), Some(&None), "written");
        let written: Value =
            serde_json::from_slice(&std::fs::read(other.path().join(ID)).unwrap()).unwrap();
        assert_eq!(written["refresh_token"], "outer-refresh-old");
        assert_eq!(written["access_token"], "outer-old");
        assert_eq!(written["note"], "changed-again");
        // Control: cause `Different` creates E/f with live's outer B.
        let rig = different_cause_state().await;
        let other = other_dir(&rig);
        rig.store.files.set_base_dir(other.path());
        edit_note(&rig.manager, other.path(), "changed");
        assert_eq!(deferrals(&rig).last(), Some(&None), "written");
        let created: Value =
            serde_json::from_slice(&std::fs::read(other.path().join(ID)).unwrap()).unwrap();
        assert_eq!(created["refresh_token"], "b-refresh");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b38_settle_equal_identity_with_another_field_keeps_the_read_back_cause() {
    // The read-back settle case: the file holds B (live's identity) but another
    // token field differs, so 4c neither clears the mark nor changes the
    // cause, and the send fence defers with `ReadBackUnsettled`. Mutation: 4c clearing on identity alone (it would send B, whose landing
    // was never verified), or the fence ignoring the surviving mark.
    bounded(async {
        let rig = rig();
        let probe = Probe::new("codex", false);
        b_then_r(&probe);
        rig.manager.register_executor(probe.clone());
        quick_commit_retries(&rig);
        rig.store
            .fail_after_lease_commit
            .store(u32::MAX, Ordering::SeqCst);
        let store = Arc::downgrade(&rig.store);
        at_first_commit(&rig, move || {
            if let Some(store) = store.upgrade() {
                store.fail_lease_read.store(1, Ordering::SeqCst);
            }
        });
        // After the tail's save wrote B, another field of the file moves.
        let path = rig.path();
        *lock(&rig.store.save_hook) = Some(Arc::new(move |_: &Auth| {
            edit_file(&path, |file| {
                file.insert("expired".into(), json!("2098-01-01T00:00:00Z"));
            });
        }));
        rig.manager.force_refresh(ID).await.unwrap();
        *lock(&rig.store.save_hook) = None;
        *lock(&rig.store.before_lease_commit_hook) = None;
        rig.store.fail_after_lease_commit.store(0, Ordering::SeqCst);
        assert_eq!(rig.disk()["refresh_token"], "b-refresh");
        assert_eq!(mark(&rig.manager, true), (true, "read-back"));
        expect_unsettled(&rig, &probe, "unsettled").await;
        assert_eq!(rig.disk()["refresh_token"], "b-refresh");
    })
    .await;
}

// ---- B-39 (b), (c), (d)

/// Arms the send fence's pause and spawns a forced pass; returns the pause
/// (awaited with `0`, released with `1`) and the pass's handle.
async fn paused_pass(rig: &Rig) -> (Pause, PassHandle) {
    paused_pass_of(rig, ID).await
}

type Pause = Arc<(tokio::sync::Notify, tokio::sync::Notify)>;
type PassHandle = tokio::task::JoinHandle<Result<Arc<Auth>, ManagerError>>;

/// `paused_pass` for the credential `id`.
async fn paused_pass_of(rig: &Rig, id: &'static str) -> (Pause, PassHandle) {
    let pause = Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
    *lock(&rig.manager.shared.send_fence_pause) = Some(pause.clone());
    let manager = rig.manager.clone();
    let pass = tokio::spawn(async move { manager.force_refresh(id).await });
    pause.0.notified().await;
    (pause, pass)
}

/// Releases a paused pass and takes the pause down.
fn release_pause(rig: &Rig, pause: &Pause) {
    pause.1.notify_one();
    *lock(&rig.manager.shared.send_fence_pause) = None;
}

#[tokio::test(flavor = "current_thread")]
async fn b39_b_a_token_command_during_the_leased_pause_supersedes_the_send() {
    // B-39 (b): a peer rotated outer to B; the pass adopts it and pauses at
    // the fence still holding the lease. An outer token command installs N
    // (a raw write, as the store's own would wait on the lease). The fence
    // finds N against the snapshot's B: nothing is sent, the call returns
    // live with N, and the lease is free for the next pass.
    // Mutation: the fence not comparing the snapshot's refresh token (A or B would be sent after the command).
    bounded(async {
        let rig = rig();
        let probe = Probe::new("codex", false).rotating("r");
        rig.manager.register_executor(probe.clone());
        peer_write(&rig.path(), true, "b");
        let (pause, pass) = paused_pass(&rig).await;
        token_command(&rig, true, "n");
        pause.1.notify_one();
        *lock(&rig.manager.shared.send_fence_pause) = None;
        let live = pass.await.unwrap().expect("superseded is Ok");
        assert_eq!(live.metadata["refresh_token"], "n-refresh");
        assert_eq!(probe.sent(), 0, "nothing was sent");
        assert_eq!(rig.live().metadata["refresh_token"], "n-refresh");
        // The lease token was dropped: a lock on the file succeeds at once.
        let lock_now = crate::auth::credential_lock::acquire_sync(&rig.path());
        assert!(lock_now.is_ok(), "the lease was released");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b39_c_a_changed_registration_at_the_pause_sends_nothing() {
    // B-39 (c): a re-registration (new epoch) and a bare removal both give
    // the "registration changed" error and send nothing.
    // Mutation: the fence not checking the registration epoch (a removed or replaced record would be sent).
    for remove_only in [false, true] {
        bounded(async move {
            let rig = rig();
            let probe = Probe::new("codex", false).rotating("r");
            rig.manager.register_executor(probe.clone());
            peer_write(&rig.path(), true, "b");
            let (pause, pass) = paused_pass(&rig).await;
            rig.manager.remove(ID);
            if !remove_only {
                rig.manager.register_unsaved(fixture()).unwrap();
            }
            pause.1.notify_one();
            *lock(&rig.manager.shared.send_fence_pause) = None;
            let error = pass.await.unwrap().unwrap_err();
            assert!(
                error.to_string().contains("registration changed"),
                "remove_only={remove_only}: {error}"
            );
            assert_eq!(probe.sent(), 0, "nothing was sent");
        })
        .await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn b39_d_no_change_at_the_pause_sends_once() {
    // B-39 (d): with no action at the pause the fence says `Send`, and the
    // executor is entered once with the adopted B.
    // Mutation: the fence refusing without a cause (the plain send would not happen).
    bounded(async {
        let rig = rig();
        let probe = Probe::new("codex", false).rotating("r");
        rig.manager.register_executor(probe.clone());
        peer_write(&rig.path(), true, "b");
        let (pause, pass) = paused_pass(&rig).await;
        pause.1.notify_one();
        *lock(&rig.manager.shared.send_fence_pause) = None;
        pass.await.unwrap().unwrap();
        assert_eq!(probe.sent(), 1);
        assert_eq!(probe.sent_refresh(0), "b-refresh");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b39_d_an_ineligible_pass_with_no_change_at_the_pause_sends_once() {
    // B-39 (d), the ineligible route: the same pause with nothing changed
    // sends once. Mutation: the fence refusing an ineligible pass without a
    // cause (nothing would be sent).
    bounded(async {
        let (rig, probe) = ineligible_rig();
        let (pause, pass) = paused_pass(&rig).await;
        released(&rig, &pause, pass).await.unwrap();
        assert_eq!(probe.sent(), 1, "the executor was entered once");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b39_a2_an_outer_token_command_clears_the_cause_at_the_pause() {
    // B-39 (a2): from the `ReadBack` state with no lease to settle it, the
    // fence would defer `ReadBackUnsettled`; an outer token command at the
    // pause clears the cause and installs N, so the fence returns live with
    // N and nothing is sent.
    // Mutation: the fence checking the cause from the snapshot instead of live (the command's clear would be missed).
    bounded(async {
        let (rig, probe) = third_token_after_unconfirmed_landing().await;
        rig.store.lease_unsupported.store(true, Ordering::SeqCst);
        assert_eq!(mark(&rig.manager, true), (true, "read-back"));
        let (pause, pass) = paused_pass(&rig).await;
        token_command(&rig, true, "n");
        pause.1.notify_one();
        *lock(&rig.manager.shared.send_fence_pause) = None;
        let live = pass.await.unwrap().expect("superseded is Ok");
        assert_eq!(live.metadata["refresh_token"], "n-refresh");
        assert_eq!(probe.sent(), 1, "nothing more was sent");
        assert_eq!(mark(&rig.manager, true), (false, "none"));
    })
    .await;
}

/// Makes the record a config-sourced API key that keeps its tokens and its
/// `use_siwc`, so SIWC stays the active partition.
fn make_ineligible_keeping_siwc(manager: &Manager) {
    let mut changed = (*manager.get(ID).unwrap()).clone();
    changed.metadata.insert("auth_kind".into(), json!("apikey"));
    changed
        .attributes
        .insert("source".into(), "config:openai[0]".into());
    changed.attributes.remove("source_backend");
    manager.update(changed).unwrap();
}

fn siwc_field(auth: &Auth, key: &str) -> Value {
    auth.metadata["siwc"][key].clone()
}

async fn released(rig: &Rig, pause: &Pause, pass: PassHandle) -> Result<Arc<Auth>, ManagerError> {
    release_pause(rig, pause);
    pass.await.unwrap()
}

/// Arms the fence pause and returns it.
fn arm_pause(rig: &Rig) -> Pause {
    let pause = Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
    *lock(&rig.manager.shared.send_fence_pause) = Some(pause.clone());
    pause
}

/// A fresh ineligible record with SIWC active and a rotating executor.
fn ineligible_rig() -> (Rig, Arc<Probe>) {
    let rig = rig();
    let probe = Probe::new("codex", false).rotating("r");
    rig.manager.register_executor(probe.clone());
    make_ineligible_keeping_siwc(&rig.manager);
    (rig, probe)
}

#[tokio::test(flavor = "current_thread")]
async fn b39_a_a_siwc_token_command_supersedes_an_ineligible_pass() {
    // B-39 (a): the record is a config-sourced API key keeping its tokens.
    // The forced pass pauses at the fence, a SIWC token command installs N,
    // and the snapshot's SIWC refresh token no longer matches live's: the call
    // returns live with N and nothing is sent. Mutation: the snapshot
    // comparison being skipped on the ineligible branch (it would send the old
    // SIWC token, already replaced).
    bounded(async {
        let (rig, probe) = ineligible_rig();
        let (pause, pass) = paused_pass(&rig).await;
        token_command(&rig, false, "n");
        let live = released(&rig, &pause, pass)
            .await
            .expect("superseded is Ok");
        assert_eq!(siwc_field(&live, "refresh_token"), json!("n-refresh"));
        assert_eq!(probe.sent(), 0, "nothing was sent");
        assert_eq!(rig.live().metadata["siwc"]["refresh_token"], "n-refresh");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b39_g_a_401_call_when_only_the_inactive_partition_changed() {
    // B-39 (g): `use_siwc` on, an ineligible record. (1) An outer command at
    // the pause: a 401 call for SIWC's access token finds that token still
    // live, so it errors and `try_refresh_after_unauthorized` gives `None`.
    // (2) A SIWC command installing a new access token returns live.
    // (3) `force_refresh` (no failed token) returns live on an outer command.
    // Mutation: the superseded branch answering with a record whose
    // active access token is the rejected one (the caller would retry with it).
    bounded(async {
        // (1a) refresh_at_epoch with the active token T.
        let (rig, probe) = ineligible_rig();
        let pause = arm_pause(&rig);
        let manager = rig.manager.clone();
        let pass = tokio::spawn(async move { manager.refresh_at_epoch(ID, "siwc-old", 0).await });
        pause.0.notified().await;
        token_command(&rig, true, "n");
        let error = released(&rig, &pause, pass).await.unwrap_err();
        assert!(
            error.to_string().contains("auth changed before refresh"),
            "{error}"
        );
        assert_eq!(probe.sent(), 0);
        // (1b) the 401 path gives `None`.
        let (rig, probe) = ineligible_rig();
        let pause = arm_pause(&rig);
        let (manager, auth) = (rig.manager.clone(), rig.live());
        let pass = tokio::spawn(async move {
            manager
                .try_refresh_after_unauthorized(&auth, &ExecError::upstream(401, "no"), false)
                .await
        });
        pause.0.notified().await;
        token_command(&rig, true, "n");
        release_pause(&rig, &pause);
        assert!(pass.await.unwrap().is_none(), "the 401 path gives None");
        assert_eq!(probe.sent(), 0);
        // (2) a SIWC command installs a new access token.
        let (rig, probe) = ineligible_rig();
        let pause = arm_pause(&rig);
        let manager = rig.manager.clone();
        let pass = tokio::spawn(async move { manager.refresh_at_epoch(ID, "siwc-old", 0).await });
        pause.0.notified().await;
        token_command(&rig, false, "t");
        let live = released(&rig, &pause, pass).await.expect("live answers");
        assert_eq!(siwc_field(&live, "access_token"), json!("t-access"));
        assert_eq!(probe.sent(), 0);
        // (3) no failed token: an outer-only command still returns live.
        let (rig, probe) = ineligible_rig();
        let (pause, pass) = paused_pass(&rig).await;
        token_command(&rig, true, "n");
        let live = released(&rig, &pause, pass).await.expect("live answers");
        assert_eq!(live.metadata["refresh_token"], "n-refresh");
        assert_eq!(probe.sent(), 0);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b39_e_a_non_codex_record_is_sent_whatever_changes_at_the_pause() {
    // B-39 (e): the fence returns `Send` for a record that is not Codex without
    // reading the entry, so a refresh-token change at the pause does not stop
    // the send, which carries the snapshot's token. Mutation: the
    // snapshot comparison running for every provider (it would not send).
    bounded(async {
        let rig = rig();
        let probe = Probe::new("claude", false).rotating("r");
        rig.manager.register_executor(probe.clone());
        let id = "claude.json";
        let claude = Auth {
            id: id.into(),
            file_name: id.into(),
            provider: "claude".into(),
            metadata: [
                ("access_token".to_owned(), json!("claude-old")),
                ("refresh_token".to_owned(), json!("claude-refresh-old")),
            ]
            .into_iter()
            .collect(),
            ..Auth::default()
        };
        rig.manager.register_unsaved(claude).unwrap();
        let (pause, pass) = paused_pass_of(&rig, "claude.json").await;
        let mut changed = (*rig.manager.get(id).unwrap()).clone();
        changed
            .metadata
            .insert("refresh_token".into(), json!("claude-refresh-new"));
        rig.manager.update_unsaved(changed).unwrap();
        released(&rig, &pause, pass).await.unwrap();
        assert_eq!(probe.sent(), 1, "the fence let the send through");
        assert_eq!(probe.sent_refresh(0), "claude-refresh-old");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b39_f_a_held_record_with_a_stale_snapshot_still_defers_unsettled() {
    // B-39 (f): outer is marked `ReadBack` (no lease to settle it); at the
    // pause live's outer refresh token changes while the cause stays. The
    // fence reports `ReadBackUnsettled`, not `Superseded` (its order), so the
    // call is a store error with the unsettled backoff. Mutation: the
    // snapshot comparison running before the mark checks (the call would
    // return Ok and the mark's backoff would never apply).
    bounded(async {
        let (rig, probe) = third_token_after_unconfirmed_landing().await;
        rig.store.lease_unsupported.store(true, Ordering::SeqCst);
        let (pause, pass) = paused_pass(&rig).await;
        {
            let mut state = rig.manager.lock();
            let entry = state.auths.get_mut(ID).unwrap();
            Arc::make_mut(&mut entry.auth)
                .metadata
                .insert("refresh_token".into(), json!("z-refresh"));
        }
        let error = released(&rig, &pause, pass).await.unwrap_err();
        assert_store_error(error, "unsettled");
        assert_eq!(probe.sent(), 1, "A only");
        assert_eq!(warned(&rig.manager), (true, false));
        assert_eq!(rig.manager.refresh_hold(ID), None);
        assert_eq!(mark(&rig.manager, true), (true, "read-back"));
        assert!(wait_seconds(&rig) >= 59 && wait_seconds(&rig) <= 1800);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b36_a_a_parked_save_is_written_before_the_pass_may_send() {
    // B-36 (a): a save holds E's credential lock past stage G (blocked at the
    // lock's "read" event) when the store is bound back and a forced pass is
    // started. The pass must not enter the executor while the save runs; once
    // the save is released it writes E/f, and only then does the pass send.
    // The explicit `path` is what lets the save be admitted to E/f at all;
    // the refresh lock is therefore asserted directly, because the pass would
    // also wait on E's credential lock.
    // Mutation: check (vi) not keeping the refresh lock across the write
    // (`Ok(guard) => drop(guard)`): the lock is free while the save is parked.
    bounded(async {
        let rig = rig();
        let probe = Probe::new("codex", false);
        b_then_r(&probe);
        rig.manager.register_executor(probe.clone());
        rig.manager.force_refresh(ID).await.unwrap();
        assert_eq!(rig.disk()["refresh_token"], "b-refresh");
        let other = other_dir(&rig);
        let file = other.path().join(ID);
        rig.store.files.set_base_dir(other.path());
        let (reached_tx, reached) = std::sync::mpsc::channel();
        let (go_tx, go) = std::sync::mpsc::channel::<()>();
        let (reached_tx, go) = (Mutex::new(reached_tx), Mutex::new(go));
        crate::auth::credential_lock::set_test_hook(
            &file,
            Some(Arc::new(move |event: &str| {
                if event == "read" {
                    let _ = lock(&reached_tx).send(());
                    let _ = lock(&go).recv();
                }
            })),
        );
        let (manager, dir) = (rig.manager.clone(), other.path().to_path_buf());
        let save = std::thread::spawn(move || edit_note(&manager, &dir, "parked"));
        reached
            .recv_timeout(Duration::from_secs(10))
            .expect("the save reached E's read");
        crate::auth::credential_lock::set_test_hook(&file, None);
        let refresh_lock = lock(&rig.manager.shared.refresh_locks)
            .get(ID)
            .cloned()
            .expect("the id has a refresh lock");
        assert!(
            refresh_lock.try_lock_owned().is_err(),
            "the parked save holds the refresh lock from check (vi) to its write"
        );
        rig.store.files.set_base_dir(rig.dir.path());
        let existed_at_send = Arc::new(Mutex::new(None));
        let (log, watched) = (existed_at_send.clone(), file.clone());
        probe.set_at_entry(move |_| *lock(&log) = Some(watched.exists()));
        let sent_before = probe.sent();
        let manager = rig.manager.clone();
        let mut pass = tokio::spawn(async move { manager.force_refresh(ID).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(200), &mut pass)
                .await
                .is_err(),
            "the pass waits for the parked save"
        );
        assert_eq!(probe.sent(), sent_before, "nothing was sent meanwhile");
        go_tx.send(()).unwrap();
        save.join().expect("the save did not panic");
        pass.await.unwrap().unwrap();
        assert_eq!(probe.sent(), sent_before + 1);
        assert_eq!(
            *lock(&existed_at_send),
            Some(true),
            "the save wrote E/f before the send"
        );
        assert_eq!(probe.sent_refresh(sent_before), "b-refresh");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b34_ii_a_parked_save_bound_back_to_its_directory_is_still_refused() {
    // B-34 (ii): E is the store's directory again when the parked save is
    // released at the tail, so its origin is not moved; check (vi) still
    // refuses it for the pass in flight. E keeps its bytes, P gets R and the
    // change. Mutation: (vi) being skipped when the origin matches (the
    // save would write E/f with live's B over E's A).
    bounded(async {
        let rig = rig();
        let probe = Probe::new("codex", false).rotating("r");
        rig.manager.register_executor(probe.clone());
        rig.manager.register_unsaved(pending_b()).unwrap();
        let parked = Arc::new(park_update(&rig, true, "parked"));
        let before = std::fs::read(parked.file()).unwrap();
        let (release, here) = (parked.clone(), rig.dir.path().to_path_buf());
        let store = rig.store.clone();
        at_tail(&rig, move |_| {
            store.files.set_base_dir(release.other.path());
            let _ = release.release();
            store.files.set_base_dir(&here);
        });
        rig.manager.force_refresh(ID).await.unwrap();
        *lock(&rig.manager.shared.lease_tail_hook) = None;
        assert!(
            deferrals(&rig).contains(&Some(SaveDeferral::RefreshInFlight)),
            "{:?}",
            deferrals(&rig)
        );
        assert_eq!(std::fs::read(parked.file()).unwrap(), before);
        let in_e: Value = serde_json::from_slice(&before).unwrap();
        assert_eq!(in_e["refresh_token"], "outer-refresh-old", "never B");
        assert_eq!(rig.disk()["refresh_token"], "r-refresh");
        assert_eq!(disk_note(&rig), "parked");
        assert_eq!(probe.sent(), 1);
        assert_eq!(probe.sent_refresh(0), "b-refresh");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b34_iii_a_parked_save_over_a_file_holding_live_is_refused_by_the_pass() {
    // B-34 (iii): no operation is pending. An ordinary first refresh has
    // landed B, E/f does not exist, and a settings-only save is parked on E.
    // A second pass runs; released at the tail with E the store's directory,
    // the save is refused (a pass is in flight), so it never creates E/f with
    // live's B, and P gets R and the change. With no pending work, only
    // check (vi) keeps the save out. Mutation: check (vi) removed (the parked
    // save would create E/f holding B during the pass).
    bounded(async {
        let rig = rig();
        let probe = Probe::new("codex", false);
        b_then_r(&probe);
        rig.manager.register_executor(probe.clone());
        rig.manager.force_refresh(ID).await.unwrap();
        assert_eq!(rig.disk()["refresh_token"], "b-refresh");
        assert!(
            rig.live().codex_pending_refresh.is_empty(),
            "no pending work"
        );
        // The record names its file by the store's directory, as a record
        // with no explicit path does, so the save follows the store to E.
        rig.manager
            .edit_unsaved(ID, |auth| {
                auth.attributes.remove("path");
                Ok::<bool, ()>(true)
            })
            .unwrap();
        lock(&rig.store.guarded).clear();
        let parked = Arc::new(park_save(&rig, false, "parked", true));
        assert!(!parked.file().exists(), "E/f is absent before the release");
        let (release, here) = (parked.clone(), rig.dir.path().to_path_buf());
        let store = rig.store.clone();
        at_tail(&rig, move |_| {
            store.files.set_base_dir(release.other.path());
            let _ = release.release();
            store.files.set_base_dir(&here);
        });
        rig.manager.force_refresh(ID).await.unwrap();
        *lock(&rig.manager.shared.lease_tail_hook) = None;
        assert_eq!(
            deferrals(&rig).first(),
            Some(&Some(SaveDeferral::RefreshInFlight))
        );
        let parked_generation = deferrals_with_generation(&rig)[0].0;
        assert!(
            !lock(&rig.store.writes).contains(&parked_generation),
            "the parked save wrote nothing"
        );
        assert!(!parked.file().exists(), "E/f was never created");
        assert_eq!(rig.disk()["refresh_token"], "r-refresh");
        assert_eq!(disk_note(&rig), "parked", "the owed change reached P");
        assert_eq!(probe.sent(), 2);
        assert_eq!(probe.sent_refresh(1), "b-refresh");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b34_i_a_parked_settings_save_with_no_file_in_e_is_refused_as_moved() {
    // B-34 (i): E/f is missing and no pass is in flight. A settings-only edit
    // with no explicit path parks on E's credential lock, which it takes
    // before it reads the file. Released with the store back on D, the guard's
    // origin check refuses it: E/f is never created and the owed change is
    // written to the file live names. Mutation: dropping the origin check (the
    // save would create E/f).
    bounded(async {
        let rig = rig();
        rig.manager.register_unsaved(pending_b()).unwrap();
        let parked = park_save(&rig, false, "parked", true);
        assert!(!parked.file().exists(), "E/f is missing");
        let result = parked.release();
        assert!(result.is_ok(), "the edit itself succeeds");
        let refused = deferrals(&rig);
        assert!(
            refused.contains(&Some(SaveDeferral::OriginMoved)),
            "{refused:?}"
        );
        assert!(!parked.file().exists(), "no E/f was created");
        assert_eq!(disk_note(&rig), "parked", "the owed save wrote P");
        assert_eq!(owed(&rig.manager), None);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn stale_a_save_whose_base_live_moved_past_is_refused_with_no_pass_in_flight() {
    // Manager-level `Stale` (v): a settings-only save has passed its early
    // generation check when, inside the store, a one-shot hook raw-writes
    // peer N and installs N in live. `edit_unsaved` stands in for live moving
    // past the save's base; this pins the guard's version check. With no pass
    // in flight the guard answers `Stale` and the refused save writes
    // nothing (the owed flush that follows is a separate write). No reload
    // is queued and nothing is owed when the hook runs, so the held save
    // mutex is not waited for.
    // Mutation: the guard's version check removed (the save would write the
    // note and a stale base).
    bounded(async {
        let rig = rig();
        assert_eq!(owed(&rig.manager), None);
        let (manager, path) = (rig.manager.clone(), rig.path());
        let peer_bytes = Arc::new(Mutex::new(None));
        let (armed, kept) = (AtomicBool::new(true), peer_bytes.clone());
        *lock(&rig.store.before_save_hook) = Some(Arc::new(move |_: &Auth| {
            if armed.swap(false, Ordering::SeqCst) {
                peer_write(&path, true, "n");
                *lock(&kept) = Some(std::fs::read(&path).unwrap());
                manager
                    .edit_unsaved(ID, |auth| {
                        auth.metadata
                            .insert("access_token".into(), json!("n-access"));
                        auth.metadata
                            .insert("refresh_token".into(), json!("n-refresh"));
                        Ok::<bool, ()>(true)
                    })
                    .unwrap();
            }
        }));
        rig.manager
            .edit(ID, |auth| {
                auth.metadata.insert("note".into(), json!("stale"));
                Ok::<bool, ()>(true)
            })
            .unwrap();
        *lock(&rig.store.before_save_hook) = None;
        assert_eq!(
            deferrals(&rig).first(),
            Some(&Some(SaveDeferral::Stale)),
            "{:?}",
            deferrals(&rig)
        );
        // The refused save wrote nothing. The owed change is then flushed
        // once, outside the save, and that flush (not the refused save) is
        // the only write: it carries live's N tokens and the note, so the
        // stale base never reaches the file.
        let guarded = lock(&rig.store.guarded).clone();
        assert_eq!(guarded.len(), 2, "the refused save and one flush");
        assert_eq!(guarded[1].1, None, "the flush was written");
        assert_eq!(
            *lock(&rig.store.writes),
            vec![guarded[1].0],
            "only the flush wrote"
        );
        assert!(lock(&peer_bytes).take().is_some(), "the hook ran");
        assert_eq!(rig.disk()["refresh_token"], "n-refresh");
        assert_eq!(disk_note(&rig), "stale");
        assert_eq!(owed(&rig.manager), None);
        assert_eq!(rig.live().metadata["refresh_token"], "n-refresh");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b36_e_a_peer_write_during_a_leased_pass_is_marked_and_not_adopted() {
    // The leased peer write: both partitions rotate in a leased pass, and a
    // peer writes SIWC's file tokens at the pause. Outer lands and is clear;
    // SIWC is marked `Different` because the file no longer holds what the
    // pass read; the file keeps the peer's tokens and live holds the pass's.
    // Mutation: the commit overwriting the peer's SIWC tokens, or the
    // pass adopting them over its own rotation.
    bounded(async {
        let rig = rig();
        let probe = Probe::new("codex", false);
        probe
            .fake
            .set_refresh(|before| Ok(rotated_both(before, "ob", "sb")));
        rig.manager.register_executor(probe.clone());
        let (pause, pass) = paused_pass(&rig).await;
        peer_write(&rig.path(), false, "p");
        released(&rig, &pause, pass).await.unwrap();
        assert_eq!(probe.sent(), 1);
        assert_eq!(mark(&rig.manager, true), (false, "none"), "outer landed");
        assert_eq!(mark(&rig.manager, false), (true, "different"));
        assert_eq!(rig.disk()["refresh_token"], "ob-refresh");
        assert_eq!(rig.disk()["siwc"]["refresh_token"], "p-refresh");
        assert_eq!(rig.live().metadata["siwc"]["refresh_token"], "sb-refresh");
        // The mark's baselines are the SIWC tokens the pass started from and
        // the ones it rotated to; the peer's tokens are not among them.
        let state = rig.manager.lock();
        let baselines = &state.auths[ID].undurable_baselines;
        assert!(baselines.outer.is_empty(), "outer landed: no baseline");
        assert_eq!(
            baselines
                .siwc
                .iter()
                .map(|baseline| baseline.get("refresh_token").cloned())
                .collect::<Vec<_>>(),
            vec![Some(json!("siwc-refresh-old")), Some(json!("sb-refresh"))],
            "the pass's start and its own rotation"
        );
    })
    .await;
}
