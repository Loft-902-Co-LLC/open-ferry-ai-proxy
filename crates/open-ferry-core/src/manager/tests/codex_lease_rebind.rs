//! Tests of the Codex refresh lease: origin binding and fence 3 (B6b fills
//! this file).

use super::siwc_persistence::{Probe, Rig, SaveHook, bounded, discard_work, fixture, rig};
use crate::auth::classification::ATTRIBUTE_PATH;
use crate::auth::codex_refresh::{CodexPersistIntent, CredentialRefreshPatch};
use crate::auth::{Auth, AuthError, CodexOrigin, ModelState, QuotaState, Status};
use crate::manager::{Entry, ManagerError, MarkCause, UndurablePartitions, lock};
use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const ID: &str = "codex.json";

/// The seams of the leased tail's ordinary save, in the order they fire.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Seam {
    /// After the save mutex is held and before the write resolves its path.
    Admitted,
    /// After the write, before the read-back.
    Reconcile,
    /// After the read-back, before fence 3 compares.
    ReadBack,
}

/// Runs `act` once, at `seam`, for the first save that carries a rotation.
fn at(rig: &Rig, seam: Seam, act: impl Fn() + Send + Sync + 'static) {
    let armed = AtomicBool::new(true);
    let hook: SaveHook = Arc::new(move |auth: &Auth| {
        if matches!(auth.codex_persist_intent, CodexPersistIntent::Refresh(_))
            && armed.swap(false, Ordering::SeqCst)
        {
            act();
        }
    });
    let shared = &rig.manager.shared;
    let slot = match seam {
        Seam::Admitted => &shared.persist_admitted_hook,
        Seam::Reconcile => &shared.reconcile_hook,
        Seam::ReadBack => &shared.read_back_hook,
    };
    *lock(slot) = Some(hook);
}

/// The JSON a credential file holds.
fn file(path: &Path) -> Map<String, Value> {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

/// Sets the outer tokens a file holds to `name`'s, and nothing else.
fn write_outer(path: &Path, name: &str) {
    let mut map = file(path);
    map.insert("access_token".into(), json!(format!("{name}-access")));
    map.insert("refresh_token".into(), json!(format!("{name}-refresh")));
    std::fs::write(path, serde_json::to_vec(&map).unwrap()).unwrap();
}

/// The disk-only `note` the Reconcile seam writes into the file the
/// read-back reads. Live carries it after the pass only if the overlay ran.
const NOTE: &str = "disk-only";

fn write_note(path: &Path, note: &str) {
    let mut map = file(path);
    map.insert("note".into(), json!(note));
    std::fs::write(path, serde_json::to_vec(&map).unwrap()).unwrap();
}

/// `act`, after writing [`NOTE`] into `path`.
fn noted(path: &Path, act: impl Fn() + Send + Sync + 'static) -> impl Fn() + Send + Sync + 'static {
    let path = path.to_path_buf();
    move || {
        write_note(&path, NOTE);
        act();
    }
}

/// A peer process: takes `path`'s credential lock, writes `name`'s outer
/// tokens over the file and releases.
async fn peer_writes(path: &Path, name: &str) {
    let held = crate::auth::credential_lock::acquire_with_timeout(
        path.to_path_buf(),
        Duration::from_secs(5),
    )
    .await
    .expect("the peer takes the credential lock");
    write_outer(path, name);
    drop(held);
}

/// A second spelling of the directory `dir`: on Windows a case variant (needs
/// a case-insensitive directory, the NTFS default), on Unix a directory
/// symlink made inside `holder`.
#[cfg(any(unix, windows))]
fn alias_dir(dir: &Path, holder: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let _ = holder;
        let name = dir.file_name().unwrap().to_string_lossy().to_uppercase();
        dir.with_file_name(name)
    }
    #[cfg(unix)]
    {
        let alias = holder.join("alias");
        std::os::unix::fs::symlink(dir, &alias).unwrap();
        alias
    }
}

/// The precondition of every alias test, made with the crate directly: both
/// spellings open to one file, and they differ lexically. A case-sensitive
/// directory fails here instead of passing vacuously.
#[cfg(any(unix, windows))]
fn assert_alias(original: &Path, alias: &Path) {
    assert_ne!(original, alias, "the alias must differ lexically");
    let a = same_file::Handle::from_path(original).unwrap();
    let b = same_file::Handle::from_path(alias).unwrap();
    assert!(a == b, "the two spellings are not one file");
}

/// The level and text of each event logged on this thread, fields included.
type Lines = Arc<Mutex<Vec<(tracing::Level, String)>>>;

fn capture_logs() -> (tracing::dispatcher::DefaultGuard, Lines) {
    use tracing::field::{Field, Visit};

    struct Text(String);
    impl Visit for Text {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if !self.0.is_empty() {
                self.0.push(' ');
            }
            if field.name() == "message" {
                self.0.push_str(&format!("{value:?}"));
            } else {
                self.0.push_str(&format!("{}={value:?}", field.name()));
            }
        }
    }

    struct Capture(Lines);
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
            let mut text = Text(String::new());
            event.record(&mut text);
            lock(&self.0).push((*event.metadata().level(), text.0));
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    crate::test_tracing::keep_every_callsite_open();
    let lines: Lines = Arc::new(Mutex::new(Vec::new()));
    let dispatch = tracing::Dispatch::new(Capture(Arc::clone(&lines)));
    (tracing::dispatcher::set_default(&dispatch), lines)
}

/// Every token a test here writes: none may appear in a log line.
const TOKENS: [&str; 8] = [
    "outer-old",
    "outer-refresh-old",
    "b-access",
    "b-refresh",
    "c-access",
    "c-refresh",
    "d-access",
    "d-refresh",
];

fn logged(lines: &Lines, level: tracing::Level, needle: &str) -> usize {
    let lines = lock(lines);
    for (_, text) in lines.iter() {
        for token in TOKENS {
            assert!(!text.contains(token), "a log line carries a token");
        }
    }
    lines
        .iter()
        .filter(|(l, text)| *l == level && text.contains(needle))
        .count()
}

/// Makes the executor rotate the outer partition by the token it is sent:
/// A -> B, B -> C, C -> D. A rotation depends on its input and not on the
/// number of sends, so a send of a token the test did not expect shows in the
/// result.
fn chain(probe: &Probe) {
    probe.fake.set_refresh(|before| {
        let next = match before.metadata["refresh_token"].as_str() {
            Some("outer-refresh-old") => "b",
            Some("b-refresh") => "c",
            Some("c-refresh") => "d",
            _ => panic!("the executor was sent a token it cannot rotate"),
        };
        let mut after = before.clone();
        after
            .metadata
            .insert("access_token".into(), json!(format!("{next}-access")));
        after
            .metadata
            .insert("refresh_token".into(), json!(format!("{next}-refresh")));
        after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
            &before.metadata,
            &after.metadata,
            true,
            false,
        ));
        Ok(after)
    });
}

/// A relative registration in D (`codex.json`, disk and live at A), with an
/// executor that rotates the outer partition to B. Q is a copy of the file in
/// another directory whose `note` is its own.
struct Scene {
    rig: Rig,
    probe: Arc<Probe>,
    q: tempfile::TempDir,
    /// P: the origin of D's file, as the lease pins it.
    origin: CodexOrigin,
}

impl Scene {
    fn new() -> Self {
        let rig = rig();
        let probe = Probe::new("codex", false);
        chain(&probe);
        rig.manager.register_executor(probe.clone());
        let q = tempfile::tempdir_in(rig.dir.path().parent().unwrap()).unwrap();
        let mut copy = file(&rig.path());
        copy.insert("note".into(), json!("from-q"));
        std::fs::write(q.path().join(ID), serde_json::to_vec(&copy).unwrap()).unwrap();
        let origin = CodexOrigin::of(&rig.path()).unwrap();
        Self {
            rig,
            probe,
            q,
            origin,
        }
    }

    /// Rebinds the store to `dir`.
    fn rebind(&self, dir: &Path) -> impl Fn() + Send + Sync + 'static {
        let (files, dir) = (self.rig.store.files.clone(), dir.to_path_buf());
        move || files.set_base_dir(&dir)
    }

    fn read<T>(&self, read: impl FnOnce(&Entry) -> T) -> T {
        read(&self.rig.manager.lock().auths[ID])
    }

    fn live_note(&self) -> Value {
        self.rig.live().metadata["note"].clone()
    }

    /// No partition is marked and no state is recorded.
    fn assert_clean(&self) {
        self.read(|entry| {
            assert_eq!(entry.undurable, UndurablePartitions::default());
            assert!(entry.undurable_causes.outer.is_none());
            assert!(entry.undurable_causes.siwc.is_none());
            assert!(entry.unresolved_origin.is_none(), "a state was recorded");
        });
    }

    /// The state names P and the outer partition, nothing is marked, the
    /// rotation is live and the next refresh is held back.
    fn assert_recorded(&self, started: chrono::DateTime<chrono::Utc>, lines: &Lines) {
        self.read(|entry| {
            let state = entry
                .unresolved_origin
                .as_ref()
                .expect("the state is recorded");
            assert!(state.origin == self.origin, "the state names P");
            assert_eq!(
                state.partitions,
                UndurablePartitions {
                    outer: true,
                    siwc: false
                }
            );
            assert_eq!(entry.undurable, UndurablePartitions::default());
            assert!(entry.undurable_causes.outer.is_none());
            let held = entry.auth.next_refresh_after.expect("a wait was set");
            assert!(held >= started + chrono::Duration::seconds(5), "{held}");
            assert!(matches!(
                entry.auth.codex_persist_intent,
                CodexPersistIntent::Configuration
            ));
        });
        assert_eq!(self.rig.live().metadata["refresh_token"], "b-refresh");
        assert_eq!(self.live_note(), "initial", "nothing was overlaid");
        assert_eq!(
            logged(lines, tracing::Level::WARN, "origin unresolved"),
            1,
            "step 13 warns once"
        );
    }

    /// The pass after a rebind to Q: it leases Q, proves P against Q
    /// `Different`, marks the rotation with its baselines and clears the
    /// state, keeps live's B, sends B (never A) and lands C in Q.
    async fn assert_rebound_follow_up(&self) {
        self.rig.manager.force_refresh(ID).await.unwrap();
        assert_eq!(self.probe.sent(), 2);
        assert_eq!(self.probe.sent_refresh(1), "b-refresh", "B is sent, not A");
        assert_eq!(self.probe.reused(), 0, "a spent token was presented again");
        assert_eq!(file(&self.q.path().join(ID))["refresh_token"], "c-refresh");
        assert_eq!(file(&self.rig.path())["refresh_token"], "b-refresh");
        assert_eq!(self.rig.live().metadata["refresh_token"], "c-refresh");
        self.assert_clean();
    }

    /// A peer takes P's credential lock through its original spelling and
    /// writes C over B. The next pass adopts C, sends C (never B) and commits
    /// D, which both spellings read.
    async fn assert_peer_follow_up(&self) {
        peer_writes(&self.rig.path(), "c").await;
        self.rig.manager.force_refresh(ID).await.unwrap();
        assert_eq!(self.probe.sent(), 2);
        assert_eq!(self.probe.sent_refresh(1), "c-refresh", "C is sent, not B");
        assert_eq!(self.probe.reused(), 0, "a spent token was presented again");
        assert_eq!(file(&self.rig.path())["refresh_token"], "d-refresh");
        assert_eq!(self.rig.live().metadata["refresh_token"], "d-refresh");
        self.assert_clean();
    }

    /// The rotation commit never lands: every attempt fails and the retry
    /// window is 1 ms, so publication leaves B pending and marked and the
    /// ordinary save carries it.
    fn exhaust_commits(&self) {
        self.exhaust_commits_within(Duration::from_millis(1));
    }

    fn exhaust_commits_within(&self, window: Duration) {
        self.rig
            .store
            .fail_lease_commit
            .store(u32::MAX, Ordering::SeqCst);
        *lock(&self.rig.manager.shared.lease_retry_window) = Some(window);
    }

    /// Runs `act` in the executor, once the send has begun.
    fn at_entered(&self, act: impl Fn() + Send + Sync + 'static) {
        self.probe.set_at_entry(move |_| act());
    }

    /// Respells the registration's path, keeping its epoch.
    fn respell(&self, path: &Path) -> impl Fn() + Send + Sync + 'static {
        let (manager, path) = (
            self.rig.manager.clone(),
            path.to_string_lossy().into_owned(),
        );
        move || {
            manager
                .edit_unsaved(ID, |auth| {
                    auth.attributes.insert(ATTRIBUTE_PATH.into(), path.clone());
                    Ok::<_, ()>(true)
                })
                .unwrap();
        }
    }

    /// What the live record holds when the lease is released: the pending
    /// operations and whether the outer partition is marked.
    fn watch_tail(&self) -> Arc<Mutex<Option<(usize, bool)>>> {
        let seen = Arc::new(Mutex::new(None));
        let (manager, sink) = (self.rig.manager.clone(), Arc::clone(&seen));
        *lock(&self.rig.manager.shared.lease_tail_hook) = Some(Arc::new(move |_: &Auth| {
            let state = manager.lock();
            let entry = &state.auths[ID];
            *lock(&sink) = Some((
                entry.auth.codex_pending_refresh.len(),
                entry.undurable.outer,
            ));
        }));
        seen
    }

    /// The ordinary save wrote B once, through the bound store.
    fn assert_written_once(&self) {
        assert_eq!(self.probe.sent(), 1);
        assert_eq!(self.rig.store.save_bound_calls.load(Ordering::SeqCst), 1);
        assert_eq!(lock(&self.rig.store.writes).len(), 1);
        assert_eq!(file(&self.rig.path())["refresh_token"], "b-refresh");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn without_a_rebind_the_leased_save_is_kept() {
    // B-21 (i): the control. Nothing moves, so fence 3 keeps the save.
    bounded(async {
        let scene = Scene::new();
        let (_guard, lines) = capture_logs();
        at(&scene.rig, Seam::Reconcile, noted(&scene.rig.path(), || {}));
        scene.rig.manager.force_refresh(ID).await.unwrap();
        scene.assert_written_once();
        scene.assert_clean();
        assert_eq!(scene.rig.live().metadata["refresh_token"], "b-refresh");
        assert_eq!(scene.live_note(), NOTE, "the read-back overlay ran");
        assert_eq!(logged(&lines, tracing::Level::WARN, "origin unresolved"), 0);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_rebind_before_the_reconcile_records_the_origin() {
    // B-21 (ii): fence 2 wrote P, then the store moved to Q, so the read-back
    // and the live record both name Q.
    bounded(async {
        let scene = Scene::new();
        let (_guard, lines) = capture_logs();
        at(&scene.rig, Seam::Reconcile, scene.rebind(scene.q.path()));
        let started = chrono::Utc::now();
        scene.rig.manager.force_refresh(ID).await.unwrap();
        scene.assert_written_once();
        scene.assert_recorded(started, &lines);
        scene.assert_rebound_follow_up().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_rebind_before_the_read_back_records_the_origin() {
    // B-21 (iii): the read-back still reads P, but live now names Q.
    bounded(async {
        let scene = Scene::new();
        let (_guard, lines) = capture_logs();
        at(&scene.rig, Seam::Reconcile, noted(&scene.rig.path(), || {}));
        at(&scene.rig, Seam::ReadBack, scene.rebind(scene.q.path()));
        let started = chrono::Utc::now();
        scene.rig.manager.force_refresh(ID).await.unwrap();
        scene.assert_written_once();
        scene.assert_recorded(started, &lines);
        scene.assert_rebound_follow_up().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_rebind_away_and_back_records_nothing() {
    // B-21 (iv): the read-back reads Q, but live is back on P, which holds B.
    bounded(async {
        let scene = Scene::new();
        let (_guard, lines) = capture_logs();
        at(&scene.rig, Seam::Reconcile, scene.rebind(scene.q.path()));
        at(
            &scene.rig,
            Seam::ReadBack,
            scene.rebind(scene.rig.dir.path()),
        );
        scene.rig.manager.force_refresh(ID).await.unwrap();
        scene.assert_written_once();
        scene.assert_clean();
        assert_eq!(scene.rig.live().metadata["refresh_token"], "b-refresh");
        assert_eq!(scene.live_note(), "initial", "Q's file was not overlaid");
        assert_eq!(logged(&lines, tracing::Level::WARN, "origin unresolved"), 0);
        scene.assert_peer_follow_up().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_alias_bound_before_the_save_resolves_is_kept() {
    // B-21 (v): fence 2 proves P against alias/f `Same` and writes through it,
    // and W = R = L = alias/f, so the tail runs as today.
    bounded(async {
        let scene = Scene::new();
        let (_guard, lines) = capture_logs();
        let alias = alias_dir(scene.rig.dir.path(), scene.q.path());
        assert_alias(&scene.rig.path(), &alias.join(ID));
        at(&scene.rig, Seam::Admitted, scene.rebind(&alias));
        at(&scene.rig, Seam::Reconcile, noted(&alias.join(ID), || {}));
        scene.rig.manager.force_refresh(ID).await.unwrap();
        scene.assert_written_once();
        scene.assert_clean();
        assert_eq!(scene.rig.live().metadata["refresh_token"], "b-refresh");
        assert_eq!(scene.live_note(), NOTE, "the read-back overlay ran");
        assert_eq!(logged(&lines, tracing::Level::WARN, "origin unresolved"), 0);
        scene.assert_peer_follow_up().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_alias_bound_before_the_reconcile_records_the_origin() {
    // B-21 (vi), at `reconcile_hook`: L = alias/f is neither P nor W, so the
    // verdict is `Unproven`.
    bounded(async {
        let scene = Scene::new();
        let (_guard, lines) = capture_logs();
        let alias = alias_dir(scene.rig.dir.path(), scene.q.path());
        assert_alias(&scene.rig.path(), &alias.join(ID));
        at(&scene.rig, Seam::Reconcile, scene.rebind(&alias));
        let started = chrono::Utc::now();
        scene.rig.manager.force_refresh(ID).await.unwrap();
        scene.assert_written_once();
        scene.assert_recorded(started, &lines);
        // Step 4b proves P against alias/f under the shared lock: `Same`.
        scene.assert_peer_follow_up().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_alias_bound_before_the_read_back_records_the_origin() {
    // B-21 (vi), at `read_back_hook`: R = P, L = alias/f.
    bounded(async {
        let scene = Scene::new();
        let (_guard, lines) = capture_logs();
        let alias = alias_dir(scene.rig.dir.path(), scene.q.path());
        assert_alias(&scene.rig.path(), &alias.join(ID));
        at(&scene.rig, Seam::Reconcile, noted(&scene.rig.path(), || {}));
        at(&scene.rig, Seam::ReadBack, scene.rebind(&alias));
        let started = chrono::Utc::now();
        scene.rig.manager.force_refresh(ID).await.unwrap();
        scene.assert_written_once();
        scene.assert_recorded(started, &lines);
        scene.assert_peer_follow_up().await;
    })
    .await;
}

/// How a rebind leaves live naming P lexically (B-4's first two forms).
#[derive(Clone, Copy, Debug)]
enum Lexical {
    /// P's absolute `ATTRIBUTE_PATH`, and a rebind to an unrelated directory.
    AbsolutePath,
    /// A rebind to the directory already bound.
    SameDirectory,
}

async fn lexical_case(seam: Seam, form: Lexical) {
    let scene = Scene::new();
    let (_guard, lines) = capture_logs();
    let rebind = match form {
        Lexical::AbsolutePath => {
            let path = scene.rig.path().to_string_lossy().into_owned();
            scene
                .rig
                .manager
                .edit_unsaved(ID, |auth| {
                    auth.attributes.insert(ATTRIBUTE_PATH.into(), path.clone());
                    Ok::<_, ()>(true)
                })
                .unwrap();
            scene.rebind(scene.q.path())
        }
        Lexical::SameDirectory => scene.rebind(scene.rig.dir.path()),
    };
    // The overlay reads P in every form: the registration still names it.
    let path = scene.rig.path();
    match seam {
        Seam::Reconcile => at(&scene.rig, seam, noted(&path, rebind)),
        _ => {
            at(&scene.rig, Seam::Reconcile, noted(&path, || {}));
            at(&scene.rig, seam, rebind);
        }
    }
    scene.rig.manager.force_refresh(ID).await.unwrap();
    scene.assert_written_once();
    scene.assert_clean();
    assert_eq!(scene.rig.live().metadata["refresh_token"], "b-refresh");
    assert_eq!(
        scene.live_note(),
        NOTE,
        "the read-back overlay ran, {form:?}"
    );
    assert_eq!(
        logged(&lines, tracing::Level::WARN, "origin unresolved"),
        0,
        "{form:?}"
    );
    scene.assert_peer_follow_up().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_rebind_that_leaves_live_on_p_lexically_records_nothing() {
    // B-21, the lexical same-file variants (N1 (v5)): at either hook, in both
    // forms, R and L are `Same` and the tail runs as today.
    bounded(async {
        for seam in [Seam::Reconcile, Seam::ReadBack] {
            for form in [Lexical::AbsolutePath, Lexical::SameDirectory] {
                lexical_case(seam, form).await;
            }
        }
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_discard_during_the_leased_save_still_records_the_origin() {
    // B-21, decision 10: fence 3 records the state before the epoch and
    // discard gate, so a discard during the save does not lose it. The commit
    // is exhausted, so the ordinary save carries the unwritten rotation and a
    // discard has work to drop.
    bounded(async {
        let scene = Scene::new();
        let (_guard, lines) = capture_logs();
        scene.exhaust_commits();
        let epoch = scene.read(|entry| entry.discard_epoch);
        let manager = scene.rig.manager.clone();
        at(&scene.rig, Seam::Admitted, move || discard_work(&manager));
        at(&scene.rig, Seam::ReadBack, scene.rebind(scene.q.path()));
        scene.rig.manager.force_refresh(ID).await.unwrap();
        scene.assert_written_once();
        scene.read(|entry| {
            assert_ne!(entry.discard_epoch, epoch, "the discard failed the gate");
            let state = entry
                .unresolved_origin
                .as_ref()
                .expect("the state is recorded");
            assert!(state.origin == scene.origin, "the state names P");
            assert!(state.partitions.outer && !state.partitions.siwc);
            // The mark is the discard's, not fence 3's: the gate failed, so
            // the read-back was not applied.
            assert!(matches!(
                entry.undurable_causes.outer,
                Some(MarkCause::Different)
            ));
        });
        assert_eq!(scene.rig.live().metadata["refresh_token"], "b-refresh");
        assert_eq!(scene.live_note(), "initial", "Q's file was not overlaid");
        assert_eq!(logged(&lines, tracing::Level::WARN, "origin unresolved"), 1);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_commit_retry_stops_when_the_window_closes_during_the_backoff() {
    // B-22 (M1): the first attempt fails and the check before sleeping passes,
    // the 200 ms backoff runs, and the check after it stops the loop. A loop
    // that checked only before sleeping would start a second attempt here.
    bounded(async {
        let scene = Scene::new();
        scene.exhaust_commits_within(Duration::from_millis(100));
        let tail = scene.watch_tail();
        scene.rig.manager.force_refresh(ID).await.unwrap();
        assert_eq!(
            scene.rig.store.lease_commit_attempts.load(Ordering::SeqCst),
            1,
            "a second attempt started after the window closed"
        );
        // Publication left B pending and marked; the ordinary save then wrote
        // it.
        let seen = *lock(&tail);
        assert_eq!(seen, Some((1, true)));
        assert_eq!(scene.probe.sent(), 1);
        assert_eq!(file(&scene.rig.path())["refresh_token"], "b-refresh");
        assert_eq!(scene.rig.live().metadata["refresh_token"], "b-refresh");
        scene.assert_clean();
    })
    .await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_commit_retry_does_not_back_off_when_the_window_is_already_closed() {
    // B-22, the pre-sleep check: with a zero window the first failure ends the
    // loop before any backoff. The clock is paused, so a backoff would show as
    // 200 ms of virtual time, with no real waiting and no ordering by sleep.
    bounded(async {
        let scene = Scene::new();
        scene.exhaust_commits_within(Duration::ZERO);
        let started = tokio::time::Instant::now();
        scene.rig.manager.force_refresh(ID).await.unwrap();
        assert_eq!(
            scene.rig.store.lease_commit_attempts.load(Ordering::SeqCst),
            1
        );
        assert_eq!(
            started.elapsed(),
            Duration::ZERO,
            "a backoff ran although the window had closed"
        );
        assert_eq!(scene.rig.live().metadata["refresh_token"], "b-refresh");
        scene.assert_clean();
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_absolute_respelling_of_the_same_file_is_admitted_after_exhausted_commits() {
    // B-23 (a), the oracle's sequence: at `entered` the path becomes the
    // absolute path of the same file, so publication is `Admitted` (lexically
    // equal) and the tail's save is proved without I/O.
    bounded(async {
        let scene = Scene::new();
        let (_guard, lines) = capture_logs();
        scene.exhaust_commits();
        let tail = scene.watch_tail();
        scene.at_entered(scene.respell(&scene.rig.path()));
        scene.rig.manager.force_refresh(ID).await.unwrap();
        let seen = *lock(&tail);
        assert_eq!(seen, Some((1, true)), "final is pending under the marker");
        scene.assert_written_once();
        scene.assert_clean();
        scene.read(|entry| assert!(entry.auth.codex_pending_refresh.is_empty()));
        assert_eq!(scene.rig.live().metadata["refresh_token"], "b-refresh");
        assert_eq!(logged(&lines, tracing::Level::ERROR, ""), 0);
        assert_eq!(logged(&lines, tracing::Level::WARN, "origin unresolved"), 0);
    })
    .await;
}

/// B-23 (b) and (c): at `entered` the path becomes another file, g, in the
/// same directory. Returns the scene, g and g's bytes before the refresh.
fn other_file_scene() -> (Scene, PathBuf, Vec<u8>) {
    let scene = Scene::new();
    let g = scene.rig.dir.path().join("g.json");
    let mut copy = file(&scene.rig.path());
    copy.insert("note".into(), json!("from-g"));
    std::fs::write(&g, serde_json::to_vec(&copy).unwrap()).unwrap();
    let before = std::fs::read(&g).unwrap();
    scene.at_entered(scene.respell(&g));
    (scene, g, before)
}

#[tokio::test(flavor = "current_thread")]
async fn a_respelling_to_another_file_loses_the_origin_but_keeps_the_rotation() {
    // B-23 (b): the commits succeed and the pinned commit lands B in f.
    // Publication finds g `Different` and marks B; fence 2 proves f against g
    // `Different`, marks again and skips the write.
    bounded(async {
        let (scene, g, before) = other_file_scene();
        scene.rig.manager.force_refresh(ID).await.unwrap();
        assert_eq!(
            file(&scene.rig.path())["refresh_token"],
            "b-refresh",
            "f holds B"
        );
        assert_eq!(std::fs::read(&g).unwrap(), before, "g is untouched");
        assert!(
            lock(&scene.rig.store.writes).is_empty(),
            "no ordinary save wrote"
        );
        scene.read(|entry| {
            assert!(entry.undurable.outer, "B is marked");
            assert!(entry.unresolved_origin.is_none(), "no state");
        });
        assert_eq!(scene.rig.live().metadata["refresh_token"], "b-refresh");
        assert_eq!(scene.probe.reused(), 0);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_respelling_to_another_file_with_exhausted_commits_keeps_the_work_pending() {
    // B-23 (c): as (b) with the commits exhausted. Publication retains B
    // marked with `final` pending; fence 2's `Skipped` keeps it pending, and
    // nothing is dropped or logged as an error.
    bounded(async {
        let (scene, g, before) = other_file_scene();
        let (_guard, lines) = capture_logs();
        scene.exhaust_commits();
        scene.rig.manager.force_refresh(ID).await.unwrap();
        assert_eq!(std::fs::read(&g).unwrap(), before, "g is untouched");
        assert_eq!(
            file(&scene.rig.path())["refresh_token"],
            "outer-refresh-old",
            "f was never written"
        );
        assert!(lock(&scene.rig.store.writes).is_empty());
        scene.read(|entry| {
            assert!(entry.undurable.outer, "B is marked");
            assert!(entry.unresolved_origin.is_none(), "no state");
            assert_eq!(
                entry.auth.codex_pending_refresh.len(),
                1,
                "final stays pending"
            );
        });
        assert_eq!(scene.rig.live().metadata["refresh_token"], "b-refresh");
        assert_eq!(logged(&lines, tracing::Level::ERROR, ""), 0, "no H4 error");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_replaced_registration_with_exhausted_commits_refuses_and_logs_the_id() {
    // B-23 (d), the control: the id is removed and registered again at
    // `entered`, so publication gives `RegistrationChanged` and `Refused`.
    bounded(async {
        let scene = Scene::new();
        let (_guard, lines) = capture_logs();
        scene.exhaust_commits();
        let epoch = scene.rig.live().registration_epoch;
        let manager = scene.rig.manager.clone();
        scene.at_entered(move || {
            manager.remove(ID);
            manager.register_unsaved(fixture()).unwrap();
        });
        let error = scene.rig.manager.force_refresh(ID).await.unwrap_err();
        assert!(matches!(error, ManagerError::Other(_)), "{error}");
        assert_ne!(scene.rig.live().registration_epoch, epoch);
        assert_eq!(
            scene.rig.live().metadata["refresh_token"],
            "outer-refresh-old",
            "the new registration is untouched"
        );
        scene.assert_clean();
        assert_eq!(
            logged(&lines, tracing::Level::ERROR, "registration changed"),
            1
        );
        assert_eq!(
            logged(&lines, tracing::Level::ERROR, &format!("auth_id={ID}")),
            1,
            "the log names the id"
        );
    })
    .await;
}

/// The concurrent change B-24 makes at `entered`, besides `runtime_only`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    /// (a) a new non-401 `last_error`, with its status fields.
    NewError,
    /// (b) an active cooldown.
    Cooldown,
    /// (c) an active credential quota.
    CredentialQuota,
    /// (d) nothing.
    Nothing,
    /// (e) a new model 401, on a registration with no model states.
    ModelUnauthorized,
    /// (f) a model 500 turned into a 401.
    ModelChanged,
    /// (g) a model 401 under an active model quota.
    ModelQuota,
    /// (h) a model 401 plus an active top-level cooldown.
    ModelAndCooldown,
    /// (i) a concurrent top-level 401.
    TopLevel401,
    /// (j) a model 401 with its own cooldown, and the aggregate derived from it.
    ModelOwnCooldown,
    /// Beyond the spec's list: a top-level 401 under an active credential quota.
    TopLevel401UnderQuota,
}

fn auth_error(message: &str, http_status: u16) -> AuthError {
    AuthError {
        code: String::new(),
        message: message.to_owned(),
        retryable: http_status != 401,
        http_status,
    }
}

fn model_state(http_status: u16, message: &str) -> ModelState {
    ModelState {
        status: Status::Error,
        status_message: message.to_owned(),
        last_error: Some(auth_error(message, http_status)),
        ..ModelState::default()
    }
}

/// The registration before the refresh: (f) starts with model m in a 500.
/// Every case also starts with a prior `next_refresh_after`, so that the
/// merge's rule for it (the response's value replaces it) is observable.
fn prepare(scene: &Scene, change: Change) {
    let prior = Utc::now() + chrono::Duration::hours(1);
    scene
        .rig
        .manager
        .edit_unsaved(ID, |auth| {
            auth.next_refresh_after = Some(prior);
            if change == Change::ModelChanged {
                auth.model_states
                    .insert("m".into(), model_state(500, "upstream failed"));
            }
            Ok::<_, ()>(true)
        })
        .unwrap();
}

fn apply(change: Change, auth: &mut Auth, now: DateTime<Utc>) {
    let later = now + chrono::Duration::seconds(60);
    match change {
        Change::NewError => {
            auth.last_error = Some(auth_error("upstream failed", 500));
            auth.status = Status::Error;
            auth.unavailable = true;
            auth.status_message = "upstream failed".into();
        }
        Change::Cooldown => {
            auth.unavailable = true;
            auth.next_retry_after = Some(later);
        }
        Change::CredentialQuota => {
            auth.quota.exceeded = true;
            auth.quota.reason = "credential_quota".into();
            auth.quota.next_recover_at = Some(later);
        }
        Change::Nothing => {}
        Change::ModelUnauthorized | Change::ModelChanged => {
            auth.model_states
                .insert("m".into(), model_state(401, "unauthorized"));
        }
        Change::ModelQuota => {
            let mut state = model_state(401, "unauthorized");
            state.quota.exceeded = true;
            state.quota.reason = "model quota".into();
            state.quota.next_recover_at = Some(later);
            auth.model_states.insert("m".into(), state);
        }
        Change::ModelAndCooldown => {
            auth.model_states
                .insert("m".into(), model_state(401, "unauthorized"));
            auth.unavailable = true;
            auth.next_retry_after = Some(later);
        }
        Change::TopLevel401 => {
            auth.last_error = Some(auth_error("unauthorized", 401));
            auth.status = Status::Error;
            auth.unavailable = true;
            auth.status_message = "unauthorized".into();
        }
        Change::TopLevel401UnderQuota => {
            apply(Change::TopLevel401, auth, now);
            apply(Change::CredentialQuota, auth, now);
            auth.next_retry_after = Some(later);
        }
        Change::ModelOwnCooldown => {
            let mut state = model_state(401, "unauthorized");
            state.unavailable = true;
            state.next_retry_after = Some(later);
            auth.model_states.insert("m".into(), state);
            crate::manager::cooldown::update_aggregated_availability(auth, now);
        }
    }
}

/// The availability fields of the live record after the refresh, with the
/// `updated_at` of model m cleared (it is the time of the run).
#[derive(Debug, PartialEq)]
struct Availability {
    model: Option<ModelState>,
    unavailable: bool,
    next_retry_after: Option<DateTime<Utc>>,
    quota: QuotaState,
    status: Status,
    status_message: String,
    last_error: Option<AuthError>,
    next_refresh_after: Option<DateTime<Utc>>,
}

impl Availability {
    /// Without the schedule, which a refused run and an ordinary run set
    /// differently.
    fn without_schedule(mut self) -> Self {
        self.next_refresh_after = None;
        self
    }
}

/// Runs one B-24 case and returns the live availability with the deadline the
/// change seeded. With `runtime_only` the record becomes permanently
/// ineligible at `entered`, so the tail is refused; without it the tail is the
/// ordinary save.
async fn availability_run(change: Change, runtime_only: bool) -> (Availability, DateTime<Utc>) {
    let scene = Scene::new();
    prepare(&scene, change);
    let now = Utc::now();
    let later = now + chrono::Duration::seconds(60);
    let derived = Arc::new(Mutex::new(None));
    let (manager, sink) = (scene.rig.manager.clone(), Arc::clone(&derived));
    scene.at_entered(move || {
        manager
            .edit_unsaved(ID, |auth| {
                if runtime_only {
                    auth.attributes.insert("runtime_only".into(), "true".into());
                }
                apply(change, auth, now);
                Ok::<_, ()>(true)
            })
            .unwrap();
        // (j): the derived hold is on the live record before the executor
        // returns, so the case cannot pass vacuously.
        let live = manager.get(ID).unwrap();
        *lock(&sink) = Some((live.unavailable, live.next_retry_after));
    });
    scene.rig.manager.force_refresh(ID).await.unwrap();
    let held = lock(&derived).expect("the change was made at entered");
    if change == Change::ModelOwnCooldown {
        assert!(held.0, "the aggregate hold was derived");
        assert!(held.1.is_some_and(|t| t > now), "{held:?}");
    }
    let live = scene.rig.live();
    // The pass stamps the refresh; the merge takes it even when the tail is
    // refused.
    assert!(
        live.last_refreshed_at.is_some_and(|t| t >= now),
        "{:?}",
        live.last_refreshed_at
    );
    assert_eq!(live.metadata["refresh_token"], "b-refresh");
    if runtime_only {
        // Refused: no save of any kind, the rotation stays live and marked.
        assert_eq!(scene.rig.store.save_bound_calls.load(Ordering::SeqCst), 0);
        assert!(lock(&scene.rig.store.writes).is_empty());
        assert_eq!(
            scene.rig.store.lease_commit_attempts.load(Ordering::SeqCst),
            0
        );
        assert!(
            live.next_refresh_after.is_none(),
            "the response's schedule replaced the prior one"
        );
        assert_eq!(
            file(&scene.rig.path())["refresh_token"],
            "outer-refresh-old"
        );
        scene.read(|entry| {
            assert!(entry.undurable.outer);
            assert!(entry.auth.codex_pending_refresh.is_empty());
            assert!(matches!(
                entry.auth.codex_persist_intent,
                CodexPersistIntent::Configuration
            ));
        });
    }
    let mut model = live.model_states.get("m").cloned();
    if let Some(model) = &mut model {
        model.updated_at = None;
    }
    let seen = Availability {
        model,
        unavailable: live.unavailable,
        next_retry_after: live.next_retry_after,
        quota: live.quota.clone(),
        status: live.status,
        status_message: live.status_message.clone(),
        last_error: live.last_error.clone(),
        next_refresh_after: live.next_refresh_after,
    };
    (seen, later)
}

/// Model m after `reset_model_state`: active, with nothing else set.
fn reset_model() -> ModelState {
    ModelState {
        status: Status::Active,
        ..ModelState::default()
    }
}

fn assert_reset(seen: &Availability) {
    assert_eq!(seen.model, Some(reset_model()), "model m is reset");
}

#[tokio::test(flavor = "current_thread")]
async fn a_concurrent_error_survives_a_refused_refresh() {
    // B-24 (a): live keeps the concurrent error and its status fields.
    bounded(async {
        let (seen, _) = availability_run(Change::NewError, true).await;
        assert_eq!(seen.last_error, Some(auth_error("upstream failed", 500)));
        assert_eq!(seen.status, Status::Error);
        assert!(seen.unavailable);
        assert_eq!(seen.status_message, "upstream failed");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_concurrent_cooldown_survives_a_refused_refresh() {
    // B-24 (b): live keeps the concurrent hold and its status fields.
    bounded(async {
        let (seen, seeded) = availability_run(Change::Cooldown, true).await;
        assert!(seen.unavailable);
        assert_eq!(seen.next_retry_after, Some(seeded));
        // The fixture is `Unknown` and the change leaves it so; the ordinary
        // result would make it `Active`.
        assert_eq!(seen.status, Status::Unknown);
        assert!(seen.last_error.is_none());
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_concurrent_credential_quota_survives_a_refused_refresh() {
    // B-24 (c): live keeps the concurrent quota.
    bounded(async {
        let (seen, seeded) = availability_run(Change::CredentialQuota, true).await;
        assert!(seen.quota.exceeded);
        assert_eq!(seen.quota.reason, "credential_quota");
        assert_eq!(seen.quota.next_recover_at, Some(seeded));
        assert_eq!(seen.status, Status::Unknown);
        assert!(!seen.unavailable);
        assert!(seen.last_error.is_none());
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_refused_refresh_with_no_concurrent_change_leaves_the_record_active() {
    // B-24 (d): the control.
    bounded(async {
        let (seen, _) = availability_run(Change::Nothing, true).await;
        assert_eq!(seen.status, Status::Active);
        assert!(!seen.unavailable);
        assert!(seen.last_error.is_none());
        assert!(seen.status_message.is_empty());
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_new_model_401_is_reset_by_a_refused_refresh() {
    // B-24 (e) and (f).
    bounded(async {
        for change in [Change::ModelUnauthorized, Change::ModelChanged] {
            let (seen, _) = availability_run(change, true).await;
            assert_reset(&seen);
            assert!(!seen.unavailable, "{change:?}");
            assert!(seen.next_retry_after.is_none(), "{change:?}");
        }
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_model_401_under_a_model_quota_keeps_the_quota_when_refused() {
    // B-24 (g): the 401 is cleared, the quota stays, and the status message is
    // the reason of the quota.
    bounded(async {
        let (seen, seeded) = availability_run(Change::ModelQuota, true).await;
        // The quota branch: the 401 is gone and the message is the quota's
        // reason, but the status and the quota stay as the change set them.
        let held = ModelState {
            status: Status::Error,
            status_message: "model quota".into(),
            quota: QuotaState {
                exceeded: true,
                reason: "model quota".into(),
                next_recover_at: Some(seeded),
                ..QuotaState::default()
            },
            ..ModelState::default()
        };
        assert_eq!(seen.model, Some(held));
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_model_401_with_a_top_level_cooldown_gives_the_ordinary_result_when_refused() {
    // B-24 (h): the refused run and the ordinary run agree.
    bounded(async {
        let (refused, _) = availability_run(Change::ModelAndCooldown, true).await;
        assert_reset(&refused);
        assert!(!refused.unavailable);
        assert!(refused.next_retry_after.is_none());
        let (ordinary, _) = availability_run(Change::ModelAndCooldown, false).await;
        assert_reset(&ordinary);
        assert_eq!(refused.without_schedule(), ordinary.without_schedule());
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_concurrent_top_level_401_is_not_kept_by_a_refused_refresh() {
    // B-24 (i).
    bounded(async {
        let (seen, _) = availability_run(Change::TopLevel401, true).await;
        assert_eq!(seen.status, Status::Active);
        assert!(!seen.unavailable);
        assert!(seen.last_error.is_none());
        assert!(seen.status_message.is_empty());
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_credential_quota_under_a_concurrent_401_is_kept_when_refused() {
    // Beyond B-24: the 401 is cleared and the quota hold it sat under is
    // restored, which only the refusal's own settling does.
    bounded(async {
        let (seen, seeded) = availability_run(Change::TopLevel401UnderQuota, true).await;
        assert!(seen.quota.exceeded);
        assert_eq!(seen.quota.next_recover_at, Some(seeded));
        assert!(seen.unavailable);
        assert!(seen.next_retry_after.is_some());
        assert!(seen.last_error.is_none());
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_hold_derived_only_from_a_model_401_does_not_survive_a_refused_refresh() {
    // B-24 (j), the case from the oracle: the refused run and the ordinary run
    // agree.
    bounded(async {
        let (refused, _) = availability_run(Change::ModelOwnCooldown, true).await;
        assert_reset(&refused);
        assert!(!refused.unavailable);
        assert!(refused.next_retry_after.is_none());
        let (ordinary, _) = availability_run(Change::ModelOwnCooldown, false).await;
        assert_reset(&ordinary);
        assert_eq!(refused.without_schedule(), ordinary.without_schedule());
    })
    .await;
}

/// A second spelling of the credential file, made with the crate directly:
/// on Windows a case-only respelling of the file, on Unix the file through a
/// directory symlink. The alias is asserted before use.
#[cfg(windows)]
fn respelled_file(scene: &Scene) -> PathBuf {
    let respelled = scene.rig.dir.path().join("CODEX.JSON");
    assert_alias(&scene.rig.path(), &respelled);
    respelled
}

#[cfg(unix)]
fn respelled_file(scene: &Scene) -> PathBuf {
    let respelled = alias_dir(scene.rig.dir.path(), scene.q.path()).join(ID);
    assert_alias(&scene.rig.path(), &respelled);
    respelled
}

#[cfg(any(unix, windows))]
#[tokio::test(flavor = "current_thread")]
async fn a_respelled_path_is_serialized_with_a_peer_on_the_original_spelling() {
    // B-25: the respelling is the same file, so publication admits it and
    // nothing is marked, and the OS lock on the one lock file still keeps a
    // peer on the original spelling and this process apart.
    bounded(async {
        let scene = Scene::new();
        let respelled = respelled_file(&scene);
        scene.at_entered(scene.respell(&respelled));
        scene.rig.manager.force_refresh(ID).await.unwrap();
        // Step 1: B landed, and the save went through the respelled path.
        scene.assert_written_once();
        scene.assert_clean();
        assert_eq!(scene.rig.live().metadata["refresh_token"], "b-refresh");
        assert_eq!(file(&respelled)["refresh_token"], "b-refresh");
        // Step 2: the peer holds the lock through the original spelling, and
        // the respelled spelling cannot take it. Only the OS lock refuses it:
        // the in-process mutexes are keyed by the lexical lock path.
        let held = crate::auth::credential_lock::acquire_with_timeout(
            scene.rig.path(),
            Duration::from_secs(5),
        )
        .await
        .expect("the peer takes the lock");
        let refused = crate::auth::credential_lock::acquire_with_timeout(
            respelled.clone(),
            Duration::from_millis(100),
        )
        .await;
        assert_eq!(
            refused.err().map(|e| e.kind()),
            Some(std::io::ErrorKind::TimedOut)
        );
        // Step 3: the peer writes C over B and releases.
        write_outer(&scene.rig.path(), "c");
        drop(held);
        // Step 4: the next pass adopts C, sends C and commits D, which both
        // spellings read.
        scene.rig.manager.force_refresh(ID).await.unwrap();
        assert_eq!(scene.probe.sent(), 2);
        assert_eq!(scene.probe.sent_refresh(1), "c-refresh", "C is sent, not B");
        assert_eq!(scene.probe.reused(), 0);
        assert_eq!(file(&scene.rig.path())["refresh_token"], "d-refresh");
        assert_eq!(file(&respelled)["refresh_token"], "d-refresh");
        scene.assert_clean();
    })
    .await;
}

/// A peer that logs in with C's tokens (distinct from A's and B's) through its
/// own store bound to `dir`, with the `OuterLogin` intent.
#[cfg(any(unix, windows))]
fn peer_login(dir: &Path) -> std::thread::JoinHandle<()> {
    let dir = dir.to_path_buf();
    std::thread::spawn(move || {
        let auth = Auth {
            id: ID.into(),
            file_name: ID.into(),
            provider: "codex".into(),
            metadata: json!({
                "type": "codex",
                "access_token": "c-access",
                "refresh_token": "c-refresh",
            })
            .as_object()
            .unwrap()
            .clone(),
            codex_persist_intent: CodexPersistIntent::OuterLogin,
            ..Auth::default()
        };
        use crate::auth::AuthStore as _;
        crate::auth::FileStore::new(&dir).save(&auth).unwrap();
    })
}

/// The test hooks of B-27: the first `"origin-between"` starts the peer, waits
/// for its `"waiting"` and checks that the file has not changed.
#[cfg(any(unix, windows))]
struct Between {
    keys: Vec<PathBuf>,
    fired: Arc<std::sync::atomic::AtomicUsize>,
    peer: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
}

#[cfg(any(unix, windows))]
impl Between {
    /// `keys` are the spellings the hook sits on; the peer's store is bound to
    /// `peer_dir`.
    fn install(file: &Path, keys: &[PathBuf], peer_dir: &Path) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let (tx, rx) = (Mutex::new(tx), Mutex::new(rx));
        let fired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let peer = Arc::new(Mutex::new(None));
        let hook = {
            let (fired, peer) = (Arc::clone(&fired), Arc::clone(&peer));
            let (file, peer_dir) = (file.to_path_buf(), peer_dir.to_path_buf());
            move |event: &str| match event {
                "waiting" => {
                    let _ = lock(&tx).send(());
                }
                "origin-between" if fired.fetch_add(1, Ordering::SeqCst) == 0 => {
                    let before = std::fs::read(&file).unwrap();
                    *lock(&peer) = Some(peer_login(&peer_dir));
                    lock(&rx)
                        .recv_timeout(Duration::from_secs(5))
                        .expect("the peer reaches the lock wait");
                    // It waits on the lock the prover holds, so nothing moved.
                    assert_eq!(std::fs::read(&file).unwrap(), before);
                }
                _ => {}
            }
        };
        let hook: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(hook);
        for key in keys {
            crate::auth::credential_lock::set_test_hook(key, Some(Arc::clone(&hook)));
        }
        Self {
            keys: keys.to_vec(),
            fired,
            peer,
        }
    }

    /// A proof ran between the opens and started the peer, which has finished.
    fn finish(self) {
        for key in &self.keys {
            crate::auth::credential_lock::set_test_hook(key, None);
        }
        // Later proofs of the same pass also open two files; the hook ignores them.
        assert!(
            self.fired.load(Ordering::SeqCst) >= 1,
            "no proof opened two files"
        );
        let peer = lock(&self.peer).take().expect("the peer was started");
        peer.join().expect("the peer's save succeeded");
    }
}

/// After the join: D/f holds C, nothing is marked or recorded, live is B or C,
/// and the next pass sends C, never B.
#[cfg(any(unix, windows))]
async fn assert_peer_won(scene: &Scene) {
    assert_eq!(file(&scene.rig.path())["refresh_token"], "c-refresh");
    scene.assert_clean();
    let live = scene.rig.live().metadata["refresh_token"].clone();
    assert!(
        live == "b-refresh" || live == "c-refresh",
        "live outer is neither B nor C"
    );
    scene.rig.manager.force_refresh(ID).await.unwrap();
    assert_eq!(scene.probe.sent(), 2);
    assert_eq!(scene.probe.sent_refresh(1), "c-refresh", "C is sent, not B");
    assert_eq!(scene.probe.reused(), 0, "a spent token was presented again");
    assert_eq!(file(&scene.rig.path())["refresh_token"], "d-refresh");
    scene.assert_clean();
}

#[cfg(any(unix, windows))]
#[tokio::test(flavor = "current_thread")]
async fn a_peer_between_the_opens_of_the_fence_two_proof_cannot_split_it() {
    // B-27 (a): fence 2 proves D/f against alias/f under alias/f's lock; the
    // peer, bound to D, waits on the one OS lock and writes C after the save.
    bounded(async {
        let scene = Scene::new();
        let alias = alias_dir(scene.rig.dir.path(), scene.q.path());
        let via = alias.join(ID);
        assert_alias(&scene.rig.path(), &via);
        let between = Between::install(
            &scene.rig.path(),
            &[scene.rig.path(), via.clone()],
            scene.rig.dir.path(),
        );
        let files = scene.rig.store.files.clone();
        *lock(&scene.rig.manager.shared.lease_tail_hook) =
            Some(Arc::new(move |_: &Auth| files.set_base_dir(&alias)));
        scene.rig.manager.force_refresh(ID).await.unwrap();
        between.finish();
        assert_eq!(scene.probe.sent(), 1);
        assert_peer_won(&scene).await;
    })
    .await;
}

#[cfg(any(unix, windows))]
#[tokio::test(flavor = "current_thread")]
async fn a_peer_between_the_opens_of_the_publication_proof_cannot_split_it() {
    // B-27 (b): the commit's admission proves D/f against alias/f under the
    // lease's lock; the peer, bound to the alias, waits on the one OS lock.
    bounded(async {
        let scene = Scene::new();
        let alias = alias_dir(scene.rig.dir.path(), scene.q.path());
        let via = alias.join(ID);
        assert_alias(&scene.rig.path(), &via);
        let between = Between::install(&scene.rig.path(), std::slice::from_ref(&via), &alias);
        scene.at_entered(scene.rebind(&alias));
        scene.rig.manager.force_refresh(ID).await.unwrap();
        between.finish();
        assert_eq!(scene.probe.sent(), 1);
        assert_peer_won(&scene).await;
    })
    .await;
}
