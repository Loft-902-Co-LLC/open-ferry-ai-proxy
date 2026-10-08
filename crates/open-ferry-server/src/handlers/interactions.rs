// Ported from CLIProxyAPI sdk/api/handlers/gemini/interactions_handlers.go
// (parseInteractionsRequestTarget, prepareInteractionsExecutionTarget,
// normalizeGeminiModelResourceName, buildInteractionsExecutionRequest,
// Interactions, handleInteractionsNonStream, handleInteractionsStream and
// forwardInteractionsStream), and the forced-provider branch of
// providersForExecution in sdk/api/handlers/handlers_routing.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The Gemini Interactions API: `POST /v1beta/interactions`.
//!
//! The body must be JSON naming exactly one of `model` and `agent`, and its
//! `stream`, when there is one, must be a boolean. Anything else gets a 400
//! `invalid_request_error`. A model named as a resource, `models/<name>`, is
//! called by its bare name, which is written into the body too.
//!
//! The call goes in and comes back in the Interactions format. A model is
//! routed as on the other routes, with `gemini-interactions` tried first
//! among its providers (see [`crate::entry_protocol`]); the others take the
//! request through the translators. An `agent` is no model the catalog
//! knows: it goes to `gemini-interactions` alone, and the credential is
//! picked as for `gemini-2.5-flash`, while the executor gets the agent's
//! name.
//!
//! A stream is SSE. A payload that is already an `event:` or `data:` line
//! goes on as it is, any other gets `data: `, and each ends with a blank
//! line. An error before the first payload is an OpenAI error body, as on
//! the other routes; one partway through ends the stream with
//! `event: error` and that body. Nothing marks a stream that ends well.
//!
//! Deviations from upstream:
//! - A body over the configured limit gets 413, and the message for a body
//!   that fails to read starts with `Invalid request: `.
//! - There is no model router or plugin executor to send an `agent`
//!   elsewhere, so upstream's 400 "agent is only supported for native
//!   interactions execution" comes from the manager, when the providers a
//!   call is given leave out the forced one.
//! - The 500 upstream gives when the response can't be flushed can't happen.
//! - A body with 128 or more arrays and objects inside one another gets a
//!   400, for a model and for an `agent` alike (see [`body::check_depth`]).
//!   Upstream forwards a body of any depth.

#[cfg(test)]
mod tests;

use axum::body::Body;
use axum::extract::State;
use axum::response::Response;
use bytes::{Bytes, BytesMut};
use futures_util::{StreamExt, stream};
use open_ferry_core::exec::Format;
use open_ferry_translate::go;

use crate::body;
use crate::entry_protocol::GEMINI_INTERACTIONS;
use crate::errors::{ErrorMessage, local_error, openai_body, openai_error_response};
use crate::exec::{Call, ClientRequest, Started};
use crate::json;
use crate::routing::{self, Route};
use crate::state::AppState;
use crate::stream::{Peeked, StreamWriter, forward, json_response, keep_alive, peek, sse_response};

/// The model an `agent` call's credential is picked by (upstream's
/// `interactionsAgentAuthSelectionModel`).
const AGENT_AUTH_SELECTION_MODEL: &str = "gemini-2.5-flash";

/// The prefix of a Gemini model resource name.
const MODEL_RESOURCE_PREFIX: &str = "models/";

/// What a request asks for (upstream's `interactionsRequestTarget`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Target {
    /// The trimmed `model`, or empty.
    model: String,
    /// The trimmed `agent`, or empty.
    agent: String,
    stream: bool,
}

/// `POST /v1beta/interactions` (upstream's `Interactions`).
pub(crate) async fn interactions(
    State(state): State<AppState>,
    client: ClientRequest,
    body: Body,
) -> Response {
    let settings = state.settings();
    let passthrough = settings.config.passthrough_headers;
    let limit = settings.config.body_limit;
    let nonstream_keepalive = settings.config.nonstream_keepalive;
    let keepalive = settings.config.streaming.keepalive;
    drop(settings);
    let raw = match body::read_raw(&client.headers, body, limit).await {
        Ok(raw) => raw,
        Err(response) => return response,
    };
    let target = match parse_target(&raw) {
        Ok(target) => target,
        Err(message) => return local_error(400, message, "invalid_request_error"),
    };
    let (model, raw) = prepare_target(raw, &target);
    let call = new_call(&state, &client, &target, &model, raw);

    if target.stream {
        let started = match call {
            Ok(call) => call.stream().await,
            Err(error) => Started::failed(error),
        };
        return match peek(started.items).await {
            Peeked::Failed(error) => openai_error_response(&error, passthrough),
            Peeked::Closed => sse_response(&started.headers, Body::empty()),
            Peeked::First(first, rest) => {
                let items = stream::iter([Ok(first)]).chain(rest).boxed();
                let body = forward(items, InteractionsWriter, keepalive);
                sse_response(&started.headers, body)
            }
        };
    }

    let call = match call {
        Ok(call) => call,
        Err(error) => return openai_error_response(&error, passthrough),
    };
    keep_alive(
        nonstream_keepalive,
        call.execute(),
        move |result| match result {
            Ok(reply) => json_response(&reply.headers, reply.body),
            Err(error) => openai_error_response(&error, passthrough),
        },
    )
    .await
}

/// Reads what a body asks for (upstream's `parseInteractionsRequestTarget`),
/// or the message of the 400 it gets.
fn parse_target(raw: &[u8]) -> Result<Target, &'static str> {
    if !json::gjson_valid(raw) {
        return Err("invalid JSON body");
    }
    let model = json::str_at(raw, "model").trim().to_owned();
    let agent = json::str_at(raw, "agent").trim().to_owned();
    if model.is_empty() == agent.is_empty() {
        return Err("request requires exactly one of model or agent");
    }
    let stream = match json::get(raw, "stream") {
        None => false,
        Some(node) => match node.raw.first() {
            Some(b't') => true,
            Some(b'f') => false,
            _ => return Err("stream must be a boolean"),
        },
    };
    Ok(Target {
        model,
        agent,
        stream,
    })
}

/// The model or agent to call, and the body with a model resource name made
/// bare (upstream's `prepareInteractionsExecutionTarget`). Where the body
/// can't be edited it goes as it came.
fn prepare_target(raw: Bytes, target: &Target) -> (String, Bytes) {
    if !target.agent.is_empty() {
        return (target.agent.clone(), raw);
    }
    let model = normalize_model_resource_name(&target.model);
    if model == target.model {
        return (model, raw);
    }
    match json::try_set_str(&raw, "model", &model) {
        Some(updated) => (model, Bytes::from(updated)),
        None => (model, raw),
    }
}

/// `model`, trimmed, without a leading `models/` that has a name after it
/// (upstream's `normalizeGeminiModelResourceName`).
fn normalize_model_resource_name(model: &str) -> String {
    let model = model.trim();
    match model.strip_prefix(MODEL_RESOURCE_PREFIX) {
        Some(name) if !name.is_empty() => name.to_owned(),
        _ => model.to_owned(),
    }
}

/// The call for `target` (upstream's `buildInteractionsExecutionRequest`):
/// a model is routed as on the other routes, and an `agent` is forced to
/// `gemini-interactions`, its credential picked by
/// [`AGENT_AUTH_SELECTION_MODEL`].
fn new_call(
    state: &AppState,
    client: &ClientRequest,
    target: &Target,
    model: &str,
    raw: Bytes,
) -> Result<Call, ErrorMessage> {
    let format = Format::INTERACTIONS;
    if target.agent.is_empty() {
        return Call::new(
            state,
            client,
            format,
            model,
            raw,
            &client.alt,
            target.stream,
        );
    }
    // `Call::routed` doesn't look at the depth as `Call::new` does.
    body::check_depth(&raw)?;
    let route = forced_route(model)?;
    let mut call = Call::routed(
        state,
        client,
        format,
        model,
        route,
        raw,
        &client.alt,
        target.stream,
    );
    let metadata = &mut call.options.metadata;
    metadata.forced_provider = Some(GEMINI_INTERACTIONS.to_owned());
    metadata.auth_selection_model = Some(AGENT_AUTH_SELECTION_MODEL.to_owned());
    Ok(call)
}

/// Where an `agent` call goes: to `gemini-interactions` alone, with the
/// trimmed name as its model, unless the name is an image-only or
/// speech-only model
/// (upstream's `providersForExecution` with a forced provider). The entry
/// adjustment leaves a lone `gemini-interactions` as it is, so isn't made.
fn forced_route(model: &str) -> Result<Route, ErrorMessage> {
    let model = model.trim();
    routing::check_image_only(model)?;
    routing::check_speech_only(model)?;
    Ok(Route {
        providers: vec![GEMINI_INTERACTIONS.to_owned()],
        model: model.to_owned(),
    })
}

/// Writes an Interactions stream (upstream's `forwardInteractionsStream`).
struct InteractionsWriter;

impl StreamWriter for InteractionsWriter {
    fn write_chunk(&mut self, chunk: Bytes, out: &mut BytesMut) {
        if chunk.is_empty() {
            return;
        }
        let trimmed = go::trim_space(&chunk);
        if !trimmed.starts_with(b"event:") && !trimmed.starts_with(b"data:") {
            out.extend_from_slice(b"data: ");
        }
        out.extend_from_slice(&chunk);
        if !chunk.ends_with(b"\n\n") {
            out.extend_from_slice(b"\n\n");
        }
    }

    fn write_terminal_error(&mut self, error: &ErrorMessage, out: &mut BytesMut) {
        let body = openai_body(error.http_status(), &error.text, false);
        out.extend_from_slice(b"event: error\ndata: ");
        out.extend_from_slice(body.as_bytes());
        out.extend_from_slice(b"\n\n");
    }
}
