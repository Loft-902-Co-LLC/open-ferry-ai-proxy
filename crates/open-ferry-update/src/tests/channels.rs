use super::support::*;
use crate::{SelfUpdateMode, Settings};
use open_ferry_core::config::{SelfUpdate, SelfUpdateChannel};

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
        f.updater.check(SelfUpdateMode::Notify).await.unwrap();
        let state = f.updater.state();
        assert_eq!(state.latest.as_deref(), Some("0.2.0"));
        assert_eq!(state.release_source.as_deref(), Some("latest"));
        assert_eq!(state.channel_error.is_some(), body.is_some());
        assert!(state.staged.is_none());
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
