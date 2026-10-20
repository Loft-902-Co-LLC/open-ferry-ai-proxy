//! Automatic checks use the shared switch client only in supervised auto mode.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::support::*;
use crate::background::test_log::Capture;
use crate::fetch::{BoxFuture, Fetch, FetchError};
use crate::journal::{Artifact, Journal, MarkFailed, Outcome};
use crate::pins::Pin;
use crate::state::SwitchRecord;
use crate::{
    CancelOutcome, CheckNow, Report, SwitchError, SwitchEvent, SwitchHandle, SwitchProgress,
    Switcher, UpdateService,
};
use tracing::instrument::WithSubscriber;

// These waits only detect a hung fixture or task. Reaching the fake includes
// staging, syncing files and preparing the install on blocking workers; loaded
// Windows CI has taken over eight seconds. Keep behavioral deadlines (the
// short negative waits and stop responsiveness checks) separate below.
const TEST_WAIT: Duration = Duration::from_secs(30);

struct MemoryFetch {
    bodies: HashMap<String, Vec<u8>>,
    calls: AtomicUsize,
    gate: Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>,
    download_ended: tokio::sync::Notify,
}

struct Downloading<'a>(&'a tokio::sync::Notify);
impl Drop for Downloading<'_> {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

impl Fetch for MemoryFetch {
    fn get<'a>(
        &'a self,
        url: &'a url::Url,
        _limit: u64,
        _timeout: Duration,
    ) -> BoxFuture<'a, Result<Vec<u8>, FetchError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let gate = self.gate.lock().unwrap().clone();
            if url.path().contains("/download/v")
                && let Some((arrived, release)) = gate
            {
                let _downloading = Downloading(&self.download_ended);
                arrived.notify_one();
                release.notified().await;
            }
            self.bodies
                .get(url.path())
                .cloned()
                .ok_or(FetchError::Status(404))
        })
    }
}

struct Done;
impl SwitchProgress for Done {
    fn next(&mut self, _deadline: Instant) -> BoxFuture<'_, Result<SwitchEvent, SwitchError>> {
        Box::pin(async {
            Ok(SwitchEvent::Done {
                result: serde_json::json!({"result":"switched"}),
            })
        })
    }
}

struct Fake {
    data: crate::DataDir,
    calls: Mutex<Vec<(String, bool)>>,
    refuse: bool,
    errors: Mutex<VecDeque<SwitchError>>,
    progress: Mutex<Option<Box<dyn SwitchProgress>>>,
    poll_gate: Mutex<Option<Arc<PollGate>>>,
    gate: Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>,
}

impl Switcher for Fake {
    fn status(&self, _deadline: Instant) -> BoxFuture<'_, Result<serde_json::Value, SwitchError>> {
        Box::pin(async { panic!("automatic checks do not poll the endpoint") })
    }
    fn update<'a>(
        &'a self,
        pin: &'a Pin,
        auto: bool,
        _deadline: Instant,
    ) -> BoxFuture<'a, Result<SwitchHandle, SwitchError>> {
        Box::pin(async move {
            let gate = self.poll_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.entered.notify_one();
                {
                    let (released, _) = gate
                        .wake
                        .wait_timeout_while(gate.released.lock().unwrap(), TEST_WAIT, |released| {
                            !*released
                        })
                        .unwrap();
                    assert!(*released, "the test must release the in-progress poll");
                }
                tokio::task::yield_now().await;
            }
            assert!(
                self.data.try_lock().unwrap().is_some(),
                "endpoint called under the update lock"
            );
            self.calls.lock().unwrap().push((pin.version.clone(), auto));
            let gate = self.gate.lock().unwrap().clone();
            if let Some((arrived, release)) = gate {
                arrived.notify_one();
                release.notified().await;
            }
            if self.refuse {
                return Err(SwitchError::Refused("busy: watching".into()));
            }
            if let Some(error) = self.errors.lock().unwrap().pop_front() {
                return Err(error);
            }
            Ok(SwitchHandle::new(
                "auto".into(),
                self.progress
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap_or_else(|| Box::new(Done)),
            ))
        })
    }
    fn rollback<'a>(
        &'a self,
        _target: &'a Artifact,
        _deadline: Instant,
    ) -> BoxFuture<'a, Result<SwitchHandle, SwitchError>> {
        Box::pin(async { panic!("the supervisor owns rollback") })
    }
    fn cancel(&self, _deadline: Instant) -> BoxFuture<'_, Result<CancelOutcome, SwitchError>> {
        Box::pin(async { panic!("mode changes do not cancel from the worker") })
    }
}

fn config(mode: &str) -> open_ferry_core::config::Config {
    let mut config = open_ferry_core::config::Config::default();
    config.self_update.mode = mode.into();
    config
}

fn service(
    f: &Fixture,
    mode: &str,
    version: &str,
    refuse: bool,
) -> (UpdateService, Arc<Fake>, Arc<MemoryFetch>) {
    let archive = release_archive(version, &f.updater.target, &good_binary(version));
    let name = archive_name(version, &f.updater.target);
    let sums = sums_line(&archive, &name);
    let signature = f
        .key
        .sign(sums.as_bytes(), &format!("open-ferry {version} SHA256SUMS"));
    let fetch = Arc::new(MemoryFetch {
        bodies: HashMap::from([
            (LIST_PATHS[0].into(), sums.into_bytes()),
            (LIST_PATHS[1].into(), signature.into_bytes()),
            (format!("/releases/download/v{version}/{name}"), archive),
        ]),
        calls: AtomicUsize::new(0),
        gate: Mutex::new(None),
        download_ended: tokio::sync::Notify::new(),
    });
    let fake = Arc::new(Fake {
        data: f.data.clone(),
        calls: Mutex::new(Vec::new()),
        refuse,
        errors: Mutex::new(VecDeque::new()),
        progress: Mutex::new(None),
        poll_gate: Mutex::new(None),
        gate: Mutex::new(None),
    });
    let make_fetch: crate::background::MakeFetch = {
        let fetch = fetch.clone();
        Arc::new(move |_| Ok(fetch.clone() as Arc<dyn Fetch>))
    };
    let service = UpdateService::new(f.updater.clone(), make_fetch, &config(mode))
        .with_switcher(fake.clone());
    (service, fake, fetch)
}

#[tokio::test]
async fn an_auto_check_stages_and_starts_one_switch() {
    let f = Fixture::new().await;
    let (service, fake, fetch) = service(&f, "auto", "0.2.0", false);
    let log = Capture::default();
    assert!(matches!(
        service
            .check_once()
            .with_subscriber(log.clone())
            .await
            .unwrap()
            .unwrap()
            .report,
        Report::Staged { .. }
    ));
    let events = log.0.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.contains("asking the supervisor to switch to it")),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .all(|event| !event.contains("run `open-ferry update`")
                && !event.contains("then restart open-ferry")),
        "{events:?}"
    );
    assert_eq!(*fake.calls.lock().unwrap(), [("0.2.0".into(), true)]);
    assert_eq!(
        fetch.calls.load(Ordering::SeqCst),
        3,
        "switching reuses the stage"
    );
}

#[tokio::test]
async fn disabled_switching_logs_once_at_info_without_its_local_reason() {
    let f = Fixture::new().await;
    let (service, fake, _) = service(&f, "auto", "0.2.0", false);
    *fake.errors.lock().unwrap() = VecDeque::from([
        SwitchError::Disabled("/private/first-policy".into()),
        SwitchError::Disabled("/private/second-policy".into()),
    ]);
    let log = Capture::default();
    async {
        service.check_once().await.unwrap().unwrap();
        service.check_once().await.unwrap().unwrap();
    }
    .with_subscriber(log.clone())
    .await;
    let events = log.0.lock().unwrap();
    let notices: Vec<_> = events
        .iter()
        .filter(|event| event.contains("automatic switches are off"))
        .collect();
    assert_eq!(notices.len(), 1, "{events:?}");
    assert!(notices[0].starts_with("INFO"), "{events:?}");
    assert!(notices[0].contains("the supervisor has switching disabled"));
    assert!(events.iter().all(|event| !event.contains("/private/")));
    assert!(
        events
            .iter()
            .all(|event| !event.contains("the supervisor cannot be reached"))
    );
    assert_eq!(fake.calls.lock().unwrap().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn automatic_errors_keep_the_clients_request_certainty_and_never_resend() {
    use crate::UpdateError;

    for (error, level, message) in [
        (
            SwitchError::Unreachable("/private/pre-send-timeout".into()),
            "WARN",
            "the supervisor cannot be reached",
        ),
        (
            SwitchError::Update(UpdateError::Io {
                what: "prepare the update",
                error: "/private/preparation-task".into(),
            }),
            "WARN",
            "the automatic switch could not be prepared",
        ),
        (
            SwitchError::Update(UpdateError::RollbackChanged {
                expected: "0.1.0".into(),
                actual: Some("0.0.9".into()),
            }),
            "WARN",
            "the automatic switch could not be prepared",
        ),
        (
            SwitchError::Update(UpdateError::NotNewer {
                running: "0.2.0".into(),
                latest: "0.2.0".into(),
            }),
            "DEBUG",
            "no newer version is staged",
        ),
        (
            SwitchError::Update(UpdateError::NothingStaged),
            "DEBUG",
            "no newer version is staged",
        ),
        (
            SwitchError::TimedOut,
            "INFO",
            "it may have taken the automatic switch",
        ),
        (
            SwitchError::Stopped,
            "INFO",
            "it may have taken the automatic switch",
        ),
        (
            SwitchError::Protocol("/private/partial-reply".into()),
            "INFO",
            "it may have taken the automatic switch",
        ),
    ] {
        let f = Fixture::new().await;
        let (service, fake, fetch) = service(&f, "auto", "0.2.0", false);
        let uncertain = matches!(
            &error,
            SwitchError::TimedOut | SwitchError::Stopped | SwitchError::Protocol(_)
        );
        fake.errors.lock().unwrap().push_back(error);
        let log = Capture::default();
        async {
            service.check_once().await.unwrap().unwrap();
            let interval = Duration::from_secs(6 * 3600);
            if uncertain {
                service.start(interval);
                let due = Instant::now() + Duration::from_secs(30);
                while service.status().next_check.is_none() {
                    assert!(Instant::now() < due, "the scheduled timer did not start");
                    tokio::task::yield_now().await;
                }
                tokio::time::advance(interval - Duration::from_secs(1)).await;
            } else {
                tokio::time::advance(Duration::from_secs(5 * 60)).await;
            }
            tokio::task::yield_now().await;
            assert_eq!(
                fake.calls.lock().unwrap().len(),
                1,
                "no resend before the next check"
            );
            if uncertain {
                tokio::time::advance(Duration::from_secs(2)).await;
                let due = Instant::now() + Duration::from_secs(30);
                while fake.calls.lock().unwrap().len() < 2 {
                    assert!(
                        Instant::now() < due,
                        "the scheduled check did not ask for a switch"
                    );
                    tokio::task::yield_now().await;
                }
                assert_eq!(
                    *fake.calls.lock().unwrap(),
                    [("0.2.0".into(), true), ("0.2.0".into(), true)]
                );
                assert_eq!(
                    fetch.calls.load(Ordering::SeqCst),
                    5,
                    "the next request follows a fresh scheduled check"
                );
                service.stop_and_wait().await;
            }
        }
        .with_subscriber(log.clone())
        .await;
        assert_eq!(
            fake.calls.lock().unwrap().len(),
            if uncertain { 2 } else { 1 }
        );
        let events = log.0.lock().unwrap();
        let notices: Vec<_> = events
            .iter()
            .filter(|event| event.contains(message))
            .collect();
        assert_eq!(notices.len(), 1, "{events:?}");
        assert!(notices[0].starts_with(level), "{events:?}");
        assert!(events.iter().all(|event| !event.contains("/private/")));
    }
}

#[tokio::test]
async fn overlapping_automatic_checks_send_only_one_switch_request() {
    let f = Fixture::new().await;
    let (service, fake, _) = service(&f, "auto", "0.2.0", false);
    let (arrived, release) = hold_request(&fake);
    let first = tokio::spawn({
        let service = service.clone();
        async move { service.check_once().await }
    });
    tokio::time::timeout(TEST_WAIT, arrived.notified())
        .await
        .unwrap();
    service.check_once().await.unwrap().unwrap();
    assert_eq!(*fake.calls.lock().unwrap(), [("0.2.0".into(), true)]);
    release.notify_one();
    first.await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn a_manual_install_during_an_automatic_request_is_busy() {
    let f = Fixture::new().await;
    let (service, fake, _) = service(&f, "auto", "0.2.0", false);
    let (arrived, release) = hold_request(&fake);
    let first = tokio::spawn({
        let service = service.clone();
        async move { service.check_once().await }
    });
    tokio::time::timeout(TEST_WAIT, arrived.notified())
        .await
        .expect("the automatic check reaches the held switch request");
    assert!(matches!(
        service.install_now(false).await,
        Err(SwitchError::Update(crate::UpdateError::Busy))
    ));
    assert_eq!(fake.calls.lock().unwrap().len(), 1);
    release.notify_one();
    first.await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn a_rollback_during_an_install_request_is_busy() {
    let f = Fixture::new().await;
    let (service, fake, _) = service(&f, "auto", "0.2.0", false);
    let (arrived, release) = hold_request(&fake);
    let first = tokio::spawn({
        let service = service.clone();
        async move { service.install_now(false).await }
    });
    tokio::time::timeout(TEST_WAIT, arrived.notified())
        .await
        .unwrap();
    assert!(matches!(
        service.roll_back("0.1.0").await,
        Err(SwitchError::Update(crate::UpdateError::Busy))
    ));
    assert_eq!(fake.calls.lock().unwrap().len(), 1);
    release.notify_one();
    first.await.unwrap().unwrap();
}

fn hold_request(fake: &Fake) -> (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>) {
    let arrived = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    *fake.gate.lock().unwrap() = Some((arrived.clone(), release.clone()));
    (arrived, release)
}

#[tokio::test]
async fn check_now_uses_the_same_automatic_switch_path() {
    let f = Fixture::new().await;
    let (service, fake, _) = service(&f, "auto", "0.2.0", false);
    let (arrived, release) = hold_request(&fake);
    assert_eq!(service.check_now(), CheckNow::Started);
    tokio::time::timeout(TEST_WAIT, arrived.notified())
        .await
        .expect("the explicit check reaches the automatic switch");
    assert_eq!(*fake.calls.lock().unwrap(), [("0.2.0".into(), true)]);
    assert!(!service.status().checking);
    release.notify_one();
}

#[tokio::test]
async fn stopping_a_check_now_during_staging_prevents_its_switch_request() {
    let f = Fixture::new().await;
    let (service, fake, fetch) = service(&f, "auto", "0.2.0", false);
    let arrived = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    *fetch.gate.lock().unwrap() = Some((arrived.clone(), release.clone()));
    assert_eq!(service.check_now(), CheckNow::Started);
    tokio::time::timeout(TEST_WAIT, arrived.notified())
        .await
        .unwrap();
    service.stop();
    tokio::time::timeout(TEST_WAIT, fetch.download_ended.notified())
        .await
        .unwrap();
    release.notify_one();
    assert!(fake.calls.lock().unwrap().is_empty());
    assert!(!service.status().checking);
    assert_eq!(service.check_now(), CheckNow::Off);
}

struct PendingProgress {
    polled: Arc<tokio::sync::Notify>,
    dropped: Arc<AtomicBool>,
}

impl SwitchProgress for PendingProgress {
    fn next(&mut self, _deadline: Instant) -> BoxFuture<'_, Result<SwitchEvent, SwitchError>> {
        self.polled.notify_one();
        Box::pin(std::future::pending())
    }
}

impl Drop for PendingProgress {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn dropping_the_service_closes_an_accepted_switch_progress_stream() {
    for stop_first in [false, true] {
        let f = Fixture::new().await;
        let (service, fake, _) = service(&f, "auto", "0.2.0", false);
        let polled = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(AtomicBool::new(false));
        *fake.progress.lock().unwrap() = Some(Box::new(PendingProgress {
            polled: polled.clone(),
            dropped: dropped.clone(),
        }));
        service.start(Duration::from_secs(3600));
        service.check_once().await.unwrap().unwrap();
        tokio::time::timeout(TEST_WAIT, polled.notified())
            .await
            .unwrap();
        if stop_first {
            service.stop();
        }
        assert!(
            !dropped.load(Ordering::SeqCst),
            "stop keeps followers alive"
        );
        let other = service.clone();
        drop(service);
        assert!(
            !dropped.load(Ordering::SeqCst),
            "another service clone owns the follower"
        );
        drop(other);
        tokio::time::timeout(TEST_WAIT, async {
            while !dropped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(fake.calls.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn notify_and_off_leave_an_existing_stage_unused() {
    for mode in ["notify", "off"] {
        let f = Fixture::new().await;
        f.release("0.2.0");
        f.updater.check(crate::SelfUpdateMode::Auto).await.unwrap();
        let (service, fake, fetch) = service(&f, mode, "0.2.0", false);
        service.check_once().await;
        assert!(matches!(
            service.install_now(true).await,
            Err(SwitchError::UpdatesOff)
        ));
        assert!(fake.calls.lock().unwrap().is_empty());
        assert_eq!(
            fetch.calls.load(Ordering::SeqCst),
            if mode == "off" { 0 } else { 2 }
        );
    }
}

#[tokio::test]
async fn a_refused_stage_is_retried_only_on_the_next_check() {
    let f = Fixture::new().await;
    let (service, fake, fetch) = service(&f, "auto", "0.2.0", true);
    service.check_once().await.unwrap().unwrap();
    assert_eq!(fake.calls.lock().unwrap().len(), 1);
    tokio::time::pause();
    let interval = Duration::from_secs(6 * 3600);
    service.start(interval);
    while service.status().next_check.is_none() {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(interval - Duration::from_secs(1)).await;
    assert_eq!(fake.calls.lock().unwrap().len(), 1);
    // Move the timer away again so the explicit next check is the only one.
    service.start(interval);
    tokio::time::resume();
    service.check_once().await.unwrap().unwrap();
    assert_eq!(
        *fake.calls.lock().unwrap(),
        [("0.2.0".into(), true), ("0.2.0".into(), true)]
    );
    assert_eq!(
        fetch.calls.load(Ordering::SeqCst),
        5,
        "the next check keeps the existing stage"
    );
    service.set_config(&config("notify"));
    service.check_once().await.unwrap().unwrap();
    service.set_config(&config("off"));
    assert!(service.check_once().await.is_none());
    assert_eq!(fake.calls.lock().unwrap().len(), 2);
    service.stop();
}

#[tokio::test]
async fn pending_only_journal_exclusions_stop_the_automatic_trigger_without_replaying_state() {
    for failed in [false, true] {
        let f = Fixture::new().await;
        let (service, fake, fetch) = service(&f, "auto", "0.2.0", false);
        let active = Artifact {
            version: "0.1.0".into(),
            sha256: crate::staging::hash_file(&f.installed).unwrap(),
            size: std::fs::metadata(&f.installed).unwrap().len(),
        };
        let mut journal = Journal::first_start(active, None);
        let record = if failed {
            SwitchRecord::failed_after_switch(
                "earlier",
                "update",
                "0.1.0",
                "0.2.0",
                "2026-10-19T00:00:00Z",
                "watching",
                "new-version",
                "exited",
                "0.1.0",
            )
        } else {
            SwitchRecord::started(
                "earlier",
                "rollback",
                "0.2.0",
                "0.1.0",
                "2026-10-19T00:00:00Z",
            )
        };
        journal.push_outcome(Outcome {
            record,
            mark_failed: failed.then(|| MarkFailed {
                version: "0.2.0".into(),
                why: "exited".into(),
            }),
        });
        let path = f.data.root().join(crate::journal::JOURNAL_FILE);
        let bytes = serde_json::to_vec(&journal).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let report = service.check_once().await.unwrap().unwrap().report;
        assert!(matches!(
            (failed, report),
            (true, Report::SkippedFailed { .. }) | (false, Report::SkippedRolledBack { .. })
        ));
        assert!(fake.calls.lock().unwrap().is_empty());
        assert_eq!(fetch.calls.load(Ordering::SeqCst), 2);
        assert!(!f.data.version_dir("0.2.0").exists());
        let state = f.updater.state();
        assert!(state.failed.is_empty() && state.rolled_back.is_none() && state.history.is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}

#[tokio::test]
async fn missing_or_odd_welcome_install_paths_only_notify_and_never_stage_or_switch() {
    for case in ["absent", "missing", "odd"] {
        let mut f = Fixture::new().await;
        let kept = f.data.binary("0.1.0", "open-ferry");
        std::fs::create_dir_all(kept.parent().unwrap()).unwrap();
        std::fs::write(&kept, good_binary("0.1.0")).unwrap();
        f.updater.system = Arc::new(FakeSystem {
            exe: kept,
            container: false,
            writable: true,
        });
        let (service, fake, fetch) = service(&f, "auto", "0.2.0", false);
        let service = match case {
            "missing" => service.with_installed_path(f.temp.path().join("missing/open-ferry")),
            "odd" => {
                let odd = f.temp.path().join("other-program");
                std::fs::write(&odd, good_binary("0.1.0")).unwrap();
                service.with_installed_path(odd)
            }
            _ => service,
        };
        assert!(!service.status().can_update_itself);
        assert!(matches!(
            service.check_once().await.unwrap().unwrap().report,
            Report::Available {
                why_not: Some(_),
                ..
            }
        ));
        assert!(f.updater.state().staged.is_none());
        assert!(!f.data.version_dir("0.2.0").exists());
        assert_eq!(fetch.calls.load(Ordering::SeqCst), 2);
        assert!(fake.calls.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn failed_rolled_back_and_running_releases_never_switch() {
    for excluded in ["failed", "rolled-back", "running"] {
        let f = Fixture::new().await;
        let version = if excluded == "running" {
            "0.1.0"
        } else {
            "0.2.0"
        };
        f.updater
            .mutate_state(|state| match excluded {
                "failed" => state.mark_failed(version),
                "rolled-back" => state.rolled_back = Some(version.into()),
                _ => {}
            })
            .unwrap();
        let (service, fake, fetch) = service(&f, "auto", version, false);
        let report = service.check_once().await.unwrap().unwrap().report;
        assert!(matches!(
            (excluded, report),
            ("failed", Report::SkippedFailed { .. })
                | ("rolled-back", Report::SkippedRolledBack { .. })
                | ("running", Report::UpToDate { .. })
        ));
        assert!(fake.calls.lock().unwrap().is_empty());
        assert_eq!(fetch.calls.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn a_worker_running_from_a_kept_version_still_stages_the_next_release() {
    let mut f = Fixture::new().await;
    let kept = f.data.binary("0.1.0", "open-ferry");
    std::fs::create_dir_all(kept.parent().unwrap()).unwrap();
    std::fs::write(&kept, good_binary("0.1.0")).unwrap();
    f.updater.system = Arc::new(FakeSystem {
        exe: kept,
        container: false,
        writable: true,
    });
    assert!(!f.updater.install().can_update_itself());
    let (service, fake, _) = service(&f, "auto", "0.2.0", false);
    let service = service.with_installed_path(f.installed.clone());
    assert!(service.status().can_update_itself);
    assert!(matches!(
        service.check_once().await.unwrap().unwrap().report,
        Report::Staged { .. }
    ));
    assert!(service.status().can_update_itself);
    assert_eq!(*fake.calls.lock().unwrap(), [("0.2.0".into(), true)]);
}

#[tokio::test]
async fn lowering_the_mode_while_staging_never_starts_a_switch() {
    for mode in ["notify", "off"] {
        let f = Fixture::new().await;
        let (service, fake, fetch) = service(&f, "auto", "0.2.0", false);
        let arrived = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        *fetch.gate.lock().unwrap() = Some((arrived.clone(), release.clone()));
        let check = tokio::spawn({
            let service = service.clone();
            async move { service.check_once().await }
        });
        tokio::time::timeout(TEST_WAIT, arrived.notified())
            .await
            .unwrap();
        service.set_config(&config(mode));
        release.notify_one();
        check.await.unwrap();
        assert!(fake.calls.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn lowering_the_mode_during_install_preparation_never_sends_an_automatic_request() {
    for mode in ["notify", "off"] {
        let f = Fixture::new().await;
        let (service, fake, fetch) = service(&f, "auto", "0.2.0", false);
        let arrived = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        *fetch.gate.lock().unwrap() = Some((arrived.clone(), release.clone()));
        let install = tokio::spawn({
            let service = service.clone();
            async move { service.install_now(true).await }
        });
        tokio::time::timeout(TEST_WAIT, arrived.notified())
            .await
            .unwrap();
        service.set_config(&config(mode));
        release.notify_one();
        assert!(matches!(
            install.await.unwrap(),
            Err(SwitchError::UpdatesOff)
        ));
        assert!(fake.calls.lock().unwrap().is_empty());
    }
}

struct LaterProgress {
    polled: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}
impl SwitchProgress for LaterProgress {
    fn next(&mut self, _deadline: Instant) -> BoxFuture<'_, Result<SwitchEvent, SwitchError>> {
        Box::pin(async move {
            self.polled.notify_one();
            self.release.notified().await;
            Ok(SwitchEvent::Done {
                result: serde_json::json!({"result": "failed-to-load"}),
            })
        })
    }
}

#[tokio::test]
async fn a_switch_follower_logs_its_outcome_after_update_checks_stop() {
    for asynchronous in [false, true] {
        let f = Fixture::new().await;
        let (service, fake, _) = service(&f, "auto", "0.2.0", false);
        let polled = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let logs = Capture::default();
        *fake.progress.lock().unwrap() = Some(Box::new(LaterProgress {
            polled: polled.clone(),
            release: release.clone(),
        }));
        service
            .check_once()
            .with_subscriber(logs.clone())
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(TEST_WAIT, polled.notified())
            .await
            .unwrap();
        if asynchronous {
            tokio::time::timeout(Duration::from_secs(1), service.stop_and_wait())
                .await
                .expect("stopping checks must not wait for the in-flight follower");
        } else {
            service.stop();
        }
        release.notify_one();
        tokio::time::timeout(TEST_WAIT, async {
            loop {
                if logs.0.lock().unwrap().iter().any(|line| {
                    line.starts_with("INFO")
                        && line.contains("automatic switch reported a result")
                        && line.contains("failed-to-load")
                }) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(fake.calls.lock().unwrap().len(), 1);
    }
}

struct PollGate {
    entered: tokio::sync::Notify,
    released: Mutex<bool>,
    wake: std::sync::Condvar,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_stop_waits_for_a_running_poll_before_returning_without_a_send() {
    for earlier in ["none", "sync-stop", "replaced-timer"] {
        let f = Fixture::new().await;
        let (service, fake, _) = service(&f, "auto", "0.2.0", false);
        let gate = Arc::new(PollGate {
            entered: tokio::sync::Notify::new(),
            released: Mutex::new(false),
            wake: std::sync::Condvar::new(),
        });
        *fake.poll_gate.lock().unwrap() = Some(gate.clone());
        if earlier == "replaced-timer" {
            service.start(Duration::ZERO);
        } else {
            assert_eq!(service.check_now(), CheckNow::Started);
        }
        tokio::time::timeout(TEST_WAIT, gate.entered.notified())
            .await
            .unwrap();
        match earlier {
            "sync-stop" => service.stop(),
            "replaced-timer" => service.start(Duration::from_secs(3600)),
            _ => {}
        }
        let stopped = service.stop_and_wait();
        tokio::pin!(stopped);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut stopped)
                .await
                .is_err(),
            "stop must join the poll on the other worker: {earlier}"
        );
        *gate.released.lock().unwrap() = true;
        gate.wake.notify_one();
        tokio::time::timeout(TEST_WAIT, &mut stopped).await.unwrap();
        assert!(
            fake.calls.lock().unwrap().is_empty(),
            "the abort wins before the send: {earlier}"
        );
        assert_eq!(service.check_now(), CheckNow::Off);
    }
}

struct HeldPoll(Arc<PollGate>);

impl HeldPoll {
    async fn start(service: &UpdateService, fake: &Fake) -> Self {
        let held = Self(Arc::new(PollGate {
            entered: tokio::sync::Notify::new(),
            released: Mutex::new(false),
            wake: std::sync::Condvar::new(),
        }));
        *fake.poll_gate.lock().unwrap() = Some(held.0.clone());
        assert_eq!(service.check_now(), CheckNow::Started);
        tokio::time::timeout(TEST_WAIT, held.0.entered.notified())
            .await
            .expect("the automatic check reaches the held switch poll");
        held
    }

    fn release(&self) {
        *self.0.released.lock().unwrap() = true;
        self.0.wake.notify_one();
    }
}

impl Drop for HeldPoll {
    fn drop(&mut self) {
        self.release();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_async_stop_keeps_the_poll_for_the_next_stop() {
    let f = Fixture::new().await;
    let (service, fake, _) = service(&f, "auto", "0.2.0", false);
    let held = HeldPoll::start(&service, &fake).await;
    let mut first = Box::pin(service.stop_and_wait());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut first)
            .await
            .is_err()
    );
    drop(first);
    let mut second = Box::pin(service.stop_and_wait());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut second)
            .await
            .is_err(),
        "cancelling a stop must put the held poll back"
    );
    held.release();
    tokio::time::timeout(TEST_WAIT, second).await.unwrap();
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timed_out_async_stop_keeps_the_poll_for_the_next_stop() {
    let f = Fixture::new().await;
    let (service, fake, _) = service(&f, "auto", "0.2.0", false);
    let held = HeldPoll::start(&service, &fake).await;
    let logs = Capture::default();
    tokio::time::timeout(
        Duration::from_secs(7),
        service.stop_and_wait().with_subscriber(logs.clone()),
    )
    .await
    .unwrap();
    let mut second = Box::pin(service.stop_and_wait());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut second)
            .await
            .is_err(),
        "the five-second escape must put the held poll back"
    );
    held.release();
    tokio::time::timeout(TEST_WAIT, second).await.unwrap();
    assert!(fake.calls.lock().unwrap().is_empty());
    assert!(logs.0.lock().unwrap().iter().any(|line| {
        line.contains("update checks have not stopped after five seconds; continuing")
            && !line.contains("shutdown")
    }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_queued_async_stop_aborts_checks_before_it_can_take_the_gate() {
    let f = Fixture::new().await;
    let (service, fake, _) = service(&f, "auto", "0.2.0", false);
    let held = HeldPoll::start(&service, &fake).await;
    let mut first = Box::pin(service.stop_and_wait());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut first)
            .await
            .is_err()
    );
    service.start(Duration::from_secs(3600));
    let mut second = Box::pin(service.stop_and_wait());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut second)
            .await
            .is_err()
    );
    assert_eq!(
        service.check_now(),
        CheckNow::Off,
        "a queued stop must already abort checks"
    );
    drop(second);
    held.release();
    tokio::time::timeout(TEST_WAIT, first).await.unwrap();
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_async_stop_aborts_a_timer_restarted_while_it_waits_for_the_gate() {
    let f = Fixture::new().await;
    let (service, fake, _) = service(&f, "auto", "0.2.0", false);
    let held = HeldPoll::start(&service, &fake).await;
    let mut first = Box::pin(service.stop_and_wait());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut first)
            .await
            .is_err()
    );
    let mut second = Box::pin(service.stop_and_wait());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut second)
            .await
            .is_err()
    );
    service.start(Duration::from_secs(3600));
    drop(first);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut second)
            .await
            .is_err(),
        "the held poll must remain tracked across the first stop's cancellation"
    );
    held.release();
    tokio::time::timeout(Duration::from_secs(1), second)
        .await
        .expect("the restarted timer must have been aborted before it was taken");
    assert_eq!(service.check_now(), CheckNow::Off);
    assert!(fake.calls.lock().unwrap().is_empty());
}
