use super::support::*;
use crate::background::test_log::Capture;
use crate::{SelfUpdateMode, Settings};
use open_ferry_core::config::{SelfUpdate, SelfUpdateChannel};
use tracing::instrument::WithSubscriber;

fn pointer(f: &Fixture, content: &[u8]) {
    f.server.reply(
        "/releases/download/channel-preview/VERSION",
        Reply::Body(content.to_vec()),
    );
}

#[tokio::test]
async fn stable_never_requests_the_pointer_and_ignores_prereleases() {
    let f = Fixture::new().await;
    f.release("0.2.0-rc.1");
    pointer(&f, b"0.3.0\n");
    let found = f.updater.latest(&f.updater.state()).await.unwrap();
    assert_eq!(found.verdict, crate::release::Verdict::Prerelease);
    assert!(
        f.server
            .paths()
            .iter()
            .all(|path| !path.contains("channel-preview"))
    );
}

#[tokio::test]
async fn preview_selects_newer_and_ties_prefer_latest_with_state_and_status() {
    for (preview, expected, source) in [
        ("0.3.0", "0.3.0", "preview"),
        ("0.3.0-rc.1", "0.3.0-rc.1", "preview"),
        ("0.1.0", "0.2.0", "latest"),
        ("0.2.0", "0.2.0", "latest"),
    ] {
        let mut f = Fixture::new().await;
        f.updater.channel = SelfUpdateChannel::Preview;
        f.release(preview);
        f.release("0.2.0");
        pointer(&f, format!("{preview}\n").as_bytes());
        f.updater.check(SelfUpdateMode::Notify).await.unwrap();
        let state = f.updater.state();
        assert_eq!(state.latest.as_deref(), Some(expected));
        assert_eq!(state.channel.as_deref(), Some("preview"));
        assert_eq!(state.release_source.as_deref(), Some(source));
        assert!(state.channel_error.is_none());
        let settings = Settings::resolve(
            &SelfUpdate {
                channel: "preview".into(),
                ..Default::default()
            },
            None,
        );
        let status = f.updater.status(&settings, &f.updater.install());
        assert_eq!(status.channel, "preview");
        assert_eq!(status.release_source.as_deref(), Some(source));
        assert!(status.update_available);
    }
}

#[tokio::test]
async fn pointer_absence_is_quiet_but_invalid_or_untrusted_sources_are_reported() {
    for body in [
        None,
        Some(b"bad\n".to_vec()),
        Some(b"0.3.0\r\n".to_vec()),
        Some(b"\xef\xbb\xbf0.3.0\n".to_vec()),
        Some(b" 0.3.0\n".to_vec()),
        Some(b"0.3.0 \n".to_vec()),
        Some(b"0.3.0/evil\n".to_vec()),
        Some(b"0.3.0%20\n".to_vec()),
        Some(b"..\n".to_vec()),
        Some(b"0.3.0+build\n".to_vec()),
        Some(b"0.3.0\n0.4.0\n".to_vec()),
        Some(b"0.3.0".to_vec()),
        Some(vec![b'1'; 65]),
        Some(b"0.3.0-01\n".to_vec()),
        Some(b"0.4.0\n".to_vec()),
        Some(b"0.3.0\n".to_vec()),
    ] {
        let mut f = Fixture::new().await;
        f.updater.channel = SelfUpdateChannel::Preview;
        f.release("0.3.0");
        f.server.reply(
            "/releases/download/v0.3.0/SHA256SUMS.minisig",
            Reply::Body(b"invalid signature".to_vec()),
        );
        f.release("0.2.0");
        if let Some(body) = &body {
            pointer(&f, body);
        }
        let capture = Capture::default();
        f.updater
            .check(SelfUpdateMode::Auto)
            .with_subscriber(capture.clone())
            .await
            .unwrap();
        let state = f.updater.state();
        assert_eq!(state.latest.as_deref(), Some("0.2.0"));
        assert_eq!(state.release_source.as_deref(), Some("latest"));
        assert_eq!(state.channel_error.is_some(), body.is_some());
        assert_eq!(state.staged.as_deref(), Some("0.2.0"));
        assert!(
            f.server
                .paths()
                .iter()
                .filter(|path| path.ends_with(".tar.gz") || path.ends_with(".zip"))
                .all(|path| !path.contains("/v0.3.0/") && !path.contains("/v0.4.0/"))
        );
        let settings = Settings::resolve(
            &SelfUpdate {
                channel: "preview".into(),
                ..Default::default()
            },
            None,
        );
        let status = f.updater.status(&settings, &f.updater.install());
        assert_eq!(status.channel_error, state.channel_error);
        let events = capture.0.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .any(|line| line.contains("WARN") && line.contains("preview source:")),
            body.is_some(),
            "{events:?}"
        );
    }
}

#[tokio::test]
async fn preview_survives_latest_failure_and_both_failures_fail_the_check() {
    let mut f = Fixture::new().await;
    f.updater.channel = SelfUpdateChannel::Preview;
    f.release("0.3.0");
    pointer(&f, b"0.3.0\n");
    f.server
        .reply("/releases/latest/download/SHA256SUMS", Reply::Status(500));
    f.updater.check(SelfUpdateMode::Notify).await.unwrap();
    assert_eq!(f.updater.state().latest.as_deref(), Some("0.3.0"));
    assert!(
        f.updater
            .state()
            .channel_error
            .unwrap()
            .contains("latest source")
    );
    pointer(&f, b"bad\n");
    assert!(f.updater.check(SelfUpdateMode::Notify).await.is_err());
    assert!(
        f.updater
            .state()
            .last_error
            .unwrap()
            .contains("preview source")
    );
}

#[tokio::test]
async fn neither_channel_stages_an_older_release() {
    for channel in [SelfUpdateChannel::Stable, SelfUpdateChannel::Preview] {
        let mut f = Fixture::new().await;
        f.updater.channel = channel;
        f.updater.running = "0.5.0".into();
        f.release("0.3.0");
        f.release("0.2.0");
        pointer(&f, b"0.3.0\n");
        f.updater.check(SelfUpdateMode::Auto).await.unwrap();
        assert!(f.updater.state().staged.is_none());
    }
}

#[tokio::test]
async fn preview_stages_a_signed_pointer_release_from_a_local_server() {
    let mut f = Fixture::new().await;
    f.updater.channel = SelfUpdateChannel::Preview;
    f.release("0.3.0");
    f.release("0.2.0");
    pointer(&f, b"0.3.0\n");
    f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    assert_eq!(f.updater.state().staged.as_deref(), Some("0.3.0"));
    assert_eq!(
        std::fs::read(f.data.binary("0.3.0", f.updater.binary_name())).unwrap(),
        good_binary("0.3.0")
    );
}

#[tokio::test]
async fn pointer_http_errors_and_a_signed_different_version_fall_back() {
    for mismatch in [false, true] {
        let mut f = Fixture::new().await;
        f.updater.channel = SelfUpdateChannel::Preview;
        if mismatch {
            let archive = release_archive("0.5.0", &f.updater.target, &good_binary("0.5.0"));
            let name = archive_name("0.5.0", &f.updater.target);
            let sums = sums_line(&archive, &name);
            let signature = f.key.sign(sums.as_bytes(), "open-ferry 0.5.0 SHA256SUMS");
            f.server.publish_raw(
                "0.4.0",
                sums.as_bytes(),
                Some(&signature),
                &[(name, archive)],
            );
            pointer(&f, b"0.4.0\n");
        } else {
            f.server.reply(
                "/releases/download/channel-preview/VERSION",
                Reply::Status(503),
            );
        }
        f.release("0.2.0");
        let capture = Capture::default();
        f.updater
            .check(SelfUpdateMode::Auto)
            .with_subscriber(capture.clone())
            .await
            .unwrap();
        let state = f.updater.state();
        assert_eq!(state.staged.as_deref(), Some("0.2.0"));
        let warning = state.channel_error.unwrap();
        assert!(
            warning.contains(if mismatch {
                "signed release is 0.5.0"
            } else {
                "503"
            }),
            "{warning}"
        );
        assert!(
            capture
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.contains(&warning))
        );
        assert!(
            f.server
                .paths()
                .iter()
                .filter(|p| p.ends_with(".tar.gz"))
                .all(|p| p.contains("/v0.2.0/"))
        );
    }
}

#[tokio::test]
async fn pointer_releases_remember_failed_and_rolled_back_exclusions() {
    for failed in [false, true] {
        let mut f = Fixture::new().await;
        f.updater.channel = SelfUpdateChannel::Preview;
        f.release("0.3.0");
        f.release("0.2.0");
        pointer(&f, b"0.3.0\n");
        let mut state = f.updater.state();
        if failed {
            state.mark_failed("0.3.0");
        } else {
            state.rolled_back = Some("0.3.0".into());
        }
        f.updater.save(&state).unwrap();
        let report = f.updater.check(SelfUpdateMode::Auto).await.unwrap().report;
        assert!(matches!(
            (failed, report),
            (true, crate::Report::SkippedFailed { .. })
                | (false, crate::Report::SkippedRolledBack { .. })
        ));
        assert_eq!(f.updater.state().release_source.as_deref(), Some("preview"));
        assert!(f.updater.state().staged.is_none());
        assert!(f.server.paths().iter().all(|p| !p.ends_with(".tar.gz")));
    }
}

#[tokio::test]
async fn status_hides_the_other_channels_release_and_stage() {
    let f = Fixture::new().await;
    let state = crate::State {
        channel: Some("preview".into()),
        latest: Some("0.3.0-rc.1".into()),
        staged: Some("0.3.0-rc.1".into()),
        release_source: Some("preview".into()),
        channel_error: Some("old pointer error".into()),
        ..Default::default()
    };
    f.updater.save(&state).unwrap();
    let status = f.updater.status(
        &Settings::resolve(&SelfUpdate::default(), None),
        &f.updater.install(),
    );
    assert!(
        status.latest_version.is_none()
            && status.staged_version.is_none()
            && status.release_source.is_none()
            && status.channel_error.is_none()
    );
    assert!(!status.update_available);
    assert_eq!(status.checked_channel.as_deref(), Some("preview"));
}

#[tokio::test]
async fn republishing_unsigned_clears_the_tagged_signature() {
    let f = Fixture::new().await;
    f.release("0.3.0");
    f.server.publish_raw("0.3.0", b"unsigned", None, &[]);
    let mut updater = f.updater.clone();
    updater.channel = SelfUpdateChannel::Preview;
    f.release("0.2.0");
    pointer(&f, b"0.3.0\n");
    let found = updater.latest(&updater.state()).await.unwrap();
    assert_eq!(found.release.version, "0.2.0");
    assert!(found.warning.unwrap().contains("404"));
}
