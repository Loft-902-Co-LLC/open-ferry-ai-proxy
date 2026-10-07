// Ported from forwardResponsesWebsocket, responsesWebsocketErrorStatus,
// shouldExposeResponsesUpstreamError, writeResponsesWebsocketTerminalError,
// shouldReplayResponsesWebsocketPinnedAuthFailure,
// shouldReleaseResponsesWebsocketPinnedAuth,
// collectResponsesWebsocketOutputItem,
// restoreResponsesWebsocketCompletionOutput,
// reconcileResponsesWebsocketCompletionToolCalls,
// responseCompletedOutputFromPayload, recordPendingToolCallIDsFromPayload,
// updatePendingToolCallIDsFromItem, websocketJSONPayloadsFromChunk and
// buildResponsesWebsocketErrorPayload in CLIProxyAPI
// sdk/api/handlers/openai/openai_responses_websocket_forward.go, and
// isResponsesWebsocketCompletionEvent and
// responsesWebsocketErrorMessageFromPayload in
// sdk/api/handlers/openai/openai_responses_websocket_timeline.go (v8.0.15,
// MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A turn's events, from the call's stream to the client: each one as a
//! message, with the completed response's output restored where the
//! upstream left it out, and errors closing the socket.
//!
//! A Codex duplex stream (response steering) carries every response of the
//! socket, so its end closes the socket rather than ending a turn, and an
//! error event after a response has started goes to the client as any
//! other event; the stream's own error still closes the socket.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Duration;

use futures_util::StreamExt;
use http::HeaderMap;
use open_ferry_core::exec::ExecError;
use open_ferry_core::observe::RequestContext;
use open_ferry_translate::go;
use tokio::time::{Instant, Interval, MissedTickBehavior};

use super::client_error::is_request_fault;
use super::repair::{
    ServerToolCaches, ToolCacheTurn, is_complete_tool_call, is_tool_call, is_tool_output,
    record_tool_calls_from_payload,
};
use super::writer::{Conn, Socket};
use crate::auth::Principal;
use crate::errors::{ErrorMessage, openai_body};
use crate::exec::HandlerStream;
use crate::json::{self, Val, str_at};
use crate::request_log;
use crate::status::status_text;

/// The event that ends a turn with an error (`wsEventTypeError`).
const EVENT_ERROR: &str = "error";

/// How to forward a turn (`responsesWebsocketForwardOptions`).
pub(super) struct ForwardOptions<'a> {
    /// The server's tool caches.
    pub(super) caches: &'a ServerToolCaches,
    /// Who the client authenticated as.
    pub(super) principal: Principal,
    /// The key the client's tool calls are kept under.
    pub(super) session_key: &'a str,
    /// Send the completed response as the upstream wrote it.
    pub(super) preserve_completion_output: bool,
    /// The turn noting tool calls, or `None` to keep them in the shared cache
    /// as they come.
    pub(super) turn: Option<&'a mut ToolCacheTurn>,
    /// Whether to hand an error back without telling the client.
    pub(super) suppress_error: &'a (dyn Fn(&ErrorMessage) -> bool + Sync),
    /// How often to ping the client while the turn runs.
    pub(super) keepalive: Option<Duration>,
    /// The session's request context, whose log gets the turn's errors.
    pub(super) context: Option<&'a RequestContext>,
    /// Whether the call's credential holds a Codex duplex stream, which
    /// ends with the socket (`duplexStream`).
    pub(super) duplex: bool,
}

/// How a turn ended.
#[derive(Debug)]
pub(super) enum Forwarded {
    /// The response completed: its output, ID, and the tool calls it left
    /// unanswered, sorted.
    Completed {
        output: Vec<u8>,
        response_id: String,
        pending_call_ids: Vec<String>,
    },
    /// The turn failed with an error the client wasn't told of.
    Suppressed(ErrorMessage),
    /// The connection closed or is closing.
    Closed,
}

/// The output items a turn's `response.output_item.done` events gave.
#[derive(Debug, Default)]
pub(super) struct OutputItems {
    /// Items by `output_index`.
    pub(super) by_index: BTreeMap<i64, Vec<u8>>,
    /// Items without one, in order.
    pub(super) fallback: Vec<Vec<u8>>,
}

impl OutputItems {
    fn is_empty(&self) -> bool {
        self.by_index.is_empty() && self.fallback.is_empty()
    }

    /// The items, those with an index first, in index order.
    fn iter(&self) -> impl Iterator<Item = &[u8]> {
        self.by_index
            .values()
            .chain(&self.fallback)
            .map(Vec::as_slice)
    }

    /// Keeps the item of a `response.output_item.done` event
    /// (`collectResponsesWebsocketOutputItem`).
    pub(super) fn collect(&mut self, payload: &[u8]) {
        if str_at(payload, "type") != "response.output_item.done" {
            return;
        }
        let Some(item) = json::get(payload, "item").filter(Val::is_object) else {
            return;
        };
        match json::get(payload, "output_index") {
            Some(index) => {
                self.by_index.insert(index.int(), item.raw.to_vec());
            }
            None => self.fallback.push(item.raw.to_vec()),
        }
    }
}

/// Sends `items` to the client until the response completes, or something
/// ends the turn (`forwardResponsesWebsocket`).
pub(super) async fn forward<S: Socket>(
    conn: &mut Conn<S>,
    mut items: HandlerStream,
    mut options: ForwardOptions<'_>,
) -> Forwarded {
    let mut completed = false;
    let mut response_started = false;
    let mut completed_output = b"[]".to_vec();
    let mut completed_id = String::new();
    let mut outputs = OutputItems::default();
    let mut pending: BTreeSet<String> = BTreeSet::new();
    let mut ticker = options.keepalive.filter(|d| !d.is_zero()).map(|period| {
        let mut ticker = tokio::time::interval_at(Instant::now() + period, period);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        ticker
    });

    loop {
        let next = tokio::select! {
            () = tick(ticker.as_mut()) => {
                if conn.ping().await.is_err() {
                    return Forwarded::Closed;
                }
                continue;
            }
            // The client is gone; the stream is dropped with the turn.
            () = conn.gone() => return Forwarded::Closed,
            next = items.next() => next,
        };
        let chunk = match next {
            None if options.duplex => {
                // A duplex stream ends with its socket, not with a response.
                conn.close_without_error();
                return Forwarded::Closed;
            }
            None if completed => {
                return Forwarded::Completed {
                    output: completed_output,
                    response_id: completed_id,
                    pending_call_ids: pending.into_iter().collect(),
                };
            }
            None => {
                let error = ErrorMessage::new(408, "stream closed before response.completed");
                tracing::debug!(error = %error.text, "responses websocket: stream ended early");
                request_log::record_api_error(options.context, &error);
                conn.close_without_error();
                return Forwarded::Closed;
            }
            Some(Err(error)) => {
                tracing::debug!(status = error.status, error = %error.text, "responses websocket: upstream error");
                request_log::record_api_error(options.context, &error);
                if (options.suppress_error)(&error) {
                    return Forwarded::Suppressed(error);
                }
                if !conn.close_for_upstream_error(&error).await {
                    write_terminal_error(conn, &error, None).await;
                }
                return Forwarded::Closed;
            }
            Some(Ok(chunk)) => chunk,
        };
        if let Some(ticker) = ticker.as_mut() {
            ticker.reset();
        }

        for mut payload in payloads_from_chunk(&chunk) {
            let event_type = str_at(&payload, "type");
            if event_type == "response.created" {
                response_started = true;
                completed = false;
                outputs = OutputItems::default();
                pending.clear();
            }
            outputs.collect(&payload);
            if is_completion_event(&event_type) && !options.preserve_completion_output {
                payload = restore_completion_output(&payload, &outputs);
            }
            match options.turn.as_deref_mut() {
                Some(turn) => turn.record_response(&payload),
                None => record_tool_calls_from_payload(
                    &mut options.caches.lock(options.principal).calls,
                    options.session_key,
                    &payload,
                ),
            }
            record_pending_call_ids(&mut pending, &payload);

            // On a duplex stream the executor closes the connection: an
            // error event after a response has started is one the client
            // can recover from.
            if event_type == EVENT_ERROR && !(response_started && options.duplex) {
                let error = error_message_from_payload(&payload);
                tracing::debug!(status = error.status, error = %error.text, "responses websocket: error event");
                request_log::record_api_error(options.context, &error);
                if (options.suppress_error)(&error) {
                    return Forwarded::Suppressed(error);
                }
                if !conn.close_for_upstream_error(&error).await {
                    write_terminal_error(conn, &error, Some(&payload)).await;
                }
                return Forwarded::Closed;
            }
            if is_completion_event(&event_type) {
                completed = true;
                completed_output = completed_output_from_payload(&payload, &outputs);
                str_at(&payload, "response.id")
                    .trim()
                    .clone_into(&mut completed_id);
            }
            if conn.write(&payload).await.is_err() {
                tracing::debug!(
                    event = %event_type,
                    "responses websocket: downstream write failed"
                );
                return Forwarded::Closed;
            }
        }
    }
}

/// The ticker's next tick, or never without one.
async fn tick(ticker: Option<&mut Interval>) {
    match ticker {
        Some(ticker) => {
            ticker.tick().await;
        }
        None => std::future::pending().await,
    }
}

/// Whether `event_type` ends a response (`isResponsesWebsocketCompletionEvent`).
pub(super) fn is_completion_event(event_type: &str) -> bool {
    event_type == "response.completed" || event_type == "response.done"
}

/// An `error` event as a message: its status, or 500, and the event as the
/// text (`responsesWebsocketErrorMessageFromPayload`).
pub(super) fn error_message_from_payload(payload: &[u8]) -> ErrorMessage {
    let status_at = |path: &str| {
        json::get(payload, path)
            .map(|status| status.int())
            .unwrap_or_default()
    };
    let mut status = status_at("status");
    if status <= 0 {
        status = status_at("status_code");
    }
    let status = u16::try_from(status)
        .ok()
        .filter(|&status| status > 0)
        .unwrap_or(500);
    let trimmed = go::trim_space(payload);
    if trimmed.is_empty() {
        return ErrorMessage::new(status, status_text(status));
    }
    // A call's error, so the status counts where upstream asks the error
    // for one.
    ErrorMessage::from_exec(ExecError::upstream(
        status,
        String::from_utf8_lossy(trimmed),
    ))
}

/// The status of `error`, or 0 (`responsesWebsocketErrorStatus`).
pub(super) fn error_status(error: &ErrorMessage) -> u16 {
    if error.status > 0 {
        return error.status;
    }
    error.source.as_ref().map_or(0, ExecError::http_status)
}

/// Whether the client should hear of `error`: only a rejected credential or
/// a fault in the request, which no retry can fix. Anything else closes the
/// socket, and the client reconnects with the whole conversation
/// (`shouldExposeResponsesUpstreamError`).
pub(super) fn should_expose(error: &ErrorMessage) -> bool {
    error.terminal_auth || is_request_fault(error_status(error), &error.text)
}

/// Whether a pinned credential's failure means the client must replay the
/// turn over a new socket (`shouldReplayResponsesWebsocketPinnedAuthFailure`).
pub(super) fn should_replay_pinned_failure(error: &ErrorMessage) -> bool {
    matches!(error_status(error), 401 | 429)
}

/// Whether to stop pinning a credential after `error`
/// (`shouldReleaseResponsesWebsocketPinnedAuth`).
pub(super) fn should_release_pinned(error: &ErrorMessage) -> bool {
    if matches!(
        error_status(error),
        401 | 402 | 403 | 429 | 408 | 502 | 503 | 504
    ) {
        return true;
    }
    let text = go::to_lower(&error.text);
    [
        "stream closed before response.completed",
        "previous_response_not_found",
        "ws_failed",
        "upstream stream closed before first payload",
        "empty_stream",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

/// Tells the client of `error` and closes, or just closes when it shouldn't
/// hear of it. `payload` is the error event to send, when the upstream sent
/// one (`writeResponsesWebsocketTerminalError`).
pub(super) async fn write_terminal_error<S: Socket>(
    conn: &mut Conn<S>,
    error: &ErrorMessage,
    payload: Option<&[u8]>,
) {
    if !should_expose(error) {
        tracing::debug!(
            status = error.status,
            error = %error.text,
            "responses websocket: closing without the upstream error"
        );
        conn.close_without_error();
        return;
    }
    let built;
    let payload = match payload {
        Some(payload) if !payload.is_empty() => payload,
        _ => {
            built = error_payload(error);
            &built
        }
    };
    if conn.close_with_payload(payload).await {
        tracing::info!(
            payload = %String::from_utf8_lossy(payload),
            "responses websocket: downstream_out terminal error"
        );
    }
}

/// The completion event with its output restored: the tool calls the turn's
/// done events gave in place of those in the output, or, where the output is
/// empty, the done events' items (`restoreResponsesWebsocketCompletionOutput`).
pub(super) fn restore_completion_output(payload: &[u8], outputs: &OutputItems) -> Vec<u8> {
    if let Some(output) = json::get(payload, "response.output").filter(Val::is_array)
        && !output.array().is_empty()
    {
        return match reconcile_tool_calls(output, outputs) {
            Some(reconciled) => json::set_raw(payload, "response.output", &reconciled),
            None => payload.to_vec(),
        };
    }
    if outputs.is_empty() {
        return payload.to_vec();
    }
    json::set_raw(
        payload,
        "response.output",
        &completed_output_from_payload(payload, outputs),
    )
}

/// `output` with each tool call the done events gave whole in place of the
/// output's own, or `None` when none differs
/// (`reconcileResponsesWebsocketCompletionToolCalls`).
fn reconcile_tool_calls(output: Val<'_>, outputs: &OutputItems) -> Option<Vec<u8>> {
    let mut collected: HashMap<String, &[u8]> = HashMap::new();
    for raw in outputs.iter() {
        let item = Val::parse(raw);
        if is_complete_tool_call(item) {
            let call_id = item
                .and_then(|item| item.get("call_id"))
                .map(|id| id.str().trim().to_owned())
                .unwrap_or_default();
            collected.insert(call_id, raw);
        }
    }
    if collected.is_empty() {
        return None;
    }
    let mut changed = false;
    let items = output.array();
    let reconciled: Vec<&[u8]> = items
        .iter()
        .map(|item| {
            let item_type = item.get("type").map(|kind| kind.str()).unwrap_or_default();
            if is_tool_call(&item_type) {
                let call_id = item
                    .get("call_id")
                    .map(|id| id.str().trim().to_owned())
                    .unwrap_or_default();
                if let Some(&raw) = collected.get(&call_id)
                    && raw != item.raw
                {
                    changed = true;
                    return raw;
                }
            }
            item.raw
        })
        .collect();
    if !changed {
        return None;
    }
    json::compact_html(&reconciled)
}

/// The completed response's output: as the upstream gave it, or else the
/// done events' items, less any tool call that isn't whole
/// (`responseCompletedOutputFromPayload`).
pub(super) fn completed_output_from_payload(payload: &[u8], outputs: &OutputItems) -> Vec<u8> {
    if let Some(output) = json::get(payload, "response.output").filter(Val::is_array)
        && !output.array().is_empty()
    {
        return output.raw.to_vec();
    }
    if outputs.is_empty() {
        return b"[]".to_vec();
    }
    let items: Vec<&[u8]> = outputs
        .iter()
        .filter(|raw| {
            let item = Val::parse(raw);
            let item_type = item
                .and_then(|item| item.get("type"))
                .map(|kind| kind.str())
                .unwrap_or_default();
            !is_tool_call(&item_type) || is_complete_tool_call(item)
        })
        .collect();
    json::compact_html(&items).unwrap_or_else(|| b"[]".to_vec())
}

/// Notes the tool calls an event leaves waiting for output, and those it
/// answers (`recordPendingToolCallIDsFromPayload`).
pub(super) fn record_pending_call_ids(pending: &mut BTreeSet<String>, payload: &[u8]) {
    if payload.is_empty() {
        return;
    }
    if let Some(item) = json::get(payload, "item") {
        update_pending_call_ids(pending, item);
    }
    if let Some(output) = json::get(payload, "response.output").filter(Val::is_array) {
        for item in output.array() {
            update_pending_call_ids(pending, item);
        }
    }
}

/// `updatePendingToolCallIDsFromItem`.
fn update_pending_call_ids(pending: &mut BTreeSet<String>, item: Val<'_>) {
    let item_type = item.get("type").map(|kind| kind.str()).unwrap_or_default();
    let call_id = || {
        item.get("call_id")
            .map(|id| id.str().trim().to_owned())
            .unwrap_or_default()
    };
    if is_tool_call(&item_type) {
        if is_complete_tool_call(Some(item)) {
            pending.insert(call_id());
        }
    } else if is_tool_output(&item_type) {
        pending.remove(&call_id());
    }
}

/// The JSON events in a chunk of SSE text, or the chunk itself when it is
/// one (`websocketJSONPayloadsFromChunk`).
pub(super) fn payloads_from_chunk(chunk: &[u8]) -> Vec<Vec<u8>> {
    let payloads: Vec<Vec<u8>> = chunk
        .split(|&b| b == b'\n')
        .filter_map(|line| {
            let line = go::trim_space(line);
            if line.is_empty() || line.starts_with(b"event:") {
                return None;
            }
            let line = match line.strip_prefix(b"data:") {
                Some(data) => go::trim_space(data),
                None => line,
            };
            (!line.is_empty() && line != b"[DONE]" && json::valid(line)).then(|| line.to_vec())
        })
        .collect();
    if !payloads.is_empty() {
        return payloads;
    }
    let mut trimmed = go::trim_space(chunk);
    if let Some(data) = trimmed.strip_prefix(b"data:") {
        trimmed = go::trim_space(data);
    }
    if !trimmed.is_empty() && trimmed != b"[DONE]" && json::valid(trimmed) {
        return vec![trimmed.to_vec()];
    }
    Vec::new()
}

/// The `error` event that tells the client of `error`
/// (`buildResponsesWebsocketErrorPayload`).
pub(super) fn error_payload(error: &ErrorMessage) -> Vec<u8> {
    let status = if error.status > 0 { error.status } else { 500 };
    let text = if error.text.trim().is_empty() {
        status_text(status)
    } else {
        &error.text
    };
    let body = openai_body(status, text, error.terminal_auth);
    let mut payload = format!(r#"{{"type":"error","status":{status}}}"#).into_bytes();
    if let Some(headers) = headers_object(&error.addon) {
        payload = json::set_raw(&payload, "headers", &headers);
    }
    if json::valid(body.as_bytes()) {
        let node = json::get(body.as_bytes(), "error").map_or(body.as_bytes(), |node| node.raw);
        payload = json::set_raw(&payload, "error", node);
    }
    if json::get(&payload, "error").is_none() {
        payload = json::set_str(&payload, "error.type", "server_error");
        payload = json::set_str(&payload, "error.message", text);
    }
    payload
}

/// The first value of each header, by its name as Go writes it, or `None`
/// when there are none.
fn headers_object(headers: &HeaderMap) -> Option<Vec<u8>> {
    let mut first: BTreeMap<String, String> = BTreeMap::new();
    for name in headers.keys() {
        if let Some(value) = headers.get(name) {
            first.insert(
                canonical_header_key(name.as_str()),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            );
        }
    }
    if first.is_empty() {
        return None;
    }
    let mut out = String::from("{");
    for (index, (name, value)) in first.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&json::json_string(name));
        out.push(':');
        out.push_str(&json::json_string(value));
    }
    out.push('}');
    Some(out.into_bytes())
}

/// A header name as Go's `CanonicalMIMEHeaderKey` writes it: each word
/// capitalized.
pub(super) fn canonical_header_key(name: &str) -> String {
    let mut upper = true;
    name.chars()
        .map(|c| {
            let out = if upper {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            };
            upper = c == '-';
            out
        })
        .collect()
}
