// Ported from websocketClosePayloadForUpstreamError, responsesWebsocketWriter
// and its methods, and truncateWebsocketCloseReason in CLIProxyAPI
// sdk/api/handlers/openai/openai_responses_websocket.go, and
// writeResponsesWebsocketPayload in
// sdk/api/handlers/openai/openai_responses_websocket_timeline.go (v8.0.10,
// MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The client's connection: reads its requests, writes events, and closes it
//! the ways upstream does.

use axum::extract::ws::{CloseFrame, Message, Utf8Bytes};
use bytes::Bytes;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use open_ferry_core::exec::WsClose;

use crate::errors::ErrorMessage;
use crate::json;

/// The close reason for an upstream that needs the turn replayed over HTTP
/// (`wsHTTPReplayRequiredCloseReason`).
const REPLAY_REQUIRED_REASON: &str = "upstream requires HTTP replay";

/// The most bytes a close reason may have (`wsCloseReasonMaxBytes`).
const CLOSE_REASON_MAX_BYTES: usize = 123;

/// A WebSocket, as the session drives it: axum's, or a test double.
pub(super) trait Socket:
    Sink<Message, Error = axum::Error> + Stream<Item = Result<Message, axum::Error>> + Unpin + Send
{
}

impl<T> Socket for T where
    T: Sink<Message, Error = axum::Error>
        + Stream<Item = Result<Message, axum::Error>>
        + Unpin
        + Send
{
}

/// The connection is closed or closing, so nothing more can be written
/// (gorilla's `ErrCloseSent`, or a failed write).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Closed;

/// The client's connection (upstream's `conn` and `responsesWebsocketWriter`).
pub(super) struct Conn<S> {
    /// The socket, until the connection starts closing.
    socket: Option<S>,
    /// Whether the client sent a close frame, which needs its reply flushed.
    client_closed: bool,
}

impl<S: Socket> Conn<S> {
    pub(super) fn new(socket: S) -> Self {
        Self {
            socket: Some(socket),
            client_closed: false,
        }
    }

    /// The next text or binary message, or `None` once the client is gone
    /// (gorilla's `ReadMessage`). Pings and pongs are skipped; tungstenite
    /// answers pings itself.
    pub(super) async fn read(&mut self) -> Option<Bytes> {
        let socket = self.socket.as_mut()?;
        loop {
            match socket.next().await? {
                Ok(Message::Text(text)) => return Some(Bytes::from(text)),
                Ok(Message::Binary(data)) => return Some(data),
                Ok(Message::Ping(_) | Message::Pong(_)) => {}
                Ok(Message::Close(_)) => {
                    self.client_closed = true;
                    return None;
                }
                Err(_) => return None,
            }
        }
    }

    /// Writes `payload` as a text message (`writeResponsesWebsocketPayload`).
    pub(super) async fn write(&mut self, payload: &[u8]) -> Result<(), Closed> {
        let socket = self.socket.as_mut().ok_or(Closed)?;
        let text = match std::str::from_utf8(payload) {
            Ok(text) => Utf8Bytes::from(text),
            Err(_) => Utf8Bytes::from(String::from_utf8_lossy(payload).into_owned()),
        };
        socket.send(Message::Text(text)).await.map_err(|_| Closed)
    }

    /// Sends a ping (`writePing`).
    pub(super) async fn ping(&mut self) -> Result<(), Closed> {
        let socket = self.socket.as_mut().ok_or(Closed)?;
        socket
            .send(Message::Ping(Bytes::new()))
            .await
            .map_err(|_| Closed)
    }

    /// Mirrors a transport-level upstream close to the client, when `error`
    /// is one, and closes (`closeForUpstreamError`). Returns whether it was
    /// one.
    pub(super) async fn close_for_upstream_error(&mut self, error: &ErrorMessage) -> bool {
        let Some((code, reason)) = close_frame_for(error) else {
            return false;
        };
        let Some(mut socket) = self.socket.take() else {
            return true;
        };
        let frame = CloseFrame {
            code,
            reason: Utf8Bytes::from(reason),
        };
        if let Err(err) = socket.send(Message::Close(Some(frame))).await {
            tracing::debug!(error = %err, "responses websocket: close frame failed");
        }
        true
    }

    /// Closes without telling the client why (`closeWithoutError`). Returns
    /// whether this call closed it.
    pub(super) fn close_without_error(&mut self) -> bool {
        self.socket.take().is_some()
    }

    /// Writes `payload` as the last message and closes (`closeWithPayload`).
    /// Returns whether the message was written.
    pub(super) async fn close_with_payload(&mut self, payload: &[u8]) -> bool {
        if self.socket.is_none() {
            return false;
        }
        let wrote = self.write(payload).await.is_ok();
        self.socket = None;
        wrote
    }

    /// Ends the connection: answers a client's close frame, then drops the
    /// socket, which closes it (upstream's deferred `conn.Close`).
    pub(super) async fn finish(mut self) {
        if let Some(mut socket) = self.socket.take()
            && self.client_closed
        {
            let _ = socket.flush().await;
        }
    }
}

/// The close code and reason that mirror `error` to the client, when it is a
/// transport-level close: an upstream that needs the turn replayed over HTTP
/// (1012), or a message too big for it (1009)
/// (`websocketClosePayloadForUpstreamError`).
pub(super) fn close_frame_for(error: &ErrorMessage) -> Option<(u16, String)> {
    let reason = match error
        .source
        .as_ref()
        .and_then(|source| source.ws_close.as_ref())
    {
        Some(WsClose::ReplayRequired) => {
            return Some((
                1012,
                truncate_close_reason(REPLAY_REQUIRED_REASON, CLOSE_REASON_MAX_BYTES),
            ));
        }
        Some(WsClose::MessageTooBig(text)) => text.clone(),
        None => {
            // Only a call's error carries the status upstream checks here.
            let status = error.source.as_ref()?.http_status();
            let text = error.text.as_bytes();
            if !json::valid(text) {
                return None;
            }
            let code = json::get(text, "error.code").map(|code| code.str());
            if status != 413 || code.as_deref() != Some("message_too_big") {
                return None;
            }
            json::get(text, "error.message")
                .map(|message| message.str().trim().to_owned())
                .unwrap_or_default()
        }
    };
    let reason = if reason.is_empty() {
        "message too big"
    } else {
        &reason
    };
    Some((1009, truncate_close_reason(reason, CLOSE_REASON_MAX_BYTES)))
}

/// `reason` cut to at most `max_bytes` bytes without splitting a character
/// (`truncateWebsocketCloseReason`). Upstream also replaces bytes that
/// aren't UTF-8, which a `str` can't hold.
pub(super) fn truncate_close_reason(reason: &str, max_bytes: usize) -> String {
    if reason.len() <= max_bytes {
        return reason.to_owned();
    }
    let mut end = 0;
    for (start, c) in reason.char_indices() {
        if start + c.len_utf8() > max_bytes {
            break;
        }
        end = start + c.len_utf8();
    }
    reason[..end].to_owned()
}
