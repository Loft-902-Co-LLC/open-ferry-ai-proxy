//! Tests of the Codex refresh lease (B7 fills this file). B5's own tests are
//! in `siwc_persistence.rs`, beside the store they observe.
//!
//! This file covers the unresolved origin state and its resolution: how a
//! pass's admission and publication treat a record that moves under the lease
//! (B-26), how the next leased pass settles the state (B-28), what unleased
//! adoption and saves do while it is present (B-29, B-30), how its deferrals
//! back off (B-31), and how ineligibility clears it (B-33).

use super::auto_refresh_issue6199::TestLoop;
use super::siwc_persistence::{
    Probe, Rig, bounded, fixture, observe, on_lease_held, partition, seeded,
};
use super::support::*;
use crate::auth::Auth;
use crate::auth::codex_refresh::OUTER_KEYS;
use crate::manager::{Manager, ManagerError, Settings, lock};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A directory beside the rig's, which holds no credential file.
fn sibling_dir(rig: &Rig) -> tempfile::TempDir {
    tempfile::tempdir_in(rig.dir.path().parent().unwrap()).unwrap()
}

/// Directories a hook made, kept alive until the test ends.
type Made = Arc<Mutex<Vec<tempfile::TempDir>>>;

/// A closure that binds the store to a new empty sibling directory each time.
fn fresh_binder(rig: &Rig, made: &Made) -> impl Fn() + Send + Sync + 'static {
    let files = rig.store.files.clone();
    let parent = rig.dir.path().parent().unwrap().to_path_buf();
    let made = made.clone();
    move || {
        let fresh = tempfile::tempdir_in(&parent).unwrap();
        files.set_base_dir(fresh.path());
        lock(&made).push(fresh);
    }
}

/// Runs `act` with the 1-based attempt at every origin proof of a pass, from
/// the moment the pass holds the lease. Counts the proofs in `calls`.
fn on_origin_checks(rig: &Rig, calls: Arc<AtomicU32>, act: impl Fn(u32) + Send + Sync + 'static) {
    let manager = rig.manager.clone();
    let act = Arc::new(act);
    on_lease_held(rig, move || {
        let (act, calls) = (act.clone(), calls.clone());
        *lock(&manager.shared.origin_check_hook) = Some(Arc::new(move |attempt: u32| {
            calls.fetch_add(1, Ordering::SeqCst);
            act(attempt);
        }));
    });
}

/// Takes the lease and origin hooks off, as a test ends.
fn disarm(rig: &Rig) {
    crate::auth::credential_lock::set_test_hook(&rig.path(), None);
    *lock(&rig.manager.shared.origin_check_hook) = None;
}

/// A rig over the seeded fixture, with the executor rotating the outer
/// partition to `name`.
fn rotating_rig(name: &'static str) -> (Rig, Arc<Probe>) {
    let rig = super::siwc_persistence::rig();
    let probe = Probe::new("codex", false).rotating(name);
    rig.manager.register_executor(probe.clone());
    (rig, probe)
}

/// [`rotating_rig`] with a manager on a test clock.
fn clocked_rig(name: &'static str) -> (Rig, Arc<Probe>, TestClock) {
    let seeded = seeded();
    let store = observe(seeded.files.clone(), false);
    let clock = TestClock::new();
    let models = Arc::new(FakeModels::default());
    models.register("codex.json", &["gpt-5"]);
    let manager = Manager::with_clock(
        Settings::default(),
        models.clone(),
        Some(store.clone()),
        clock.clock(),
    );
    manager.set_gate(seeded.gate.clone());
    manager.register_unsaved(fixture()).unwrap();
    let rig = Rig {
        dir: seeded.dir,
        store,
        manager,
        models,
    };
    let probe = Probe::new("codex", false).rotating(name);
    rig.manager.register_executor(probe.clone());
    (rig, probe, clock)
}

/// The refresh-hold name the manager surfaces, if any.
fn hold_name(rig: &Rig) -> Option<&'static str> {
    rig.manager
        .refresh_hold("codex.json")
        .map(|(hold, _)| hold.as_str())
}

/// The partitions (outer, siwc) the unresolved state holds, if it is present.
fn unresolved(rig: &Rig) -> Option<(bool, bool)> {
    let state = rig.manager.lock();
    state.auths["codex.json"]
        .unresolved_origin
        .as_ref()
        .map(|u| (u.partitions.outer, u.partitions.siwc))
}

/// How many deferrals the unresolved state has counted.
fn unproven(rig: &Rig) -> Option<u32> {
    let state = rig.manager.lock();
    state.auths["codex.json"]
        .unresolved_origin
        .as_ref()
        .map(|u| u.unproven)
}

/// The pending rotations no save has written.
fn unwritten_pending(rig: &Rig) -> usize {
    rig.live()
        .codex_pending_refresh
        .iter()
        .filter(|patch| !patch.is_written())
        .count()
}

fn marked_outer(rig: &Rig) -> bool {
    rig.manager.lock().auths["codex.json"].undurable.outer
}

fn read_file(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

/// `file` with its outer partition replaced by `name`'s tokens.
fn with_outer(mut file: Value, name: &str) -> Vec<u8> {
    file["access_token"] = json!(format!("{name}-access"));
    file["refresh_token"] = json!(format!("{name}-refresh"));
    serde_json::to_vec(&file).unwrap()
}

fn is_unresolved_deferral(error: &ManagerError) -> bool {
    matches!(error, ManagerError::Store(e) if e.to_string().contains("origin unresolved"))
}

/// A name for `dir` that reaches the same directory: its case variant on
/// Windows, a link to it elsewhere.
fn alias_of(dir: &Path, made: &Made) -> PathBuf {
    #[cfg(windows)]
    {
        let _ = made;
        PathBuf::from(dir.to_string_lossy().to_uppercase())
    }
    #[cfg(unix)]
    {
        let holder = tempfile::tempdir_in(dir.parent().unwrap()).unwrap();
        let link = holder.path().join("alias");
        std::os::unix::fs::symlink(dir, &link).unwrap();
        lock(made).push(holder);
        link
    }
}

/// The pass has rotated A -> B, and the refresh stays in memory of how it got
/// there.
struct Lost {
    rig: Rig,
    probe: Arc<Probe>,
    clock: TestClock,
    made: Made,
    /// The origin proofs made after the send.
    calls: u32,
}

/// B-26 (f)'s end state: the pass sent A and got B; the commit landed B in
/// D/f; the publication's proof of the record's file was `Unproven` at every
/// attempt, so the state is recorded and the tail saved nothing. The store is
/// left bound to the last empty directory.
async fn published_unproven() -> Lost {
    let (rig, probe, clock) = clocked_rig("b");
    let (calls, made) = (Arc::new(AtomicU32::new(0)), Made::default());
    let entered = Arc::new(AtomicBool::new(false));
    let admitted = Arc::new(AtomicU32::new(0));
    let (seen, count, before) = (entered.clone(), calls.clone(), admitted.clone());
    probe.set_at_entry(move |_| {
        before.store(count.load(Ordering::SeqCst), Ordering::SeqCst);
        seen.store(true, Ordering::SeqCst);
    });
    let rebind = fresh_binder(&rig, &made);
    on_origin_checks(&rig, calls.clone(), move |_| {
        if entered.load(Ordering::SeqCst) {
            rebind();
        }
    });
    rig.manager.force_refresh("codex.json").await.unwrap();
    disarm(&rig);
    Lost {
        rig,
        probe,
        clock,
        made,
        calls: calls.load(Ordering::SeqCst) - admitted.load(Ordering::SeqCst),
    }
}

/// B-26 (g)'s end state: the pass sent A and got B and landed it in D/f; then
/// D/f was deleted and E/f written as a copy of A, and the store rebound to E,
/// so the tail's fence 2 could not prove the file it was to write.
async fn fenced_unproven() -> (Lost, PathBuf) {
    let (rig, probe, clock) = clocked_rig("b");
    let made = Made::default();
    let e_dir = sibling_dir(&rig);
    let e_path = e_dir.path().to_path_buf();
    lock(&made).push(e_dir);
    let original = std::fs::read(rig.path()).unwrap();
    let (files, d_file, e_file) = (
        rig.store.files.clone(),
        rig.path(),
        e_path.join("codex.json"),
    );
    let target = e_path.clone();
    *lock(&rig.manager.shared.lease_tail_hook) = Some(Arc::new(move |_: &Auth| {
        std::fs::remove_file(&d_file).unwrap();
        std::fs::write(&e_file, &original).unwrap();
        files.set_base_dir(&target);
    }));
    rig.manager.force_refresh("codex.json").await.unwrap();
    *lock(&rig.manager.shared.lease_tail_hook) = None;
    (
        Lost {
            rig,
            probe,
            clock,
            made,
            calls: 0,
        },
        e_path,
    )
}

#[tokio::test(flavor = "current_thread")]
async fn an_unprovable_admission_defers_before_the_send() {
    // B-26 (a): the record names a fresh empty directory at every proof, so no
    // attempt can prove it. Three proofs, then the pass defers.
    bounded(async {
        let (logs, _guard) = capture_logs();
        let (rig, probe) = rotating_rig("c");
        let (calls, made) = (Arc::new(AtomicU32::new(0)), Made::default());
        let rebind = fresh_binder(&rig, &made);
        on_origin_checks(&rig, calls.clone(), move |_| rebind());
        let error = rig.manager.force_refresh("codex.json").await.unwrap_err();
        disarm(&rig);
        assert!(is_unresolved_deferral(&error), "{error}");
        // No rotation was received, so the line says none is held.
        let line = only_line(&logs.since(0), "rotation_held=");
        assert_eq!(line.0, tracing::Level::WARN);
        assert!(line.1.contains("auth_id=codex.json"), "{}", line.1);
        assert!(line.1.contains("rotation_held=false"), "{}", line.1);
        assert_logs_clean(&logs, &rig, &[]);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(probe.sent(), 0, "nothing was sent");
        assert_eq!(rig.disk()["refresh_token"], "outer-refresh-old");
        assert_eq!(rig.live().metadata["refresh_token"], "outer-refresh-old");
        for dir in lock(&made).iter() {
            assert!(!dir.path().join("codex.json").exists());
        }
        assert_eq!(unresolved(&rig), None, "no rotation was received");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_lost_file_at_admission_refuses_the_pass() {
    // B-26 (b): the record moves to another directory at the first proof and
    // stays there, so the second proof finds the file the registration no
    // longer names.
    bounded(async {
        let (rig, probe) = rotating_rig("c");
        let (calls, made) = (Arc::new(AtomicU32::new(0)), Made::default());
        let rebind = fresh_binder(&rig, &made);
        on_origin_checks(&rig, calls.clone(), move |attempt| {
            if attempt == 1 {
                rebind();
            }
        });
        let error = rig.manager.force_refresh("codex.json").await.unwrap_err();
        disarm(&rig);
        assert!(
            error
                .to_string()
                .contains("registration changed before refresh"),
            "{error}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(probe.sent(), 0);
        assert_eq!(rig.disk()["refresh_token"], "outer-refresh-old");
        assert_eq!(unresolved(&rig), None);
    })
    .await;
}

/// Runs a refresh whose first admission proof calls `act`, and returns the
/// proofs made before the send and the probe.
async fn admitted_after(
    rig: &Rig,
    probe: &Arc<Probe>,
    act: impl Fn(u32) + Send + Sync + 'static,
) -> u32 {
    let calls = Arc::new(AtomicU32::new(0));
    let at_send = Arc::new(AtomicU32::new(0));
    let entered = Arc::new(AtomicBool::new(false));
    let (count, snapshot, seen) = (calls.clone(), at_send.clone(), entered.clone());
    probe.set_at_entry(move |_| {
        snapshot.store(count.load(Ordering::SeqCst), Ordering::SeqCst);
        seen.store(true, Ordering::SeqCst);
    });
    on_origin_checks(rig, calls, move |attempt| {
        if !entered.load(Ordering::SeqCst) {
            act(attempt);
        }
    });
    rig.manager.force_refresh("codex.json").await.unwrap();
    disarm(rig);
    at_send.load(Ordering::SeqCst)
}

/// What an admitted, committed pass leaves: C sent for A, no mark, no state.
fn assert_committed_normally(rig: &Rig, probe: &Probe) {
    assert_eq!(probe.sent(), 1);
    assert_eq!(probe.reused(), 0, "a spent token was presented again");
    assert_eq!(rig.disk()["refresh_token"], "c-refresh");
    assert_eq!(rig.live().metadata["refresh_token"], "c-refresh");
    assert!(!marked_outer(rig), "no mark");
    assert_eq!(unresolved(rig), None, "no state");
}

#[tokio::test(flavor = "current_thread")]
async fn a_rebind_to_the_same_directory_at_admission_is_admitted() {
    // B-26 (c): the store is bound to D again during the first proof.
    bounded(async {
        let (rig, probe) = rotating_rig("c");
        let (files, d) = (rig.store.files.clone(), rig.dir.path().to_path_buf());
        let proofs = admitted_after(&rig, &probe, move |attempt| {
            if attempt == 1 {
                files.set_base_dir(&d);
            }
        })
        .await;
        assert_eq!(proofs, 1);
        assert_committed_normally(&rig, &probe);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_rebind_to_an_alias_at_admission_is_admitted() {
    // B-26 (d): the store is bound to another name for D during the first
    // proof, so the second proof resolves another path to the same file.
    bounded(async {
        let (rig, probe) = rotating_rig("c");
        let made = Made::default();
        let alias = alias_of(rig.dir.path(), &made);
        let files = rig.store.files.clone();
        let proofs = admitted_after(&rig, &probe, move |attempt| {
            if attempt == 1 {
                files.set_base_dir(&alias);
            }
        })
        .await;
        assert_eq!(proofs, 2);
        assert_committed_normally(&rig, &probe);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_rebind_away_and_back_at_admission_is_admitted() {
    // B-26 (e): E at the first proof, D again at the second.
    bounded(async {
        let (rig, probe) = rotating_rig("c");
        let made = Made::default();
        let rebind = fresh_binder(&rig, &made);
        let (files, d) = (rig.store.files.clone(), rig.dir.path().to_path_buf());
        let proofs = admitted_after(&rig, &probe, move |attempt| match attempt {
            1 => rebind(),
            _ => files.set_base_dir(&d),
        })
        .await;
        assert_eq!(proofs, 2);
        assert_committed_normally(&rig, &probe);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_unprovable_publication_records_the_state_and_saves_nothing() {
    // B-26 (f): after the send the record names a fresh empty directory at
    // every proof. The commit has landed B in D/f; the tail may not save to
    // any of them.
    bounded(async {
        let (logs, _guard) = capture_logs();
        let Lost {
            rig,
            probe,
            calls,
            made,
            ..
        } = published_unproven().await;
        // Step 13 holds the next pass back and warns, naming the id only; the
        // pass itself succeeded, so no deferral was counted.
        let held = only_line(&logs.since(0), "a received rotation is held");
        assert_eq!(held.0, tracing::Level::WARN);
        assert!(held.1.contains("auth_id=codex.json"), "{}", held.1);
        assert!(
            logs.since(0)
                .iter()
                .all(|l| !l.1.contains("rotation_held="))
        );
        assert_logs_clean(&logs, &rig, &[]);
        assert_eq!(calls, 6, "the proofs after the send");
        assert_eq!(probe.sent(), 1);
        assert_eq!(rig.live().metadata["refresh_token"], "b-refresh");
        assert_eq!(rig.disk()["refresh_token"], "b-refresh");
        assert!(!marked_outer(&rig), "the rotation is not marked");
        assert_eq!(unresolved(&rig), Some((true, false)));
        assert!(lock(&rig.store.writes).is_empty(), "the tail saved nothing");
        assert_eq!(rig.store.save_bound_calls.load(Ordering::SeqCst), 0);
        for dir in lock(&made).iter() {
            assert!(!dir.path().join("codex.json").exists());
        }
        assert_eq!(hold_name(&rig), None, "one pass is not yet surfaced");
        // The file is named again: the next pass proves it, sends B and not
        // A, and lands C.
        rig.store.files.set_base_dir(rig.dir.path());
        let probe = probe.rotating("c");
        rig.manager.force_refresh("codex.json").await.unwrap();
        assert_eq!(unresolved(&rig), None);
        assert_eq!(probe.sent(), 2);
        assert_eq!(probe.sent_refresh(1), "b-refresh");
        assert_eq!(probe.reused(), 0, "a spent token was presented again");
        assert_eq!(rig.disk()["refresh_token"], "c-refresh");
        assert_eq!(rig.live().metadata["refresh_token"], "c-refresh");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_unprovable_save_target_records_the_state_and_keeps_the_file() {
    // B-26 (g): the file the tail was to write went away and another took its
    // place before the save, so fence 2 cannot prove it.
    bounded(async {
        let (logs, _guard) = capture_logs();
        let (lost, e_path) = fenced_unproven().await;
        let Lost { rig, probe, .. } = lost;
        let held = only_line(&logs.since(0), "a received rotation is held");
        assert_eq!(held.0, tracing::Level::WARN);
        assert!(held.1.contains("auth_id=codex.json"), "{}", held.1);
        let seen = logs.len();
        assert_eq!(probe.sent(), 1);
        assert_eq!(rig.live().metadata["refresh_token"], "b-refresh");
        assert!(!marked_outer(&rig), "the rotation is not marked");
        assert_eq!(unresolved(&rig), Some((true, false)));
        assert_eq!(
            read_file(&e_path.join("codex.json"))["refresh_token"],
            "outer-refresh-old",
            "the other file is as it was"
        );
        assert!(lock(&rig.store.writes).is_empty(), "nothing was saved");
        // The next pass cannot prove the state either, and sends nothing.
        let error = rig.manager.force_refresh("codex.json").await.unwrap_err();
        assert!(is_unresolved_deferral(&error), "{error}");
        assert_eq!(probe.sent(), 1, "the executor was not called");
        assert_eq!(unresolved(&rig), Some((true, false)));
        assert_eq!(unproven(&rig), Some(1), "a new deferral was counted");
        let line = only_line(&logs.since(seen), "rotation_held=");
        assert_eq!(line.0, tracing::Level::WARN);
        assert!(line.1.contains("rotation_held=true"), "{}", line.1);
        assert_logs_clean(&logs, &rig, &[&e_path]);
        assert_eq!(
            read_file(&e_path.join("codex.json"))["refresh_token"],
            "outer-refresh-old"
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_read_back_without_identity_records_the_state_without_deciding() {
    // B-26 (h): the record's file has no resolvable identity when the save is
    // read back, so fence 3 cannot call it the file that was written.
    bounded(async {
        let (logs, _guard) = capture_logs();
        let (rig, probe, _clock) = clocked_rig("b");
        let flag = rig.store.clone();
        *lock(&rig.manager.shared.read_back_hook) = Some(Arc::new(move |_: &Auth| {
            flag.opaque_origin.store(true, Ordering::SeqCst);
        }));
        rig.manager.force_refresh("codex.json").await.unwrap();
        *lock(&rig.manager.shared.read_back_hook) = None;
        assert_eq!(probe.sent(), 1);
        assert_eq!(rig.live().metadata["refresh_token"], "b-refresh");
        assert!(!marked_outer(&rig), "the read-back marks nothing");
        assert_eq!(unresolved(&rig), Some((true, false)));
        let held = only_line(&logs.since(0), "a received rotation is held");
        assert_eq!(held.0, tracing::Level::WARN);
        assert!(held.1.contains("auth_id=codex.json"), "{}", held.1);
        let seen = logs.len();
        // With the identity still unresolvable the next pass defers.
        let error = rig.manager.force_refresh("codex.json").await.unwrap_err();
        assert!(is_unresolved_deferral(&error), "{error}");
        let line = only_line(&logs.since(seen), "rotation_held=");
        assert_eq!(line.0, tracing::Level::WARN);
        assert!(line.1.contains("rotation_held=true"), "{}", line.1);
        assert_logs_clean(&logs, &rig, &[]);
        assert_eq!(probe.sent(), 1, "the executor was not called");
        assert_eq!(unresolved(&rig), Some((true, false)));
        // Once the identity is back the file is proven the same one, and the
        // pass sends B.
        rig.store.opaque_origin.store(false, Ordering::SeqCst);
        let probe = probe.rotating("c");
        rig.manager.force_refresh("codex.json").await.unwrap();
        assert_eq!(unresolved(&rig), None);
        assert_eq!(probe.sent(), 2);
        assert_eq!(probe.sent_refresh(1), "b-refresh");
        assert_eq!(probe.reused(), 0);
        assert_eq!(rig.disk()["refresh_token"], "c-refresh");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_pass_not_due_resolves_the_state_and_sends_nothing() {
    // B-26 (i): the scheduler's pass for the state alone. The record names D
    // again and the hold has passed; the proof is `Same`, the state clears,
    // and step 6c ends the pass with no send and no deferral.
    bounded(async {
        let Lost {
            rig, probe, clock, ..
        } = published_unproven().await;
        rig.store.files.set_base_dir(rig.dir.path());
        clock.advance(Duration::from_secs(60));
        let epoch = rig.live().registration_epoch;
        rig.manager
            .refresh_at_epoch("codex.json", "", epoch)
            .await
            .unwrap();
        assert_eq!(unresolved(&rig), None);
        assert_eq!(probe.sent(), 1, "nothing more was sent");
        assert_eq!(rig.live().metadata["refresh_token"], "b-refresh");
        assert_eq!(rig.disk()["refresh_token"], "b-refresh");
        assert!(rig.live().last_error.is_none());
        assert!(!marked_outer(&rig));
    })
    .await;
}

/// B-26 (j)'s setup: (f)'s end state with D restored and the hold past, and a
/// loop that queues the one job the state alone asks for.
async fn queued_resolution_job() -> (Lost, TestLoop) {
    let lost = published_unproven().await;
    lost.rig.store.files.set_base_dir(lost.rig.dir.path());
    lost.clock.advance(Duration::from_secs(60));
    let test_loop = TestLoop::install(&lost.rig.manager);
    let now = lost.clock.now();
    test_loop.rebuild(&lost.rig.manager, now);
    test_loop.handle_due(&lost.rig.manager, now);
    assert_eq!(test_loop.queued(), 1, "one job for the state");
    (lost, test_loop)
}

/// Runs one worker over the queue until the job it took is done.
async fn run_one_job(rig: &Rig, test_loop: &TestLoop) {
    let worker = test_loop.worker(&rig.manager);
    loop {
        if test_loop.queued() == 0 && rig.manager.lock().refresh_jobs.is_empty() {
            break;
        }
        tokio::task::yield_now().await;
    }
    test_loop.cancel();
    worker.await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn a_queued_resolution_job_whose_state_is_gone_sends_nothing() {
    // B-26 (j) steps 1 to 3: a token command clears the state before the
    // worker runs. Step 6c ends the pass with no send.
    bounded(async {
        let (lost, test_loop) = queued_resolution_job().await;
        let Lost { rig, probe, .. } = &lost;
        let epoch = rig.live().registration_epoch;
        let command = rig
            .manager
            .capture_token_command("codex.json", partition(true))
            .unwrap();
        let login = {
            let mut login = fixture();
            login.metadata["access_token"] = json!("n-access");
            login.metadata["refresh_token"] = json!("n-refresh");
            login
        };
        std::fs::write(rig.path(), with_outer(read_file(&rig.path()), "n")).unwrap();
        rig.manager
            .adopt_completed_token_command(&login, command)
            .unwrap();
        assert_eq!(unresolved(rig), None, "the command cleared the state");
        assert_eq!(rig.live().registration_epoch, epoch);
        run_one_job(rig, &test_loop).await;
        assert_eq!(probe.sent(), 1, "the job sent nothing");
        assert_eq!(rig.live().metadata["refresh_token"], "n-refresh");
        assert_eq!(rig.disk()["refresh_token"], "n-refresh");
        assert!(rig.live().last_error.is_none());
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_queued_resolution_job_for_an_ineligible_record_sends_nothing() {
    // B-26 (j) step 4: the record becomes ineligible, which discards the
    // state; the worker takes step 1's ineligible branch and step 0c ends it.
    bounded(async {
        let (lost, test_loop) = queued_resolution_job().await;
        let Lost { rig, probe, .. } = &lost;
        rig.manager
            .edit_unsaved("codex.json", |auth| {
                auth.attributes.insert("runtime_only".into(), "true".into());
                Ok::<_, ()>(true)
            })
            .unwrap();
        assert_eq!(unresolved(rig), None, "the discard cleared the state");
        run_one_job(rig, &test_loop).await;
        assert_eq!(probe.sent(), 1, "the job sent nothing");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_due_job_with_no_state_is_an_ordinary_refresh() {
    // B-26 (j) control: the same loop and worker send for a token that is due.
    bounded(async {
        let (rig, probe, clock) = clocked_rig("c");
        rig.manager
            .edit_unsaved("codex.json", |auth| {
                auth.metadata
                    .insert("expired".into(), json!("2026-05-31T00:00:00Z"));
                Ok::<_, ()>(true)
            })
            .unwrap();
        let test_loop = TestLoop::install(&rig.manager);
        let now = clock.now();
        test_loop.rebuild(&rig.manager, now);
        test_loop.handle_due(&rig.manager, now);
        assert_eq!(test_loop.queued(), 1);
        run_one_job(&rig, &test_loop).await;
        assert_eq!(probe.sent(), 1, "an ordinary job sends");
        assert_eq!(rig.live().metadata["refresh_token"], "c-refresh");
    })
    .await;
}

/// One scheduler pass for the state: a refresh of the current epoch that is
/// neither forced nor, with the fixture's far expiry, due.
async fn state_pass(rig: &Rig) -> Result<(), ManagerError> {
    let epoch = rig.live().registration_epoch;
    rig.manager
        .refresh_at_epoch("codex.json", "", epoch)
        .await
        .map(|_| ())
}

#[tokio::test(flavor = "current_thread")]
async fn a_resolution_that_proves_the_same_file_clears_the_state() {
    // B-28 (a): the record now names D through another path. The proof is
    // `Same`, so the state clears, the peer's newer chain is adopted, and the
    // chain after it is the one the next send uses; B is never sent.
    bounded(async {
        let Lost {
            rig,
            probe,
            clock,
            made,
            ..
        } = published_unproven().await;
        rig.store
            .files
            .set_base_dir(alias_of(rig.dir.path(), &made));
        std::fs::write(rig.path(), with_outer(read_file(&rig.path()), "c")).unwrap();
        clock.advance(Duration::from_secs(60));
        state_pass(&rig).await.unwrap();
        assert_eq!(unresolved(&rig), None);
        assert_eq!(probe.sent(), 1, "the pass sent nothing");
        assert!(!marked_outer(&rig), "nothing is marked");
        assert_eq!(rig.live().metadata["refresh_token"], "c-refresh");
        assert!(rig.live().last_error.is_none());
        let probe = probe.rotating("d");
        rig.manager.force_refresh("codex.json").await.unwrap();
        assert_eq!(probe.sent(), 2);
        assert_eq!(probe.sent_refresh(1), "c-refresh", "B is never sent");
        assert_eq!(rig.disk()["refresh_token"], "d-refresh");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_resolution_that_proves_a_different_file_marks_the_partition() {
    // B-28 (b): the record now names a file that is not the one B went to. The
    // state clears and the partition is marked, so B is owed to the new file.
    bounded(async {
        let Lost {
            rig,
            probe,
            clock,
            made,
            ..
        } = published_unproven().await;
        let other = sibling_dir(&rig);
        let q_file = other.path().join("codex.json");
        let mut older = read_file(&rig.path());
        older["access_token"] = json!("outer-old");
        older["refresh_token"] = json!("outer-refresh-old");
        std::fs::write(&q_file, serde_json::to_vec(&older).unwrap()).unwrap();
        rig.store.files.set_base_dir(other.path());
        lock(&made).push(other);
        clock.advance(Duration::from_secs(60));
        state_pass(&rig).await.unwrap();
        assert_eq!(unresolved(&rig), None, "the state cleared");
        assert!(marked_outer(&rig), "the partition is marked");
        assert_eq!(probe.sent(), 1, "the pass sent nothing");
        assert_eq!(rig.live().metadata["refresh_token"], "b-refresh");
        assert_eq!(
            read_file(&q_file)["refresh_token"],
            "outer-refresh-old",
            "the new file is untouched by the resolution"
        );
        let probe = probe.rotating("c");
        rig.manager.force_refresh("codex.json").await.unwrap();
        assert_eq!(probe.sent(), 2);
        assert_eq!(probe.sent_refresh(1), "b-refresh");
        assert_eq!(read_file(&q_file)["refresh_token"], "c-refresh");
        assert!(!marked_outer(&rig));
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_resolution_that_proves_nothing_keeps_the_state() {
    // B-28 (c): the proof is `Unproven`, so the pass defers, sends nothing,
    // keeps the state, and counts the deferral.
    bounded(async {
        let (lost, e_path) = fenced_unproven().await;
        let Lost {
            rig, probe, clock, ..
        } = lost;
        clock.advance(Duration::from_secs(60));
        // A file that cannot be resolved cannot be proven either.
        let error = state_pass(&rig).await.unwrap_err();
        assert!(is_unresolved_deferral(&error), "{error}");
        assert_eq!(probe.sent(), 1);
        assert_eq!(unresolved(&rig), Some((true, false)));
        assert_eq!(unproven(&rig), Some(1));
        assert!(!marked_outer(&rig));
        assert_eq!(rig.live().metadata["refresh_token"], "b-refresh");
        assert_eq!(
            read_file(&e_path.join("codex.json"))["refresh_token"],
            "outer-refresh-old"
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_resolution_with_an_opaque_origin_keeps_the_state() {
    // B-28 (c), `opaque_origin`: the record's file has no resolvable identity.
    bounded(async {
        let (rig, probe, clock) = clocked_rig("b");
        let flag = rig.store.clone();
        *lock(&rig.manager.shared.read_back_hook) = Some(Arc::new(move |_: &Auth| {
            flag.opaque_origin.store(true, Ordering::SeqCst);
        }));
        rig.manager.force_refresh("codex.json").await.unwrap();
        *lock(&rig.manager.shared.read_back_hook) = None;
        assert_eq!(unresolved(&rig), Some((true, false)));
        clock.advance(Duration::from_secs(60));
        let error = state_pass(&rig).await.unwrap_err();
        assert!(is_unresolved_deferral(&error), "{error}");
        assert_eq!(probe.sent(), 1);
        assert_eq!(unresolved(&rig), Some((true, false)));
        assert_eq!(unproven(&rig), Some(1));
        assert!(!marked_outer(&rig));
    })
    .await;
}

/// The seconds the record's next refresh is held beyond the test clock.
fn hold_secs(rig: &Rig, clock: &TestClock) -> i64 {
    let until = rig.live().next_refresh_after.expect("a hold is set");
    (until - clock.now()).num_seconds()
}

/// The fields a deferral must leave alone, whole.
fn status_fields(rig: &Rig) -> (String, String, bool, Option<String>, String) {
    let auth = rig.live();
    (
        format!("{:?}", auth.status),
        auth.status_message.clone(),
        auth.unavailable,
        auth.next_retry_after.map(|t| t.to_rfc3339()),
        format!("{:?}", auth.last_error),
    )
}

/// What a scoped subscriber saw: each event's level and its fields as text.
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<(tracing::Level, String)>>>);

impl Logs {
    fn len(&self) -> usize {
        lock(&self.0).len()
    }

    /// The events from the `from`th on.
    fn since(&self, from: usize) -> Vec<(tracing::Level, String)> {
        lock(&self.0)[from..].to_vec()
    }
}

struct Fields(String);

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!("{}={value:?} ", field.name()));
    }
}

struct Capture(Logs);

impl tracing::Subscriber for Capture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        lock(&(self.0).0).push((*event.metadata().level(), fields.0));
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// Captures what this thread logs until the guard drops. The tests that use it
/// run on one thread (`current_thread`), so every event the async code emits is
/// seen; one emitted on a blocking thread is not.
fn capture_logs() -> (Logs, tracing::subscriber::DefaultGuard) {
    crate::test_tracing::keep_every_callsite_open();
    let logs = Logs::default();
    let guard = tracing::subscriber::set_default(Capture(logs.clone()));
    (logs, guard)
}

/// The one line among `lines` that holds `needle`. It fails when none or more
/// than one does, so an assertion on the line cannot pass for want of it.
fn only_line(lines: &[(tracing::Level, String)], needle: &str) -> (tracing::Level, String) {
    let found: Vec<_> = lines
        .iter()
        .filter(|(_, text)| text.contains(needle))
        .collect();
    assert_eq!(found.len(), 1, "lines holding {needle:?}: {}", found.len());
    found[0].clone()
}

/// No captured line carries a token of the fixture or a test directory.
fn assert_logs_clean(logs: &Logs, rig: &Rig, extra: &[&Path]) {
    let mut secrets: Vec<String> = [
        "outer-old",
        "outer-refresh-old",
        "siwc-old",
        "siwc-refresh-old",
        "b-access",
        "b-refresh",
        "c-access",
        "c-refresh",
        "n-access",
        "n-refresh",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    secrets.push(rig.dir.path().to_string_lossy().into_owned());
    secrets.extend(extra.iter().map(|p| p.to_string_lossy().into_owned()));
    for (level, text) in logs.since(0) {
        for secret in &secrets {
            assert!(
                !text.contains(secret.as_str()),
                "a {level} line holds a secret"
            );
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn deferrals_back_off_and_surface_without_touching_status() {
    // B-31 steps 1 to 4: sixteen passes that cannot prove the state, the last
    // of them forced while the hold from the one before is in the future, then
    // a seventeenth. Each counts, holds the next refresh with the backoff, and
    // leaves the status fields alone; the hold is surfaced from the tenth, and
    // the lines are warn up to it and debug after.
    bounded(async {
        let (logs, _guard) = capture_logs();
        let (lost, e_path) = fenced_unproven().await;
        let Lost {
            rig, probe, clock, ..
        } = lost;
        let recorded = only_line(&logs.since(0), "a received rotation is held");
        assert_eq!(recorded.0, tracing::Level::WARN);
        assert!(recorded.1.contains("auth_id=codex.json"), "{}", recorded.1);
        rig.manager
            .edit_unsaved("codex.json", |auth| {
                auth.status = crate::auth::Status::Error;
                auth.status_message = "kept".into();
                auth.unavailable = true;
                auth.next_retry_after = Some(chrono::Utc::now() + chrono::TimeDelta::days(30));
                auth.last_error = Some(crate::auth::AuthError {
                    code: "kept_code".into(),
                    message: "kept error".into(),
                    retryable: true,
                    http_status: 503,
                });
                Ok::<_, ()>(true)
            })
            .unwrap();
        let before = status_fields(&rig);
        assert_ne!(before.4, "None", "the error is seeded");
        assert_eq!(hold_name(&rig), None);
        let expected = [
            30, 30, 30, 30, 30, 30, 30, 30, 30, 30, 60, 120, 240, 480, 960, 1800, 1800,
        ];
        let mut seen = logs.len();
        for (index, want) in expected.iter().enumerate() {
            let pass = index as u32 + 1;
            // The sixteenth is forced while the hold from the fifteenth, 960 s,
            // is still in the future: it runs the resolution at once.
            let error = if pass == 16 {
                let remaining = hold_secs(&rig, &clock);
                assert!((957..=960).contains(&remaining), "held {remaining} s");
                rig.manager.force_refresh("codex.json").await.unwrap_err()
            } else {
                state_pass(&rig).await.unwrap_err()
            };
            assert!(is_unresolved_deferral(&error), "pass {pass}: {error}");
            assert_eq!(unproven(&rig), Some(pass));
            let held = hold_secs(&rig, &clock);
            assert!(
                (want - 3..=*want).contains(&held),
                "pass {pass} holds {held} s, wanted {want}"
            );
            assert_eq!(hold_name(&rig).is_some(), pass >= 10, "pass {pass}");
            if pass >= 10 {
                assert_eq!(hold_name(&rig), Some("origin_unresolved"));
            }
            assert_eq!(status_fields(&rig), before, "pass {pass}");
            assert_eq!(probe.sent(), 1, "pass {pass} sent");
            assert_eq!(unresolved(&rig), Some((true, false)));
            assert_eq!(rig.live().metadata["refresh_token"], "b-refresh");
            let lines = logs.since(seen);
            seen = logs.len();
            let line = only_line(&lines, "rotation_held=");
            let level = if pass > 10 {
                tracing::Level::DEBUG
            } else {
                tracing::Level::WARN
            };
            assert_eq!(line.0, level, "pass {pass}: {}", line.1);
            assert!(line.1.contains("auth_id=codex.json"), "{}", line.1);
            assert!(line.1.contains("rotation_held=true"), "{}", line.1);
            assert_eq!(
                line.1.contains("refresh paused"),
                pass == 10,
                "pass {pass}: {}",
                line.1
            );
            // The hold of pass 15 must still be in the future at the sixteenth.
            if pass != 15 {
                clock.advance(Duration::from_secs(*want as u64 + 1));
            }
        }
        let e_file = read_file(&e_path.join("codex.json"));
        assert_eq!(e_file["refresh_token"], "outer-refresh-old");
        assert!(e_file.get("refresh_hold").is_none(), "memory only");
        assert!(rig.live().metadata.get("refresh_hold").is_none());
        assert!(!marked_outer(&rig));
        assert_logs_clean(&logs, &rig, &[&e_path]);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_request_result_and_the_hold_leave_each_other_alone() {
    // B-31 step 5: a request failure sets its own message before the hold is
    // surfaced; from the tenth pass the message is still there beside the hold,
    // and a later success clears the message and leaves the hold.
    bounded(async {
        let (lost, _e_path) = fenced_unproven().await;
        let Lost {
            rig, probe, clock, ..
        } = lost;
        let live = rig.live();
        let failure = crate::manager::CallResult {
            auth_id: live.id.clone(),
            provider: live.provider.clone(),
            success: false,
            error: Some(crate::auth::AuthError {
                code: String::new(),
                message: "upstream down".into(),
                retryable: true,
                http_status: 503,
            }),
            credential_version: live.credential_version,
            registration_epoch: live.registration_epoch,
            ..Default::default()
        };
        rig.manager.mark_result(&failure);
        let message = rig.live().status_message.clone();
        assert!(!message.is_empty(), "the failure set a message");
        assert!(rig.live().last_error.is_some());
        for pass in 1..=10u32 {
            let error = state_pass(&rig).await.unwrap_err();
            assert!(is_unresolved_deferral(&error), "pass {pass}: {error}");
            assert_eq!(rig.live().status_message, message, "pass {pass}");
            assert_eq!(hold_name(&rig).is_some(), pass >= 10, "pass {pass}");
            clock.advance(Duration::from_secs(31));
        }
        assert_eq!(hold_name(&rig), Some("origin_unresolved"));
        assert_eq!(probe.sent(), 1);
        let live = rig.live();
        let success = crate::manager::CallResult {
            auth_id: live.id.clone(),
            provider: live.provider.clone(),
            success: true,
            credential_version: live.credential_version,
            registration_epoch: live.registration_epoch,
            ..Default::default()
        };
        rig.manager.mark_result(&success);
        assert_eq!(rig.live().status_message, "", "the success cleared it");
        assert!(rig.live().last_error.is_none());
        assert_eq!(hold_name(&rig), Some("origin_unresolved"), "the hold stays");
        assert_eq!(unresolved(&rig), Some((true, false)));
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_surfaced_deferral_state_is_left_by_a_proof() {
    // B-31 step 6, by a proof: after the hold is surfaced, a forced pass still
    // runs the resolution at once and a `Same` proof clears the state and the
    // hold.
    bounded(async {
        let (lost, e_path) = fenced_unproven().await;
        let Lost {
            rig, probe, clock, ..
        } = lost;
        for _ in 0..10 {
            state_pass(&rig).await.unwrap_err();
            clock.advance(Duration::from_secs(31));
        }
        assert_eq!(hold_name(&rig), Some("origin_unresolved"));
        // The file B went to was moved away; put it back as B left it.
        std::fs::write(
            rig.path(),
            with_outer(read_file(&e_path.join("codex.json")), "b"),
        )
        .unwrap();
        rig.store.files.set_base_dir(rig.dir.path());
        let probe = probe.rotating("c");
        rig.manager.force_refresh("codex.json").await.unwrap();
        assert_eq!(unresolved(&rig), None);
        assert_eq!(hold_name(&rig), None);
        assert_eq!(probe.sent(), 2);
        assert_eq!(probe.sent_refresh(1), "b-refresh");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_surfaced_deferral_state_is_left_by_a_token_command() {
    // B-31 step 6, by a token command: B-29 step 4's sequence after the hold is
    // surfaced clears the state and the hold, and live holds the login's chain.
    bounded(async {
        let (lost, e_path) = fenced_unproven().await;
        let Lost {
            rig, probe, clock, ..
        } = lost;
        for _ in 0..10 {
            state_pass(&rig).await.unwrap_err();
            clock.advance(Duration::from_secs(31));
        }
        assert_eq!(hold_name(&rig), Some("origin_unresolved"));
        let command = rig
            .manager
            .capture_token_command("codex.json", partition(true))
            .unwrap();
        let e_file = e_path.join("codex.json");
        std::fs::write(&e_file, with_outer(read_file(&e_file), "n")).unwrap();
        let mut login = fixture();
        login.metadata["access_token"] = json!("n-access");
        login.metadata["refresh_token"] = json!("n-refresh");
        rig.manager
            .adopt_completed_token_command(&login, command)
            .unwrap();
        assert_eq!(unresolved(&rig), None);
        assert_eq!(hold_name(&rig), None);
        assert_eq!(rig.live().metadata["refresh_token"], "n-refresh");
        assert!(!marked_outer(&rig));
        assert_eq!(probe.sent(), 1);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_observation_does_not_overwrite_the_protected_partition() {
    // B-29 steps 1 and 3: while the state is present the file the record now
    // names holds the old chain A, and an observation of it takes the file's
    // other keys but not A over the live B. Without the state it takes A.
    bounded(async {
        let (lost, e_path) = fenced_unproven().await;
        let Lost { rig, probe, .. } = lost;
        // The landed rotation leaves its operation on the record, which would
        // keep B on its own; take it off so only the state protects B.
        rig.manager
            .edit_unsaved("codex.json", |auth| {
                auth.codex_pending_refresh.clear();
                Ok::<_, ()>(true)
            })
            .unwrap();
        let e_file = e_path.join("codex.json");
        let mut edited = read_file(&e_file);
        edited["note"] = json!("from the file");
        std::fs::write(&e_file, serde_json::to_vec(&edited).unwrap()).unwrap();
        let result = super::siwc_persistence::observed(&rig.manager, &rig.store.files, &rig.live());
        assert!(matches!(
            result,
            crate::manager::AdoptionResult::Published { .. }
        ));
        let live = rig.live();
        assert_eq!(live.metadata["note"], "from the file", "other keys adopted");
        assert_eq!(live.metadata["refresh_token"], "b-refresh", "B kept");
        assert_eq!(live.metadata["access_token"], "b-access");
        assert_eq!(unresolved(&rig), Some((true, false)));
        assert!(!marked_outer(&rig));
        assert_eq!(probe.sent(), 1);

        // Control: the same observation with no state takes the file's chain.
        let control = super::siwc_persistence::rig();
        control
            .manager
            .edit_unsaved("codex.json", |auth| {
                auth.metadata
                    .insert("access_token".into(), json!("b-access"));
                auth.metadata
                    .insert("refresh_token".into(), json!("b-refresh"));
                Ok::<_, ()>(true)
            })
            .unwrap();
        assert_eq!(unresolved(&control), None);
        super::siwc_persistence::observed(&control.manager, &control.store.files, &control.live());
        assert_eq!(
            control.live().metadata["refresh_token"],
            "outer-refresh-old",
            "without the state the file's chain is adopted"
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_saved_edit_does_not_write_the_protected_partition() {
    // B-29 step 2: a settings edit saved while the state is present writes the
    // setting to the file the record names, and neither the live B nor A moves.
    bounded(async {
        let (lost, e_path) = fenced_unproven().await;
        let Lost { rig, probe, .. } = lost;
        rig.manager
            .edit("codex.json", |auth| {
                auth.metadata.insert("note".into(), json!("saved"));
                Ok::<_, ()>(true)
            })
            .unwrap();
        let file = read_file(&e_path.join("codex.json"));
        assert_eq!(file["note"], "saved", "the setting is written");
        assert_eq!(file["refresh_token"], "outer-refresh-old", "B is not");
        assert_eq!(file["access_token"], "outer-old");
        assert_eq!(rig.live().metadata["refresh_token"], "b-refresh");
        assert_eq!(unresolved(&rig), Some((true, false)));
        assert!(!marked_outer(&rig));
        assert_eq!(probe.sent(), 1);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_saved_edit_read_back_keeps_the_protected_partition_by_the_state() {
    // B-29 step 2, the read-back: a save is read back only when it carries an
    // unwritten rotation, so B stays pending here (every lease commit fails).
    // The mark is taken off so that only the state protects B, as when the
    // mark has been cleared by a verified write of the outer partition while
    // the state remains. The read-back of the file, which holds A, must not
    // take A over the live B.
    bounded(async {
        let Lost {
            rig, probe, made, ..
        } = pending_unproven().await;
        let q = sibling_dir(&rig);
        chain_a_in(&rig, q.path());
        rig.store.files.set_base_dir(q.path());
        let q_file = q.path().join("codex.json");
        lock(&made).push(q);
        rig.manager
            .lock()
            .auths
            .get_mut("codex.json")
            .unwrap()
            .undurable
            .outer = false;
        assert!(unwritten_pending(&rig) > 0, "B waits to be saved");
        rig.manager
            .edit("codex.json", |auth| {
                auth.metadata.insert("note".into(), json!("saved"));
                Ok::<_, ()>(true)
            })
            .unwrap();
        let file = read_file(&q_file);
        assert_eq!(file["note"], "saved", "the setting is written");
        assert_eq!(file["refresh_token"], "outer-refresh-old", "B is not");
        let live = rig.live();
        assert_eq!(live.metadata["refresh_token"], "b-refresh", "live is B");
        assert_eq!(live.metadata["access_token"], "b-access");
        assert_eq!(live.metadata["note"], "saved");
        assert_eq!(unresolved(&rig), Some((true, false)));
        assert_eq!(probe.sent(), 1);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_token_command_after_the_rebind_replaces_the_chain_and_the_state() {
    // B-29 step 4: a login the user completes against the file the record now
    // names is the newest word on that chain; it clears the state.
    bounded(async {
        let (lost, e_path) = fenced_unproven().await;
        let Lost { rig, probe, .. } = lost;
        let command = rig
            .manager
            .capture_token_command("codex.json", partition(true))
            .unwrap();
        let e_file = e_path.join("codex.json");
        std::fs::write(&e_file, with_outer(read_file(&e_file), "n")).unwrap();
        let mut login = fixture();
        login.metadata["access_token"] = json!("n-access");
        login.metadata["refresh_token"] = json!("n-refresh");
        rig.manager
            .adopt_completed_token_command(&login, command)
            .unwrap();
        assert_eq!(rig.live().metadata["refresh_token"], "n-refresh");
        assert_eq!(unresolved(&rig), None);
        assert!(!marked_outer(&rig));
        assert_eq!(hold_name(&rig), None);
        assert_eq!(probe.sent(), 1);
    })
    .await;
}

/// B-30's start: the pass sent A and got B, the lease commit of B fails at
/// every attempt so B stays pending in memory, and the publication cannot prove
/// the record's file, so the state is recorded as well.
async fn pending_unproven() -> Lost {
    let (rig, probe, clock) = clocked_rig("b");
    rig.store
        .fail_lease_commit
        .store(u32::MAX, Ordering::SeqCst);
    *lock(&rig.manager.shared.lease_retry_window) = Some(Duration::from_millis(1));
    let (calls, made) = (Arc::new(AtomicU32::new(0)), Made::default());
    let entered = Arc::new(AtomicBool::new(false));
    let seen = entered.clone();
    probe.set_at_entry(move |_| seen.store(true, Ordering::SeqCst));
    let rebind = fresh_binder(&rig, &made);
    on_origin_checks(&rig, calls.clone(), move |_| {
        if entered.load(Ordering::SeqCst) {
            rebind();
        }
    });
    let _ = rig.manager.force_refresh("codex.json").await;
    disarm(&rig);
    Lost {
        rig,
        probe,
        clock,
        made,
        calls: calls.load(Ordering::SeqCst),
    }
}

/// A copy of the chain A at `dir`, as a file the record could be rebound to.
fn chain_a_in(rig: &Rig, dir: &Path) {
    let mut older = read_file(&rig.path());
    older["access_token"] = json!("outer-old");
    older["refresh_token"] = json!("outer-refresh-old");
    std::fs::write(dir.join("codex.json"), serde_json::to_vec(&older).unwrap()).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn a_pending_rotation_waits_for_the_resolution_to_land() {
    // B-30 steps 1 to 5: B is pending and the state is present. A settings
    // edit writes the setting to the new file but not B; the resolution proves
    // a different file, so the pending commit lands B there and the mark ends.
    bounded(async {
        let Lost {
            rig,
            probe,
            clock,
            made,
            ..
        } = pending_unproven().await;
        assert_eq!(probe.sent(), 1);
        assert_eq!(unresolved(&rig), Some((true, false)));
        assert_eq!(rig.live().metadata["refresh_token"], "b-refresh");
        assert!(unwritten_pending(&rig) > 0, "B is pending");
        let q = sibling_dir(&rig);
        chain_a_in(&rig, q.path());
        rig.store.files.set_base_dir(q.path());
        let q_file = q.path().join("codex.json");
        lock(&made).push(q);
        assert!(marked_outer(&rig), "B is marked while it is pending");
        rig.manager
            .edit("codex.json", |auth| {
                auth.metadata.insert("note".into(), json!("saved"));
                Ok::<_, ()>(true)
            })
            .unwrap();
        let file = read_file(&q_file);
        assert_eq!(file["note"], "saved", "the setting is written");
        assert_eq!(
            file["refresh_token"], "outer-refresh-old",
            "B is not written"
        );
        assert_eq!(owed_reason(&rig), None, "the save was written, not owed");
        assert!(marked_outer(&rig), "the edit did not clear the mark");
        assert!(
            unwritten_pending(&rig) > 0,
            "B is still an unwritten pending operation"
        );
        assert_eq!(unresolved(&rig), Some((true, false)));
        // The commit works again; the resolution finds a different file.
        rig.store.fail_lease_commit.store(0, Ordering::SeqCst);
        clock.advance(Duration::from_secs(60));
        state_pass(&rig).await.unwrap();
        assert_eq!(unresolved(&rig), None);
        assert_eq!(probe.sent(), 1, "nothing more was sent");
        assert_eq!(read_file(&q_file)["refresh_token"], "b-refresh", "B landed");
        assert!(!marked_outer(&rig));
        // The landed commit is what writes the operation: had the edit written
        // B, this would have held before the resolution. (A written operation
        // stays on the record until a later edit prunes it.)
        assert_eq!(
            unwritten_pending(&rig),
            0,
            "no unwritten pending operation is left"
        );
        // A peer's later chain is adopted and sent from.
        std::fs::write(&q_file, with_outer(read_file(&q_file), "c")).unwrap();
        let probe = probe.rotating("d");
        rig.manager.force_refresh("codex.json").await.unwrap();
        assert_eq!(probe.sent(), 2);
        assert_eq!(probe.sent_refresh(1), "c-refresh");
    })
    .await;
}

fn owed_reason(rig: &Rig) -> Option<String> {
    let state = rig.manager.lock();
    state.auths["codex.json"]
        .owed_save
        .as_ref()
        .map(|owed| format!("{:?}", owed.reason))
}

fn save_note(rig: &Rig) {
    let _ = rig.manager.edit("codex.json", |auth| {
        auth.metadata.insert("note".into(), json!("saved"));
        Ok::<_, ()>(true)
    });
}

#[tokio::test(flavor = "current_thread")]
async fn a_saved_edit_to_a_missing_file_is_owed_and_creates_nothing() {
    // B-30 (b): the file the record names is gone. The save is owed, the file
    // is not created with a partition the state withholds, and pointing the
    // record back at D lets a pass settle it.
    bounded(async {
        let Lost {
            rig,
            probe,
            clock,
            made,
            ..
        } = pending_unproven().await;
        let q = sibling_dir(&rig);
        rig.store.files.set_base_dir(q.path());
        let q_file = q.path().join("codex.json");
        lock(&made).push(q);
        save_note(&rig);
        assert_eq!(owed_reason(&rig).as_deref(), Some("MissingFile"));
        assert!(!q_file.exists(), "no file was created");
        assert_eq!(unresolved(&rig), Some((true, false)));
        // The scheduler's pass cannot settle it while the file is missing: it
        // sends nothing and the save stays owed.
        clock.advance(Duration::from_secs(60));
        let error = state_pass(&rig).await.unwrap_err();
        assert!(
            error.to_string().contains("credential file is missing"),
            "{error}"
        );
        assert_eq!(probe.sent(), 1, "the pass sent nothing");
        assert_eq!(owed_reason(&rig).as_deref(), Some("MissingFile"));
        assert!(!q_file.exists(), "still no file");
        // The record names D again, which holds A: the pass proves it, lands
        // B and the owed setting there, and ends the state.
        rig.store.fail_lease_commit.store(0, Ordering::SeqCst);
        rig.store.files.set_base_dir(rig.dir.path());
        chain_a_in(&rig, rig.dir.path());
        clock.advance(Duration::from_secs(3600));
        state_pass(&rig).await.unwrap();
        let file = read_file(&rig.path());
        assert_eq!(file["refresh_token"], "b-refresh", "B landed");
        assert_eq!(file["note"], "saved", "the owed setting landed");
        assert_eq!(owed_reason(&rig), None, "the owed save is settled");
        assert_eq!(unresolved(&rig), None, "the state ended");
        assert!(!marked_outer(&rig), "the mark ended");
        assert_eq!(probe.sent(), 1, "the executor was not called again");
        assert!(!q_file.exists(), "the missing file was never created");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_saved_edit_to_a_file_without_the_partition_is_owed_and_changes_nothing() {
    // B-30 (c): the file lacks the withheld partition, so the save is owed and
    // the file is exactly as it was.
    bounded(async {
        let Lost {
            rig,
            probe,
            clock,
            made,
            ..
        } = pending_unproven().await;
        let q = sibling_dir(&rig);
        let q_file = q.path().join("codex.json");
        let mut bare = read_file(&rig.path());
        let object = bare.as_object_mut().unwrap();
        object.remove("access_token");
        object.remove("refresh_token");
        let bytes = serde_json::to_vec(&bare).unwrap();
        std::fs::write(&q_file, &bytes).unwrap();
        rig.store.files.set_base_dir(q.path());
        lock(&made).push(q);
        save_note(&rig);
        assert_eq!(owed_reason(&rig).as_deref(), Some("MissingPartition"));
        assert_eq!(std::fs::read(&q_file).unwrap(), bytes, "byte for byte");
        assert_eq!(unresolved(&rig), Some((true, false)));
        // The record names D again, which holds A: the pass proves it and lands
        // B and the owed setting there, and the other file is never touched.
        rig.store.fail_lease_commit.store(0, Ordering::SeqCst);
        rig.store.files.set_base_dir(rig.dir.path());
        chain_a_in(&rig, rig.dir.path());
        clock.advance(Duration::from_secs(60));
        state_pass(&rig).await.unwrap();
        let file = read_file(&rig.path());
        assert_eq!(file["refresh_token"], "b-refresh", "B landed");
        assert_eq!(file["note"], "saved", "the owed setting landed");
        assert_eq!(owed_reason(&rig), None, "the owed save is settled");
        assert_eq!(unresolved(&rig), None, "the state ended");
        assert!(!marked_outer(&rig), "the mark ended");
        assert_eq!(probe.sent(), 1, "the executor was not called again");
        assert_eq!(std::fs::read(&q_file).unwrap(), bytes, "still untouched");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_saved_edit_to_a_file_without_markers_leaves_its_chain_alone() {
    // B-30 (d): the file has no OAuth markers, so the save is written and every
    // key the file had before, the outer chain's included, is as it was; with
    // no state the same save would add the live chain.
    bounded(async {
        let Lost { rig, made, .. } = pending_unproven().await;
        // Holds an access token and an account id, so the save is not deferred,
        // but not the refresh token or the refresh time.
        let plain = json!({ "type": "codex", "access_token": "a-token", "account_id": "acct" });
        let q = sibling_dir(&rig);
        let q_file = q.path().join("codex.json");
        std::fs::write(&q_file, serde_json::to_vec(&plain).unwrap()).unwrap();
        rig.store.files.set_base_dir(q.path());
        lock(&made).push(q);
        let before = read_file(&q_file);
        save_note(&rig);
        let file = read_file(&q_file);
        assert_eq!(file["note"], "saved", "the setting is written");
        assert_eq!(owed_reason(&rig), None);
        for key in OUTER_KEYS {
            assert_eq!(file.get(key), before.get(key), "{key} is as it was");
        }
        assert!(file.get("refresh_token").is_none(), "no chain was added");

        let control = super::siwc_persistence::rig();
        let other = sibling_dir(&control);
        let other_file = other.path().join("codex.json");
        std::fs::write(&other_file, serde_json::to_vec(&plain).unwrap()).unwrap();
        control.store.files.set_base_dir(other.path());
        save_note(&control);
        assert_eq!(
            read_file(&other_file)["refresh_token"],
            "outer-refresh-old",
            "without the state the live chain is written"
        );
    })
    .await;
}

/// B-33's start: a record whose pass rotated the SIWC partition only. Every
/// lease commit of it fails, so the rotation stays pending and marked, and the
/// publication cannot prove where the record's file is, so the state holds the
/// SIWC partition beside the mark.
async fn siwc_state_record() -> (Rig, crate::gate::ActiveGate, Made) {
    let seeded = seeded();
    let gate = seeded.gate.clone();
    let store = observe(seeded.files.clone(), false);
    let models = Arc::new(FakeModels::default());
    models.register("codex.json", &["gpt-5"]);
    let manager = Manager::new(Settings::default(), models.clone(), Some(store.clone()));
    manager.set_gate(seeded.gate.clone());
    manager.register_unsaved(fixture()).unwrap();
    let rig = Rig {
        dir: seeded.dir,
        store,
        manager,
        models,
    };
    rig.store
        .fail_lease_commit
        .store(u32::MAX, Ordering::SeqCst);
    *lock(&rig.manager.shared.lease_retry_window) = Some(Duration::from_millis(1));
    let entered = Arc::new(AtomicBool::new(false));
    let seen = entered.clone();
    let executor = FakeExecutor::new("codex");
    executor.set_refresh(move |before| {
        seen.store(true, Ordering::SeqCst);
        let mut after = before.clone();
        after.metadata["siwc"]["access_token"] = json!("siwc-new");
        after.metadata["siwc"]["refresh_token"] = json!("siwc-refresh-new");
        after.codex_refresh_failure = None;
        after.codex_persist_intent = crate::auth::codex_refresh::CodexPersistIntent::Refresh(
            crate::auth::codex_refresh::CredentialRefreshPatch::new(
                &before.metadata,
                &after.metadata,
                false,
                true,
            ),
        );
        Ok(after)
    });
    rig.manager.register_executor(executor);
    let made = Made::default();
    let rebind = fresh_binder(&rig, &made);
    on_origin_checks(&rig, Arc::new(AtomicU32::new(0)), move |_| {
        if entered.load(Ordering::SeqCst) {
            rebind();
        }
    });
    let _ = rig.manager.force_refresh("codex.json").await;
    disarm(&rig);
    (rig, gate, made)
}

fn siwc_baselines(rig: &Rig) -> usize {
    rig.manager.lock().auths["codex.json"]
        .undurable_baselines
        .siwc
        .len()
}

fn marked_siwc(rig: &Rig) -> bool {
    rig.manager.lock().auths["codex.json"].undurable.siwc
}

/// The file the record names after a 401 adopts it: the record's own file with
/// the outer chain replaced, as OAuth or as an API key.
fn write_adoptable_file(rig: &Rig, name: &str, api_key: bool) {
    let mut file = read_file(&rig.path());
    file["use_siwc"] = json!(false);
    if api_key {
        file["auth_kind"] = json!("apikey");
    }
    std::fs::write(rig.path(), with_outer(file, name)).unwrap();
}

async fn adopt_after_rejection(rig: &Rig) -> Option<Arc<Auth>> {
    let failed = rig.live();
    rig.manager
        .try_refresh_after_unauthorized(
            &failed,
            &crate::exec::ExecError::upstream(401, "synthetic rejection"),
            false,
        )
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn a_record_that_becomes_ineligible_drops_the_state_and_keeps_its_marks() {
    // B-33: an ineligible record is never leased, so the state it carries could
    // never resolve; it goes, and the marks beside it stay. Adoption of a file
    // that keeps the record eligible keeps the state.
    bounded(async {
        let (rig, gate, _made) = siwc_state_record().await;
        assert_eq!(unresolved(&rig), Some((false, true)), "SIWC only");
        assert!(marked_siwc(&rig), "the rotation is marked");
        assert!(!marked_outer(&rig));
        let baselines = siwc_baselines(&rig);
        assert!(baselines > 0, "the mark has baselines");
        // The record is a config entry on the outer chain. Its metadata still
        // says OAuth, so these edits leave it eligible; only the file read by
        // the adoption below makes it an API key.
        rig.manager
            .edit_unsaved("codex.json", |auth| {
                auth.metadata.insert("use_siwc".into(), json!(false));
                auth.attributes.remove("source_backend");
                auth.attributes.insert("source".into(), "config:rig".into());
                Ok::<_, ()>(true)
            })
            .unwrap();
        assert!(!rig.manager.permanently_ineligible(&rig.live()));
        assert_eq!(unresolved(&rig), Some((false, true)), "the edit keeps it");
        rig.store.files.set_base_dir(rig.dir.path());
        gate.close(gate.epoch()).await.unwrap();

        // Control: the file is still OAuth, so the adoption keeps the state.
        write_adoptable_file(&rig, "peer", false);
        let adopted = adopt_after_rejection(&rig).await.expect("adopted");
        assert_eq!(adopted.metadata["access_token"], "peer-access");
        assert!(!rig.manager.permanently_ineligible(&rig.live()));
        assert_eq!(unresolved(&rig), Some((false, true)), "control keeps it");

        // The file is now an API key: the adoption makes the record ineligible.
        write_adoptable_file(&rig, "api", true);
        let adopted = adopt_after_rejection(&rig).await.expect("adopted");
        assert_eq!(adopted.metadata["access_token"], "api-access");
        assert!(rig.manager.permanently_ineligible(&rig.live()));
        assert_eq!(unresolved(&rig), None, "the state is dropped");
        assert!(marked_siwc(&rig), "the mark stays");
        assert_eq!(siwc_baselines(&rig), baselines, "so do its baselines");
        assert!(!marked_outer(&rig));
        assert_eq!(hold_name(&rig), None, "no hold is left");
    })
    .await;
}
