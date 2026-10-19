//! Automatic checks use the shared switch client only in supervised auto mode.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::support::*;
use crate::fetch::{BoxFuture, Fetch, FetchError};
use crate::journal::Artifact;
use crate::pins::Pin;
use crate::{
    CancelOutcome, CheckNow, Report, SwitchError, SwitchEvent, SwitchHandle, SwitchProgress,
    Switcher, UpdateService,
};

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
    progress: Mutex<Option<Box<dyn SwitchProgress>>>,
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
        progress: Mutex::new(None),
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
    assert!(matches!(
        service.check_once().await.unwrap().unwrap().report,
        Report::Staged { .. }
    ));
    assert_eq!(*fake.calls.lock().unwrap(), [("0.2.0".into(), true)]);
    assert_eq!(
        fetch.calls.load(Ordering::SeqCst),
        3,
        "switching reuses the stage"
    );
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
    tokio::time::timeout(Duration::from_secs(5), arrived.notified())
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
    tokio::time::timeout(Duration::from_secs(5), arrived.notified())
        .await
        .unwrap();
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
    tokio::time::timeout(Duration::from_secs(5), arrived.notified())
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
    assert_eq!(service.check_now(), CheckNow::Started);
    for _ in 0..500 {
        if !fake.calls.lock().unwrap().is_empty() && !service.status().checking {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(*fake.calls.lock().unwrap(), [("0.2.0".into(), true)]);
    assert!(!service.status().checking);
}

#[tokio::test]
async fn stopping_a_check_now_during_staging_prevents_its_switch_request() {
    let f = Fixture::new().await;
    let (service, fake, fetch) = service(&f, "auto", "0.2.0", false);
    let arrived = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    *fetch.gate.lock().unwrap() = Some((arrived.clone(), release.clone()));
    assert_eq!(service.check_now(), CheckNow::Started);
    tokio::time::timeout(Duration::from_secs(5), arrived.notified())
        .await
        .unwrap();
    service.stop();
    tokio::time::timeout(Duration::from_secs(5), fetch.download_ended.notified())
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
async fn stopping_the_service_closes_an_accepted_switch_progress_stream() {
    let f = Fixture::new().await;
    let (service, fake, _) = service(&f, "auto", "0.2.0", false);
    let polled = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    *fake.progress.lock().unwrap() = Some(Box::new(PendingProgress {
        polled: polled.clone(),
        dropped: dropped.clone(),
    }));
    service.check_once().await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(5), polled.notified())
        .await
        .unwrap();
    service.stop();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !dropped.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(fake.calls.lock().unwrap().len(), 1);
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
    tokio::task::yield_now().await;
    assert_eq!(fake.calls.lock().unwrap().len(), 1);
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
        tokio::time::timeout(Duration::from_secs(5), arrived.notified())
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
        tokio::time::timeout(Duration::from_secs(5), arrived.notified())
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
