// Ported from CLIProxyAPI internal/runtime/executor/codex_websockets_execute.go
// (Execute) and the connection steps it shares with codex_websockets_stream.go
// (ExecuteStream) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Opening a WebSocket call, and the non-streaming call.
//!
//! [`open`] takes the session (the client's, or one for this call alone),
//! waits for its calls before to end, connects if it has no connection for
//! this credential, URL, proxy and token, and sends the `response.create`
//! message. A send that fails on a session's connection is tried once more
//! on a new one, unless the message was too big.
//!
//! The taps are told the handshake and the message once, before connecting,
//! and told again just before each send that the request is going out
//! ([`observe_send::request_sent`]), so the usage statistics' time to first
//! token starts once the connection is up, not at the dial.
//!
//! A failed handshake is the call's error: its status and body (the secrets
//! the handshake sent redacted if they are of eight bytes or more, as in
//! every client error; see [`observe_send::secrets`] and `Policy::Client`),
//! with a usage limit's cooling as for HTTP. Another failure to connect,
//! such as a refused `CONNECT`, is the call's error too, also with the
//! secrets redacted. A 426 for a client not
//! on a WebSocket goes over HTTP instead, as upstream does; the WebSocket
//! route only takes WebSocket clients, so this only happens when the route
//! is called directly.
//!
//! [`execute`] reads Codex's events to the completed response and
//! translates it, as the HTTP call does. Each message has the secrets the
//! call sent redacted before it is read, and so do the call's errors; a
//! call keeping a connection redacts the secrets its handshake sent too,
//! which the taps are told as well.
//!
//! Deviations from upstream:
//! - A failed handshake when trying the send again gives its status error,
//!   as the first handshake does; upstream gives gorilla's `websocket: bad
//!   handshake`.
//! - A dropped call closes its connection; see [`super::session`].
//! - A failure to connect has the secrets the handshake sent, of eight
//!   bytes or more, redacted from its text, as a refused handshake's body
//!   has; upstream passes them on.
//! - The taps are told the request is going out, just before the send,
//!   once the connection is up. Upstream starts the time to first token
//!   there (`StartResponseTTFT`) but logs the request before it dials; the
//!   taps get both as separate steps.
//! - Each message Codex sends has the secrets the call sent redacted before
//!   it is read, if they are of eight bytes or more, as every client error
//!   is (see `Policy::Client` in the crate's `redact` module): a failure
//!   event, an error event, and what a model says in a successful answer,
//!   which upstream passes on as it came. The call's taps read each message
//!   as it came. A call keeping a connection redacts the secrets its
//!   handshake sent as well, from its messages and its errors, as a custom
//!   header may have changed since (see [`super::session`]).

use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;
use http::{HeaderMap, Method};
use open_ferry_core::auth::Auth;
use open_ferry_core::exec::{ExecError, Format, Options, Request, Response};
use open_ferry_core::observe::AttemptKind;
use open_ferry_translate::registry::{Registry, ResponseContext};
use serde_json::Value;

use super::dial::{self, DialError};
use super::errors;
use super::request::{Prepared, prepare};
use super::session::{Hold, Session, Target};
use crate::codex::executor::{CodexExecutor, finish_payload};
use crate::codex::ext;
use crate::codex::request::{Kind, original_request, response_format};
use crate::codex::terminal::{
    APPLY_PATCH_ERROR_MESSAGE, OutputItems, StatusError, empty_incomplete_stream_error,
    has_meaningful_output_delta, is_terminal_empty_incomplete, normalize_completion,
    status_error_with_cooling, terminal_failure,
};
use crate::json::str_at;
use crate::observe_send::{self, Attempt};
use crate::redact::{Policy, Secrets};

/// A call whose message is sent.
pub(super) struct Call {
    /// Its hold on the session and connection.
    pub(super) hold: Hold,
    /// The handshake's response headers, when the call connected.
    pub(super) headers: Option<HeaderMap>,
    /// The secrets the call sends and those its connection's handshake
    /// sent, redacted from each message Codex sends and from the call's
    /// errors (see [`observe_send::secrets`]).
    pub(super) secrets: Secrets,
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
    let proxy = executor.proxy_for(auth);
    let mut secrets = observe_send::secrets(&prepared.url, &prepared.headers, &proxy, auth);
    let token = crate::codex::request::credentials(auth).0;
    let target = Target::new(&auth.id, &prepared.url, &proxy, token).with_secrets(&secrets);
    // A connection kept for the call was opened with the secrets of an
    // earlier one, which Codex may quote back too.
    if let Some(kept) = session.kept_secrets(&target) {
        secrets.extend(&kept);
    }
    // The handshake and the message, told once, before connecting.
    let tap = options.tapped().map(|observation| {
        let model = str_at(&prepared.body, "model");
        let message = Bytes::from(prepared.message.clone());
        observe_send::announce(
            observation,
            &Attempt::new(
                options,
                AttemptKind::Websocket,
                "codex",
                &model,
                &Format::CODEX,
                auth,
            )
            .request(
                &Method::GET,
                &prepared.url,
                &prepared.headers,
                &message,
                &secrets,
            ),
        )
    });
    let model_level_cooling = executor.model_level_cooling();

    let connect = || dial::dial(&proxy, &prepared.url, &prepared.headers);
    let (conn, mut headers) = match session.ensure_conn(target.clone(), connect).await {
        Ok(found) => found,
        Err(DialError::Handshake { status: 426, .. }) if !options.downstream_websocket => {
            return Ok(Opened::Fallback);
        }
        Err(error) => return Err(dial_error(error, &secrets, model_level_cooling)),
    };
    secrets.extend(conn.secrets());

    let mut hold = Hold::new(Arc::clone(&session), ephemeral, guard, conn);
    let sender = tap.clone();
    hold.observe(tap);
    prepared
        .turn
        .set_multi_agent_v2_restore(restores(prepared, &session, hold.conn().id()));
    observe_send::request_sent(sender.as_ref());
    if let Err(failure) = hold.conn().send(prepared.message.clone()).await {
        let failure = failure.redacted(&secrets);
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
                return Err(dial_error(error, &secrets, model_level_cooling));
            }
        };
        secrets.extend(conn.secrets());
        hold.switch(conn);
        prepared
            .turn
            .set_multi_agent_v2_restore(restores(prepared, &session, hold.conn().id()));
        observe_send::request_sent(sender.as_ref());
        if let Err(failure) = hold.conn().send(prepared.message.clone()).await {
            let failure = failure.redacted(&secrets);
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
    Ok(Opened::Ws(Call {
        hold,
        headers,
        secrets,
    }))
}

/// Whether Codex's events on `conn` name the collaboration namespace back:
/// this request renamed it, or an earlier one on the connection did, and
/// the request didn't already use the new name.
fn restores(prepared: &Prepared, session: &Session, conn: u64) -> bool {
    !prepared.conflict && (prepared.optimize || session.is_multi_agent_optimized(conn))
}

/// The call's error for a failed connection: a handshake's status and body,
/// or the failure itself, with the `secrets` the handshake sent redacted
/// either way.
fn dial_error(error: DialError, secrets: &Secrets, model_level_cooling: bool) -> ExecError {
    match error {
        DialError::Handshake { status: 426, body } => {
            let body = secrets.bytes(&body, Policy::Client);
            StatusError::new(426, String::from_utf8_lossy(&body)).into()
        }
        DialError::Handshake { status, body } => status_error_with_cooling(
            status,
            &secrets.bytes(&body, Policy::Client),
            model_level_cooling,
        )
        .into(),
        DialError::Failed(mut error) => {
            error.message = secrets.text(std::mem::take(&mut error.message), Policy::Client);
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
    let Opened::Ws(Call {
        mut hold, secrets, ..
    }) = open(executor, auth, &mut prepared, options).await?
    else {
        return executor.execute_inner(auth, request, options).await;
    };
    let model_level_cooling = executor.model_level_cooling();
    let mut items = OutputItems::default();
    let mut saw_output_delta = false;
    loop {
        let payload = match hold.recv().await {
            Ok(payload) => payload,
            Err(failure) => {
                hold.release();
                return Err(errors::error(&failure.redacted(&secrets)));
            }
        };
        if payload.is_empty() {
            continue;
        }
        // Each message, as it is read; the taps read it as it came.
        let payload = secrets.text(payload, Policy::Client);
        let data = ext::restore(&prepared.turn, payload.as_bytes());
        let mut event: Value = serde_json::from_slice(&data).unwrap_or(Value::Null);
        if let Some((error, status, raw)) =
            errors::parse_ws_error(&event, model_level_cooling, &secrets, SystemTime::now())
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
            return Err(error.redacted(&secrets).into());
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
                    let error: ExecError = empty_incomplete_stream_error().into();
                    hold.report(&error);
                    return Err(error);
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
