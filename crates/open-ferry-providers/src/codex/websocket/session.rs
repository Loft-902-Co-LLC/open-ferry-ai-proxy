// Ported from CLIProxyAPI internal/runtime/executor/codex_websockets_session.go
// (codexWebsocketSessionStore, codexWebsocketSession, codexWebsocketRead,
// setActive, activate, activeForConn, clearActive, clearRetryActiveState,
// writeMessage, setMultiAgentV2Optimized, isMultiAgentV2Optimized,
// sendTerminalWebsocketRead, configureConn, detachMismatchedWebsocketSessionConn,
// websocketSessionTargetMatches, setLastEventType, getLastEventType,
// newEphemeralCodexWebsocketSession, setUpstreamDisconnectError,
// getOrCreateSession, ensureUpstreamConn, readUpstreamLoop,
// invalidateUpstreamConn, CloseExecutionSession, closeAllExecutionSessions,
// closeCodexWebsocketSession, isTerminalEvent) and codex_websockets_connection.go
// (readCodexWebsocketMessage) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Sessions, their connection, and the reader that hands its messages to
//! the call in progress.
//!
//! A [`Session`] is a Responses WebSocket session's (kept in the [`Store`]
//! by its execution session ID) or a single call's (ephemeral). It holds at
//! most one [`Conn`], for one [`Target`]: the credential, URL, proxy and
//! token it was opened with. Its calls take turns: a call holds the
//! session's request lock from before it connects until it ends.
//!
//! Each connection has a reader task. It reads messages until the
//! connection fails or is closed, handing each text message to the call
//! that is active on the connection, if any, through a channel (a
//! [`Hold`]'s); a failure goes to that call too, then the connection is let
//! go. Nothing for five minutes is a failure as well.
//!
//! Deviations from upstream:
//! - The store belongs to the executor; upstream's is global by default.
//! - A call dropped before it ends (its client went away) closes the
//!   connection, so that the rest of its response can't reach the next
//!   call; upstream leaves the connection open.
//! - The target includes a hash of the token, so a refreshed token connects
//!   again; upstream keeps the connection.
//! - Pings are answered by the WebSocket library as the reader reads, and a
//!   message is sent in one write; upstream writes in 32 KiB pieces.
//! - Closing a connection drops it without a close frame, as gorilla's
//!   `Close` does; a close from Codex is answered.
//! - Connections are logged at debug level.

use std::borrow::Cow;
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt as _, StreamExt as _};
use http::HeaderMap;
use open_ferry_core::executor::CLOSE_ALL_EXECUTION_SESSIONS;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{OwnedMutexGuard, mpsc, watch};
use tokio::time::{Instant, timeout, timeout_at};
use tokio_tungstenite::tungstenite::Message;

use super::dial::{DialError, Dialed, WsStream};
use super::errors::Failure;

/// How long a connection may go without a message
/// (`codexResponsesWebsocketIdleTimeout`).
const IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// How many reads wait for a call (the active channel's capacity).
const READ_CAPACITY: usize = 4096;
/// How long the reader waits to answer Codex's close.
const CLOSE_REPLY_TIMEOUT: Duration = Duration::from_secs(1);

/// The sessions of an executor, by execution session ID.
pub(in crate::codex) struct Store {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    idle: Duration,
}

impl Store {
    pub(in crate::codex) fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            idle: IDLE_TIMEOUT,
        }
    }

    /// A store whose connections close after `idle` without a message.
    #[cfg(test)]
    pub(in crate::codex) fn with_idle(idle: Duration) -> Self {
        Self {
            idle,
            ..Self::new()
        }
    }

    fn sessions(&self) -> MutexGuard<'_, HashMap<String, Arc<Session>>> {
        lock(&self.sessions)
    }

    /// The session `id` names, made if new; `None` for a blank ID
    /// (`getOrCreateSession`).
    pub(super) fn get_or_create(&self, id: &str) -> Option<Arc<Session>> {
        let id = id.trim();
        if id.is_empty() {
            return None;
        }
        let mut sessions = self.sessions();
        let session = sessions
            .entry(id.to_owned())
            .or_insert_with(|| Arc::new(Session::new(id, self.idle)));
        Some(Arc::clone(session))
    }

    /// A session for one call (`newEphemeralCodexWebsocketSession`).
    pub(super) fn ephemeral(&self) -> Arc<Session> {
        Arc::new(Session::new("", self.idle))
    }

    /// Closes the session `id` names, or all of them for
    /// [`CLOSE_ALL_EXECUTION_SESSIONS`] (`CloseExecutionSession`).
    pub(in crate::codex) fn close(&self, id: &str) {
        let id = id.trim();
        if id.is_empty() {
            return;
        }
        if id == CLOSE_ALL_EXECUTION_SESSIONS {
            let sessions: Vec<_> = self
                .sessions()
                .drain()
                .map(|(_, session)| session)
                .collect();
            for session in sessions {
                session.close("executor_shutdown");
            }
            return;
        }
        let session = self.sessions().remove(id);
        if let Some(session) = session {
            session.close("session_closed");
        }
    }

    /// How many sessions there are.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.sessions().len()
    }
}

/// What a connection was opened for; another target needs another
/// connection (`websocketSessionTargetMatches`, with the token).
#[derive(Clone, PartialEq, Eq)]
pub(super) struct Target {
    pub(super) auth_id: String,
    pub(super) url: String,
    pub(super) proxy: String,
    /// A hash of the token, so that a new token connects again.
    token: [u8; 32],
}

impl Target {
    pub(super) fn new(auth_id: &str, url: &str, proxy: &str, token: &str) -> Self {
        Self {
            auth_id: auth_id.trim().to_owned(),
            url: url.trim().to_owned(),
            proxy: proxy.trim().to_owned(),
            token: Sha256::digest(token.as_bytes()).into(),
        }
    }
}

/// What the reader read for a call: a trimmed text message, or why the
/// connection failed (`codexWebsocketRead`).
pub(super) struct Read {
    pub(super) conn: u64,
    pub(super) result: Result<String, Failure>,
}

impl Read {
    pub(super) fn new(conn: u64, result: Result<String, Failure>) -> Self {
        Self { conn, result }
    }
}

/// The IDs of connections.
static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

/// A connection to Codex.
pub(super) struct Conn {
    id: u64,
    target: Target,
    /// The sending half, `None` once the connection is let go.
    sink: tokio::sync::Mutex<Option<SplitSink<WsStream, Message>>>,
    /// Set when the connection is closed.
    closing: watch::Sender<bool>,
    /// The code Codex closed with (`upstreamDisconnectError`).
    disconnect: Mutex<Option<u16>>,
    /// The type of the last event read (`lastEventType`).
    last_event: Mutex<String>,
}

impl Conn {
    fn new(target: Target, sink: Option<SplitSink<WsStream, Message>>) -> Self {
        Self {
            id: NEXT_CONN.fetch_add(1, Ordering::Relaxed),
            target,
            sink: tokio::sync::Mutex::new(sink),
            closing: watch::Sender::new(false),
            disconnect: Mutex::new(None),
            last_event: Mutex::new(String::new()),
        }
    }

    /// A connection without a socket, for tests of the bookkeeping.
    #[cfg(test)]
    pub(super) fn detached(target: Target) -> Arc<Self> {
        Arc::new(Self::new(target, None))
    }

    pub(super) fn id(&self) -> u64 {
        self.id
    }

    /// Sends a text message (`writeMessage`); a closed connection fails.
    pub(super) async fn send(&self, text: String) -> Result<(), Failure> {
        let mut closing = self.closing.subscribe();
        if *closing.borrow_and_update() {
            return Err(Failure::closed());
        }
        let mut sink = self.sink.lock().await;
        let Some(sink) = sink.as_mut() else {
            return Err(Failure::closed());
        };
        tokio::select! {
            biased;
            _ = closing.wait_for(|closing| *closing) => Err(Failure::closed()),
            sent = sink.send(Message::text(text)) => sent.map_err(|error| Failure::from_ws(&error)),
        }
    }

    /// Closes the connection: the reader stops and drops it.
    pub(super) fn close(&self) {
        self.closing.send_replace(true);
    }

    /// Whether the connection was closed.
    pub(super) fn is_closed(&self) -> bool {
        *self.closing.borrow()
    }

    /// The code Codex closed with, if it did.
    pub(super) fn disconnect_code(&self) -> Option<u16> {
        *lock(&self.disconnect)
    }

    /// Notes the code Codex closed with, unless one was noted.
    pub(super) fn set_disconnect(&self, code: u16) {
        lock(&self.disconnect).get_or_insert(code);
    }

    /// The type of the last event read (`getLastEventType`).
    pub(super) fn last_event(&self) -> String {
        lock(&self.last_event).clone()
    }

    /// Notes the type of an event read (`setLastEventType`).
    pub(super) fn set_last_event(&self, event_type: &str) {
        if !event_type.is_empty() {
            event_type.clone_into(&mut lock(&self.last_event));
        }
    }
}

/// Whether an event ends a response (`isTerminalEvent`).
pub(super) fn is_terminal_event(event_type: &str) -> bool {
    matches!(
        event_type,
        "response.completed"
            | "response.done"
            | "response.incomplete"
            | "response.failed"
            | "error"
    )
}

/// The call active on a connection.
struct Active {
    conn: u64,
    /// Tells this activation from a later one on the same connection.
    token: u64,
    tx: mpsc::Sender<Read>,
    /// Dropped when the call is no longer active (`activeDone`).
    done: watch::Sender<()>,
}

#[derive(Default)]
struct State {
    conn: Option<Arc<Conn>>,
    /// The connection on which a request renamed the collaboration
    /// namespace (`multiAgentV2OptimizedConn`).
    multi_agent: Option<u64>,
    active: Option<Active>,
    next_token: u64,
}

/// A session (`codexWebsocketSession`).
pub(super) struct Session {
    /// The execution session ID; empty for an ephemeral session.
    id: String,
    idle: Duration,
    /// Held by the call in progress (`reqMu`).
    requests: Arc<tokio::sync::Mutex<()>>,
    state: Mutex<State>,
}

impl Session {
    fn new(id: &str, idle: Duration) -> Self {
        Self {
            id: id.to_owned(),
            idle,
            requests: Arc::new(tokio::sync::Mutex::new(())),
            state: Mutex::new(State::default()),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn kind(&self) -> &'static str {
        if self.id.is_empty() {
            "ephemeral"
        } else {
            "persistent"
        }
    }

    /// Waits for the calls before to end, then holds the session.
    pub(super) async fn lock_requests(&self) -> OwnedMutexGuard<()> {
        Arc::clone(&self.requests).lock_owned().await
    }

    /// Whether a call holds the session.
    #[cfg(test)]
    pub(super) fn is_locked(&self) -> bool {
        self.requests.try_lock().is_err()
    }

    /// The session's connection.
    #[cfg(test)]
    pub(super) fn conn(&self) -> Option<Arc<Conn>> {
        self.state().conn.clone()
    }

    /// Sets `conn` as the session's connection, for tests of the
    /// bookkeeping.
    #[cfg(test)]
    pub(super) fn set_conn(&self, conn: Arc<Conn>) {
        let mut state = self.state();
        state.conn = Some(conn);
        state.multi_agent = None;
    }

    /// Makes a channel for the reads of `conn`'s call, replacing the one
    /// before (`activate`). Returns the activation's token and the channel.
    pub(super) fn activate(&self, conn: u64) -> (u64, mpsc::Receiver<Read>) {
        let (tx, rx) = mpsc::channel(READ_CAPACITY);
        let (done, _) = watch::channel(());
        let mut state = self.state();
        state.next_token += 1;
        let token = state.next_token;
        state.active = Some(Active {
            conn,
            token,
            tx,
            done,
        });
        (token, rx)
    }

    /// The channel of the call active on `conn`, with its token and a
    /// receiver that ends when the call stops being active
    /// (`activeForConn`).
    pub(super) fn active_for(
        &self,
        conn: u64,
    ) -> Option<(u64, mpsc::Sender<Read>, watch::Receiver<()>)> {
        let state = self.state();
        let active = state.active.as_ref().filter(|active| active.conn == conn)?;
        Some((active.token, active.tx.clone(), active.done.subscribe()))
    }

    /// Ends the activation `token` on `conn`, if it is still the active one
    /// (`clearActive`); its channel closes once nothing else holds it.
    pub(super) fn clear_active(&self, conn: u64, token: u64) -> bool {
        let mut state = self.state();
        if state
            .active
            .as_ref()
            .is_some_and(|active| active.conn == conn && active.token == token)
        {
            state.active = None;
            true
        } else {
            false
        }
    }

    /// Notes whether a request on `conn` renamed the collaboration
    /// namespace (`setMultiAgentV2Optimized`).
    pub(super) fn set_multi_agent_optimized(&self, conn: u64, optimized: bool) {
        let mut state = self.state();
        if state
            .conn
            .as_ref()
            .is_some_and(|current| current.id == conn)
        {
            state.multi_agent = optimized.then_some(conn);
        }
    }

    /// Whether a request on `conn`, still the session's connection, renamed
    /// the collaboration namespace (`isMultiAgentV2Optimized`).
    pub(super) fn is_multi_agent_optimized(&self, conn: u64) -> bool {
        let state = self.state();
        state
            .conn
            .as_ref()
            .is_some_and(|current| current.id == conn)
            && state.multi_agent == Some(conn)
    }

    /// Lets `conn` go and closes it, if it is still the session's
    /// connection (`invalidateUpstreamConn`).
    pub(super) fn invalidate(&self, conn: &Conn, reason: &str, failure: Option<&Failure>) {
        let detached = {
            let mut state = self.state();
            if state
                .conn
                .as_ref()
                .is_some_and(|current| current.id == conn.id)
            {
                state.multi_agent = None;
                state.conn.take()
            } else {
                None
            }
        };
        if let Some(detached) = detached {
            self.log_disconnected(&detached, reason, failure);
            detached.close();
        }
    }

    /// Lets the session's connection go and closes it
    /// (`closeCodexWebsocketSession`).
    pub(super) fn close(&self, reason: &str) {
        let detached = {
            let mut state = self.state();
            state.multi_agent = None;
            state.conn.take()
        };
        if let Some(detached) = detached {
            self.log_disconnected(&detached, reason, None);
            detached.close();
        }
    }

    fn log_disconnected(&self, conn: &Conn, reason: &str, failure: Option<&Failure>) {
        let last_event = conn.last_event();
        tracing::debug!(
            session = %self.id,
            auth = %conn.target.auth_id,
            url = %conn.target.url,
            session_object = self.kind(),
            reason,
            last_event = %last_event,
            is_terminal = is_terminal_event(&last_event),
            error = %failure.map(Failure::text).unwrap_or_default(),
            "codex websockets: upstream disconnected"
        );
    }

    fn log_connected(&self, conn: &Conn, reused: bool) {
        tracing::debug!(
            session = %self.id,
            auth = %conn.target.auth_id,
            url = %conn.target.url,
            session_object = self.kind(),
            reused,
            "codex websockets: upstream connected"
        );
    }

    /// The session's connection for `target`, connecting with `dial` when
    /// there is none (`ensureUpstreamConn`). A connection for another
    /// target is closed first, and one already closed is let go. The
    /// handshake's headers come back for a new connection.
    pub(super) async fn ensure_conn<F, Fut>(
        self: &Arc<Self>,
        target: Target,
        dial: F,
    ) -> Result<(Arc<Conn>, Option<HeaderMap>), DialError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Dialed, DialError>>,
    {
        // detachMismatchedWebsocketSessionConn
        let stale = {
            let mut state = self.state();
            if state
                .conn
                .as_ref()
                .is_some_and(|conn| conn.target != target || conn.is_closed())
            {
                state.multi_agent = None;
                state.conn.take()
            } else {
                None
            }
        };
        if let Some(stale) = stale {
            let reason = if stale.target == target {
                "closed"
            } else {
                "target_changed"
            };
            self.log_disconnected(&stale, reason, None);
            stale.close();
        }
        let existing = self.state().conn.clone();
        if let Some(conn) = existing {
            self.log_connected(&conn, true);
            return Ok((conn, None));
        }

        let Dialed { stream, headers } = dial().await?;
        let (sink, stream) = stream.split();
        let conn = Arc::new(Conn::new(target, Some(sink)));
        {
            let mut state = self.state();
            if let Some(previous) = state.conn.clone() {
                // Another call connected meanwhile: keep its connection.
                drop(state);
                self.log_connected(&previous, true);
                return Ok((previous, None));
            }
            state.conn = Some(Arc::clone(&conn));
            state.multi_agent = None;
        }
        self.log_connected(&conn, false);
        tokio::spawn(read_loop(
            Arc::downgrade(self),
            Arc::clone(&conn),
            stream,
            self.idle,
        ));
        Ok((conn, Some(headers)))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let state = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
        if let Some(conn) = state.conn.take() {
            conn.close();
        }
    }
}

/// Hands a failure to the active call (`sendTerminalWebsocketRead`). When
/// its channel is full, the connection is let go first (`invalidate`), then
/// the failure waits for room or for the call to stop being active.
/// Returns whether it let the connection go.
pub(super) async fn send_terminal(
    tx: &mpsc::Sender<Read>,
    mut done: watch::Receiver<()>,
    read: Read,
    invalidate: impl FnOnce(),
) -> bool {
    if done.has_changed().is_err() {
        return false;
    }
    let read = match tx.try_send(read) {
        Ok(()) | Err(TrySendError::Closed(_)) => return false,
        Err(TrySendError::Full(read)) => read,
    };
    invalidate();
    tokio::select! {
        _ = tx.send(read) => {}
        _ = done.changed() => {}
    }
    true
}

/// An event's `type`, without reading the rest of it into memory.
#[derive(Deserialize)]
struct Typed<'a> {
    #[serde(borrow, rename = "type")]
    kind: Option<Cow<'a, str>>,
}

/// Reads `conn` until it fails or is closed (`readUpstreamLoop`). Either
/// way, the call active on it gets the failure, as gorilla's read fails
/// once the connection is closed.
async fn read_loop(
    session: Weak<Session>,
    conn: Arc<Conn>,
    mut stream: SplitStream<WsStream>,
    idle: Duration,
) {
    let mut closing = conn.closing.subscribe();
    let mut deadline = Instant::now() + idle;
    let mut peer_closed = false;
    let (failure, reason) = loop {
        let next = tokio::select! {
            biased;
            _ = closing.wait_for(|closing| *closing) => {
                break (Failure::closed(), "upstream_disconnected");
            }
            next = timeout_at(deadline, stream.next()) => next,
        };
        let message = match next {
            Err(_) => break (Failure::timeout(), "upstream_disconnected"),
            Ok(None) => break (Failure::eof(), "upstream_disconnected"),
            Ok(Some(Err(error))) => {
                break (Failure::from_ws(&error), "upstream_disconnected");
            }
            Ok(Some(Ok(message))) => message,
        };
        let payload = match message {
            Message::Text(text) => text.as_str().trim().to_owned(),
            Message::Binary(_) => break (Failure::binary(), "unexpected_binary"),
            Message::Close(frame) => {
                peer_closed = true;
                let (code, reason) = frame.map_or((1005, String::new()), |frame| {
                    (u16::from(frame.code), frame.reason.as_str().to_owned())
                });
                conn.set_disconnect(code);
                break (Failure::Close { code, reason }, "upstream_disconnected");
            }
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
        };
        deadline = Instant::now() + idle;
        if !payload.is_empty()
            && let Ok(Typed { kind: Some(kind) }) = serde_json::from_str::<Typed<'_>>(&payload)
        {
            conn.set_last_event(&kind);
        }
        let Some((_, tx, mut done)) = session
            .upgrade()
            .and_then(|session| session.active_for(conn.id))
        else {
            continue;
        };
        tokio::select! {
            biased;
            _ = closing.wait_for(|closing| *closing) => {
                break (Failure::closed(), "upstream_disconnected");
            }
            _ = tx.send(Read::new(conn.id, Ok(payload))) => {}
            _ = done.changed() => {}
        }
    };

    // Closed first: no send starts after this, so a call that becomes
    // active on the connection from here on fails to send, and one active
    // before is found below.
    conn.close();
    if let Some(session) = session.upgrade() {
        let mut invalidated = false;
        if let Some((token, tx, done)) = session.active_for(conn.id) {
            let read = Read::new(conn.id, Err(failure.clone()));
            invalidated = send_terminal(&tx, done, read, || {
                session.invalidate(&conn, reason, Some(&failure));
            })
            .await;
            drop(tx);
            session.clear_active(conn.id, token);
        }
        if !invalidated {
            session.invalidate(&conn, reason, Some(&failure));
        }
    }
    if peer_closed {
        // Sends the reply to Codex's close.
        let _ = timeout(CLOSE_REPLY_TIMEOUT, stream.next()).await;
    }
    let sink = conn.sink.lock().await.take();
    drop(sink);
    drop(stream);
}

/// A call's hold on its session and connection: the session's request lock,
/// the active channel, and whether the call ended. Dropping it before
/// [`release`](Hold::release) closes the connection (the call was
/// cancelled).
pub(super) struct Hold {
    session: Arc<Session>,
    ephemeral: bool,
    guard: Option<OwnedMutexGuard<()>>,
    conn: Arc<Conn>,
    token: u64,
    rx: mpsc::Receiver<Read>,
    finished: bool,
}

impl Hold {
    /// Makes `conn` the active connection of `session` for a call that
    /// holds `guard`.
    pub(super) fn new(
        session: Arc<Session>,
        ephemeral: bool,
        guard: Option<OwnedMutexGuard<()>>,
        conn: Arc<Conn>,
    ) -> Self {
        let (token, rx) = session.activate(conn.id);
        Self {
            session,
            ephemeral,
            guard,
            conn,
            token,
            rx,
            finished: false,
        }
    }

    pub(super) fn conn(&self) -> &Arc<Conn> {
        &self.conn
    }

    /// The next read of the connection (`readCodexWebsocketMessage`).
    pub(super) async fn recv(&mut self) -> Result<String, Failure> {
        loop {
            match self.rx.recv().await {
                None => return Err(Failure::channel_closed()),
                Some(read) if read.conn != self.conn.id => {}
                Some(read) => return read.result,
            }
        }
    }

    /// Lets the connection go.
    pub(super) fn invalidate(&self, reason: &str) {
        self.session.invalidate(&self.conn, reason, None);
    }

    /// Lets the next call in before this one ends.
    pub(super) fn unlock(&mut self) {
        self.guard = None;
    }

    /// Moves the call to `conn`, a new connection after a failed send
    /// (`clearRetryActiveState`, then `activate`).
    pub(super) fn switch(&mut self, conn: Arc<Conn>) {
        self.session.clear_active(self.conn.id, self.token);
        let (token, rx) = self.session.activate(conn.id);
        self.conn = conn;
        self.token = token;
        self.rx = rx;
    }

    /// Ends the call: it is no longer active, the next call may go, and an
    /// ephemeral session is closed.
    pub(super) fn release(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.session.clear_active(self.conn.id, self.token);
        self.guard = None;
        if self.ephemeral {
            self.session.close("completed");
        }
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        if !self.finished {
            self.invalidate("canceled");
            self.release();
        }
    }
}

/// Locks a mutex, poisoned or not.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
