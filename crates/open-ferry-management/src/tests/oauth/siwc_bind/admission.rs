//! Local regressions for bounded bind admission, including real management
//! edits and runtimes whose workers or blocking pool are occupied.

use super::*;
use futures_util::{FutureExt, future::FusedFuture};
use std::sync::atomic::AtomicBool;

const BOUND: Duration = Duration::from_millis(800);
// Completion includes real file persistence and retry backoff; withdrawal has
// its separate 800 ms bound.
const PAIR_BOUND: Duration = Duration::from_secs(12);

pub(super) struct Pause {
    armed: Arc<AtomicBool>,
    entered: Arc<tokio::sync::Notify>,
    release: std::sync::mpsc::Sender<()>,
}

impl Pause {
    fn new() -> (Self, impl Fn() + Send + Sync + 'static) {
        let armed = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(tokio::sync::Notify::new());
        let (release, released) = std::sync::mpsc::channel();
        let released = std::sync::Mutex::new(released);
        let check = armed.clone();
        let signal = entered.clone();
        (
            Self {
                armed,
                entered,
                release,
            },
            move || {
                if check.swap(false, Ordering::SeqCst) {
                    signal.notify_one();
                    released
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(30))
                        .unwrap();
                }
            },
        )
    }
    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }
    pub(super) fn resume(&self) {
        let _ = self.release.send(());
    }
}

impl Drop for Pause {
    fn drop(&mut self) {
        self.resume();
    }
}

pub(super) fn fixture() -> (AuthDir, Pause) {
    fixture_for("held.json")
}

pub(super) fn fixture_for(name: &str) -> (AuthDir, Pause) {
    let name = name.to_owned();
    let (pause, hook) = Pause::new();
    let name_to_hold = name;
    let dir = AuthDir::guarded(Some(open_ferry_core::auth::file_guard::FileGuard::new(
        move |path| {
            if path
                .file_name()
                .is_some_and(|name| name == name_to_hold.as_str())
            {
                hook();
            }
            Ok(())
        },
    )));
    for name in ["codex-bind.json", "held.json", "edited.json"] {
        dir.write(name, OUTER);
    }
    (dir, pause)
}

pub(super) async fn shared_holder(api: &Api, pause: &Pause) -> std::thread::JoinHandle<()> {
    shared_holder_for(api, pause, "held.json").await
}

pub(super) async fn shared_holder_for(
    api: &Api,
    pause: &Pause,
    name: &str,
) -> std::thread::JoinHandle<()> {
    let manager = api.manager.clone();
    let id = manager
        .list()
        .into_iter()
        .find(|a| a.file_name == name)
        .unwrap()
        .id
        .clone();
    pause.arm();
    let task = std::thread::spawn(move || {
        manager
            .edit(&id, |auth| {
                auth.metadata.insert("probe".into(), json!(true));
                Ok::<_, ()>(true)
            })
            .unwrap();
    });
    tokio::time::timeout(Duration::from_secs(2), pause.entered.notified())
        .await
        .unwrap();
    task
}

pub(super) fn probe(api: &Api) -> impl FnOnce() + Send + 'static {
    let manager = api.manager.clone();
    let id = manager
        .list()
        .into_iter()
        .find(|a| a.file_name == "edited.json")
        .unwrap()
        .id
        .clone();
    move || {
        manager.edit_unsaved(&id, |_| Ok::<_, ()>(false)).unwrap();
    }
}

async fn patch_pair(fields: bool, patch_first: bool) {
    // Each ordering is a separate nextest case, including PATCH-first.
    {
        let (dir, holder_pause) = fixture();
        let upstream = Upstream::answering(tokens()).await;
        let mut api = login_api(&dir, &upstream.url);
        let session = start_login(&api, START).await;
        let (edit_pause, hook) = Pause::new();
        let state = api.state.clone().with_before_edit(hook);
        api.router = crate::router(state.clone());
        api.state = state;
        edit_pause.arm();
        let active = shared_holder(&api, &holder_pause).await;
        let waiting = watch_admission(&api);
        let (resume_bind, resumed_bind) = std::sync::mpsc::channel();
        let drained = Arc::new((
            tokio::sync::Notify::new(),
            std::sync::Mutex::new(resumed_bind),
        ));
        api.state.oauth_sessions().overrides().bind_before_file = Some(drained.clone());
        if !patch_first {
            deliver(&api, &session, "codex", issued());
            tokio::time::timeout(Duration::from_secs(2), waiting.notified())
                .await
                .unwrap();
        }
        let route = if fields {
            "/v0/management/auth-files/fields"
        } else {
            "/v0/management/auth-files/status"
        };
        let body = if fields {
            r#"{"name":"edited.json","priority":7}"#
        } else {
            r#"{"name":"edited.json","disabled":true}"#
        };
        let mut patch = Box::pin(api.call(Method::PATCH, route, body).fuse());
        tokio::select! {
            () = edit_pause.entered.notified() => {},
            _ = &mut patch => panic!("PATCH did not reach before_edit"),
            () = tokio::time::sleep(Duration::from_secs(2)) => panic!("PATCH did not acquire management lock"),
        }
        if patch_first {
            deliver(&api, &session, "codex", issued());
            tokio::time::timeout(Duration::from_secs(2), waiting.notified())
                .await
                .unwrap();
        }
        holder_pause.resume();
        let joining = tokio::task::spawn_blocking(move || active.join().unwrap());
        // Establish the cycle precondition: R has drained while PATCH still
        // owns M. PATCH cannot finish its shared mutation until R withdraws.
        tokio::time::timeout(PAIR_BOUND, drained.0.notified())
            .await
            .unwrap();
        api.state.oauth_sessions().overrides().bind_before_file = None;
        edit_pause.resume();
        resume_bind.send(()).unwrap();
        // The outer joint deadline owns this wait; the ordinary OAuth helper
        // has its own shorter timeout, which would hide that deadline.
        let completed_bind = async {
            loop {
                let outcome = api.state.oauth_sessions().store().get(&session).unwrap();
                assert!(outcome.status.is_empty(), "bind failed: {}", outcome.status);
                if outcome.completed {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let result = tokio::time::timeout(PAIR_BOUND, async {
            let (response, (), joined) = tokio::join!(&mut patch, completed_bind, joining);
            joined.unwrap();
            response.expect(StatusCode::OK);
        })
        .await;
        if result.is_err() {
            api.state.oauth_sessions().store().cancel(&session);
            if !patch.is_terminated() {
                tokio::time::timeout(Duration::from_secs(12), &mut patch)
                    .await
                    .unwrap()
                    .expect(StatusCode::OK);
            }
        }
        assert!(
            result.is_ok(),
            "bind and PATCH waited on each other ({fields}, {patch_first})"
        );
        assert_eq!(dir.read_json("codex-bind.json")["use_siwc"], true);
    }
}

#[tokio::test]
async fn round2_bind_and_patch_status_finish_after_shared_drain() {
    patch_pair(false, false).await;
}

#[tokio::test]
async fn round2_bind_and_patch_fields_finish_after_shared_drain() {
    patch_pair(true, false).await;
}

#[test]
fn round2_small_blocking_pool_withdraws_and_resumes_every_holder() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(4)
        .build()
        .unwrap();
    rt.block_on(async {
        let (dir, pause) = fixture();
        let upstream = Upstream::answering(tokens()).await;
        let api = login_api(&dir, &upstream.url);
        let session = start_login(&api, START).await;
        let active = shared_holder(&api, &pause).await;
        let waiting = watch_admission(&api);
        deliver(&api, &session, "codex", issued());
        tokio::time::timeout(Duration::from_secs(2), waiting.notified())
            .await
            .unwrap();
        let mut parked = Vec::new();
        for _ in 0..4 {
            parked.push(tokio::task::spawn_blocking(probe(&api)));
        }
        let mut resumed = Box::pin(async {
            for task in parked {
                task.await.unwrap();
            }
        });
        let result = tokio::time::timeout(BOUND, &mut resumed).await;
        if result.is_err() {
            api.state.oauth_sessions().store().cancel(&session);
        }
        if result.is_err() {
            tokio::time::timeout(Duration::from_secs(2), &mut resumed)
                .await
                .unwrap();
        }
        pause.resume();
        active.join().unwrap();
        assert!(
            result.is_ok(),
            "reservation occupied every blocking-pool slot"
        );
        completed(&api, &session).await;
    });
}

#[tokio::test]
async fn round2_overlapping_binds_admit_waiting_shared_mutations() {
    let (dir, pause) = fixture();
    dir.write("second.json", OUTER);
    let upstream = Upstream::answering(tokens()).await;
    let api = login_api(&dir, &upstream.url);
    let first = start_login(&api, START).await;
    let second = start_login(&api, &START.replace("codex-bind.json", "second.json")).await;
    let active = shared_holder(&api, &pause).await;
    let waiting = watch_admission(&api);
    deliver(&api, &first, "codex", issued());
    deliver(&api, &second, "codex", issued());
    tokio::time::timeout(Duration::from_secs(2), waiting.notified())
        .await
        .unwrap();
    let mut shared = Box::pin(tokio::task::spawn_blocking(probe(&api)));
    let result = tokio::time::timeout(BOUND, &mut shared).await;
    if result.is_err() {
        for session in [&first, &second] {
            api.state.oauth_sessions().store().cancel(session);
        }
    }
    if result.is_err() {
        tokio::time::timeout(Duration::from_secs(2), &mut shared)
            .await
            .unwrap()
            .unwrap();
    }
    pause.resume();
    active.join().unwrap();
    assert!(
        result.is_ok(),
        "overlapping reservations starved a shared mutation"
    );
    completed(&api, &first).await;
    completed(&api, &second).await;
    // The route-level liveness check alone can pass with either guard.
    // Exercise the production turn API on a worker to isolate both rules.
    let manager = api.manager.clone();
    tokio::task::spawn_blocking(move || {
        let older = manager.siwc_bind_turn("codex-bind.json");
        let newer = manager.siwc_bind_turn("second.json");
        let reservation = older.reserve_siwc_bind().unwrap();
        assert!(
            older.reserve_siwc_bind().is_none(),
            "overlapping reservations were allowed"
        );
        drop(reservation);
        assert!(
            newer.reserve_siwc_bind().is_none(),
            "a newer bind overtook an older FIFO turn"
        );
        drop(older);
        assert!(newer.reserve_siwc_bind().is_some());
    })
    .await
    .unwrap();
}

fn cancellation_with_free_worker(stop: &str) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(4)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (dir, pause) = fixture_for("codex-bind.json");
        let api = bind_api(&dir);
        let active = shared_holder_for(&api, &pause, "codex-bind.json").await;
        let waiting = watch_admission(&api);
        let finished = Arc::new(tokio::sync::Notify::new());
        api.state.oauth_sessions().overrides().bind_drain_budget = Some(Duration::from_secs(30));
        api.state.oauth_sessions().overrides().bind_worker_finished = Some(finished.clone());
        let state = api.state.clone();
        let saving = tokio::spawn(async move {
            crate::oauth::siwc_bind::save_for_test(&state, "round3-signal").await
        });
        tokio::time::timeout(Duration::from_secs(12), waiting.notified())
            .await
            .unwrap();
        // Give the running worker time to enter its timed drain wait. The
        // enlarged test budget separates signal withdrawal from the deadline.
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(api.state.oauth_sessions().bind_workers.counts().0, 1);
        let withdrew = tokio::time::timeout(BOUND, async {
            match stop {
                "expiry" => api
                    .state
                    .oauth_sessions()
                    .store()
                    .expire_bind_for_test("round3-signal"),
                "cancel" => assert!(api.state.oauth_sessions().store().cancel("round3-signal")),
                "disconnect" => saving.abort(),
                _ => api.state.oauth_sessions().shutdown().await,
            }
            finished.notified().await;
        })
        .await
        .is_ok();
        // Release the shared holder before the final assertions. A signal
        // must prevent the later write too, not just delay it behind the holder.
        pause.resume();
        tokio::task::spawn_blocking(move || active.join().unwrap())
            .await
            .unwrap();
        if !withdrew {
            tokio::time::timeout(Duration::from_secs(12), finished.notified())
                .await
                .unwrap();
        }
        let result = saving.await;
        assert!(
            withdrew,
            "{stop} did not withdraw before the independent drain deadline"
        );
        assert!(result.is_err() || result.unwrap().is_err());
        assert!(dir.read_json("codex-bind.json").get("use_siwc").is_none());
    });
}

#[test]
fn round3_expiry_signal_withdraws_before_deadline() {
    cancellation_with_free_worker("expiry");
}
#[test]
fn round3_cancel_signal_withdraws_before_deadline() {
    cancellation_with_free_worker("cancel");
}
#[test]
fn round3_disconnect_signal_withdraws_before_deadline() {
    cancellation_with_free_worker("disconnect");
}
#[test]
fn round3_shutdown_signal_withdraws_before_deadline() {
    cancellation_with_free_worker("shutdown");
}

#[tokio::test]
async fn round3_patch_first_status_finishes_after_shared_drain() {
    patch_pair(false, true).await;
}
#[tokio::test]
async fn round3_patch_first_fields_finishes_after_shared_drain() {
    patch_pair(true, true).await;
}

#[test]
fn round3_four_pending_binds_leave_the_blocking_pool_available() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(4)
        .build()
        .unwrap();
    rt.block_on(async {
        let upstream = Upstream::answering(tokens()).await;
        let mut fixtures = Vec::new();
        let mut parked = Vec::new();
        for _ in 0..4 {
            let (dir, pause) = fixture_for("codex-bind.json");
            let api = login_api(&dir, &upstream.url);
            let session = start_login(&api, START).await;
            let holder = shared_holder_for(&api, &pause, "codex-bind.json").await;
            let waiting = watch_admission(&api);
            deliver(&api, &session, "codex", issued());
            tokio::time::timeout(Duration::from_secs(2), waiting.notified())
                .await
                .unwrap();
            fixtures.push((dir, api, pause, session, holder));
        }
        for (_, api, _, _, _) in &fixtures {
            let manager = api.manager.clone();
            let id = manager
                .list()
                .into_iter()
                .find(|a| a.file_name == "codex-bind.json")
                .unwrap()
                .id
                .clone();
            parked.push(tokio::task::spawn_blocking(move || {
                manager.edit_unsaved(&id, |_| Ok::<_, ()>(false)).unwrap();
            }));
        }
        let mut resumed = Box::pin(async {
            for task in parked {
                task.await.unwrap();
            }
        });
        let result = tokio::time::timeout(BOUND, &mut resumed).await;
        for (_, api, _, session, _) in &fixtures {
            api.state.oauth_sessions().store().cancel(session);
        }
        if result.is_err() {
            tokio::time::timeout(Duration::from_secs(3), &mut resumed)
                .await
                .unwrap();
        }
        for (_, api, pause, _, holder) in fixtures {
            pause.resume();
            tokio::task::spawn_blocking(move || holder.join().unwrap())
                .await
                .unwrap();
            wait_for_bind_task(&api).await;
        }
        assert!(
            result.is_ok(),
            "retained bind turns occupied every blocking-pool slot"
        );
    });
}
