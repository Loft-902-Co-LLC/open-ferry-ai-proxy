// Ported from CLIProxyAPI sdk/api/handlers/openai/openai_videos_handlers.go
// (rejectUnsupportedVideosModel, rejectUnsupportedNativeVideosModel,
// readVideosCreateRequest, readXAIVideosNativeRequest,
// bindVideoAuthIDAndModelFromPayload, bindVideoAuthID,
// contextWithVideoAuthBinding, modelWithVideoAuthBinding,
// writeVideosFailedError, VideosCreate, XAIVideosGenerations, XAIVideosEdits,
// XAIVideosExtensions, handleXAIVideosNativePost, XAIVideosRetrieve,
// VideosRetrieve, VideosContent, writeVideoContentFromURL,
// videoContentDownloadAuth, copyVideoContentHeaders,
// collectXAIVideosNative and collectXAIVideosCreate) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The video endpoints, all served by xAI's Grok Imagine Video:
//! - `POST /v1/videos`, `/v1/videos/generations`, `/v1/videos/edits` and
//!   `/v1/videos/extensions` take xAI's own request, and `GET
//!   /v1/videos/{request_id}` asks after a video, each answered with xAI's
//!   own body;
//! - `POST /openai/v1/videos` takes OpenAI's create, as JSON or a form, and
//!   `GET /openai/v1/videos/{video_id}` asks after a video, each answered
//!   with an OpenAI video object (see [`build`]);
//! - `GET /openai/v1/videos/{video_id}/content` sends the finished video.
//!
//! A video can only be asked after with the credential that made it, so
//! each call's credential is held for its video (see [`bindings`]), and
//! later calls about the video are pinned to it and use the model it was
//! made with. The finished video is fetched through that credential's
//! proxy, without its key ([`Dispatcher::download`]), and streamed to the
//! client as it comes: it is never written to disk or held whole.
//!
//! The native routes take xAI's models alone, the OpenAI ones `sora-2` and
//! its variants too. A refused OpenAI create is answered with a failed
//! video object; anything else refused gets an OpenAI error. A call that
//! takes a while is kept alive as on the other routes, but not on the
//! content route: a newline before the video would spoil it. Nothing here
//! is logged.
//!
//! Deviations from upstream:
//! - A body over the configured limit gets 413, and a body that fails to
//!   read gets a 400 in the shape the other routes use, where upstream's
//!   create answers either with a failed video.
//! - A body sent on to xAI with 128 or more arrays and objects inside one
//!   another gets a 400 (see [`crate::body::check_depth`]).
//! - The content route has no keep-alive while it asks xAI for the video:
//!   upstream's would put newlines before the video.
//! - The body of a download that fails is read up to 4 MiB, or the body
//!   limit if that is less; upstream reads it whole.
//! - The finished video is fetched by the xAI executor, through the proxy
//!   of the credential that made it, whichever provider that is.
//! - A path parameter is decoded before it is read, so `%2F` is a `/` in a
//!   video ID, where gin, which matches the decoded path, finds no route.
//!   One that isn't UTF-8 once decoded counts as missing: 400.
//!
//! [`Dispatcher::download`]: open_ferry_core::exec::Dispatcher::download

mod bindings;
mod build;
#[cfg(test)]
mod tests;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::body::Body;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::response::Response;
use bytes::Bytes;
use futures_util::StreamExt;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use open_ferry_core::exec::{Download, Downloaded, Format};
use open_ferry_translate::go;

pub(crate) use bindings::VideoBindings;

use self::bindings::Binding;
use self::build::{DEFAULT_MODEL, OPENAI_VIDEOS_PATH, SORA_MODEL};
use crate::body;
use crate::errors::{
    ErrorMessage, error_response, invalid_request, local_error, openai_error_response,
};
use crate::exec::{Call, ClientRequest};
use crate::json;
use crate::query;
use crate::state::AppState;
use crate::stream::{json_response, keep_alive};

/// The route the native retrieve is matched by, which the xAI executor
/// tells a retrieve from a create by.
const NATIVE_RETRIEVE_PATH: &str = "/v1/videos/{request_id}";

/// The longest download error body read, as the xAI executor reads it.
const MAX_DOWNLOAD_ERROR_BODY: usize = 4 << 20;

/// The headers of a finished video sent on with it
/// (`copyVideoContentHeaders`).
const CONTENT_HEADERS: [header::HeaderName; 6] = [
    header::CONTENT_TYPE,
    header::CONTENT_LENGTH,
    header::CONTENT_DISPOSITION,
    header::CACHE_CONTROL,
    header::ETAG,
    header::LAST_MODIFIED,
];

/// The settings a video request reads once, at its start.
#[derive(Clone, Copy)]
struct Settings {
    passthrough: bool,
    body_limit: usize,
    keepalive: Option<Duration>,
    /// How long a video's credential is held.
    ttl: Duration,
}

impl Settings {
    fn read(state: &AppState) -> Self {
        let settings = state.settings();
        Self {
            passthrough: settings.config.passthrough_headers,
            body_limit: settings.config.body_limit,
            keepalive: settings.config.nonstream_keepalive,
            ttl: settings.config.video_auth_ttl,
        }
    }
}

/// The credential a call was last given (upstream's
/// `WithSelectedAuthIDCallback`).
#[derive(Clone, Default)]
struct Selected(Arc<Mutex<String>>);

impl Selected {
    /// Has `call` report its credential here.
    fn on(call: &mut Call) -> Self {
        let selected = Self::default();
        let slot = Arc::clone(&selected.0);
        call.options.metadata.selected_auth = Some(Arc::new(move |auth_id: &str| {
            *slot.lock().unwrap_or_else(PoisonError::into_inner) = auth_id.to_owned();
        }));
        selected
    }

    fn get(&self) -> String {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Holds `video_id` as made by `auth_id` with `model`
/// (`bindVideoAuthID`).
fn bind(state: &AppState, video_id: &str, auth_id: &str, model: &str, ttl: Duration) {
    state
        .video_bindings()
        .set(video_id, auth_id, build::routing_model(model), ttl);
}

/// Pins `call` to the credential that made its video, if one is held
/// (`contextWithVideoAuthBinding`).
fn pin(call: &mut Call, binding: Option<&Binding>) {
    if let Some(binding) = binding {
        call.options.metadata.pinned_auth_id = Some(binding.auth_id.clone());
    }
}

/// The model to call for a held video: the one it was made with, else
/// `fallback` (`modelWithVideoAuthBinding`).
fn bound_model(binding: Option<&Binding>, fallback: &str) -> String {
    binding
        .map(|binding| binding.model.trim())
        .filter(|model| !model.is_empty())
        .unwrap_or(fallback)
        .to_owned()
}

/// The body that asks after `video_id`.
fn retrieve_payload(video_id: &str) -> Bytes {
    Bytes::from(json::set_str(b"{}", "request_id", video_id))
}

/// A path parameter, trimmed, or empty when it can't be read.
fn param(param: Result<Path<String>, PathRejection>) -> String {
    param
        .map(|Path(value)| value.trim().to_owned())
        .unwrap_or_default()
}

/// The model a body names, trimmed, else [`DEFAULT_MODEL`].
fn body_model(raw: &[u8]) -> String {
    match json::str_at(raw, "model").trim() {
        "" => DEFAULT_MODEL.to_owned(),
        model => model.to_owned(),
    }
}

/// A failed video object for a refused create (`writeVideosFailedError`).
fn failed(model: &str, message: &str) -> Response {
    error_response(
        400,
        HeaderMap::new(),
        Bytes::from(build::failed_response(
            model,
            "invalid_request_error",
            message,
        )),
        "application/json",
    )
}

/// `POST /openai/v1/videos` (`VideosCreate`).
pub(crate) async fn create(
    State(state): State<AppState>,
    client: ClientRequest,
    body: Body,
) -> Response {
    let settings = Settings::read(&state);
    let raw = if build::is_form(&build::content_type(&client.headers)) {
        match body::read_raw(&client.headers, body, settings.body_limit).await {
            Ok(raw) => build::form_request(&client.headers, &raw),
            Err(response) => return response,
        }
    } else {
        let raw = match body::read_decoded(&client.headers, body, settings.body_limit).await {
            Ok(raw) => raw,
            Err(response) => return response,
        };
        if !json::valid(&raw) {
            return failed(DEFAULT_MODEL, "Invalid request: body must be valid JSON");
        }
        raw.to_vec()
    };

    let model = body_model(&raw);
    if !build::is_supported_model(&model) {
        let path = match client.path.trim() {
            "" => OPENAI_VIDEOS_PATH,
            path => path,
        };
        return failed(
            &model,
            &format!("Model {model} is not supported on {path}. Use {SORA_MODEL}."),
        );
    }
    let (request, meta) = match build::create_request(&raw, &model) {
        Ok(built) => built,
        Err(err) => {
            return failed(
                build::canonical_model(&model),
                &format!("Invalid request: {err}"),
            );
        }
    };

    let mut call = match Call::new(
        &state,
        &client,
        Format::OPENAI_VIDEO,
        meta.routing_model,
        Bytes::from(request),
        "",
        false,
    ) {
        Ok(call) => call,
        Err(error) => return openai_error_response(&error, settings.passthrough),
    };
    let selected = Selected::on(&mut call);
    keep_alive(settings.keepalive, call.execute(), move |result| {
        let reply = match result {
            Ok(reply) => reply,
            Err(error) => return openai_error_response(&error, settings.passthrough),
        };
        let out = match build::create_response(&reply.body, &meta) {
            Ok(out) => out,
            Err(text) => {
                return openai_error_response(&ErrorMessage::new(502, text), settings.passthrough);
            }
        };
        bind(
            &state,
            &build::video_id(&out),
            &selected.get(),
            meta.routing_model,
            settings.ttl,
        );
        json_response(&reply.headers, Bytes::from(out))
    })
    .await
}

/// `POST /v1/videos`, `/v1/videos/generations`, `/v1/videos/edits` and
/// `/v1/videos/extensions` (`handleXAIVideosNativePost`).
pub(crate) async fn native_create(
    State(state): State<AppState>,
    client: ClientRequest,
    body: Body,
) -> Response {
    let settings = Settings::read(&state);
    let raw = match body::read_decoded(&client.headers, body, settings.body_limit).await {
        Ok(raw) => raw,
        Err(response) => return response,
    };
    if !json::valid(&raw) {
        return invalid_request(400, "body must be valid JSON");
    }
    let model = body_model(&raw);
    if !build::is_xai_model(&model) {
        return local_error(
            400,
            &format!(
                "Model {model} is not supported on /v1/videos/generations, /v1/videos/edits, \
                 or /v1/videos/extensions. Use {DEFAULT_MODEL}."
            ),
            "invalid_request_error",
        );
    }
    let raw = json::set_str(&raw, "model", build::canonical_model(&model));
    native(
        state,
        client,
        settings,
        Bytes::from(raw),
        build::routing_model(&model),
        None,
    )
    .await
}

/// `GET /v1/videos/{request_id}` (`XAIVideosRetrieve`).
pub(crate) async fn native_retrieve(
    State(state): State<AppState>,
    client: ClientRequest,
    request_id: Result<Path<String>, PathRejection>,
) -> Response {
    native_retrieve_id(state, client, &param(request_id)).await
}

/// `GET /v1/videos/generations`: the native retrieve of a video by that
/// ID, as gin matches it.
pub(crate) async fn native_retrieve_generations(
    State(state): State<AppState>,
    client: ClientRequest,
) -> Response {
    native_retrieve_fixed(state, client, "generations").await
}

/// `GET /v1/videos/edits`, as [`native_retrieve_generations`].
pub(crate) async fn native_retrieve_edits(
    State(state): State<AppState>,
    client: ClientRequest,
) -> Response {
    native_retrieve_fixed(state, client, "edits").await
}

/// `GET /v1/videos/extensions`, as [`native_retrieve_generations`].
pub(crate) async fn native_retrieve_extensions(
    State(state): State<AppState>,
    client: ClientRequest,
) -> Response {
    native_retrieve_fixed(state, client, "extensions").await
}

/// The native retrieve of the video `request_id` on the route of a fixed
/// path: gin keeps a tree of routes for each method, so a `GET` of a path
/// that only has a `POST` route is matched by the retrieve's. The call is
/// made as from the retrieve's route, which the executor goes by.
async fn native_retrieve_fixed(
    state: AppState,
    mut client: ClientRequest,
    request_id: &str,
) -> Response {
    NATIVE_RETRIEVE_PATH.clone_into(&mut client.path);
    native_retrieve_id(state, client, request_id).await
}

async fn native_retrieve_id(state: AppState, client: ClientRequest, request_id: &str) -> Response {
    let settings = Settings::read(&state);
    if request_id.is_empty() {
        return local_error(
            400,
            "Invalid request: request_id is required",
            "invalid_request_error",
        );
    }
    native(
        state,
        client,
        settings,
        retrieve_payload(request_id),
        DEFAULT_MODEL,
        Some(request_id),
    )
    .await
}

/// Calls xAI with a native `raw` body for `model` and answers with xAI's
/// body (`collectXAIVideosNative`). A create holds the video its answer
/// names; a retrieve, of `video_id`, is pinned to the video's credential
/// and model, and holds them again.
async fn native(
    state: AppState,
    client: ClientRequest,
    settings: Settings,
    raw: Bytes,
    model: &str,
    video_id: Option<&str>,
) -> Response {
    let binding = video_id.and_then(|id| state.video_bindings().get(id));
    let model = bound_model(binding.as_ref(), model);
    let mut call = match Call::new(
        &state,
        &client,
        Format::OPENAI_VIDEO,
        &model,
        raw,
        "",
        false,
    ) {
        Ok(call) => call,
        Err(error) => return openai_error_response(&error, settings.passthrough),
    };
    pin(&mut call, binding.as_ref());
    let selected = Selected::on(&mut call);
    let video_id = video_id.map(str::to_owned);
    keep_alive(settings.keepalive, call.execute(), move |result| {
        let reply = match result {
            Ok(reply) => reply,
            Err(error) => return openai_error_response(&error, settings.passthrough),
        };
        let video_id = video_id.unwrap_or_else(|| build::video_id(&reply.body));
        bind(&state, &video_id, &selected.get(), &model, settings.ttl);
        json_response(&reply.headers, reply.body)
    })
    .await
}

/// `GET /openai/v1/videos/{video_id}` (`VideosRetrieve`).
pub(crate) async fn retrieve(
    State(state): State<AppState>,
    client: ClientRequest,
    video_id: Result<Path<String>, PathRejection>,
) -> Response {
    let settings = Settings::read(&state);
    let video_id = param(video_id);
    if video_id.is_empty() {
        return local_error(
            400,
            "Invalid request: video_id is required",
            "invalid_request_error",
        );
    }
    let binding = state.video_bindings().get(&video_id);
    let model = bound_model(binding.as_ref(), DEFAULT_MODEL);
    let mut call = match Call::new(
        &state,
        &client,
        Format::OPENAI_VIDEO,
        &model,
        retrieve_payload(&video_id),
        "",
        false,
    ) {
        Ok(call) => call,
        Err(error) => return openai_error_response(&error, settings.passthrough),
    };
    pin(&mut call, binding.as_ref());
    let selected = Selected::on(&mut call);
    keep_alive(settings.keepalive, call.execute(), move |result| {
        let reply = match result {
            Ok(reply) => reply,
            Err(error) => return openai_error_response(&error, settings.passthrough),
        };
        let out = build::retrieve_response(&video_id, &reply.body, SORA_MODEL);
        bind(&state, &video_id, &selected.get(), &model, settings.ttl);
        json_response(&reply.headers, Bytes::from(out))
    })
    .await
}

/// `GET /openai/v1/videos/{video_id}/content` (`VideosContent`): asks xAI
/// for the video with the credential that made it, then streams the file
/// from the URL xAI gives.
pub(crate) async fn content(
    State(state): State<AppState>,
    client: ClientRequest,
    video_id: Result<Path<String>, PathRejection>,
) -> Response {
    let settings = Settings::read(&state);
    let video_id = param(video_id);
    if video_id.is_empty() {
        return local_error(
            400,
            "Invalid request: video_id is required",
            "invalid_request_error",
        );
    }
    let variant = match query::first(&client.query, "variant").map(str::trim) {
        None | Some("") => "video",
        Some(variant) => variant,
    };
    if variant != "video" {
        return local_error(
            400,
            &format!(
                "Invalid request: variant {} is not available for xAI video downloads",
                go::quote(variant)
            ),
            "invalid_request_error",
        );
    }

    let binding = state.video_bindings().get(&video_id);
    let model = bound_model(binding.as_ref(), DEFAULT_MODEL);
    let mut call = match Call::new(
        &state,
        &client,
        Format::OPENAI_VIDEO,
        &model,
        retrieve_payload(&video_id),
        "",
        false,
    ) {
        Ok(call) => call,
        Err(error) => return openai_error_response(&error, settings.passthrough),
    };
    pin(&mut call, binding.as_ref());
    let selected = Selected::on(&mut call);
    let reply = match call.execute().await {
        Ok(reply) => reply,
        Err(error) => return openai_error_response(&error, settings.passthrough),
    };
    bind(&state, &video_id, &selected.get(), &model, settings.ttl);
    let url = match build::content_url(&reply.body) {
        Ok(url) => url,
        Err(text) => {
            return openai_error_response(&ErrorMessage::new(502, text), settings.passthrough);
        }
    };

    // `videoContentDownloadAuth`: the credential held for the video now.
    let auth_id = state
        .video_bindings()
        .get(&video_id)
        .map(|binding| binding.auth_id)
        .unwrap_or_default();
    let dispatcher = state.dispatcher_arc();
    let download = Download {
        provider: "xai".to_owned(),
        auth_id,
        url,
    };
    match dispatcher.download(download).await {
        Ok(downloaded) if (200..300).contains(&downloaded.status) => video_response(downloaded),
        Ok(downloaded) => {
            let limit = MAX_DOWNLOAD_ERROR_BODY.min(settings.body_limit);
            let message = download_error(downloaded, limit).await;
            openai_error_response(&message, settings.passthrough)
        }
        Err(error) => openai_error_response(&ErrorMessage::from_exec(error), settings.passthrough),
    }
}

/// The finished video, streamed with its status and the headers
/// [`CONTENT_HEADERS`] names (the success of `writeVideoContentFromURL`).
fn video_response(downloaded: Downloaded) -> Response {
    let mut response = Response::new(Body::from_stream(downloaded.body));
    *response.status_mut() = StatusCode::from_u16(downloaded.status).unwrap_or(StatusCode::OK);
    let headers = response.headers_mut();
    for name in CONTENT_HEADERS {
        if let Some(value) = downloaded
            .headers
            .get(&name)
            .filter(|value| !value.is_empty())
        {
            headers.insert(name, value.clone());
        }
    }
    if !headers.contains_key(header::CONTENT_TYPE) {
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
    }
    response
}

/// The error for a download that didn't succeed, with its body, trimmed,
/// read up to `limit` bytes, else its status line (the failure of
/// `writeVideoContentFromURL`).
async fn download_error(downloaded: Downloaded, limit: usize) -> ErrorMessage {
    let mut body = Vec::new();
    let mut chunks = downloaded.body;
    while body.len() < limit {
        let Some(Ok(chunk)) = chunks.next().await else {
            break;
        };
        let room = limit.saturating_sub(body.len());
        body.extend_from_slice(chunk.get(..room).unwrap_or(&chunk));
    }
    let text = String::from_utf8_lossy(&body);
    let message = match text.trim() {
        "" => format!("video content download failed: {}", downloaded.status_text),
        text => format!("video content download failed: {text}"),
    };
    ErrorMessage::new(downloaded.status, message)
}
