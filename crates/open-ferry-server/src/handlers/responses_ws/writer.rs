// Ported from websocketClosePayloadForUpstreamError, responsesWebsocketWriter
// and its methods, and truncateWebsocketCloseReason in CLIProxyAPI
// sdk/api/handlers/openai/openai_responses_websocket.go,
// readResponsesWebsocketInput in
// sdk/api/handlers/openai/openai_responses_websocket_input.go, and
// writeResponsesWebsocketPayload in
// sdk/api/handlers/openai/openai_responses_websocket_timeline.go (v8.0.15,
// MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The client's connection: reads its requests, writes events, and closes it
//! the ways upstream does.
//!
//! With response steering on, the connection is split: a task reads the
//! client's messages into a bounded queue, which the session reads between
//! turns and a Codex call reads while its stream runs, and the writes go to
//! the other half.
//!
//! Deviations from upstream:
//! - The reader's queue ends when the client goes or the connection
//!   closes; upstream's reader also stops when the socket's context ends.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::extract::ws::{CloseFrame, Message, Utf8Bytes};
use bytes::Bytes;
use futures_util::stream::SplitSink;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use open_ferry_core::exec::{InputFrame, WebsocketInput, WsClose};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::errors::ErrorMessage;
use crate::json;

/// The close reason for an upstream that needs the turn replayed over HTTP
/// (`wsHTTPReplayRequiredCloseReason`).
const REPLAY_REQUIRED_REASON: &str = "upstream requires HTTP replay";

/// The most bytes a close reason may have (`wsCloseReasonMaxBytes`).
const CLOSE_REASON_MAX_BYTES: usize = 123;

/// How many of the client's messages the reader holds before it waits, so
/// a client is held back rather than its input kept without limit.
const INPUT_QUEUE: usize = 16;

/// A WebSocket, as the session drives it: axum's, or a test double.
pub(super) trait Socket:
    Sink<Message, Error = axum::Error>
    + Stream<Item = Result<Message, axum::Error>>
    + Unpin
    + Send
    + 'static
{
}

impl<T> Socket for T where
    T: Sink<Message, Error = axum::Error>
        + Stream<Item = Result<Message, axum::Error>>
        + Unpin
        + Send
        + 'static
{
}

/// A socket's writing side, as a write needs it.
type DynSink = dyn Sink<Message, Error = axum::Error> + Unpin + Send;

/// What the connection writes to: the whole socket, or, with a reader
/// task, the socket's writing half.
enum Writer<S> {
    Whole(S),
    Split(SplitSink<S, Message>),
}

impl<S: Socket> Writer<S> {
    fn sink(&mut self) -> &mut DynSink {
        match self {
            Self::Whole(socket) => socket,
            Self::Split(sink) => sink,
        }
    }
}

/// The task that reads a split connection (`readResponsesWebsocketInput`),
/// stopped when this is dropped.
struct Reader {
    /// The client's messages, as the task hands them on.
    input: WebsocketInput,
    /// Set once the client is gone.
    gone: watch::Receiver<bool>,
    /// Whether the client sent a close frame.
    client_closed: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Reader {
    /// The next message, or `None` once the client is gone.
    async fn next(&self) -> Option<Bytes> {
        let mut gone = self.gone.clone();
        tokio::select! {
            biased;
            _ = gone.wait_for(|gone| *gone) => None,
            frame = self.input.recv() => match frame {
                Some(InputFrame::Payload(payload)) => Some(payload),
                Some(InputFrame::Err(_)) | None => None,
            },
        }
    }
}

/// Reads the client's messages into `input` until the client goes or no
/// one reads them, then marks it gone. Pings and pongs are skipped
/// (`readResponsesWebsocketInput`).
async fn read_input<R>(
    mut stream: R,
    input: mpsc::Sender<InputFrame>,
    client_closed: Arc<AtomicBool>,
    gone: watch::Sender<bool>,
) where
    R: Stream<Item = Result<Message, axum::Error>> + Unpin,
{
    while let Some(message) = stream.next().await {
        let payload = match message {
            Ok(Message::Text(text)) => Bytes::from(text),
            Ok(Message::Binary(data)) => data,
            Ok(Message::Ping(_) | Message::Pong(_)) => continue,
            Ok(Message::Close(_)) => {
                client_closed.store(true, Ordering::Release);
                break;
            }
            Err(_) => break,
        };
        if input.send(InputFrame::Payload(payload)).await.is_err() {
            break;
        }
    }
    gone.send_replace(true);
}

/// The connection is closed or closing, so nothing more can be written
/// (gorilla's `ErrCloseSent`, or a failed write).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Closed;

/// The client's connection (upstream's `conn` and `responsesWebsocketWriter`).
pub(super) struct Conn<S> {
    /// The socket, or its writing half, until the connection starts
    /// closing.
    writer: Option<Writer<S>>,
    /// The task reading a split connection.
    reader: Option<Reader>,
    /// Whether the client sent a close frame, which needs its reply flushed.
    client_closed: bool,
}

impl<S: Socket> Conn<S> {
    pub(super) fn new(socket: S) -> Self {
        Self {
            writer: Some(Writer::Whole(socket)),
            reader: None,
            client_closed: false,
        }
    }

    /// The connection with a task reading it all the time, for response
    /// steering: its messages queue for [`read`](Self::read) and for a
    /// call given [`input`](Self::input) (`readResponsesWebsocketInput`).
    pub(super) fn split(socket: S) -> Self {
        let (sink, stream) = socket.split();
        let (input, rx) = mpsc::channel(INPUT_QUEUE);
        let (gone, gone_rx) = watch::channel(false);
        let client_closed = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(read_input(stream, input, Arc::clone(&client_closed), gone));
        Self {
            writer: Some(Writer::Split(sink)),
            reader: Some(Reader {
                input: WebsocketInput::new(rx),
                gone: gone_rx,
                client_closed,
                task,
            }),
            client_closed: false,
        }
    }

    /// The client's messages, for a call to read while its stream runs,
    /// when the connection is split.
    pub(super) fn input(&self) -> Option<WebsocketInput> {
        self.reader.as_ref().map(|reader| reader.input.clone())
    }

    /// Finishes once a split connection's client is gone; never, for one
    /// that isn't split (upstream's socket context).
    pub(super) fn gone(&self) -> impl Future<Output = ()> + Send + 'static {
        let gone = self.reader.as_ref().map(|reader| reader.gone.clone());
        async move {
            match gone {
                Some(mut gone) => {
                    // An error means the reader stopped, so the client is
                    // gone as far as the session can tell.
                    let _ = gone.wait_for(|gone| *gone).await;
                }
                None => std::future::pending().await,
            }
        }
    }

    /// The next text or binary message, or `None` once the client is gone
    /// (gorilla's `ReadMessage`). Pings and pongs are skipped; tungstenite
    /// answers pings itself.
    pub(super) async fn read(&mut self) -> Option<Bytes> {
        let socket = match self.writer.as_mut()? {
            Writer::Whole(socket) => socket,
            Writer::Split(_) => return self.reader.as_ref()?.next().await,
        };
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
        let socket = self.writer.as_mut().ok_or(Closed)?.sink();
        let text = match std::str::from_utf8(payload) {
            Ok(text) => Utf8Bytes::from(text),
            Err(_) => Utf8Bytes::from(String::from_utf8_lossy(payload).into_owned()),
        };
        socket.send(Message::Text(text)).await.map_err(|_| Closed)
    }

    /// Sends a ping (`writePing`).
    pub(super) async fn ping(&mut self) -> Result<(), Closed> {
        let socket = self.writer.as_mut().ok_or(Closed)?.sink();
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
        self.reader = None;
        let Some(mut writer) = self.writer.take() else {
            return true;
        };
        let frame = CloseFrame {
            code,
            reason: Utf8Bytes::from(reason),
        };
        if let Err(err) = writer.sink().send(Message::Close(Some(frame))).await {
            tracing::debug!(error = %err, "responses websocket: close frame failed");
        }
        true
    }

    /// Closes without telling the client why (`closeWithoutError`). Returns
    /// whether this call closed it.
    pub(super) fn close_without_error(&mut self) -> bool {
        self.reader = None;
        self.writer.take().is_some()
    }

    /// Writes `payload` as the last message and closes (`closeWithPayload`).
    /// Returns whether the message was written.
    pub(super) async fn close_with_payload(&mut self, payload: &[u8]) -> bool {
        if self.writer.is_none() {
            return false;
        }
        let wrote = self.write(payload).await.is_ok();
        self.reader = None;
        self.writer = None;
        wrote
    }

    /// Ends the connection: answers a client's close frame, then drops the
    /// socket, which closes it (upstream's deferred `conn.Close`).
    pub(super) async fn finish(mut self) {
        let client_closed = self.client_closed
            || self
                .reader
                .as_ref()
                .is_some_and(|reader| reader.client_closed.load(Ordering::Acquire));
        self.reader = None;
        if let Some(mut writer) = self.writer.take()
            && client_closed
        {
            let _ = writer.sink().flush().await;
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
            if !json::gjson_valid(text) {
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
