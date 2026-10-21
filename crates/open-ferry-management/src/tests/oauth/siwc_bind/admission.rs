//! Local regressions for bounded bind admission, including real management
//! edits and runtimes whose workers or blocking pool are occupied.

use super::*;
use std::sync::atomic::AtomicBool;

const BOUND: Duration = Duration::from_millis(800);

struct Pause {
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
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap();
                }
            },
        )
    }
    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }
    fn resume(&self) {
        let _ = self.release.send(());
    }
}

impl Drop for Pause {
    fn drop(&mut self) {
        self.resume();
    }
}

fn fixture() -> (AuthDir, Pause) {
    let (pause, hook) = Pause::new();
    let dir = AuthDir::guarded(Some(open_ferry_core::auth::file_guard::FileGuard::new(
        move |path| {
            if path.file_name().is_some_and(|name| name == "held.json") {
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

async fn shared_holder(api: &Api, pause: &Pause) -> std::thread::JoinHandle<()> {
    let manager = api.manager.clone();
    let id = manager
        .list()
        .into_iter()
        .find(|a| a.file_name == "held.json")
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

fn probe(api: &Api) -> impl FnOnce() + Send + 'static {
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

async fn patch_pair(fields: bool) {
    // Exercise a PATCH arriving during a reservation, then the opposite
    // ordering. The first schedule reproduces the old management-lock cycle.
    for patch_first in [false, true] {
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
        let mut patch = Box::pin(api.call(Method::PATCH, route, body));
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
        edit_pause.resume();
        let result = tokio::time::timeout(BOUND, async {
            let (response, (), joined) =
                tokio::join!(&mut patch, completed(&api, &session), joining);
            joined.unwrap();
            response.expect(StatusCode::OK);
        })
        .await;
        if result.is_err() {
            api.state.oauth_sessions().store().cancel(&session);
            tokio::time::timeout(Duration::from_secs(2), &mut patch)
                .await
                .unwrap()
                .expect(StatusCode::OK);
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
    patch_pair(false).await;
}

#[tokio::test]
async fn round2_bind_and_patch_fields_finish_after_shared_drain() {
    patch_pair(true).await;
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
}

fn cancellation_with_parked_worker(stop: &str) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(4)
        .enable_all()
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
        let work = probe(&api);
        let (started, entered) = std::sync::mpsc::channel();
        // Model the synchronous bootstrap-destructor recording path: the
        // sole worker parks in shared admission. The reservation must
        // release itself without requiring this worker to poll anything.
        let parked = tokio::spawn(async move {
            started.send(()).unwrap();
            work();
        });
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        match stop {
            "expiry" => api
                .state
                .oauth_sessions()
                .store()
                .expire_bind_for_test(&session),
            "cancel" => assert!(api.state.oauth_sessions().store().cancel(&session)),
            _ => api.state.oauth_sessions().shutdown().await,
        }
        tokio::time::timeout(BOUND, parked)
            .await
            .expect("parked runtime worker prevented withdrawal")
            .unwrap();
        wait_for_bind_task(&api).await;
        pause.resume();
        active.join().unwrap();
        assert!(dir.read_json("codex-bind.json").get("use_siwc").is_none());
    });
}

#[test]
fn round2_expiry_withdraws_with_a_parked_runtime_worker() {
    cancellation_with_parked_worker("expiry");
}

#[test]
fn round2_cancel_withdraws_with_a_parked_runtime_worker() {
    cancellation_with_parked_worker("cancel");
}

#[test]
fn round2_disconnect_withdraws_with_a_parked_runtime_worker() {
    cancellation_with_parked_worker("disconnect");
}
