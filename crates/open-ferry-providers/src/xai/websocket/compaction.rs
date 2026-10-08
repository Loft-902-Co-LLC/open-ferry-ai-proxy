// Ported from CLIProxyAPI internal/runtime/executor/xai_websockets_executor.go
// (executeCompactionTriggerFromWebsocketContext) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A `compaction_trigger` on the WebSocket, answered over HTTP.
//!
//! xAI's WebSocket can't compact, so the session's history goes to
//! `/responses/compact` as the HTTP call sends it (see
//! [`crate::xai::compact`]): the session's transcript, else the request's
//! own input without the trigger, else the previous response alone. The
//! compaction then becomes the session's transcript, its response ID maps
//! to no response of xAI's (so the next call naming it sends the transcript
//! instead), and the client gets the six events of one response holding
//! the compaction item.
//!
//! A request outside any session, or whose session has nothing to compact,
//! fails with a 400; an answer without a compaction with encrypted content
//! fails with a 502.
//!
//! Deviations from upstream:
//! - The events the client gets have the secrets the compact call sent
//!   redacted if they are of eight bytes or more, as every client error is
//!   (see `Policy::Client` in the crate's `redact` module); upstream passes
//!   them on. The session keeps the compaction as xAI sent it.
//! - The log line hides the credential's secrets and its URL's, however
//!   short, from the session and the credential's ID; upstream logs them as
//!   they are.

use std::time::SystemTime;

use bytes::Bytes;
use futures_util::StreamExt as _;
use http::HeaderMap;
use http::header::{self, HeaderValue};
use open_ferry_core::auth::Auth;
use open_ferry_core::exec::{ExecError, Options, Request, StreamResponse};
use open_ferry_core::observe::AttemptKind;
use serde_json::Value;

use super::ids::Mapper;
use super::message::{compaction_payload, validate_compaction};
use crate::codex::request::parse_object;
use crate::codex::terminal::StatusError;
use crate::json::{get, set, str_at};
use crate::observe_send;
use crate::redact::Policy;
use crate::xai::XaiExecutor;
use crate::xai::compact::{self, COMPACTION_TRIGGER, remove_input_items_by_type};
use crate::xai::request::base_url;

/// Compacts the session's history for a `compaction_trigger` request and
/// streams the compaction back
/// (`executeCompactionTriggerFromWebsocketContext`). `mapper` is the
/// request's session's, if it has one.
pub(super) async fn fallback(
    executor: &XaiExecutor,
    auth: &Auth,
    request: &Request,
    options: &Options,
    session_id: &str,
    mapper: Option<Mapper>,
) -> Result<StreamResponse, ExecError> {
    let Some(mapper) = mapper else {
        return Err(
            StatusError::new(400, "xai websocket compaction context is unavailable").into(),
        );
    };
    let client = parse_object(&request.payload);
    let transcript = mapper.state().transcript();
    let mut keep_previous_response_id = false;
    let (payload, input_items) = if transcript.is_empty() {
        let mut filtered = client.clone();
        remove_input_items_by_type(&mut filtered, COMPACTION_TRIGGER);
        match get(&filtered, "input") {
            Some(Value::Array(input)) if !input.is_empty() => {
                let input = input.clone();
                let count = input.len();
                (compaction_payload(&filtered, input), count)
            }
            _ => {
                let mut previous = mapper.upstream_previous_id().to_owned();
                if previous.is_empty() {
                    previous = str_at(&client, "previous_response_id").trim().to_owned();
                }
                if previous.is_empty() {
                    return Err(
                        StatusError::new(400, "xai websocket compaction context is empty").into(),
                    );
                }
                keep_previous_response_id = true;
                set(&mut filtered, "previous_response_id", Value::from(previous));
                (filtered, 0)
            }
        }
    } else {
        let count = transcript.len();
        (compaction_payload(&client, transcript), count)
    };
    // The log line hides what the compact call will send, however short.
    let logged = observe_send::secrets(
        base_url(auth),
        &HeaderMap::new(),
        &executor.proxy_for(auth),
        auth,
    );
    tracing::info!(
        "xai websockets: compact fallback session={} auth={} input_items={input_items} keep_previous_response_id={keep_previous_response_id}",
        logged.str(session_id, Policy::Disk),
        logged.str(auth.id.trim(), Policy::Disk),
    );

    let compact_request = Request {
        model: request.model.clone(),
        payload: Bytes::from(payload.to_string()),
    };
    let (prepared, data, mut headers, secrets) = executor
        .compact_request(auth, &compact_request, options, AttemptKind::Stream)
        .await?;
    let (response_id, item) = validate_compaction(&data, SystemTime::now())?;
    mapper.state().replace_transcript(vec![item]);
    mapper.state().map_downstream_to_upstream(&response_id, "");

    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    // Whole, before the events are made from it; the session keeps it as it
    // came.
    let data = secrets.bytes(&data, Policy::Client);
    let chunks = compact::trigger_stream_chunks(&prepared, &data, SystemTime::now());
    Ok(StreamResponse {
        headers,
        chunks: futures_util::stream::iter(chunks.into_iter().map(|chunk| Ok(Bytes::from(chunk))))
            .boxed(),
    })
}

/// Whether the request asks for a compaction (`xaiInputHasItemType`).
pub(super) fn requested(payload: &[u8]) -> bool {
    compact::input_has_item_type(payload, COMPACTION_TRIGGER)
}
