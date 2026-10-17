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
    LeaseReadHook, Probe, REFRESH_SOURCE, Rig, bounded, code_only, fixture, fn_bodies, mutated,
    partition, pending_b, rig, token_count,
};
use crate::auth::codex_refresh::{CodexPersistIntent, CredentialRefreshPatch};
use crate::auth::{Auth, CodexOrigin, SaveDeferral};
use crate::exec::ExecError;
use crate::manager::{HeldPartitions, Manager, ManagerError, MarkCause, RefreshHold, lock};
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
async fn b36_d_a_save_refused_at_the_tail_is_owed_and_written_by_step_14() {
    // B-36 (d): at the leased tail an update's save and its own flush are both
    // refused for the refresh in flight; the pass saves it again at its end.
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
        assert_eq!(refused_count(&rig), 2, "step 14 saved, it was not refused");
        assert_eq!(disk_note(&rig), "owed", "step 14 wrote the change");
        assert_eq!(owed(&rig.manager), None, "a covering save cleared the flag");
        assert_eq!(rig.disk()["refresh_token"], "r-refresh");
        assert_eq!(rig.live().metadata["note"], "owed");
        assert_eq!(probe.reused(), 0);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn b36_b_the_no_rotation_tail_writes_directly() {
    // B-36 (b) (decision 13): the executor returns live's tokens unchanged, so
    // the tail is the unleased one and saves without being refused.
    bounded(async {
        let rig = rig();
        let probe = Probe::new("codex", false);
        probe.fake.set_refresh(|before| Ok(before.clone()));
        rig.manager.register_executor(probe.clone());
        rig.manager.register_unsaved(pending_b()).unwrap();
        rig.manager.force_refresh(ID).await.unwrap();
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
async fn b37_b_a_skipped_flush_keeps_the_flag() {
    // B-37 (b): the record is disabled and its file is gone, so the save is
    // Skipped; Skipped writes nothing and keeps the flag.
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
    let mut changed = (*manager.get(ID).unwrap()).clone();
    changed.metadata.insert("note".into(), json!(note));
    let save = std::thread::spawn(move || manager.update(changed));
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
    // B-34 step 3, with no pass in flight (variant (i), no E/f, never reaches
    // E's lock in this harness: the save does not wait, so it is not parked): the guard's origin
    // check is then the one that refuses a save whose destination is not
    // where live now resolves. E's file is untouched (or never created), and
    // the owed change is written to the file live names.
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

// ---- B-39 (b), (c), (d)

/// Arms the send fence's pause and spawns a forced pass; returns the pause
/// (awaited with `0`, released with `1`) and the pass's handle.
async fn paused_pass(
    rig: &Rig,
) -> (
    Arc<(tokio::sync::Notify, tokio::sync::Notify)>,
    tokio::task::JoinHandle<Result<Arc<Auth>, ManagerError>>,
) {
    let pause = Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
    *lock(&rig.manager.shared.send_fence_pause) = Some(pause.clone());
    let manager = rig.manager.clone();
    let pass = tokio::spawn(async move { manager.force_refresh(ID).await });
    pause.0.notified().await;
    (pause, pass)
}

#[tokio::test(flavor = "current_thread")]
async fn b39_b_a_token_command_during_the_leased_pause_supersedes_the_send() {
    // B-39 (b): a peer rotated outer to B; the pass adopts it and pauses at
    // the fence still holding the lease. An outer token command installs N
    // (a raw write, as the store's own would wait on the lease). The fence
    // finds N against the snapshot's B: nothing is sent, the call returns
    // live with N, and the lease is free for the next pass.
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
async fn b39_a2_an_outer_token_command_clears_the_cause_at_the_pause() {
    // B-39 (a2): from the `ReadBack` state with no lease to settle it, the
    // fence would defer `ReadBackUnsettled`; an outer token command at the
    // pause clears the cause and installs N, so the fence returns live with
    // N and nothing is sent.
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
