// Ported from CLIProxyAPI internal/runtime/executor/codex_websockets_execute.go
// (Execute) and the connection steps it shares with codex_websockets_stream.go
// (ExecuteStream) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Opening a WebSocket call, and the non-streaming call.
//!
//! [`open`] takes the session (the client's, or one for this call alone),
//! waits for its calls before to end, connects if it has no connection for
//! this credential, URL, proxy and token, and sends the `response.create`
//! message. A send that fails on a session's connection is tried once more
//! on a new one, unless the message was too big.
//!
//! A failed handshake is the call's error: its status and body (the
//! credential's secret redacted), with a usage limit's cooling as for
//! HTTP. Another failure to connect, such as a refused `CONNECT`, is the
//! call's error too, also with the secret redacted. A 426 for a client not
//! on a WebSocket goes over HTTP instead, as upstream does; the WebSocket
//! route only takes WebSocket clients, so this only happens when the route
//! is called directly.
//!
//! [`execute`] reads Codex's events to the completed response and
//! translates it, as the HTTP call does.
//!
//! Deviations from upstream:
//! - A failed handshake when trying the send again gives its status error,
//!   as the first handshake does; upstream gives gorilla's `websocket: bad
//!   handshake`.
//! - A dropped call closes its connection; see [`super::session`].
//! - A failure to connect has the credential's secret redacted from its
//!   text, as a refused handshake's body has; upstream passes it on.

use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;
use http::HeaderMap;
use open_ferry_core::auth::Auth;
use open_ferry_core::exec::{ExecError, Format, Options, Request, Response};
use open_ferry_translate::registry::{Registry, ResponseContext};
use serde_json::Value;

use super::dial::{self, DialError};
use super::errors;
use super::request::{Prepared, prepare};
use super::session::{Hold, Session, Target};
use crate::codex::executor::{CodexExecutor, finish_payload};
use crate::codex::ext;
use crate::codex::request::{Kind, credentials, original_request, response_format};
use crate::codex::terminal::{
    APPLY_PATCH_ERROR_MESSAGE, OutputItems, StatusError, empty_incomplete_stream_error,
    has_meaningful_output_delta, is_terminal_empty_incomplete, normalize_completion,
    status_error_with_cooling, terminal_failure,
};
use crate::json::str_at;
use crate::redact;

/// A call whose message is sent.
pub(super) struct Call {
    /// Its hold on the session and connection.
    pub(super) hold: Hold,
    /// The handshake's response headers, when the call connected.
    pub(super) headers: Option<HeaderMap>,
}

/// How [`open`] went.
pub(super) enum Opened {
    /// The message went over the WebSocket.
    Ws(Call),
    /// Codex asked for HTTP (a 426) and the client isn't on a WebSocket.
    Fallback,
}

/// Opens the call `prepared` describes with `auth` and sends its message
/// (the connection steps of `Execute` and `ExecuteStream`). Sets whether
/// the turn names the collaboration namespace back.
pub(super) async fn open(
    executor: &CodexExecutor,
    auth: &Auth,
    prepared: &mut Prepared,
    options: &Options,
) -> Result<Opened, ExecError> {
    let store = executor.websockets();
    let session_id = options
        .metadata
        .execution_session_id
        .as_deref()
        .unwrap_or_default();
    let (session, ephemeral, guard) = match store.get_or_create(session_id) {
        Some(session) => {
            let guard = session.lock_requests().await;
            (session, false, Some(guard))
        }
        None => (store.ephemeral(), true, None),
    };
    let secret = credentials(auth).0;
    let model_level_cooling = executor.model_level_cooling();
    let proxy = executor.proxy_for(auth);
    let target = Target::new(&auth.id, &prepared.url, &proxy, secret);

    let connect = || dial::dial(&proxy, &prepared.url, &prepared.headers);
    let (conn, mut headers) = match session.ensure_conn(target.clone(), connect).await {
        Ok(found) => found,
        Err(DialError::Handshake { status: 426, .. }) if !options.downstream_websocket => {
            return Ok(Opened::Fallback);
        }
        Err(error) => return Err(dial_error(error, secret, model_level_cooling)),
    };

    let mut hold = Hold::new(Arc::clone(&session), ephemeral, guard, conn);
    prepared
        .turn
        .set_multi_agent_v2_restore(restores(prepared, &session, hold.conn().id()));
    if let Err(failure) = hold.conn().send(prepared.message.clone()).await {
        let error = errors::write_error(hold.conn().disconnect_code(), &failure);
        hold.invalidate("send_error");
        if ephemeral || !errors::should_retry(&error) {
            hold.release();
            return Err(error);
        }
        // Once more, on a new connection for the same session.
        let connect = || dial::dial(&proxy, &prepared.url, &prepared.headers);
        let (conn, retry_headers) = match session.ensure_conn(target, connect).await {
            Ok(found) => found,
            Err(error) => {
                hold.release();
                return Err(dial_error(error, secret, model_level_cooling));
            }
        };
        hold.switch(conn);
        prepared
            .turn
            .set_multi_agent_v2_restore(restores(prepared, &session, hold.conn().id()));
        if let Err(failure) = hold.conn().send(prepared.message.clone()).await {
            let error = errors::write_error(hold.conn().disconnect_code(), &failure);
            hold.invalidate("send_error");
            hold.release();
            return Err(error);
        }
        headers = retry_headers;
    }

    if prepared.optimize || prepared.conflict {
        session
            .set_multi_agent_optimized(hold.conn().id(), prepared.optimize && !prepared.conflict);
    }
    Ok(Opened::Ws(Call { hold, headers }))
}

/// Whether Codex's events on `conn` name the collaboration namespace back:
/// this request renamed it, or an earlier one on the connection did, and
/// the request didn't already use the new name.
fn restores(prepared: &Prepared, session: &Session, conn: u64) -> bool {
    !prepared.conflict && (prepared.optimize || session.is_multi_agent_optimized(conn))
}

/// The call's error for a failed connection: a handshake's status and body,
/// or the failure itself, with the credential's secret redacted either way.
fn dial_error(error: DialError, secret: &str, model_level_cooling: bool) -> ExecError {
    match error {
        DialError::Handshake { status: 426, body } => {
            let body = redact::bytes(&body, secret);
            StatusError::new(426, String::from_utf8_lossy(&body)).into()
        }
        DialError::Handshake { status, body } => {
            status_error_with_cooling(status, &redact::bytes(&body, secret), model_level_cooling)
                .into()
        }
        DialError::Failed(mut error) => {
            error.message = redact::text(std::mem::take(&mut error.message), secret);
            error
        }
    }
}

/// A non-streaming call over the WebSocket (`Execute`): Codex's completed
/// response, translated to the client's format.
pub(in crate::codex) async fn execute(
    executor: &CodexExecutor,
    auth: &Auth,
    request: &Request,
    options: &Options,
) -> Result<Response, ExecError> {
    let mut prepared = prepare(
        Kind::Execute,
        executor.context(auth),
        auth,
        executor.base_url(),
        request,
        options,
    )?;
    let Opened::Ws(Call { mut hold, .. }) = open(executor, auth, &mut prepared, options).await?
    else {
        return executor.execute_inner(auth, request, options).await;
    };
    let secret = credentials(auth).0;
    let model_level_cooling = executor.model_level_cooling();
    let mut items = OutputItems::default();
    let mut saw_output_delta = false;
    loop {
        let payload = match hold.recv().await {
            Ok(payload) => payload,
            Err(failure) => {
                hold.release();
                return Err(errors::error(&failure));
            }
        };
        if payload.is_empty() {
            continue;
        }
        let data = ext::restore(&prepared.turn, payload.as_bytes());
        let mut event: Value = serde_json::from_slice(&data).unwrap_or(Value::Null);
        if let Some((error, status, raw)) =
            errors::parse_ws_error(&event, model_level_cooling, secret, SystemTime::now())
        {
            hold.invalidate("upstream_error");
            ext::on_failure(&prepared.turn, status, raw.as_bytes());
            hold.release();
            return Err(error);
        }
        if let Some((error, body)) = terminal_failure(&event, model_level_cooling) {
            hold.unlock();
            hold.invalidate("terminal_failure");
            ext::on_failure(&prepared.turn, error.status, body.as_bytes());
            hold.release();
            return Err(error.redacted(secret).into());
        }

        let normalized = normalize_completion(&mut event);
        if has_meaningful_output_delta(&event) {
            saw_output_delta = true;
        }
        let event_type = str_at(&event, "type");
        match event_type.as_str() {
            "response.output_item.done" => items.collect(&event),
            "response.completed" | "response.incomplete" => {
                if is_terminal_empty_incomplete(&event, items.len(), saw_output_delta) {
                    hold.invalidate("terminal_empty_incomplete");
                    hold.release();
                    return Err(empty_incomplete_stream_error().into());
                }
                let patched = items.patch(&mut event);
                if event_type != "response.incomplete" {
                    ext::on_completed(&prepared.turn, &event);
                }
                hold.release();
                let completed = if normalized || patched {
                    event.to_string().into_bytes()
                } else {
                    data.into_owned()
                };
                let format = response_format(options);
                let original = original_request(request, options);
                let context = ResponseContext {
                    model: &request.model,
                    original_request: &original,
                    request: &prepared.body,
                };
                let out = Registry::global()
                    .translate_non_stream(&Format::CODEX, &format, &context, completed)
                    .filter(|out| !out.is_empty())
                    .ok_or_else(|| StatusError::new(502, APPLY_PATCH_ERROR_MESSAGE))?;
                return Ok(Response {
                    payload: Bytes::from(finish_payload(&format, out)),
                    headers: HeaderMap::new(),
                });
            }
            _ => {}
        }
    }
}
