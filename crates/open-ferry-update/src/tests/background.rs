//! Not upstream's: the server's background check: off makes no request,
//! the config is followed as it is reloaded, turning updates off stops a
//! check, and a failed check doesn't stop the next.

use std::time::Duration;

use open_ferry_core::config::{Config, SelfUpdateMode};

use super::support::*;
use crate::background::{CheckNow, UpdateService, http_fetch};
use crate::fetch::FetchError;
use crate::updater::{Report, UpdateError};

fn config(mode: &str) -> Config {
    let mut config = Config::default();
    config.self_update.mode = mode.to_owned();
    config
}

fn service(f: &Fixture, mode: &str) -> UpdateService {
    UpdateService::new(f.updater.clone(), http_fetch(), &config(mode))
}

/// An observer holds a blocking operation until a task on this current-thread
/// runtime acknowledges it. The timeout releases the barrier even if a fault
/// puts the operation back on the runtime, making that fault fail promptly.
#[cfg(feature = "blue-green-test")]
async fn responsive_at(f: &Fixture, name: &str) {
    let service = service(f, "auto");
    let result = responsive_operation(f, name, service.check_once()).await;
    assert!(matches!(result, Some(Ok(result)) if matches!(result.report, Report::Staged { .. })));
}

#[cfg(feature = "blue-green-test")]
async fn responsive_operation<T>(
    f: &Fixture,
    name: &str,
    operation: impl std::future::Future<Output = T>,
) -> T {
    let (held, arrived) = crate::test_seams::held::Held::new(f.data.root(), name);
    let (probe, receive) = tokio::sync::oneshot::channel();
    let (acknowledge, ack) = std::sync::mpsc::channel();
    let observer = std::thread::spawn(move || {
        let result = (|| {
            arrived
                .recv_timeout(Duration::from_secs(5))
                .map_err(|e| e.to_string())?;
            probe
                .send(())
                .map_err(|_| "runtime probe was dropped".to_owned())?;
            ack.recv_timeout(Duration::from_secs(5))
                .map_err(|e| e.to_string())
        })();
        drop(held);
        result
    });
    let (result, ()) = tokio::join!(operation, async {
        receive.await.expect("operation must reach its seam");
        let _ = acknowledge.send(());
    });
    observer
        .join()
        .unwrap()
        .expect("serving runtime stalled at the held operation");
    result
}

#[cfg(feature = "blue-green-test")]
#[tokio::test(flavor = "current_thread")]
async fn runtime_staged_rehash_remains_responsive_and_detects_changed_bytes() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    assert!(service(&f, "auto").check_once().await.unwrap().is_ok());
    let downloads = f.server.requests();
    responsive_at(&f, "stage.before_staged_hash").await;
    assert_eq!(
        f.server.requests(),
        downloads + 2,
        "only the signed release list is fetched again"
    );
    let path = f.data.binary("0.2.0", f.updater.binary_name());
    std::fs::write(&path, good_binary("0.3.0")).unwrap();
    assert!(service(&f, "auto").check_once().await.unwrap().is_ok());
    assert_eq!(std::fs::read(&path).unwrap(), good_binary("0.2.0"));
    assert_eq!(f.runner.runs.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[cfg(feature = "blue-green-test")]
#[tokio::test(flavor = "current_thread")]
async fn runtime_fresh_staged_rehash_remains_responsive() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    assert!(service(&f, "auto").check_once().await.unwrap().is_ok());
    let _lock = f.updater.lock().unwrap();
    let mut stale = crate::State::default();
    let found = f.updater.latest(&stale).await.unwrap();
    let path = responsive_operation(
        &f,
        "stage.before_staged_hash",
        f.updater.stage(&found, &mut stale),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), good_binary("0.2.0"));
    assert_eq!(f.runner.runs.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[cfg(feature = "blue-green-test")]
#[tokio::test(flavor = "current_thread")]
async fn runtime_retained_binary_hashes_remain_responsive() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    responsive_at(&f, "stage.before_cleanup_hash").await;
}

#[cfg(feature = "blue-green-test")]
#[tokio::test(flavor = "current_thread")]
async fn runtime_staging_write_and_sync_remain_responsive() {
    for seam in ["stage.before_write", "stage.before_sync"] {
        let f = Fixture::new().await;
        f.release("0.2.0");
        responsive_at(&f, seam).await;
        assert_eq!(f.updater.state().staged.as_deref(), Some("0.2.0"));
        assert_eq!(f.runner.runs.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}

#[cfg(feature = "blue-green-test")]
#[tokio::test(flavor = "current_thread")]
async fn runtime_cancelled_staging_cannot_publish_after_releasing_the_lock() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    let service = service(&f, "auto");
    let (held, arrived) = crate::test_seams::held::Held::new(f.data.root(), "stage.before_sync");
    let (notify, waiting) = tokio::sync::oneshot::channel();
    let observer = std::thread::spawn(move || {
        arrived.recv_timeout(Duration::from_secs(5)).unwrap();
        notify.send(()).unwrap();
    });
    assert_eq!(service.check_now(), CheckNow::Started);
    waiting.await.unwrap();
    observer.join().unwrap();
    service.stop_and_wait().await;
    let _lock = f.updater.lock().expect("cancelled check releases its lock");
    assert!(f.updater.state().staged.is_none());
    assert!(!f.data.version_dir("0.2.0").exists());
    assert_eq!(f.runner.runs.load(std::sync::atomic::Ordering::SeqCst), 0);
    drop(held);
    // The discarded blocking return value owns the private directory. Wait
    // for its Drop without using a sleep as synchronization.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if std::fs::read_dir(f.data.versions())
                .unwrap()
                .next()
                .is_none()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("private staging directory is removed after cancellation");
    assert!(f.updater.state().staged.is_none());
    assert!(!f.data.version_dir("0.2.0").exists());
}

// Hold archive processing until an HTTP response has completed on the same
// single-thread runtime. The observer releases the seam even on an I/O error,
// so putting unpacking back on the runtime fails instead of deadlocking.
#[cfg(feature = "blue-green-test")]
#[tokio::test(flavor = "current_thread")]
async fn archive_processing_does_not_stall_http_serving() {
    use std::io::{Read, Write};
    use std::time::Instant;

    let f = Fixture::new().await;
    f.release("0.2.0");
    f.server
        .reply("/serving", Reply::Body(b"still serving".to_vec()));
    let seams = f.data.root().join(".blue-green-test");
    std::fs::create_dir_all(&seams).unwrap();
    std::fs::write(seams.join("stage.before_unpack.hold"), []).unwrap();
    let address = f.server.address;
    assert_ne!(address.port(), 8317);
    let request = std::thread::spawn(move || {
        let response = (|| -> std::io::Result<String> {
            let due = Instant::now() + Duration::from_secs(10);
            while !seams.join("stage.before_unpack.arrived").exists() {
                if Instant::now() >= due {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "archive processing never reached its barrier",
                    ));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            let wait = Duration::from_secs(5);
            let mut stream = std::net::TcpStream::connect_timeout(&address, wait)?;
            stream.set_read_timeout(Some(wait))?;
            stream.set_write_timeout(Some(wait))?;
            stream
                .write_all(b"GET /serving HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")?;
            let mut response = String::new();
            stream.read_to_string(&mut response)?;
            Ok(response)
        })();
        std::fs::write(seams.join("stage.before_unpack.release"), []).unwrap();
        response
    });
    let result = service(&f, "auto").check_once().await;
    let response = request
        .join()
        .unwrap()
        .expect("HTTP must complete while archive processing is held");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.ends_with("still serving"), "{response}");
    assert!(matches!(result, Some(Ok(result)) if matches!(result.report, Report::Staged { .. })));
}

/// Waits up to five seconds for `done`.
async fn until(mut done: impl FnMut() -> bool) {
    for _ in 0..500 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out");
}

#[tokio::test]
async fn off_makes_no_request_at_all() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    let service = service(&f, "off");
    service.start(Duration::ZERO);
    assert_eq!(service.check_now(), CheckNow::Off);
    assert!(service.check_once().await.is_none());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(f.server.requests(), 0);
    let status = service.status();
    assert_eq!(status.mode, "off");
    assert_eq!(status.updates, "off");
    assert_eq!(status.mode_source, "config");
    assert_eq!(status.next_check, None);
    assert_eq!(status.last_check, None);
    assert!(f.data.kept_versions().is_empty());
    service.stop();
}

#[tokio::test]
async fn off_leaves_a_staged_version_unused() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    let on = service(&f, "auto");
    assert!(matches!(
        on.check_once().await,
        Some(Ok(result)) if matches!(result.report, Report::Staged { .. })
    ));
    let requests = f.server.requests();
    on.set_config(&config("off"));
    on.start(Duration::ZERO);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(on.check_once().await.is_none());
    assert_eq!(f.server.requests(), requests);
    assert_eq!(f.installed_text(), "open-ferry 0.1.0");
    assert_eq!(on.status().staged_version.as_deref(), Some("0.2.0"));
    on.stop();
}

#[tokio::test]
async fn turning_updates_on_while_running_starts_the_checks() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    let service = service(&f, "off");
    service.start(Duration::from_millis(50));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(f.server.requests(), 0);
    service.set_config(&config("notify"));
    until(|| f.server.requests() >= 2).await;
    until(|| service.status().last_result.is_some()).await;
    let status = service.status();
    assert_eq!(status.mode, "notify");
    assert_eq!(status.last_result.as_deref(), Some("update-available"));
    assert_eq!(status.latest_version.as_deref(), Some("0.2.0"));
    assert!(status.next_check.is_some());
    assert!(f.data.kept_versions().is_empty());
    service.stop();
}

#[tokio::test]
async fn turning_updates_off_stops_a_check_in_progress() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    f.server.reply(
        LIST_PATHS[0],
        Reply::Slow(Duration::from_secs(30), b"slow".to_vec()),
    );
    let service = service(&f, "auto");
    let running = tokio::spawn({
        let service = service.clone();
        async move { service.check_once().await }
    });
    until(|| f.server.requests() == 1).await;
    until(|| service.status().checking).await;
    assert_eq!(service.check_now(), CheckNow::Running);
    service.set_config(&config("off"));
    let outcome = tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("the check stops")
        .unwrap();
    assert!(outcome.is_none());
    assert!(!service.status().checking);
    assert_eq!(f.server.requests(), 1);
    assert!(f.data.kept_versions().is_empty());
}

#[tokio::test]
async fn check_now_starts_a_check_and_returns() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    let service = service(&f, "auto");
    assert_eq!(service.check_now(), CheckNow::Started);
    until(|| service.status().last_result.as_deref() == Some("staged")).await;
    assert_eq!(service.status().staged_version.as_deref(), Some("0.2.0"));
    assert_eq!(f.installed_text(), "open-ferry 0.1.0");
}

#[tokio::test(start_paused = true)]
async fn runtime_busy_check_waits_and_reports_without_writing_state() {
    let f = Fixture::new().await;
    let service = service(&f, "notify");
    let lock = f.updater.lock().unwrap();
    let before = std::fs::read(f.data.state_file()).ok();
    assert_eq!(service.check_now(), CheckNow::Started);
    // Let the explicit task poll once before advancing its retry clock.
    tokio::task::yield_now().await;
    for _ in 0..19 {
        tokio::time::advance(Duration::from_millis(250)).await;
        tokio::task::yield_now().await;
    }
    // The owner can publish while the manual request is waiting, then keep
    // its lock beyond the deadline. The final busy result must still show.
    let mut during_wait = f.updater.state();
    during_wait.last_result = Some("up-to-date".into());
    f.updater.save(&during_wait).unwrap();
    let published = std::fs::read(f.data.state_file()).ok();
    assert_ne!(published, before);
    assert!(service.status().checking);
    assert_eq!(service.check_now(), CheckNow::Running);
    tokio::time::advance(Duration::from_millis(250)).await;
    // Await completion as well as advancing the retry clock.
    tokio::time::resume();
    tokio::time::timeout(Duration::from_secs(5), async {
        while service.status().checking {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the busy check finishes at its retry deadline");
    let status = service.status();
    assert!(!status.checking);
    assert!(status.last_check.is_some());
    assert_eq!(status.last_result.as_deref(), Some("error"));
    assert_eq!(
        status.last_error.as_deref(),
        Some("another update is running; try again shortly")
    );
    assert_eq!(std::fs::read(f.data.state_file()).ok(), published);
    assert_eq!(f.server.requests(), 0);
    // A persisted result from a different process takes precedence.
    let mut state = f.updater.state();
    state.last_check = Some("2026-10-19T12:00:00Z".into());
    state.last_result = Some("up-to-date".into());
    f.updater.save(&state).unwrap();
    assert_eq!(service.status().last_result, state.last_result);
    assert_eq!(service.status().last_error, None);
    drop(lock);
}

#[tokio::test]
async fn runtime_nonbusy_failure_replaces_busy_and_is_superseded() {
    for scheduled in [false, true] {
        let f = Fixture::new().await;
        let service = service(&f, "notify");
        tokio::time::pause();
        let lock = f.updater.lock().unwrap();
        assert!(matches!(
            service.check_once().await,
            Some(Err(UpdateError::Busy))
        ));
        assert!(
            service
                .status()
                .last_error
                .unwrap()
                .contains("another update")
        );
        drop(lock);
        tokio::time::resume();
        let before = std::fs::read(f.data.state_file()).ok();
        let mut changed = config("notify");
        changed.proxy_url = "socks5://user:secret@127.0.0.1:1080".into();
        service.set_config(&changed);
        if scheduled {
            service.start(Duration::ZERO);
            tokio::time::timeout(Duration::from_secs(5), async {
                while !service
                    .status()
                    .last_error
                    .is_some_and(|error| error.contains("SOCKS"))
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("scheduled failure replaces the busy result");
            service.stop_and_wait().await;
        } else {
            assert!(service.check_once().await.unwrap().is_err());
        }
        let status = service.status();
        let error = status.last_error.unwrap();
        assert!(error.contains("SOCKS"), "{error}");
        assert!(
            !error.contains("another update") && !error.contains("secret"),
            "{error}"
        );
        assert_eq!(std::fs::read(f.data.state_file()).ok(), before);
        let _lock = f.updater.lock().unwrap();
        let mut state = f.updater.state();
        state.last_result = Some("up-to-date".into());
        f.updater.save(&state).unwrap();
        assert_eq!(service.status().last_result, state.last_result);
        assert!(service.status().last_error.is_none());
    }
}

#[cfg(feature = "blue-green-test")]
#[tokio::test(flavor = "current_thread")]
async fn runtime_status_releases_live_mutex_and_preserves_a_newer_error() {
    let f = Fixture::new().await;
    let factory: crate::background::MakeFetch =
        std::sync::Arc::new(|proxy| Err(FetchError::Proxy(proxy.to_owned())));
    let mut changed = config("notify");
    changed.proxy_url = "first failure".into();
    let service = UpdateService::new(f.updater.clone(), factory, &changed);
    assert!(service.check_once().await.unwrap().is_err());
    {
        let _lock = f.updater.lock().unwrap();
        let mut state = f.updater.state();
        state.last_result = Some("up-to-date".into());
        f.updater.save(&state).unwrap();
    }
    let (held, arrived) =
        crate::test_seams::held::Held::new(f.data.root(), "auto.before_status_state");
    let reader = std::thread::spawn({
        let service = service.clone();
        move || service.status()
    });
    let (ready, receive) = tokio::sync::oneshot::channel();
    let (done, finished) = std::sync::mpsc::channel();
    let observer = std::thread::spawn(move || {
        let result = (|| {
            arrived
                .recv_timeout(Duration::from_secs(5))
                .map_err(|e| e.to_string())?;
            ready.send(()).map_err(|_| "runtime stopped".to_owned())?;
            finished
                .recv_timeout(Duration::from_secs(5))
                .map_err(|e| e.to_string())
        })();
        drop(held);
        result
    });
    receive.await.unwrap();
    changed.proxy_url = "newer failure".into();
    service.set_config(&changed);
    assert!(service.check_once().await.unwrap().is_err());
    let _ = done.send(());
    observer
        .join()
        .unwrap()
        .expect("status I/O must not hold the live mutex");
    assert_eq!(
        reader.join().unwrap().last_result.as_deref(),
        Some("up-to-date")
    );
    assert!(
        service
            .status()
            .last_error
            .unwrap()
            .contains("newer failure")
    );
}

#[tokio::test(start_paused = true)]
async fn runtime_busy_check_retries_and_success_replaces_the_error() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    let setups = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let factory = {
        let setups = setups.clone();
        let http = http_fetch();
        std::sync::Arc::new(move |proxy: &str| {
            setups.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            http(proxy)
        }) as crate::background::MakeFetch
    };
    let service = UpdateService::new(f.updater.clone(), factory, &config("notify"));
    let lock = f.updater.lock().unwrap();
    let checking = tokio::spawn({
        let service = service.clone();
        async move { service.check_once().await }
    });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(5)).await;
    assert!(matches!(
        checking.await.unwrap(),
        Some(Err(UpdateError::Busy))
    ));
    assert!(service.status().last_error.is_some());
    assert_eq!(setups.load(std::sync::atomic::Ordering::SeqCst), 0);
    let checking = tokio::spawn({
        let service = service.clone();
        async move { service.check_once().await }
    });
    tokio::task::yield_now().await;
    assert!(!checking.is_finished());
    drop(lock);
    tokio::time::advance(Duration::from_millis(250)).await;
    tokio::time::resume();
    assert!(checking.await.unwrap().unwrap().is_ok());
    assert_eq!(service.status().last_error, None);
    assert_eq!(setups.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn runtime_busy_check_stops_when_updates_turn_off() {
    let f = Fixture::new().await;
    let service = service(&f, "notify");
    let _lock = f.updater.lock().unwrap();
    let checking = tokio::spawn({
        let service = service.clone();
        async move { service.check_once().await }
    });
    tokio::task::yield_now().await;
    assert!(service.status().checking);
    service.set_config(&config("off"));
    assert!(checking.await.unwrap().is_none());
    assert!(!service.status().checking);
    assert_eq!(service.status().last_error, None);
    assert_eq!(f.server.requests(), 0);
}

#[tokio::test]
async fn runtime_scheduled_busy_check_reports_the_next_interval() {
    let f = Fixture::new().await;
    let service = service(&f, "notify");
    let _lock = f.updater.lock().unwrap();
    let logs = crate::background::test_log::Capture::default();
    // This current-thread runtime also polls the timer task on this thread.
    let _subscriber = tracing::subscriber::set_default(logs.clone());
    service.start(Duration::ZERO);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !logs
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("update check deferred"))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    service.stop_and_wait().await;
    let events = logs.0.lock().unwrap();
    let event = events
        .iter()
        .find(|line| line.contains("update check deferred"))
        .unwrap();
    assert!(
        event.contains("next interval") && !event.contains("shortly"),
        "{event}"
    );
}

#[cfg(feature = "blue-green-test")]
#[tokio::test(start_paused = true)]
async fn runtime_lock_retake_after_probe_returns_busy() {
    let f = Fixture::new().await;
    let service = service(&f, "notify");
    let (held, arrived) = crate::test_seams::held::Held::new(f.data.root(), "auto.before_check");
    let (ready, receive) = tokio::sync::oneshot::channel();
    let observer = std::thread::spawn(move || {
        arrived.recv_timeout(Duration::from_secs(5)).unwrap();
        ready.send(()).unwrap();
    });
    let checking = tokio::spawn({
        let service = service.clone();
        async move { service.check_once().await }
    });
    receive.await.unwrap();
    observer.join().unwrap();
    let _lock = f.updater.lock().unwrap();
    drop(held);
    tokio::time::advance(Duration::from_secs(5)).await;
    tokio::time::resume();
    assert!(matches!(
        checking.await.unwrap(),
        Some(Err(UpdateError::Busy))
    ));
    assert!(
        service
            .status()
            .last_error
            .unwrap()
            .contains("another update")
    );
    assert_eq!(f.server.requests(), 0);
}

#[tokio::test]
async fn a_failed_check_is_recorded_and_the_next_one_runs() {
    let f = Fixture::new().await;
    let service = service(&f, "notify");
    let outcome = service.check_once().await.unwrap();
    assert!(matches!(
        outcome,
        Err(UpdateError::Fetch {
            error: FetchError::Status(404),
            ..
        })
    ));
    let status = service.status();
    assert_eq!(status.last_result.as_deref(), Some("error"));
    assert!(status.last_error.unwrap().contains("404"));

    f.release("0.2.0");
    let outcome = service.check_once().await.unwrap().unwrap();
    assert!(outcome.news);
    assert_eq!(service.status().last_error, None);
}

#[tokio::test]
async fn a_proxy_the_check_cant_use_fails_the_check_only() {
    let f = Fixture::new().await;
    let mut config = config("auto");
    config.proxy_url = "socks5://user:secret@127.0.0.1:1080".into();
    let service = UpdateService::new(f.updater.clone(), http_fetch(), &config);
    let error = service.check_once().await.unwrap().unwrap_err();
    assert!(error.to_string().contains("SOCKS"), "{error}");
    assert!(!error.to_string().contains("secret"));
    assert_eq!(f.server.requests(), 0);
}

#[tokio::test]
async fn the_settings_follow_the_config() {
    let f = Fixture::new().await;
    let service = service(&f, "");
    let settings = service.settings();
    assert_eq!(settings.mode, SelfUpdateMode::Auto);
    assert_eq!(service.status().mode_source, "default");
    let mut changed = config("notify");
    changed.self_update.check_every = "24h".into();
    service.set_config(&changed);
    let status = service.status();
    assert_eq!(status.mode, "notify");
    assert_eq!(status.check_every_seconds, 24 * 3600);
}
