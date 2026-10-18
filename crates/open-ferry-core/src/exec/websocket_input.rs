// Ported from CLIProxyAPI sdk/cliproxy/executor/websocket_input.go
// (WebsocketInput, WithWebsocketInput, WebsocketInputFromContext,
// WithWebsocketAuthCheck, WebsocketAuthEnabled) (v8.0.27, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The client's frames on a Responses WebSocket with response steering on,
//! for an executor that keeps reading them while its stream runs.
//!
//! The socket has one reader, which hands each frame on through a bounded
//! channel. The session reads it between calls, and a call given the input
//! in [`Options::websocket_input`](super::Options::websocket_input) reads it
//! while its stream runs; a frame read is the reader's own and is never
//! handed on twice.
//!
//! Deviations from upstream:
//! - The input and its credential check travel in the call's options, where
//!   upstream puts them in the call's context.
//! - The check returns the live enabled credential, not only a boolean, so
//!   SIWC duplex creates can refuse a changed token or credential mode.

use std::fmt;
use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::{Mutex, mpsc};

use super::ExecError;
use crate::auth::Auth;

/// A frame from the client (`WebsocketInput`).
#[derive(Debug)]
pub enum InputFrame {
    /// A message, which whoever reads it owns and must not replay.
    Payload(Bytes),
    /// A failure that ends the connection.
    Err(ExecError),
}

/// The live enabled credential, by ID, or `None` when gone or disabled.
/// The credential stays in-process, for checking a connection's authorization.
pub type AuthCheck = Arc<dyn Fn(&str) -> Option<Arc<Auth>> + Send + Sync>;

/// The client's frames, and the check a call makes before it sends one on
/// (`WithWebsocketInput` and `WithWebsocketAuthCheck`). Clones share the
/// one channel.
#[derive(Clone)]
pub struct WebsocketInput {
    rx: Arc<Mutex<mpsc::Receiver<InputFrame>>>,
    auth_check: Option<AuthCheck>,
}

impl WebsocketInput {
    /// The frames `rx` gives, with no credential check.
    pub fn new(rx: mpsc::Receiver<InputFrame>) -> Self {
        Self {
            rx: Arc::new(Mutex::new(rx)),
            auth_check: None,
        }
    }

    /// The same frames, with `check` returning the live enabled credential.
    /// It may refuse further frames but never pick another credential.
    #[must_use]
    pub fn with_auth_check(mut self, check: AuthCheck) -> Self {
        self.auth_check = Some(check);
        self
    }

    /// Whether the credential `auth_id` may still send: always, with no
    /// check (`WebsocketAuthEnabled`).
    pub fn auth_enabled(&self, auth_id: &str) -> bool {
        self.auth_check
            .as_ref()
            .is_none_or(|check| check(auth_id).is_some())
    }

    /// The live credential, or the call's snapshot when no check was installed.
    /// `None` means the installed check found it gone or disabled.
    pub fn live_auth(&self, snapshot: &Auth) -> Option<Arc<Auth>> {
        self.auth_check.as_ref().map_or_else(
            || Some(Arc::new(snapshot.clone())),
            |check| check(&snapshot.id),
        )
    }

    /// The next frame, or `None` once the client's reader has stopped.
    /// Dropping the future before it finishes loses no frame.
    pub async fn recv(&self) -> Option<InputFrame> {
        self.rx.lock().await.recv().await
    }
}

impl fmt::Debug for WebsocketInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebsocketInput")
            .field("auth_check", &self.auth_check.as_ref().map(|_| ".."))
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: frames go to whichever clone reads first, once.
    #[tokio::test]
    async fn clones_share_one_channel() {
        let (tx, rx) = mpsc::channel(4);
        let input = WebsocketInput::new(rx);
        let other = input.clone();
        tx.send(InputFrame::Payload(Bytes::from_static(b"one")))
            .await
            .unwrap();
        tx.send(InputFrame::Payload(Bytes::from_static(b"two")))
            .await
            .unwrap();
        drop(tx);
        assert!(matches!(input.recv().await, Some(InputFrame::Payload(p)) if p == "one"));
        assert!(matches!(other.recv().await, Some(InputFrame::Payload(p)) if p == "two"));
        assert!(input.recv().await.is_none());
    }

    // Not upstream's: WebsocketAuthEnabled with and without a check.
    #[test]
    fn auth_check_defaults_to_enabled() {
        let (_tx, rx) = mpsc::channel(1);
        let input = WebsocketInput::new(rx);
        assert!(input.auth_enabled("any"));
        let auth = Arc::new(Auth {
            id: "good".into(),
            ..Auth::default()
        });
        assert_eq!(input.live_auth(&auth).unwrap().id, "good");
        let input = input.with_auth_check(Arc::new(move |id: &str| {
            (id == "good").then(|| auth.clone())
        }));
        assert!(input.auth_enabled("good"));
        assert!(!input.auth_enabled("bad"));
        assert_eq!(
            input
                .live_auth(&Auth {
                    id: "good".into(),
                    ..Auth::default()
                })
                .unwrap()
                .id,
            "good"
        );
        assert!(
            input
                .live_auth(&Auth {
                    id: "bad".into(),
                    ..Auth::default()
                })
                .is_none()
        );
    }
}
