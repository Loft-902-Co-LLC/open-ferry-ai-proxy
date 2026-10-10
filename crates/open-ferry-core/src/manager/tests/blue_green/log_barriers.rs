//! Not upstream's: file barriers around token rotation and admitted saves.

use std::fs;
use std::future::Future;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::gate::{GateState, Operation};
use crate::manager::lifecycle::Save;
use crate::manager::{ManagerError, Settings};
use crate::test_hooks;

use super::super::support::{FakeExecutor, Harness, auth_with_metadata};

const ID: &str = "barrier.json";
const WAIT: Duration = Duration::from_secs(5);

// Parent-owned temp roots also survive a child panic until its blocking
// hooks finish. The watchdog kills/reaps only its own test child handle.
fn isolated<F: Future<Output = ()>>(name: &str, work: impl FnOnce(PathBuf) -> F) {
    const CHILD: &str = "OFP_BG_CORE_BARRIER_CHILD";
    const ROOT: &str = "OFP_BG_CORE_BARRIER_ROOT";
    if std::env::var(CHILD).ok().as_deref() == Some(name) {
        let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(work(root));
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("manager::tests::blue_green::log_barriers::{name}"),
        ])
        .env(CHILD, name)
        .env(ROOT, root.path())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let output = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let bytes = output.join().unwrap();
                let text = String::from_utf8_lossy(&bytes);
                assert!(
                    status.success(),
                    "barrier test child failed: {status}: {text}"
                );
                assert!(
                    text.contains("test result: ok. 1 passed;"),
                    "child ran no test: {text}"
                );
                break;
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = output.join();
                panic!("barrier test child did not finish: {result:?}");
            }
        }
    }
}

fn install(root: &Path) -> PathBuf {
    let dir = root.join(".blue-green-test");
    fs::create_dir_all(&dir).unwrap();
    let files = dir.clone();
    test_hooks::install(Box::new(move |name| {
        // A synchronous save must own its admission in this blocking thread.
        if name == "persist.before" && Operation::current().is_none() {
            return Err(io::Error::other("persist lost its operation"));
        }
        if files.join(format!("{name}.fail")).exists() {
            fs::write(files.join(format!("{name}.failed")), b"x")?;
            return Err(io::Error::other(format!("injected failure at {name}")));
        }
        if files.join(format!("{name}.hold")).exists() {
            fs::write(files.join(format!("{name}.arrived")), [])?;
            while !files.join(format!("{name}.release")).exists() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        fs::write(files.join(format!("{name}.returned")), [])
    }))
    .unwrap();
    dir
}

struct Release {
    dir: PathBuf,
    name: &'static str,
}
impl Release {
    fn hold(dir: &Path, name: &'static str) -> Self {
        fs::write(dir.join(format!("{name}.hold")), []).unwrap();
        Self {
            dir: dir.to_path_buf(),
            name,
        }
    }
    fn release(&self) {
        fs::write(self.dir.join(format!("{}.release", self.name)), []).unwrap();
    }
}
impl Drop for Release {
    fn drop(&mut self) {
        let _ = fs::write(self.dir.join(format!("{}.release", self.name)), []);
    }
}

async fn arrived(path: &Path) {
    tokio::time::timeout(WAIT, async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("barrier file did not appear");
}

fn harness() -> (
    Harness,
    std::sync::Arc<crate::auth::Auth>,
    std::sync::Arc<FakeExecutor>,
) {
    let h = Harness::with_store(Settings::default());
    let executor = FakeExecutor::new("codex");
    executor.set_refresh(|auth| {
        let mut updated = auth.clone();
        updated
            .metadata
            .insert("refresh_token".into(), json!("rotated-refresh"));
        updated
            .metadata
            .insert("access_token".into(), json!("rotated-access"));
        Ok(updated)
    });
    h.executor(&executor);
    let auth = h
        .manager
        .register_unsaved(auth_with_metadata(
            ID,
            "codex",
            json!({
                "type": "codex", "refresh_token": "old-refresh", "access_token": "old-access"
            }),
        ))
        .unwrap();
    (h, auth, executor)
}

async fn held_refresh(root: PathBuf, name: &'static str) {
    let dir = install(&root);
    let release = Release::hold(&dir, name);
    let (h, auth, executor) = harness();
    let manager = h.manager.clone();
    let caller = tokio::spawn(async move { manager.refresh_at_epoch(ID, "", 0).await });
    arrived(&dir.join(format!("{name}.arrived"))).await;
    assert_eq!(executor.refresh_count(), 1);
    assert_eq!(h.store.save_count(), 0, "saved before barrier release");
    assert!(!caller.is_finished());

    if name == "persist.before" {
        let other = h
            .manager
            .register_unsaved(auth_with_metadata(
                "other.json",
                "codex",
                json!({"type": "codex", "access_token": "other"}),
            ))
            .unwrap();
        // No admission: ordinary request accounting and another saved mutation
        // complete on this current-thread runtime while refresh owns the hold.
        h.manager.mark_result(&crate::manager::CallResult {
            auth_id: other.id.clone(),
            success: true,
            ..Default::default()
        });
        let mut updated = (*other).clone();
        updated.label = "changed".into();
        h.manager.update(updated).unwrap();
        assert_eq!(h.store.save_count(), 2);
        let tick = tokio::spawn(async {
            tokio::task::yield_now().await;
            7
        });
        assert_eq!(tokio::time::timeout(WAIT, tick).await.unwrap().unwrap(), 7);
        let manager = h.manager.clone();
        let get = tokio::task::spawn_blocking(move || manager.get(ID));
        assert!(
            tokio::time::timeout(WAIT, get)
                .await
                .unwrap()
                .unwrap()
                .is_some()
        );
    }
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    let gate = h.manager.gate();
    let epoch = gate.epoch();
    let closing = tokio::spawn(async move { gate.close(epoch).await });
    while h.manager.gate().state() == GateState::Open {
        tokio::task::yield_now().await;
    }
    assert_eq!(h.manager.gate().state(), GateState::Quiescing);
    assert!(
        !closing.is_finished(),
        "caller cancellation ended admission"
    );
    release.release();
    tokio::time::timeout(WAIT, closing)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let saved = h.store.stored(ID).unwrap();
    assert_eq!(
        h.store.save_count(),
        if name == "persist.before" { 3 } else { 1 }
    );
    assert_eq!(
        saved.metadata.get("refresh_token"),
        Some(&json!("rotated-refresh"))
    );
    assert_eq!(
        saved.metadata.get("access_token"),
        Some(&json!("rotated-access"))
    );
    assert_eq!(saved.registration_epoch, auth.registration_epoch);
    assert_eq!(h.manager.gate().state(), GateState::Closed);
}

// Not upstream's: dropping the caller after rotation cannot cancel its save.
#[test]
fn refresh_after_rotate_holds_an_owned_refresh() {
    isolated("refresh_after_rotate_holds_an_owned_refresh", |root| {
        held_refresh(root, "refresh.after_rotate")
    });
}

// Not upstream's: dropping the caller immediately before save is also safe.
#[test]
fn refresh_before_save_holds_an_owned_refresh() {
    isolated("refresh_before_save_holds_an_owned_refresh", |root| {
        held_refresh(root, "refresh.before_save")
    });
}

// Not upstream's: persist blocks its save, not the runtime or Manager get.
#[test]
fn persist_before_holds_an_owned_refresh_without_blocking_get() {
    isolated(
        "persist_before_holds_an_owned_refresh_without_blocking_get",
        |root| held_refresh(root, "persist.before"),
    );
}

// Not upstream's: an installed held persist must leave the runtime thread.
#[test]
fn held_persist_hook_runs_on_a_blocking_thread() {
    isolated(
        "held_persist_hook_runs_on_a_blocking_thread",
        |root| async move {
            let dir = root.join(".blue-green-test");
            fs::create_dir_all(&dir).unwrap();
            let release = Release::hold(&dir, "persist.before");
            let runtime_thread = std::thread::current().id();
            let files = dir.clone();
            test_hooks::install(Box::new(move |name| {
                if name == "persist.before" {
                    if std::thread::current().id() == runtime_thread {
                        return Err(io::Error::other("persist stayed on the runtime thread"));
                    }
                    fs::write(files.join("persist.before.arrived"), [])?;
                    while !files.join("persist.before.release").exists() {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                Ok(())
            }))
            .unwrap();
            let (h, _, _) = harness();
            let manager = h.manager.clone();
            let refresh = tokio::spawn(async move { manager.refresh_at_epoch(ID, "", 0).await });
            arrived(&dir.join("persist.before.arrived")).await;
            assert_eq!(h.store.save_count(), 0);
            assert!(!refresh.is_finished());
            release.release();
            tokio::time::timeout(WAIT, refresh)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(h.store.save_count(), 1);
        },
    );
}

// Not upstream's: a service-style unsaved upsert can reconcile into a save
// inside task-local admission without blocking the current-thread runtime.
#[test]
fn task_local_upsert_reconcile_skips_held_persist() {
    isolated(
        "task_local_upsert_reconcile_skips_held_persist",
        |root| async move {
            let dir = install(&root);
            let _release = Release::hold(&dir, "persist.before");
            let (h, auth, _) = harness();
            let operation = h.manager.gate().begin_operation().unwrap();
            let manager = h.manager.clone();
            let upsert = tokio::spawn(operation.scope(async move {
                let mut updated = (*auth).clone();
                updated
                    .model_states
                    .insert("removed-model".into(), Default::default());
                manager.update_unsaved(updated).unwrap();
                manager.reconcile_registry_model_states(ID);
            }));
            let tick = tokio::spawn(async {
                tokio::task::yield_now().await;
                7
            });
            assert_eq!(tokio::time::timeout(WAIT, tick).await.unwrap().unwrap(), 7);
            tokio::time::timeout(WAIT, upsert).await.unwrap().unwrap();
            assert!(h.manager.get(ID).unwrap().model_states.is_empty());
            assert_eq!(h.store.save_count(), 1, "reconcile must actually save");
            assert!(!dir.join("persist.before.arrived").exists());
        },
    );
}

// Not upstream's: a plain-thread save without admission skips the hook.
#[test]
fn unadmitted_plain_thread_save_skips_held_persist() {
    isolated(
        "unadmitted_plain_thread_save_skips_held_persist",
        |root| async move {
            let dir = install(&root);
            let (h, auth, _) = harness();
            let manager = h.manager.clone();
            let thread = std::thread::spawn(move || {
                assert!(Operation::current().is_none());
                let mut updated = (*auth).clone();
                updated.label = "plain thread".into();
                manager.update(updated)
            });
            thread.join().unwrap().unwrap();
            // The old no-runtime guard fires an unadmitted hook, which returns
            // Err before reading .hold; update swallows it and skips the save.
            // Only save_count separates that guard from the admitted-only one.
            assert_eq!(h.store.save_count(), 1);
            assert!(!dir.join("persist.before.arrived").exists());
            assert!(!dir.join("persist.before.returned").exists());
        },
    );
}

async fn failed_refresh(root: PathBuf, name: &str) {
    let dir = install(&root);
    fs::write(dir.join(format!("{name}.fail")), []).unwrap();
    let (h, _, executor) = harness();
    let watermark = || {
        super::super::super::lock(&h.manager.shared.persist_locks)
            .get(ID)
            .map(|entry| *super::super::super::lock(entry))
    };
    let before = watermark();
    let refreshed = h.manager.refresh_at_epoch(ID, "", 0).await.unwrap();
    assert_eq!(
        refreshed.metadata.get("refresh_token"),
        Some(&json!("rotated-refresh"))
    );
    assert_eq!(
        h.manager.get(ID).unwrap().metadata.get("access_token"),
        Some(&json!("rotated-access"))
    );
    assert!(refreshed.last_refreshed_at.is_some());
    assert!(dir.join(format!("{name}.failed")).exists());
    assert_eq!(executor.refresh_count(), 1);
    assert_eq!(h.store.save_count(), 0);
    assert_eq!(
        watermark(),
        before,
        "failed save advanced the disk watermark"
    );
}

// Not upstream's: failed save after rotation keeps the new tokens in memory.
#[test]
fn after_rotate_failure_keeps_rotated_tokens() {
    isolated("after_rotate_failure_keeps_rotated_tokens", |root| {
        failed_refresh(root, "refresh.after_rotate")
    });
}

// Not upstream's: a failure immediately before save writes nothing.
#[test]
fn before_save_failure_keeps_rotated_tokens() {
    isolated("before_save_failure_keeps_rotated_tokens", |root| {
        failed_refresh(root, "refresh.before_save")
    });
}

// Not upstream's: the synchronous persist API reports the injected I/O error.
#[test]
fn persist_failure_is_an_io_error() {
    isolated("persist_failure_is_an_io_error", |root| async move {
        let dir = install(&root);
        fs::write(dir.join("persist.before.fail"), []).unwrap();
        let (h, auth, _) = harness();
        let operation = h.manager.gate().begin_operation().unwrap();
        let manager = h.manager.clone();
        let saved = operation.run_blocking(move || {
            manager.persist(&auth, auth.registration_epoch, auth.generation, Save::Yes)
        });
        drop(operation);
        assert!(matches!(saved.await.unwrap(), Err(ManagerError::Store(_))));
        assert!(dir.join("persist.before.failed").exists());
        assert_eq!(h.store.save_count(), 0);
    });
}

// Not upstream's: a direct admitted persist stops before writing, then saves.
#[test]
fn persist_stops_before_the_write() {
    isolated("persist_stops_before_the_write", |root| async move {
        let dir = install(&root);
        let release = Release::hold(&dir, "persist.before");
        let (h, auth, _) = harness();
        let operation = h.manager.gate().begin_operation().unwrap();
        let manager = h.manager.clone();
        let saved = operation.run_blocking(move || {
            manager.persist(&auth, auth.registration_epoch, auth.generation, Save::Yes)
        });
        drop(operation);
        arrived(&dir.join("persist.before.arrived")).await;
        assert_eq!(h.store.save_count(), 0);
        assert!(!saved.is_finished());
        release.release();
        tokio::time::timeout(WAIT, saved)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(h.store.save_count(), 1);
    });
}

// Not upstream's: aborting a barrier await leaves only its inert file poll,
// no continuation or admission; a second hook install cannot replace it.
#[test]
fn an_aborted_barrier_has_no_continuation() {
    isolated(
        "an_aborted_barrier_has_no_continuation",
        |root| async move {
            test_hooks::barrier("refresh.after_rotate").await.unwrap();
            let dir = install(&root);
            assert_eq!(
                test_hooks::install(Box::new(|_| Ok(())))
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::AlreadyExists
            );
            let release = Release::hold(&dir, "refresh.before_save");
            let gate = crate::gate::ActiveGate::open();
            let operation = gate.begin_operation().unwrap();
            let continuation = dir.join("continuation");
            let after = continuation.clone();
            let task = tokio::spawn(operation.scope(async move {
                test_hooks::barrier("refresh.before_save").await.unwrap();
                fs::write(after, []).unwrap();
            }));
            arrived(&dir.join("refresh.before_save.arrived")).await;
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            tokio::time::timeout(WAIT, gate.close(gate.epoch()))
                .await
                .unwrap()
                .unwrap();
            assert!(!continuation.exists());
            release.release();
            arrived(&dir.join("refresh.before_save.returned")).await;
            assert!(!continuation.exists());
        },
    );
}

// Not upstream's: persist failure through refresh keeps new tokens in memory.
#[test]
fn persist_failure_through_refresh_keeps_tokens() {
    isolated("persist_failure_through_refresh_keeps_tokens", |root| {
        failed_refresh(root, "persist.before")
    });
}

// Not upstream's: pipe-sized child output must not stall the watchdog parent.
#[test]
fn barrier_child_stdout_is_drained_while_running() {
    isolated("barrier_child_stdout_is_drained_while_running", |_| async {
        use std::io::Write as _;
        std::io::stdout()
            .write_all(&vec![b'x'; 1024 * 1024])
            .unwrap();
        tokio::task::yield_now().await;
    });
}

// Not upstream's: both injected refresh failures use the common warning once.
#[test]
fn injected_refresh_failures_log_once() {
    isolated("injected_refresh_failures_log_once", |root| async move {
        let dir = install(&root);
        std::thread::spawn(move || {
            for name in ["refresh.after_rotate", "refresh.before_save"] {
                let fail = dir.join(format!("{name}.fail"));
                fs::write(&fail, []).unwrap();
                let (h, _, _) = harness();
                let (result, events) = super::logged_events(|| {
                    let mut future = std::pin::pin!(h.manager.refresh_at_epoch(ID, "", 0));
                    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
                    match future.as_mut().poll(&mut context) {
                        std::task::Poll::Ready(result) => result,
                        std::task::Poll::Pending => panic!("fake refresh unexpectedly pending"),
                    }
                });
                result.unwrap();
                let warnings: Vec<_> = events
                    .iter()
                    .filter(|(level, _)| *level == tracing::Level::WARN)
                    .collect();
                assert_eq!(warnings.len(), 1, "{name}: {events:?}");
                fs::remove_file(fail).unwrap();
            }
        })
        .join()
        .unwrap();
    });
}

// Not upstream's: an installed hook on the no-runtime refresh branch falls
// back to its blocking barrier, rather than converting it into a save failure.
#[test]
fn no_runtime_refresh_uses_blocking_barriers() {
    isolated(
        "no_runtime_refresh_uses_blocking_barriers",
        |root| async move {
            let dir = install(&root);
            let (h, _, _) = harness();
            let manager = h.manager.clone();
            let saved = std::thread::spawn(move || {
                assert!(tokio::runtime::Handle::try_current().is_err());
                let mut future = std::pin::pin!(manager.refresh_at_epoch(ID, "", 0));
                let mut context = std::task::Context::from_waker(std::task::Waker::noop());
                match future.as_mut().poll(&mut context) {
                    std::task::Poll::Ready(result) => result.unwrap(),
                    std::task::Poll::Pending => panic!("fake refresh unexpectedly pending"),
                }
            })
            .join()
            .unwrap();
            assert_eq!(
                saved.metadata.get("refresh_token"),
                Some(&json!("rotated-refresh"))
            );
            assert_eq!(h.store.save_count(), 1);
            assert!(dir.join("refresh.after_rotate.returned").exists());
            assert!(dir.join("refresh.before_save.returned").exists());
            assert!(dir.join("persist.before.returned").exists());
        },
    );
}
