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
