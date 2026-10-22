//! Integration fixtures for the binary's real sync/shutdown tests.
//! Enabled only by the binary's dev-dependency; endpoints stay on loopback.

use crate::ManagementState;
use std::sync::Arc;
use tokio::sync::Notify;

pub struct BindPause {
    entered: super::BindWritePause,
    release: Option<std::sync::mpsc::Sender<()>>,
}

impl BindPause {
    pub async fn entered(&self) {
        self.entered.0.notified().await;
    }
    pub fn release(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}

impl Drop for BindPause {
    fn drop(&mut self) {
        self.release();
    }
}

pub fn prepare_bind(state: &ManagementState, base: &str) -> BindPause {
    let url = url::Url::parse(base).unwrap();
    assert_eq!(url.host_str(), Some("127.0.0.1"));
    assert_ne!(url.port_or_known_default(), Some(8317));
    let (release, released) = std::sync::mpsc::channel();
    let pause = Arc::new((Notify::new(), std::sync::Mutex::new(released)));
    let mut hooks = state.oauth_sessions().overrides();
    hooks.endpoints = Some(base.into());
    hooks.bind_before_write = Some(pause.clone());
    BindPause {
        entered: pause,
        release: Some(release),
    }
}

pub fn finalized(state: &ManagementState, session: &str) -> bool {
    state
        .oauth_sessions()
        .store()
        .get(session)
        .is_some_and(|session| session.completed && session.status.is_empty())
}

pub fn shutting_down(state: &ManagementState) -> bool {
    state.oauth_sessions().is_shut_down()
}
