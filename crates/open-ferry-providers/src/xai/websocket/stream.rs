// Ported from CLIProxyAPI internal/runtime/executor/xai_websockets_executor.go
// (ExecuteStream, prepareResponsesWebsocketRequest, applyXAIWebsocketHeaders,
// ensureUpstreamConn, logXAIWebsocketRequest, logXAIWebsocketWarmupCompleted,
// logXAIWebsocketTerminalResponse) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The streaming call over the WebSocket.
//!
//! The request is prepared as the HTTP call's is, keeping the client's
//! `previous_response_id` (xAI's ID for it; see [`super::ids`]), and goes
//! out as a `response.create` message (see [`super::message`]). The call
//! takes its session (the client's, or one for this call alone), waits for
//! the session's call before to end, connects if the session has no
//! connection for this credential, URL, proxy and token, and sends. A send
//! that fails on a session's connection is tried once more on a new one,
//! unless the message was too big. A refused handshake is the call's error,
//! with xAI's status and body (see [`crate::xai::errors`]).
//!
//! Each message xAI sends then has the handshake's secrets redacted (see
//! [`super`]), and those of a kept connection's handshake too. An error
//! event ends the stream with its error (see [`super::errors`]) and lets
//! the connection go. Any other event is undone and bridged as the HTTP
//! stream's are (see [`crate::xai::stream`]):
//! reasoning text becomes a summary, namespace tools and a client's
//! `web_search` get their names back, X search's own calls are dropped, and
//! the `apply_patch` bridge restores the client's tool, failing the call
//! with a 502 for arguments it can't read. The completed response gets the
//! streamed items as its output, as for HTTP, and its reasoning is kept for
//! replay. Each event reaches the client with the usage details an OpenAI
//! Responses client expects and the IDs it was given.
//!
//! The stream ends after `response.completed`, `response.done` or `error`,
//! or a `response.incomplete` or `response.failed` while the `apply_patch`
//! bridge is active. A warmup (`generate: false`) ends after its
//! `response.created`, followed by a completed response made from it. The
//! turn (the message's input and the response's output) goes into the
//! session's transcript once. A broken connection ends the stream with an
//! error, or with the `apply_patch` bridge's failure events and a 502 when
//! a patch call was left unfinished.
//!
//! The call ends (the next call of the session may go, and a call's own
//! connection is closed) before its last chunk is handed on.
//!
//! Deviations from upstream:
//! - Chunks are xAI's events, for a client on the Responses WebSocket;
//!   upstream's SSE translation for other clients isn't ported, as only
//!   those clients take this route.
//! - The handshake says `User-Agent: open-ferry/<version>` where Go's says
//!   `Go-http-client/1.1`, and keeps the HTTP call's identity rules (see
//!   [`crate::xai::request`]).
//! - A dropped call closes its connection, as Codex's does.
//! - Each message has the handshake's secrets of eight bytes or more
//!   redacted, as every client error is (see `Policy::Client`), before it is
//!   read; see [`super`]. A call keeping a connection redacts the secrets
//!   that connection's handshake sent as well, from its messages and its
//!   errors, as a custom header may have changed since.
//! - The log lines hide those secrets, however short (`Policy::Disk`), from
//!   every field: the session, the credential's ID, the URL, and the event
//!   type and IDs xAI or the client sent; upstream logs them as they are.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;
use futures_util::StreamExt as _;
use http::Method;
use http::header;
use open_ferry_core::auth::Auth;
use open_ferry_core::exec::{ChunkStream, ExecError, Format, Options, Request, StreamResponse};
use open_ferry_core::observe::AttemptKind;
use serde_json::Value;
use tokio::sync::OwnedMutexGuard;

use super::errors::{parse_error, read_error, write_error};
use super::ids::Mapper;
use super::message::{generate_false, request_message, warmup_completed};
use super::{Target, compaction, websocket_url};
use crate::codex::request::parse_object;
use crate::codex::terminal::{APPLY_PATCH_ERROR_MESSAGE, OutputItems, StatusError};
use crate::codex::usage::ensure_responses_usage_details;
use crate::codex::websocket::dial::{self, DialError};
use crate::codex::websocket::errors::should_retry;
use crate::codex::websocket::session::{self, Hold};
use crate::json::{get, set, str_at};
use crate::observe_send::{self, Attempt};
use crate::redact::{Policy, Secrets};
use crate::xai::XaiExecutor;
use crate::xai::errors::status_error;
use crate::xai::reasoning::{normalize_summary_data, normalize_summary_data_events};
use crate::xai::replay;
use crate::xai::request::{PROVIDER, Prepared, build_headers, client_session_id, prepare, token};
use crate::xai::response::{
    NamespaceRestorer, XSearchFilter, patch_completed_output, restore_client_web_search_name,
};

/// The `alt` of a `/responses/compact` call, which can't stream.
const COMPACT_ALT: &str = "responses/compact";

/// A streaming call over the WebSocket (`ExecuteStream`).
pub(in crate::xai) async fn execute_stream(
    executor: &XaiExecutor,
    auth: &Auth,
    request: Request,
    options: Options,
) -> Result<StreamResponse, ExecError> {
    if options.alt == COMPACT_ALT {
        return Err(StatusError::new(400, "streaming not supported for /responses/compact").into());
    }
    let sessions = &executor.websockets;
    let session_id = options
        .metadata
        .execution_session_id
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_owned();
    // What the call remembers between calls: its WebSocket session's, else
    // its prompt cache's.
    let state_id = if session_id.is_empty() {
        client_session_id(&request.payload)
    } else {
        session_id.clone()
    };
    let state = sessions.ids.get(&state_id);
    // Calls of one prompt cache outside a WebSocket session go one at a
    // time.
    let state_guard = match &state {
        Some(state) if session_id.is_empty() => Some(state.lock_requests().await),
        _ => None,
    };
    let client = parse_object(&request.payload);
    if compaction::requested(&request.payload) {
        let _session_guard = match sessions.store.get_or_create(&session_id) {
            Some(session) => Some(session.lock_requests().await),
            None => None,
        };
        let mapper = state.map(|state| Mapper::new(state, &client));
        let result =
            compaction::fallback(executor, auth, &request, &options, &state_id, mapper).await;
        drop(state_guard);
        return result;
    }

    let mut prepared = prepare(
        executor.context(auth),
        &request,
        &options,
        true,
        Format::CODEX,
    )?;
    let previous = str_at(&client, "previous_response_id");
    let previous = previous.trim();
    if !previous.is_empty() {
        set(
            &mut prepared.body,
            "previous_response_id",
            Value::from(previous),
        );
    }
    let url = websocket_url(auth)?;
    let proxy = executor.proxy_for(auth);
    let target = Target {
        auth_id: auth.id.clone(),
        url: url.clone(),
        proxy: proxy.clone(),
    };

    let (session, ephemeral, guard) = match sessions.store.get_or_create(&session_id) {
        Some(session) => {
            let guard = session.lock_requests().await;
            (session, false, Some(guard))
        }
        None => (sessions.store.ephemeral(), true, None),
    };
    let mut mapper = state.map(|state| Mapper::new(state, &client));
    if let Some(mapper) = mapper.as_mut() {
        if !ephemeral && sessions.target_changed(&session_id, &target) {
            mapper.forget_upstream_previous();
        }
        mapper.upstream_request(&mut prepared.body);
    }

    let mut headers = build_headers(auth, &options.headers, false, &prepared.session_id)?;
    headers.remove(header::ACCEPT);
    let message = request_message(&prepared.body);
    let request_type = str_at(&client, "type");
    let transcript_reset = str_at(&message, "previous_response_id").trim().is_empty()
        && (request_type.trim() != "response.append"
            || mapper.as_ref().is_some_and(Mapper::replayed));
    let warmup = generate_false(&message);
    let mut secrets = observe_send::secrets(&url, &headers, &proxy, auth);
    let conn_target =
        session::Target::new(&auth.id, &url, &proxy, token(auth)).with_secrets(&secrets);
    // A connection kept for the call was opened with the secrets of an
    // earlier one, which xAI may quote back too.
    if let Some(kept) = session.kept_secrets(&conn_target) {
        secrets.extend(&kept);
    }
    let log = Log {
        session: session_id.clone(),
        auth: auth.id.trim().to_owned(),
        url: url.clone(),
    };
    log.request(&message, &secrets);

    // Opening the call (`ensureUpstreamConn`, then the send).
    let text = message.to_string();
    let tap = options.tapped().map(|observation| {
        let body = Bytes::from(text.clone());
        observe_send::announce(
            observation,
            &Attempt::new(
                &options,
                AttemptKind::Websocket,
                PROVIDER,
                &prepared.base_model,
                &Format::CODEX,
                auth,
            )
            .request(&Method::GET, &url, &headers, &body, &secrets),
        )
    });
    let connect = || dial::dial(&proxy, &url, &headers);
    let (conn, mut response_headers) = match session.ensure_conn(conn_target.clone(), connect).await
    {
        Ok(found) => found,
        Err(error) => return Err(dial_error(error, &secrets)),
    };
    secrets.extend(conn.secrets());
    if !ephemeral {
        sessions.record_target(&session_id, &target);
    }
    let mut hold = Hold::new(Arc::clone(&session), ephemeral, guard, conn);
    let sender = tap.clone();
    hold.observe(tap);
    observe_send::request_sent(sender.as_ref());
    if let Err(failure) = hold.conn().send(text.clone()).await {
        let failure = failure.redacted(&secrets);
        let error = write_error(hold.conn().disconnect_code(), &failure);
        hold.invalidate("send_error");
        if ephemeral || !should_retry(&error) {
            hold.release();
            return Err(error);
        }
        // Once more, on a new connection for the same session.
        let connect = || dial::dial(&proxy, &url, &headers);
        let (conn, retry_headers) = match session.ensure_conn(conn_target, connect).await {
            Ok(found) => found,
            Err(error) => {
                hold.release();
                return Err(dial_error(error, &secrets));
            }
        };
        sessions.record_target(&session_id, &target);
        secrets.extend(conn.secrets());
        hold.switch(conn);
        observe_send::request_sent(sender.as_ref());
        if let Err(failure) = hold.conn().send(text).await {
            let failure = failure.redacted(&secrets);
            let error = write_error(hold.conn().disconnect_code(), &failure);
            hold.invalidate("send_error");
            hold.release();
            return Err(error);
        }
        response_headers = retry_headers;
    }

    let filter = XSearchFilter::new(
        prepared.filter_internal_x_search,
        std::mem::take(&mut prepared.client_declared_tools),
    );
    let restorer = NamespaceRestorer::new(std::mem::take(&mut prepared.namespace_tools));
    let state = State {
        hold,
        prepared,
        filter,
        restorer,
        items: OutputItems::default(),
        mapper,
        state_guard,
        message,
        transcript_reset,
        warmup,
        recorded: false,
        secrets,
        log,
        pending: VecDeque::new(),
        failure: None,
        finished: false,
    };
    Ok(StreamResponse {
        headers: response_headers.unwrap_or_default(),
        chunks: state.into_stream(),
    })
}

/// The call's error for a failed connection: a handshake's status and body
/// as xAI's error, or the failure itself, with the `secrets` the handshake
/// sent redacted either way.
fn dial_error(error: DialError, secrets: &Secrets) -> ExecError {
    match error {
        DialError::Handshake { status, body } => {
            status_error(status, &secrets.bytes(&body, Policy::Client)).into()
        }
        DialError::Failed(mut error) => {
            error.message = secrets.text(std::mem::take(&mut error.message), Policy::Client);
            error
        }
    }
}

/// What the call's log lines name. Each line hides the call's secrets,
/// however short, from every field it fills in (`Policy::Disk`).
struct Log {
    session: String,
    auth: String,
    url: String,
}

impl Log {
    /// `logXAIWebsocketRequest`.
    fn request(&self, message: &Value, secrets: &Secrets) {
        let generate =
            get(message, "generate").map_or_else(|| "default".to_owned(), Value::to_string);
        let input_items = match get(message, "input") {
            Some(Value::Array(items)) => items.len(),
            None | Some(Value::Null) => 0,
            Some(_) => 1,
        };
        tracing::info!(
            "xai websockets: upstream request sent session={} auth={} url={} event={} previous_response_id={} generate={} input_items={input_items}",
            secrets.str(&self.session, Policy::Disk),
            secrets.str(&self.auth, Policy::Disk),
            secrets.str(&self.url, Policy::Disk),
            secrets.str(str_at(message, "type").trim(), Policy::Disk),
            secrets.str(str_at(message, "previous_response_id").trim(), Policy::Disk),
            secrets.str(&generate, Policy::Disk),
        );
    }

    /// `logXAIWebsocketWarmupCompleted`.
    fn warmup_completed(&self, created: &Value, secrets: &Secrets) {
        tracing::info!(
            "xai websockets: upstream warmup completed session={} auth={} url={} response_id={}",
            secrets.str(&self.session, Policy::Disk),
            secrets.str(&self.auth, Policy::Disk),
            secrets.str(&self.url, Policy::Disk),
            secrets.str(str_at(created, "response.id").trim(), Policy::Disk),
        );
    }

    /// `logXAIWebsocketTerminalResponse`.
    fn terminal(&self, event_type: &str, event: &Value, secrets: &Secrets) {
        tracing::info!(
            "xai websockets: upstream terminal response session={} auth={} url={} event={} response_id={} previous_response_id={}",
            secrets.str(&self.session, Policy::Disk),
            secrets.str(&self.auth, Policy::Disk),
            secrets.str(&self.url, Policy::Disk),
            secrets.str(event_type, Policy::Disk),
            secrets.str(str_at(event, "response.id").trim(), Policy::Disk),
            secrets.str(
                str_at(event, "response.previous_response_id").trim(),
                Policy::Disk
            ),
        );
    }
}

/// The state of one streaming call.
struct State {
    hold: Hold,
    prepared: Prepared,
    filter: XSearchFilter,
    restorer: NamespaceRestorer,
    items: OutputItems,
    /// The session's IDs and transcript, if the call has a session.
    mapper: Option<Mapper>,
    /// The prompt cache's lock, for a call outside a WebSocket session.
    state_guard: Option<OwnedMutexGuard<()>>,
    /// The message sent, for the transcript.
    message: Value,
    /// Whether the turn starts the transcript again.
    transcript_reset: bool,
    /// Whether the call is a warmup.
    warmup: bool,
    /// Whether the turn went into the transcript.
    recorded: bool,
    /// The secrets the call's handshake sent and, for a kept connection,
    /// those its own handshake sent, redacted from every message, from the
    /// call's errors and from its log lines.
    secrets: Secrets,
    log: Log,
    pending: VecDeque<Bytes>,
    /// An error to end the stream with once `pending` is sent.
    failure: Option<ExecError>,
    finished: bool,
}

impl State {
    /// Reads xAI's next message and queues what the client gets for it, or
    /// ends the stream.
    async fn step(&mut self) {
        let payload = match self.hold.recv().await {
            Ok(payload) => payload,
            Err(failure) => {
                if let Err(error) = self.prepared.apply_patch.finish() {
                    let (events, _) = self.prepared.apply_patch.bridge.fail(error);
                    self.fail_patch(events);
                } else {
                    self.failure = Some(read_error(&failure.redacted(&self.secrets)));
                }
                self.end();
                return;
            }
        };
        if payload.is_empty() {
            return;
        }
        let payload = self.secrets.text(payload, Policy::Client);
        let event: Value = serde_json::from_slice(payload.as_bytes()).unwrap_or(Value::Null);
        if let Some(error) =
            parse_error(&event, payload.as_bytes(), &self.secrets, SystemTime::now())
        {
            self.hold.invalidate("upstream_error");
            self.failure = Some(error);
            self.end();
            return;
        }
        for data in normalize_summary_data_events(payload.into_bytes()) {
            self.prepared.apply_patch.remember_dispatcher_event(&data);
            let mut data = self.restorer.restore(data);
            if !self.prepared.web_search_alias.is_empty() {
                data = restore_client_web_search_name(data, &self.prepared.web_search_alias);
            }
            let Some(data) = self.filter.apply(data).filter(|data| !data.is_empty()) else {
                continue;
            };
            let (events, error) = self.prepared.apply_patch.transform(&data);
            if error.is_some() {
                self.fail_patch(events);
                self.end();
                return;
            }
            for event in events {
                if self.event(event) {
                    self.end();
                    return;
                }
            }
        }
    }

    /// Fails the call for a patch call the bridge can't translate: the
    /// bridge's `events`, then a 502. The connection goes, as the response
    /// is left unfinished.
    fn fail_patch(&mut self, events: Vec<Vec<u8>>) {
        self.hold.invalidate("invalid_tool_arguments");
        self.pending.extend(events.into_iter().map(Bytes::from));
        self.failure = Some(StatusError::new(502, APPLY_PATCH_ERROR_MESSAGE).into());
    }

    /// Queues one event, bridged, for the client; returns whether the
    /// stream ends after it.
    fn event(&mut self, mut payload: Vec<u8>) -> bool {
        let event: Value = serde_json::from_slice(&payload).unwrap_or(Value::Null);
        let event_type = str_at(&event, "type");
        let patch_terminal = self.prepared.apply_patch.active()
            && matches!(
                event_type.as_str(),
                "response.incomplete" | "response.failed"
            );
        let terminal = matches!(
            event_type.as_str(),
            "response.completed" | "response.done" | "error"
        ) || patch_terminal;
        let mut warmup_completed_payload = None;
        match event_type.as_str() {
            "response.created" if self.warmup => {
                let completed = warmup_completed(&event);
                self.record(&completed);
                self.log.warmup_completed(&event, &self.secrets);
                warmup_completed_payload = Some(completed);
            }
            "response.output_item.done" => self.items.collect(&event),
            "response.completed" => {
                self.log.terminal(&event_type, &event, &self.secrets);
                payload = patch_completed_output(payload, &self.items);
                payload = normalize_summary_data(payload);
                replay::cache_completed(&self.prepared.replay, &payload);
                if !self.warmup {
                    self.record(&payload);
                }
            }
            "response.done" => {
                self.log.terminal(&event_type, &event, &self.secrets);
                if !self.warmup {
                    self.record(&payload);
                }
            }
            _ => {}
        }
        let chunk = self.downstream(ensure_responses_usage_details(payload));
        self.pending.push_back(Bytes::from(chunk));
        if let Some(completed) = warmup_completed_payload {
            let chunk = self.downstream(ensure_responses_usage_details(completed));
            self.pending.push_back(Bytes::from(chunk));
            return true;
        }
        terminal
    }

    /// Puts the turn into the session's transcript, once.
    fn record(&mut self, completed: &[u8]) {
        if self.recorded {
            return;
        }
        if let Some(mapper) = &self.mapper {
            mapper
                .state()
                .record_turn(&self.message, completed, self.transcript_reset);
            self.recorded = true;
        }
    }

    /// The event with the IDs the client was given.
    fn downstream(&mut self, payload: Vec<u8>) -> Vec<u8> {
        match self.mapper.as_mut() {
            Some(mapper) => mapper.downstream_response(payload),
            None => payload,
        }
    }

    /// Ends the call: the stream ends once what is pending is sent.
    fn end(&mut self) {
        self.hold.release();
        self.state_guard = None;
        self.finished = true;
    }

    fn into_stream(self) -> ChunkStream {
        futures_util::stream::unfold(self, |mut state| async move {
            loop {
                if let Some(chunk) = state.pending.pop_front() {
                    return Some((Ok(chunk), state));
                }
                if let Some(error) = state.failure.take() {
                    return Some((Err(error), state));
                }
                if state.finished {
                    return None;
                }
                state.step().await;
            }
        })
        .boxed()
    }
}
