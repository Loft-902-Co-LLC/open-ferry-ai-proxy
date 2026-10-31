// Ported from CLIProxyAPI sdk/cliproxy/executor/websocket_input.go
// (WebsocketInput, WithWebsocketInput, WebsocketInputFromContext,
// WithWebsocketAuthCheck, WebsocketAuthEnabled) (v8.0.42, MIT).
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
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use tokio::sync::{Mutex, mpsc};

use super::ExecError;
use crate::auth::Auth;

/// A frame from the client (`WebsocketInput`).
#[derive(Debug)]
pub enum InputFrame {
    /// A message, which whoever reads it owns and must not replay.
    Payload(Bytes),
    /// A frame stamped immediately after the socket read, before enqueueing.
    Stamped(Bytes, u64),
    /// A reader failure or scoped rejection of a stale queued turn.
    Err(ExecError),
}

/// The live enabled credential, by ID, or `None` when gone or disabled.
/// The credential stays in-process, for checking a connection's authorization.
pub type AuthCheck = Arc<dyn Fn(&str) -> Option<Arc<Auth>> + Send + Sync>;

/// A frontend access check. A returned error payload rejects only this turn;
/// an execution error ends the stream. Frontend credentials stay in the callback.
pub type TurnAuthorizer = Arc<dyn Fn(&[u8]) -> Result<Option<Bytes>, ExecError> + Send + Sync>;

/// The client's frames, and the check a call makes before it sends one on
/// (`WithWebsocketInput` and `WithWebsocketAuthCheck`). Clones share the
/// one channel.
#[derive(Clone)]
pub struct WebsocketInput {
    rx: Arc<Mutex<mpsc::Receiver<InputFrame>>>,
    epoch: Arc<AtomicU64>,
    turn_epoch: Option<u64>,
    received_epoch: Arc<AtomicU64>,
    auth_check: Option<AuthCheck>,
    turn_authorizer: Option<TurnAuthorizer>,
}

impl WebsocketInput {
    /// The frames `rx` gives, with no credential check.
    pub fn new(rx: mpsc::Receiver<InputFrame>) -> Self {
        Self {
            rx: Arc::new(Mutex::new(rx)),
            epoch: Arc::default(),
            turn_epoch: None,
            received_epoch: Arc::default(),
            auth_check: None,
            turn_authorizer: None,
        }
    }

    /// The same frames, with `check` returning the live enabled credential.
    /// It may refuse further frames but never pick another credential.
    #[must_use]
    pub fn with_auth_check(mut self, check: AuthCheck) -> Self {
        self.auth_check = Some(check);
        self
    }

    /// Installs the frontend check for each duplex create, append or steer.
    #[must_use]
    pub fn with_turn_authorizer(mut self, authorize: TurnAuthorizer) -> Self {
        self.turn_authorizer = Some(authorize);
        self
    }

    /// Authorizes a turn, with its effective model supplied in the payload.
    /// With no callback, frontend access is unrestricted.
    pub fn authorize_turn(&self, payload: &[u8]) -> Result<Option<Bytes>, ExecError> {
        self.turn_authorizer
            .as_ref()
            .map_or(Ok(None), |authorize| authorize(payload))
            .map(|payload| payload.filter(|payload| !payload.is_empty()))
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

    /// Current transport generation, shared with the socket reader.
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    /// Advances once per lost connection, before delivering its failure.
    pub fn lose(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
    }

    /// Binds a handler-consumed frame across asynchronous dispatch and dialing.
    pub fn for_turn(mut self) -> Self {
        self.turn_epoch = Some(self.received_epoch.load(Ordering::Acquire));
        self
    }

    pub fn turn_is_stale(&self) -> bool {
        self.turn_epoch.is_some_and(|epoch| epoch != self.epoch())
    }

    /// A continuation cannot use a conversation lost with its transport.
    pub fn continuation_lost(payload: &[u8]) -> ExecError {
        let mut error = ExecError::upstream(409, r#"{"error":{"message":"Previous response is not available on this websocket; resend the full conversation input without previous_response_id","type":"invalid_request_error","code":"previous_response_not_found","param":"previous_response_id"}}"#).with_websocket_turn();
        Self::correlate(&mut error, payload);
        error
    }

    pub fn not_sent(payload: &[u8]) -> ExecError {
        let value: serde_json::Value = serde_json::from_slice(payload).unwrap_or_default();
        if value["type"] == "response.append"
            || value["previous_response_id"]
                .as_str()
                .is_some_and(|id| !id.trim().is_empty())
        {
            return Self::continuation_lost(payload);
        }
        let mut error = ExecError::upstream(502, r#"{"error":{"message":"Operation was not sent because the upstream websocket was lost. Send a new response.create with full conversation input.","type":"invalid_request_error","code":"upstream_websocket_not_sent"}}"#).with_websocket_turn();
        Self::correlate(&mut error, payload);
        error
    }

    pub fn lost(payload: &[u8]) -> ExecError {
        let mut error = ExecError::upstream_websocket_lost();
        Self::correlate(&mut error, payload);
        error
    }

    fn correlate(error: &mut ExecError, payload: &[u8]) {
        let value: serde_json::Value = serde_json::from_slice(payload).unwrap_or_default();
        if let Some(id) = value["event_id"].as_str().filter(|id| !id.is_empty()) {
            let mut body: serde_json::Value =
                serde_json::from_str(&error.message).expect("static loss body");
            body["error"]["event_id"] = id.into();
            error.message = body.to_string();
        }
    }

    /// The next frame, or `None` once the client's reader has stopped.
    /// Dropping the future before it finishes loses no frame.
    pub async fn recv(&self) -> Option<InputFrame> {
        match self.rx.lock().await.recv().await {
            Some(InputFrame::Stamped(payload, epoch)) if epoch != self.epoch() => {
                Some(InputFrame::Err(Self::not_sent(&payload)))
            }
            Some(InputFrame::Stamped(payload, epoch)) => {
                self.received_epoch.store(epoch, Ordering::Release);
                Some(InputFrame::Payload(payload))
            }
            frame => frame,
        }
    }
}

impl fmt::Debug for WebsocketInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebsocketInput")
            .field("auth_check", &self.auth_check.as_ref().map(|_| ".."))
            .field(
                "turn_authorizer",
                &self.turn_authorizer.as_ref().map(|_| ".."),
            )
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

#[cfg(test)]
mod turn_tests {
    use super::*;
    use crate::exec::ErrorKind;

    #[test]
    fn turn_authorizer_defaults_clones_and_errors() {
        let (_tx, rx) = mpsc::channel(1);
        let input = WebsocketInput::new(rx);
        assert!(input.authorize_turn(b"{}").unwrap().is_none());
        let input = input.with_turn_authorizer(Arc::new(|body| match body {
            b"allowed" => Ok(None),
            b"empty" => Ok(Some(Bytes::new())),
            b"denied" => Ok(Some(Bytes::from_static(b"denial payload"))),
            _ => Err(ExecError::new(ErrorKind::Upstream, "authorization failed")),
        }));
        assert!(input.clone().authorize_turn(b"allowed").unwrap().is_none());
        assert!(input.authorize_turn(b"empty").unwrap().is_none());
        assert_eq!(
            input.authorize_turn(b"denied").unwrap().unwrap(),
            "denial payload"
        );
        assert!(input.authorize_turn(b"failed").is_err());
        assert!(!format!("{input:?}").contains("denial payload"));
    }
}

#[cfg(test)]
mod epoch_tests {
    use super::*;

    #[tokio::test]
    async fn loss_fences_frames_stamped_before_bounded_enqueue() {
        let (tx, rx) = mpsc::channel(1);
        let input = WebsocketInput::new(rx);
        tx.send(InputFrame::Stamped(
            Bytes::from_static(br#"{"type":"response.create","event_id":"first"}"#),
            input.epoch(),
        ))
        .await
        .unwrap();
        let epoch = input.epoch();
        let blocked = tokio::spawn(async move {
            tx.send(InputFrame::Stamped(Bytes::from_static(br#"{"type":"response.create","previous_response_id":"lost","event_id":"second"}"#), epoch)).await.unwrap();
        });
        input.lose();
        let Some(InputFrame::Err(first)) = input.recv().await else {
            panic!("stale full create accepted")
        };
        assert!(first.message.contains("upstream_websocket_not_sent"));
        assert!(first.message.contains("first"));
        blocked.await.unwrap();
        let Some(InputFrame::Err(second)) = input.recv().await else {
            panic!("stale continuation accepted")
        };
        assert_eq!(second.http_status(), 409);
        assert!(second.message.contains("second"));
        assert!(second.stops_replay());
        assert!(second.ws_close.is_none());
    }

    #[tokio::test]
    async fn handler_epoch_survives_loss_between_receive_and_dispatch() {
        let (tx, rx) = mpsc::channel(1);
        let input = WebsocketInput::new(rx);
        tx.send(InputFrame::Stamped(
            Bytes::from_static(b"{}"),
            input.epoch(),
        ))
        .await
        .unwrap();
        assert!(matches!(input.recv().await, Some(InputFrame::Payload(_))));
        input.lose();
        assert!(input.clone().for_turn().turn_is_stale());
        tx.send(InputFrame::Stamped(
            Bytes::from_static(b"{}"),
            input.epoch(),
        ))
        .await
        .unwrap();
        assert!(matches!(input.recv().await, Some(InputFrame::Payload(_))));
        assert!(!input.for_turn().turn_is_stale());
    }
}

#[cfg(test)]
mod loss_body_tests {
    use super::*;

    #[test]
    fn not_sent_matches_upstream_body_and_continuation_shape() {
        let error = WebsocketInput::not_sent(b"{}");
        assert_eq!(error.http_status(), 502);
        assert_eq!(
            error.message,
            r#"{"error":{"message":"Operation was not sent because the upstream websocket was lost. Send a new response.create with full conversation input.","type":"invalid_request_error","code":"upstream_websocket_not_sent"}}"#
        );
        assert_eq!(
            WebsocketInput::not_sent(br#"{"type":"response.steer"}"#).http_status(),
            502
        );
        for body in [
            br#"{"type":"response.append"}"#.as_slice(),
            br#"{"previous_response_id":"r1"}"#.as_slice(),
        ] {
            assert_eq!(WebsocketInput::not_sent(body).http_status(), 409);
        }
    }
}
