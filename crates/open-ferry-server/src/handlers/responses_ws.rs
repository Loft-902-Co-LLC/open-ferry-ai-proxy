// Ported from ResponsesWebsocket's upgrade, responsesWebsocketUpgrader and
// websocketUpgradeHeaders in CLIProxyAPI
// sdk/api/handlers/openai/openai_responses_websocket.go (v8.0.15, MIT), with
// the handshake checks of gorilla/websocket's Upgrader.Upgrade (v1.5.3,
// BSD-2-Clause).
// https://github.com/router-for-me/CLIProxyAPI

//! `GET /v1/responses`: the Responses WebSocket.
//!
//! A client sends `response.create` and `response.append` requests as JSON
//! messages and gets the response events back as messages. The session keeps
//! the transcript so a client can send only what is new, repairs tool calls
//! the client dropped, answers warm-ups itself, and hands a conversation to
//! a credential's upstream WebSocket when that credential can hold it.
//!
//! Requests are read and edited as raw JSON, as upstream's gjson and sjson
//! do, rather than parsed with serde like the other handlers: the bytes a
//! client sends go on as they came, keys in order and numbers as written.
//!
//! Deviations from upstream:
//! - The handshake is checked as gorilla checks it, then by axum, which is
//!   stricter: an `Upgrade` or `Sec-WebSocket-Version` header must equal its
//!   value rather than list it, and its other rejections answer 400. A
//!   connection axum can't upgrade answers 500.
//! - A message or frame may be at most the body limit; gorilla has no limit.
//! - What the session knows of credentials and models comes from one query,
//!   `Dispatcher::websocket_support`, where upstream reads the auth manager
//!   and the model registry itself. Credentials held only by an execution
//!   session (upstream's home runtime) aren't consulted, and image-only
//!   models have no providers here.
//! - Plugin executors, provider routes and model routers aren't ported, so
//!   a route never overrides the model, and observed compaction is keyed by
//!   model and credential alone.
//! - `prepareCodexMultiAgentV2Tools` and `prepareCodexOrphanDelegation` are
//!   left out, and `WithRequiredUpstreamWebsocket` isn't passed to calls.
//! - Response steering (the duplex reader), subscriptions to upstream
//!   disconnects, and the client's own WebSocket timeline in the request
//!   log aren't ported; those events go to `tracing`. The log of a session
//!   has the upgrade request, a `101` answer, the upstream attempts of
//!   every turn with their WebSocket timelines, and each turn's
//!   `API ERROR RESPONSE`.
//! - A client that goes away is noticed when a read, write or ping fails,
//!   as with gorilla after the hijack; upstream also cancels the call with
//!   the request's context.
//! - A message that isn't UTF-8 goes out with replacement characters, since
//!   a text message must be UTF-8.
//! - An error event's headers are sorted, one value each, where upstream's
//!   map order is random.
//! - An error event's status outside a `u16` is 500.
//! - The cause in a merge error reads `invalid JSON`, or approximates Go's
//!   `cannot unmarshal` text, where upstream quotes `encoding/json`.
//! - A request ID is a UUIDv7, where upstream's are v4.
//! - The tool-call caches belong to the server and are kept by the
//!   client's principal, then its session key. Upstream's are global and
//!   kept by session key alone, which the client chooses, so a client that
//!   sends another's session ID, under any API key, has that client's tool
//!   calls put into its requests. With no API keys configured, every client
//!   is one anonymous principal and shares the caches as upstream's do.
//! - The tool-call caches share one lock, where upstream has one per cache
//!   and one for the transactions between them. Like upstream's shared
//!   caches, whose TTL is zero, they don't expire.
//! - A `X-Codex-Turn-Metadata` header is read for its session ID only when
//!   it is JSON.
//! - A request with 128 or more arrays and objects inside one another is
//!   answered with a 400 error event before anything else is done with it,
//!   so the session is as it was (see [`crate::body::check_depth`]).
//!   Upstream forwards a request of any depth, and warm-ups and transcripts
//!   hold it as it came.

mod client_error;
mod forward;
mod prewarm;
mod repair;
mod requests;
mod session;
mod writer;

#[cfg(test)]
mod tests;

pub(crate) use repair::ServerToolCaches;

use axum::extract::State;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::response::{IntoResponse, Response};
use http::header::{CONTENT_TYPE, SEC_WEBSOCKET_VERSION, X_CONTENT_TYPE_OPTIONS};
use http::{HeaderMap, HeaderValue, Method, StatusCode};

use crate::exec::ClientRequest;
use crate::state::AppState;
use crate::status::status_text;

/// The header a client keeps its turn state in across reconnects
/// (`wsTurnStateHeader`).
const TURN_STATE_HEADER: &str = "x-codex-turn-state";

/// `GET /v1/responses`: upgrades to the WebSocket and runs the session
/// (`ResponsesWebsocket`).
pub(crate) async fn websocket(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    client: ClientRequest,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    if let Err(status) = check_handshake(&method, &headers) {
        return handshake_error(status);
    }
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(WebSocketUpgradeRejection::ConnectionNotUpgradable(_)) => return handshake_error(500),
        Err(rejection) => {
            tracing::debug!(%rejection, "responses websocket: upgrade rejected");
            return handshake_error(400);
        }
    };
    let limit = state.settings().config.body_limit;
    let turn_state = headers
        .get(TURN_STATE_HEADER)
        .map(|value| String::from_utf8_lossy(value.as_bytes()).trim().to_owned())
        .unwrap_or_default();
    let mut response = upgrade
        .max_message_size(limit)
        .max_frame_size(limit)
        .on_upgrade(move |socket| session::run(state, client, socket));
    if !turn_state.is_empty()
        && let Ok(value) = HeaderValue::from_str(&turn_state)
    {
        response.headers_mut().insert(TURN_STATE_HEADER, value);
    }
    response
}

/// The handshake checks gorilla makes, in its order, as the status to fail
/// with (`Upgrader.Upgrade`).
fn check_handshake(method: &Method, headers: &HeaderMap) -> Result<(), u16> {
    if !token_list_contains(headers, "connection", "upgrade")
        || !token_list_contains(headers, "upgrade", "websocket")
    {
        return Err(400);
    }
    if method != Method::GET {
        return Err(405);
    }
    let key = headers
        .get("sec-websocket-key")
        .map_or(&b""[..], HeaderValue::as_bytes);
    if !token_list_contains(headers, "sec-websocket-version", "13") || !is_valid_challenge_key(key)
    {
        return Err(400);
    }
    Ok(())
}

/// A failed handshake as gorilla answers it: the status text, as plain
/// text (`returnError` and `http.Error`).
fn handshake_error(status: u16) -> Response {
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut response = (code, format!("{}\n", status_text(status))).into_response();
    let headers = response.headers_mut();
    headers.insert(SEC_WEBSOCKET_VERSION, HeaderValue::from_static("13"));
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
}

/// Whether a comma-separated token list in any `name` header holds `value`,
/// ignoring ASCII case (gorilla's `tokenListContainsValue`).
fn token_list_contains(headers: &HeaderMap, name: &str, value: &str) -> bool {
    headers.get_all(name).iter().any(|header| {
        let mut rest = header.as_bytes();
        loop {
            rest = skip_space(rest);
            let end = rest
                .iter()
                .position(|&b| !is_token_octet(b))
                .unwrap_or(rest.len());
            let (token, after) = rest.split_at(end);
            if token.is_empty() {
                return false;
            }
            let after = skip_space(after);
            if after.first().is_some_and(|&b| b != b',') {
                return false;
            }
            if token.eq_ignore_ascii_case(value.as_bytes()) {
                return true;
            }
            match after.split_first() {
                Some((_, next)) => rest = next,
                None => return false,
            }
        }
    })
}

/// `s` without leading spaces and tabs (gorilla's `skipSpace`).
fn skip_space(s: &[u8]) -> &[u8] {
    let start = s
        .iter()
        .position(|&b| b != b' ' && b != b'\t')
        .unwrap_or(s.len());
    &s[start..]
}

/// Whether `b` may be in an RFC 2616 token (gorilla's `isTokenOctet`).
fn is_token_octet(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// Whether `key` is base64 for 16 bytes (gorilla's `isValidChallengeKey`).
fn is_valid_challenge_key(key: &[u8]) -> bool {
    let is_base64 = |b: &u8| b.is_ascii_alphanumeric() || *b == b'+' || *b == b'/';
    key.len() == 24 && key[..22].iter().all(is_base64) && key[22..] == *b"=="
}
