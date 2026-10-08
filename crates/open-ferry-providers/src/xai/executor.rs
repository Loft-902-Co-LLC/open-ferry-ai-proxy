// Ported from CLIProxyAPI internal/runtime/executor/xai_executor.go
// (XAIExecutor, Identifier), xai_executor_execute.go (Execute,
// executeCompact, executeCompactRequest, executeCompactionTriggerStream),
// xai_executor_stream.go (ExecuteStream) and xai_executor_tokens.go
// (CountTokens) (v8.0.15, MIT). The image calls are in the `images` module
// and the video calls in the `videos` module.
// https://github.com/router-for-me/CLIProxyAPI

//! [`XaiExecutor`], which calls Grok's Responses API with an API key.
//!
//! A call goes to `<base>/responses`, where `<base>` is the credential's
//! `base_url` attribute (or metadata) or xAI's API, with the credential's
//! `api_key` as its bearer token. xAI always streams: a non-streaming call
//! reads the stream to its `response.completed` (or `response.incomplete`)
//! event and translates that. `responses/compact` goes to
//! `<base>/responses/compact`, as OpenAI Responses, and answers with JSON;
//! a streaming request whose input holds a `compaction_trigger` item is
//! compacted the same way, and its result streamed back as the six events
//! of one response holding the compaction item.
//!
//! Each event of xAI's stream is undone before it is translated (see the
//! `response` and `reasoning` modules): reasoning text becomes a reasoning
//! summary, namespace tools flattened for xAI get their namespaces back, a
//! client's `web_search` function its name, and X search's own tool calls
//! are dropped. The `apply_patch` bridge then restores the client's custom
//! `apply_patch` tool (see [`crate::apply_patch_responses`]).
//!
//! A call from the image endpoints goes to xAI's Images API (see the
//! `images` module), and one from the video endpoints to xAI's video API
//! (see the `videos` module), as does the download of a finished video. A
//! streaming or compact image or video call is refused with a 400 before
//! anything is sent.
//!
//! Deviations from upstream:
//! - Requests go through `reqwest` with rustls, one shared client per proxy;
//!   upstream builds a client per request.
//! - A non-streaming call reads the stream a line at a time, up to 50 MiB a
//!   line, where upstream reads the whole body first, so a read error after
//!   the terminal event is ignored. Error bodies are read up to 4 MiB and
//!   compact bodies up to 50 MiB.
//! - A dropped call or stream stops at once; upstream checks its context.
//! - A secret the request sent is redacted from every answer that quotes it,
//!   an error body, a `response.failed` or `error` event, a compact answer's
//!   `error` object or what a model says in a successful answer, if it is of
//!   eight bytes or more, as every client error is (see `Policy::Client` in
//!   the crate's `redact` module): the credential headers after the custom
//!   ones, each cookie, the URL's credentials, the proxy's password and the
//!   credential's key. A compact answer is redacted whole, before it is
//!   translated or made into a compaction trigger's events, and a stream, a
//!   non-streaming call's included, a line at a time, before the line is
//!   read. Upstream passes all of it on as it came. The call's taps read the
//!   answer as it came.
//! - Usage reporting and request logging are left to the call's taps (see
//!   the crate's `observe_send` module), and payload rules to
//!   [`crate::payload`].
//! - Only API keys: no xAI sign-in, refresh or Grok CLI chat proxy, so
//!   refresh returns the credential as it is. One executor serves HTTP
//!   and the WebSocket (see the `websocket` module); upstream wraps an
//!   HTTP and a WebSocket executor in an `XAIAutoExecutor`.
//! - Reasoning replay keeps a session's last completed turn in memory, as
//!   upstream does without Home mode, and only for a client-named session
//!   (see the `replay` module).
//! - The URL is read as a WHATWG URL, and one with an ASCII control
//!   character fails before anything is sent, as Codex's does.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;
use futures_util::future::BoxFuture;
use futures_util::{FutureExt as _, StreamExt as _};
use http::header::{self, HeaderValue};
use http::{HeaderMap, Method};
use open_ferry_core::auth::Auth;
use open_ferry_core::config::Config;
use open_ferry_core::exec::{
    Downloaded, ErrorKind, ExecError, Format, HttpCall, HttpReply, Options, Request, Response,
    StreamResponse,
};
use open_ferry_core::executor::ProviderExecutor;
use open_ferry_core::models::ModelCatalog;
use open_ferry_core::observe::AttemptKind;
use open_ferry_translate::go::trim_space;
use open_ferry_translate::registry::{Registry, ResponseContext};
use serde_json::Value;

use super::compact;
use super::errors;
use super::reasoning;
use super::replay;
use super::request::{
    PROVIDER, Prepared, build_headers, endpoint, is_media_request, media_refused, prepare,
};
use super::response::{
    NamespaceRestorer, XSearchFilter, patch_completed_output, restore_client_web_search_name,
};
use super::stream::{self, StreamSetup};
use super::tokens;
use super::websocket;
use crate::codex::client::{Clients, error_chain, read_body, read_body_prefix};
use crate::codex::request::{Context, refuse_control_characters};
use crate::codex::stream::{LineReader, MAX_LINE};
use crate::codex::terminal::{APPLY_PATCH_ERROR_MESSAGE, OutputItems, StatusError};
use crate::codex::usage::ensure_responses_usage_details;
use crate::json::str_at;
use crate::observe_send::{self, Attempt, BodyTap};
use crate::redact::{Policy, Secrets};

/// The `alt` of a `/responses/compact` call.
const COMPACT_ALT: &str = "responses/compact";
/// How much of an error body is read.
const MAX_ERROR_BODY: usize = 4 << 20;
/// What a non-streaming call that never saw its terminal event fails with.
pub(crate) const DISCONNECTED_MESSAGE: &str =
    "xai stream error: stream disconnected before response.completed or response.incomplete";

/// Calls Grok's Responses API with an API key (upstream's `XAIExecutor`).
pub struct XaiExecutor {
    clients: Clients,
    config: Option<Arc<Config>>,
    models: Option<Arc<dyn ModelCatalog>>,
    pub(super) websockets: websocket::Sessions,
}

impl XaiExecutor {
    /// An executor whose credentials without a `proxy_url` go through
    /// `global_proxy_url`: empty for the environment's proxy, `direct` or
    /// `none` for no proxy, or an `http` or `https` proxy URL.
    pub fn new(global_proxy_url: impl Into<String>) -> Self {
        Self {
            clients: Clients::new(global_proxy_url).for_provider(PROVIDER),
            config: None,
            models: None,
            websockets: websocket::Sessions::new(),
        }
    }

    /// Follows `config` where upstream's executor reads its config.
    pub fn with_config(mut self, config: Arc<Config>) -> Self {
        self.config = Some(config);
        self
    }

    /// Looks up the models the proxy serves in `models`.
    pub fn with_models(mut self, models: Arc<dyn ModelCatalog>) -> Self {
        self.models = Some(models);
        self
    }

    /// The proxy setting `auth`'s calls go through (see [`Self::new`]).
    pub(super) fn proxy_for(&self, auth: &Auth) -> String {
        self.clients.effective_proxy(&auth.proxy_url).to_owned()
    }

    /// What a call with `auth` is prepared with.
    pub(super) fn context<'a>(&'a self, auth: &'a Auth) -> Context<'a> {
        Context {
            auth: Some(auth),
            config: self.config.as_deref(),
            models: self.models.as_deref(),
        }
    }

    /// Posts `body` and returns xAI's answer, whatever its status, with the
    /// secrets the request sent (see [`observe_send::secrets`]).
    async fn send(
        &self,
        auth: &Auth,
        url: &str,
        headers: HeaderMap,
        body: &Value,
        attempt: Attempt<'_>,
    ) -> Result<(reqwest::Response, Secrets), ExecError> {
        refuse_control_characters(url)?;
        let body = Bytes::from(body.to_string());
        let secrets = observe_send::secrets(url, &headers, &self.proxy_for(auth), auth);
        let tap = attempt.observation.map(|observation| {
            observe_send::announce(
                observation,
                &attempt.request(&Method::POST, url, &headers, &body, &secrets),
            )
        });
        let mut response = self
            .clients
            .get(&auth.proxy_url)
            .post(url)
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(|error| {
                ExecError::new(
                    ErrorKind::Upstream,
                    secrets.text(error_chain(&error.without_url()), Policy::Client),
                )
            })?;
        observe_send::response(tap, &mut response);
        Ok((response, secrets))
    }

    /// Sends a compact call and reads its answer
    /// (`executeCompactRequest`): the prepared request, xAI's body as it
    /// came and its headers, and the secrets the call sent, which the body
    /// is redacted of before the client gets anything made from it.
    pub(super) async fn compact_request(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
        kind: AttemptKind,
    ) -> Result<(Prepared, Vec<u8>, HeaderMap, Secrets), ExecError> {
        let mut prepared = prepare(
            self.context(auth),
            request,
            options,
            false,
            Format::OPENAI_RESPONSE,
        )?;
        compact::shape_body(&mut prepared.body, &request.payload);
        prepared.finalize(self.context(auth).config, request, options);
        // Standard API headers: a compact call is never the chat proxy's.
        let headers = build_headers(auth, &options.headers, false, &prepared.session_id)?;
        let url = endpoint(auth, true);
        let (response, secrets) = self
            .send(
                auth,
                &url,
                headers,
                &prepared.body,
                Attempt::new(
                    options,
                    kind,
                    PROVIDER,
                    &prepared.base_model,
                    &prepared.to,
                    auth,
                ),
            )
            .await?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let (body, _) = read_body_prefix(response, MAX_ERROR_BODY).await;
            tracing::debug!(status, "xai: compact request error");
            let body = secrets.bytes(&body, Policy::Client);
            return Err(errors::status_error(status, &body).into());
        }
        let response_headers = response.headers().clone();
        let data = read_body(response, MAX_LINE)
            .await
            .map_err(|error| ExecError::new(ErrorKind::Upstream, error.to_string()))?;
        replay::clear_after_compaction(&prepared.replay);
        Ok((prepared, data, response_headers, secrets))
    }

    /// `Execute` for `responses/compact` (`executeCompact`).
    async fn execute_compact(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<Response, ExecError> {
        let (mut prepared, data, headers, secrets) = self
            .compact_request(auth, request, options, AttemptKind::Execute)
            .await?;
        // Whole, before it is translated; the taps read it as it came.
        let data = secrets.bytes(&data, Policy::Client);
        let converted = prepared
            .apply_patch
            .bridge
            .transform_non_stream(&data)
            .map_err(|_| apply_patch_failure())?;
        let out = translate_completed(&prepared, request, converted)?;
        Ok(Response {
            payload: Bytes::from(out),
            headers,
        })
    }

    /// `ExecuteStream` for a request whose input holds a
    /// `compaction_trigger` item (`executeCompactionTriggerStream`): the
    /// compact call's result as the events of one response.
    async fn compaction_trigger_stream(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<StreamResponse, ExecError> {
        let (prepared, data, mut headers, secrets) = self
            .compact_request(auth, request, options, AttemptKind::Stream)
            .await?;
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        // Whole, before the events are made from it; the taps read it as it
        // came.
        let data = secrets.bytes(&data, Policy::Client);
        let chunks = compact::trigger_stream_chunks(&prepared, &data, SystemTime::now());
        Ok(StreamResponse {
            headers,
            chunks: futures_util::stream::iter(
                chunks.into_iter().map(|chunk| Ok(Bytes::from(chunk))),
            )
            .boxed(),
        })
    }

    async fn execute_inner(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<Response, ExecError> {
        if options.alt != COMPACT_ALT
            && let Some(path) = images::endpoint(options)
        {
            return self.execute_images(auth, request, options, path).await;
        }
        if videos::is_video_request(options) && options.alt != COMPACT_ALT {
            return self.execute_videos(auth, request, options).await;
        }
        if is_media_request(options) {
            return Err(media_refused());
        }
        if options.alt == COMPACT_ALT {
            return self.execute_compact(auth, request, options).await;
        }
        let mut prepared = prepare(self.context(auth), request, options, true, Format::CODEX)?;
        prepared.finalize(self.context(auth).config, request, options);
        let headers = build_headers(auth, &options.headers, true, &prepared.session_id)?;
        let url = endpoint(auth, false);
        let (response, secrets) = self
            .send(
                auth,
                &url,
                headers,
                &prepared.body,
                Attempt::new(
                    options,
                    AttemptKind::Execute,
                    PROVIDER,
                    &prepared.base_model,
                    &prepared.to,
                    auth,
                ),
            )
            .await?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let (body, _) = read_body_prefix(response, MAX_ERROR_BODY).await;
            tracing::debug!(status, "xai: request error");
            let body = secrets.bytes(&body, Policy::Client);
            return Err(errors::status_error(status, &body).into());
        }
        let response_headers = response.headers().clone();

        let mut reader = LineReader::new(response);
        let mut items = OutputItems::default();
        let mut filter = XSearchFilter::new(
            prepared.filter_internal_x_search,
            std::mem::take(&mut prepared.client_declared_tools),
        );
        let mut restorer = NamespaceRestorer::new(std::mem::take(&mut prepared.namespace_tools));
        while let Some(line) = reader.next_line().await {
            let line = match line {
                Ok(line) => line,
                Err(error) => {
                    tracing::debug!("xai: response read failed: {error}");
                    reader.report(&error);
                    return Err(ExecError::new(
                        ErrorKind::Upstream,
                        secrets.text(error.to_string(), Policy::Client),
                    ));
                }
            };
            // Each line, as it is read; the taps read it as it came.
            let line = match secrets.bytes(&line, Policy::Client) {
                Cow::Owned(redacted) => redacted,
                Cow::Borrowed(_) => line,
            };
            let Some(rest) = line.strip_prefix(b"data:") else {
                continue;
            };
            let data = reasoning::normalize_summary_data(trim_space(rest).to_vec());
            prepared.apply_patch.remember_dispatcher_event(&data);
            let mut data = restorer.restore(data);
            if !prepared.web_search_alias.is_empty() {
                data = restore_client_web_search_name(data, &prepared.web_search_alias);
            }
            let Some(data) = filter.apply(data).filter(|data| !data.is_empty()) else {
                continue;
            };
            let (events, error) = prepared.apply_patch.transform(&data);
            if error.is_some() {
                return Err(apply_patch_failure());
            }
            for event in events {
                let parsed: Value = serde_json::from_slice(&event).unwrap_or(Value::Null);
                let event_type = str_at(&parsed, "type");
                match event_type.as_str() {
                    "response.output_item.done" => items.collect(&parsed),
                    "response.completed" | "response.incomplete" => {
                        let completed = patch_completed_output(event, &items);
                        let completed = reasoning::normalize_summary_data(completed);
                        if event_type == "response.completed" {
                            // A truncated turn has no state worth replaying.
                            replay::cache_completed(&prepared.replay, &completed);
                        }
                        let out = translate_completed(&prepared, request, completed)?;
                        return Ok(Response {
                            payload: Bytes::from(out),
                            headers: response_headers,
                        });
                    }
                    _ => {}
                }
            }
        }
        if prepared.apply_patch.finish().is_err() {
            return Err(apply_patch_failure());
        }
        Err(StatusError::new(408, DISCONNECTED_MESSAGE).into())
    }

    async fn execute_stream_inner(
        &self,
        auth: &Auth,
        request: Request,
        options: Options,
    ) -> Result<StreamResponse, ExecError> {
        if is_media_request(&options) {
            return Err(media_refused());
        }
        if options.alt == COMPACT_ALT {
            return Err(
                StatusError::new(400, "streaming not supported for /responses/compact").into(),
            );
        }
        if compact::input_has_item_type(&request.payload, compact::COMPACTION_TRIGGER) {
            return self
                .compaction_trigger_stream(auth, &request, &options)
                .await;
        }
        let mut prepared = prepare(self.context(auth), &request, &options, true, Format::CODEX)?;
        prepared.finalize(self.context(auth).config, &request, &options);
        let headers = build_headers(auth, &options.headers, true, &prepared.session_id)?;
        let url = endpoint(auth, false);
        let (response, secrets) = self
            .send(
                auth,
                &url,
                headers,
                &prepared.body,
                Attempt::new(
                    &options,
                    AttemptKind::Stream,
                    PROVIDER,
                    &prepared.base_model,
                    &prepared.to,
                    auth,
                ),
            )
            .await?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let tap = BodyTap::of(&response);
            let (body, error) = read_body_prefix(response, MAX_ERROR_BODY).await;
            if let Some(error) = error {
                let error = ExecError::new(ErrorKind::Upstream, error_chain(&error));
                observe_send::attempt_error(tap.as_ref(), &error);
                return Err(error);
            }
            tracing::debug!(status, "xai: request error");
            let body = secrets.bytes(&body, Policy::Client);
            return Err(errors::status_error(status, &body).into());
        }
        let response_headers = response.headers().clone();
        let translator = Registry::global().response_stream(
            &prepared.to,
            &prepared.response_format,
            &ResponseContext {
                model: &request.model,
                original_request: &prepared.original,
                request: &prepared.body,
            },
        );
        let chunks = stream::translate(
            response,
            StreamSetup {
                translator,
                source_format: options.source_format.clone(),
                prepared,
                secrets,
            },
        );
        Ok(StreamResponse {
            headers: response_headers,
            chunks,
        })
    }

    /// Estimates the prepared request's input tokens, in the client's
    /// format (`CountTokens`).
    async fn count_tokens_inner(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<Response, ExecError> {
        let mut prepared = prepare(self.context(auth), request, options, false, Format::CODEX)?;
        prepared.finalize(self.context(auth).config, request, options);
        let Prepared {
            body,
            to,
            response_format,
            ..
        } = prepared;
        let count = tokio::task::spawn_blocking(move || tokens::count_input_tokens(&body))
            .await
            .map_err(|_| {
                ExecError::new(ErrorKind::Upstream, "xai executor: token counting failed")
            })?;
        let usage = format!(
            r#"{{"response":{{"usage":{{"input_tokens":{count},"output_tokens":0,"total_tokens":{count}}}}}}}"#
        );
        let payload = Registry::global().translate_token_count(
            &to,
            &response_format,
            count,
            usage.into_bytes(),
        );
        Ok(Response {
            payload: Bytes::from(payload),
            headers: HeaderMap::new(),
        })
    }
}

/// The error for an answer whose `apply_patch` call couldn't be carried
/// over. xAI answered, so it keeps the answer's usage (v8.0.20's
/// `upstreamUsage.PublishFailure`).
fn apply_patch_failure() -> ExecError {
    ExecError::from(StatusError::new(502, APPLY_PATCH_ERROR_MESSAGE)).with_usage_kept()
}

/// Translates xAI's terminal response to the client's format, filling in
/// the usage details an OpenAI Responses client expects. A translation that
/// fails is a 502 that keeps the answer's usage.
fn translate_completed(
    prepared: &Prepared,
    request: &Request,
    completed: Vec<u8>,
) -> Result<Vec<u8>, ExecError> {
    let context = ResponseContext {
        model: &request.model,
        original_request: &prepared.original,
        request: &prepared.body,
    };
    let out = Registry::global()
        .translate_non_stream(&prepared.to, &prepared.response_format, &context, completed)
        .filter(|out| !out.is_empty())
        .ok_or_else(apply_patch_failure)?;
    Ok(if prepared.response_format == Format::OPENAI_RESPONSE {
        ensure_responses_usage_details(out)
    } else {
        out
    })
}

impl ProviderExecutor for XaiExecutor {
    fn id(&self) -> &str {
        PROVIDER
    }

    fn execute(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        async move { self.execute_inner(&auth, &request, &options).await }.boxed()
    }

    fn execute_stream(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<StreamResponse, ExecError>> {
        async move {
            if websocket::routes(&auth, &options) {
                return websocket::execute_stream(self, &auth, request, options).await;
            }
            self.execute_stream_inner(&auth, request, options).await
        }
        .boxed()
    }

    fn count_tokens(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        async move { self.count_tokens_inner(&auth, &request, &options).await }.boxed()
    }

    /// An API key has nothing to refresh.
    fn refresh(&self, auth: Arc<Auth>) -> BoxFuture<'_, Result<Auth, ExecError>> {
        async move { Ok((*auth).clone()) }.boxed()
    }

    fn close_execution_session(&self, session_id: &str) {
        self.websockets.close(session_id);
    }

    fn http_request(
        &self,
        auth: Arc<Auth>,
        call: HttpCall,
    ) -> BoxFuture<'_, Result<HttpReply, ExecError>> {
        async move { self.http_request_inner(&auth, call).await }.boxed()
    }

    fn download(
        &self,
        auth: Option<Arc<Auth>>,
        url: String,
    ) -> BoxFuture<'_, Result<Downloaded, ExecError>> {
        async move { self.download_inner(auth.as_deref(), &url).await }.boxed()
    }
}

mod http_request;
mod images;
mod videos;

#[cfg(test)]
mod tests;
