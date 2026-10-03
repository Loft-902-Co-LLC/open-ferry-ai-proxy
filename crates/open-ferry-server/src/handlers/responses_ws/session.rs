// Ported from ResponsesWebsocket, responsesWebsocketObservedCompactionState,
// responsesWebsocketHTTPReplayRequiredError,
// responsesWebsocketRequestRequiresCurrentUpstream,
// responsesWebsocketNativePassthroughAllowed and
// responsesWebsocketPreviousResponseNotFoundError in CLIProxyAPI
// sdk/api/handlers/openai/openai_responses_websocket.go,
// responsesWebsocketProviderSetForModel in
// sdk/api/handlers/openai/openai_responses_websocket_session.go, and
// IsCodexResponsesLiteRequest in internal/util/codex.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A socket's session: each request in turn, made whole from what the
//! session holds, sent on, and answered on the socket.
//!
//! A turn goes over HTTP with the whole transcript, unless the credential
//! the session is pinned to holds the conversation on its own upstream
//! WebSocket, when the client's requests go on as they are.

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use http::HeaderMap;
use open_ferry_core::exec::{
    Dispatcher, ExecError, Format, ProviderId, WebsocketAuth, WebsocketSupport, WsClose,
};
use open_ferry_core::models::ModelCatalog;
use open_ferry_translate::go;
use uuid::Uuid;

use super::forward::{
    ForwardOptions, Forwarded, error_payload, forward, should_release_pinned,
    should_replay_pinned_failure,
};
use super::prewarm::{normalize_followup, should_handle_locally, synthetic_payloads};
use super::repair::{caches, prepare_fallback_turn, session_key};
use super::requests::{
    Normalized, TYPE_APPEND, TYPE_CREATE, input_contains_full_transcript, input_not_array,
    normalize, normalize_create, normalize_passthrough, request_type, transcript_replacement,
};
use super::writer::{Conn, Socket};
use crate::errors::ErrorMessage;
use crate::exec::{Call, ClientRequest, Started};
use crate::json::{self, Val, str_at};
use crate::routing::{parse_suffix, resolve_model, route};
use crate::state::AppState;

/// The body of the error that has the client replay the turn over a new
/// socket (upstream's `UpstreamWebsocketReplayRequiredError`).
const REPLAY_REQUIRED_BODY: &str = r#"{"error":{"message":"upstream transport requires full HTTP replay","type":"server_error","code":"upstream_http_replay_required","status":426}}"#;

/// The body of the 409 for a previous response the socket doesn't hold
/// (`responsesWebsocketPreviousResponseNotFoundError`).
const PREVIOUS_RESPONSE_NOT_FOUND_BODY: &str = r#"{"error":{"message":"Previous response is not available on this websocket; resend the full conversation input without previous_response_id","type":"invalid_request_error","code":"previous_response_not_found","param":"previous_response_id"}}"#;

/// The header a Codex client asks for lite responses with.
const LITE_HEADER: &str = "x-openai-internal-codex-responses-lite";

/// Where the last turn went (`responsesWebsocketUpstreamMode*`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Upstream {
    /// No turn has completed.
    #[default]
    Unknown,
    /// A credential's upstream WebSocket, which holds the conversation.
    Websocket,
    /// HTTP, which holds nothing between turns.
    Http,
}

/// The model and credential a response with compaction items came from,
/// which can take a compacted transcript as it is
/// (`responsesWebsocketObservedCompactionState`).
#[derive(Debug, Default)]
struct ObservedCompaction {
    model: String,
    auth_id: String,
}

/// A model's providers, as the session looks up credentials by them.
#[derive(Debug)]
struct Target {
    /// The model, with `auto` resolved
    /// (`responsesWebsocketResolvedModelName`).
    resolved: String,
    /// Its providers, in lower case (`responsesWebsocketProviderSetForModel`).
    providers: Vec<ProviderId>,
    /// Its name without a thinking suffix.
    model_key: String,
}

impl Target {
    fn new(catalog: &dyn ModelCatalog, model: &str) -> Self {
        let resolved = resolve_model(catalog, model);
        let mut providers: Vec<ProviderId> = Vec::new();
        // Image-only models are turned away here, where upstream reads their
        // providers anyway; nothing serves them over the WebSocket.
        let routed = route(catalog, &resolved).map_or_else(|_| Vec::new(), |route| route.providers);
        for provider in routed {
            let key = go::to_lower(provider.trim());
            if !key.is_empty() && !providers.contains(&key) {
                providers.push(key);
            }
        }
        let base = parse_suffix(&resolved).0.trim();
        let model_key = if base.is_empty() {
            resolved.trim().to_owned()
        } else {
            base.to_owned()
        };
        Self {
            resolved,
            providers,
            model_key,
        }
    }

    fn support(&self, dispatcher: &dyn Dispatcher, auth_id: Option<&str>) -> WebsocketSupport {
        dispatcher.websocket_support(&self.providers, &self.model_key, auth_id)
    }
}

/// A socket's session, and what it holds between turns.
struct Session<S> {
    state: AppState,
    client: ClientRequest,
    dispatcher: Arc<dyn Dispatcher>,
    conn: Conn<S>,
    /// The session's ID, for executors that keep state per socket.
    id: String,
    /// The key the client's tool calls are cached under.
    key: String,
    /// The last request sent over HTTP, whole.
    last_request: Vec<u8>,
    /// The output of the response to it.
    last_output: Vec<u8>,
    last_response_id: String,
    /// The tool calls the last response left unanswered.
    pending_call_ids: Vec<String>,
    observed: ObservedCompaction,
    /// The warm-up answered here, until a turn that generates succeeds.
    pending_prewarm: String,
    /// The credential the session sticks to.
    pinned: String,
    /// The credential pinned for each provider, so a session that switches
    /// providers keeps each one's.
    pinned_by_provider: HashMap<String, String>,
    /// The model of the last turn on an upstream WebSocket.
    passthrough_model: String,
    upstream: Upstream,
    /// The credential whose upstream WebSocket holds the conversation.
    upstream_ws_auth: String,
}

/// Runs a client's session until it or the session closes the socket
/// (`ResponsesWebsocket`, after the upgrade).
pub(super) async fn run<S: Socket>(state: AppState, client: ClientRequest, socket: S) {
    let dispatcher = state.dispatcher_arc();
    let key = session_key(&client.headers);
    let mut session = Session {
        state,
        client,
        dispatcher,
        conn: Conn::new(socket),
        id: Uuid::now_v7().to_string(),
        key,
        last_request: Vec::new(),
        last_output: b"[]".to_vec(),
        last_response_id: String::new(),
        pending_call_ids: Vec::new(),
        observed: ObservedCompaction::default(),
        pending_prewarm: String::new(),
        pinned: String::new(),
        pinned_by_provider: HashMap::new(),
        passthrough_model: String::new(),
        upstream: Upstream::Unknown,
        upstream_ws_auth: String::new(),
    };
    caches().retain(&session.key);
    tracing::info!(id = %session.id, "responses websocket: client connected");

    while let Some(payload) = session.conn.read().await {
        if session.turn(&payload).await.is_break() {
            break;
        }
    }

    caches().release(&session.key);
    tracing::info!(id = %session.id, "responses websocket: session closing");
    session.dispatcher.close_execution_session(&session.id);
    tracing::info!(id = %session.id, "responses websocket: upstream execution session closed");
    session.conn.finish().await;
}

impl<S: Socket> Session<S> {
    /// Handles one request. Breaks when the session is over.
    async fn turn(&mut self, payload: &[u8]) -> ControlFlow<()> {
        let explicit_model = str_at(payload, "model").trim().to_owned();
        let mut request_model = explicit_model.clone();
        if request_model.is_empty() {
            request_model.clone_from(&self.passthrough_model);
        }
        if request_model.is_empty() {
            request_model = str_at(&self.last_request, "model").trim().to_owned();
        }
        let target = Target::new(self.state.catalog(), &request_model);
        self.check_pin(&target);

        let pinned_auth = self.pinned_auth(&target);
        let model_support = target.support(&*self.dispatcher, None);
        let mut use_upstream = !request_model.is_empty() && model_support.upstream_passthrough;
        if let Some(auth) = &pinned_auth
            && auth.websockets
        {
            use_upstream = is_websocket_provider(&auth.provider);
        }
        let native = native_passthrough_allowed(
            self.upstream,
            use_upstream,
            &self.pinned,
            &self.upstream_ws_auth,
        );
        let requires_current = requires_current_upstream(payload);
        if self.upstream == Upstream::Websocket && !native && requires_current {
            // A delta can't go anywhere but the upstream socket that holds
            // its conversation. A full response.create can start over.
            self.close_for_replay().await;
            return ControlFlow::Break(());
        }
        if !explicit_model.is_empty() && !use_upstream {
            self.passthrough_model.clear();
        }

        let replay_auth = self.observed_compaction_auth(&target);
        let mut compaction_bypass = replay_auth.is_some();
        if !native {
            compaction_bypass |= if self.pinned.is_empty() {
                model_support.compaction_replay
            } else {
                pinned_auth
                    .as_ref()
                    .is_some_and(|auth| auth.provider.trim().eq_ignore_ascii_case("codex"))
            };
        }

        let previous_id = str_at(payload, "previous_response_id").trim().to_owned();
        let is_prewarm = !use_upstream && should_handle_locally(payload, false);
        let normalized = if !self.pending_prewarm.is_empty() && !previous_id.is_empty() {
            if previous_id == self.pending_prewarm {
                normalize_followup(payload, &self.last_request)
            } else {
                Err(previous_response_not_found())
            }
        } else if (is_prewarm && previous_id.is_empty())
            || (!self.pending_prewarm.is_empty() && str_at(payload, "type") == TYPE_CREATE)
        {
            // Without a parent, the request replaces the transcript.
            normalize_replacement(payload, &self.last_request)
        } else if native {
            normalize_passthrough(payload, &request_model).map(|request| (request, Vec::new()))
        } else if self.last_request.is_empty() && !previous_id.is_empty() {
            Err(previous_response_not_found())
        } else {
            normalize(
                payload,
                &self.last_request,
                &self.last_output,
                &self.last_response_id,
                &self.pending_call_ids,
                false,
                compaction_bypass,
            )
        };
        let (mut request, updated_last_request) = match normalized {
            Ok(normalized) => normalized,
            Err(error) => {
                let payload = error_payload(&error);
                tracing::info!(
                    id = %self.id,
                    payload = %String::from_utf8_lossy(&payload),
                    "responses websocket: downstream_out"
                );
                if self.conn.write(&payload).await.is_err() {
                    tracing::warn!(id = %self.id, "responses websocket: downstream_out write failed");
                    return ControlFlow::Break(());
                }
                return ControlFlow::Continue(());
            }
        };

        if is_prewarm {
            request = json::delete(&request, "generate");
            self.last_request = json::delete(&updated_last_request, "generate");
            self.last_output = b"[]".to_vec();
            self.observed = ObservedCompaction::default();
            self.last_response_id.clear();
            self.pending_call_ids.clear();
            let payloads = synthetic_payloads(&request);
            for payload in &payloads {
                if self.conn.write(payload).await.is_err() {
                    tracing::warn!(id = %self.id, "responses websocket: downstream_out write failed");
                    return ControlFlow::Break(());
                }
            }
            self.pending_prewarm = str_at(&payloads[0], "response.id");
            return ControlFlow::Continue(());
        }

        let mut turn = None;
        let mut next_last_request = None;
        if native {
            let model = str_at(&request, "model");
            let model = model.trim();
            if !model.is_empty() {
                model.clone_into(&mut self.passthrough_model);
            }
        } else {
            let (repaired, cache_turn) = prepare_fallback_turn(&self.key, &request);
            request = repaired;
            turn = cache_turn;
            next_last_request = Some(request.clone());
        }

        let model_name = str_at(&request, "model");
        let lite = is_lite_request(payload, &self.client.headers);
        let execution_auth = if self.pinned.is_empty() {
            replay_auth.unwrap_or_default()
        } else {
            self.pinned.clone()
        };
        let selected: Arc<Mutex<Vec<String>>> = Arc::default();
        let started = match Call::new(
            &self.state,
            &self.client,
            Format::OPENAI_RESPONSE,
            &model_name,
            Bytes::from(request),
            "",
            true,
        ) {
            Ok(mut call) => {
                let metadata = &mut call.options.metadata;
                call.options.downstream_websocket = true;
                metadata.execution_session_id = Some(self.id.clone());
                if !execution_auth.is_empty() {
                    metadata.pinned_auth_id = Some(execution_auth);
                }
                let record = Arc::clone(&selected);
                metadata.selected_auth = Some(Arc::new(move |auth_id: &str| {
                    record
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(auth_id.to_owned());
                }));
                call.stream().await
            }
            Err(error) => Started::failed(error),
        };

        // The call names each credential it tries before its stream starts.
        let mut last_attempted = self.pinned.clone();
        let mut attempted = Upstream::Unknown;
        let mut selection_observed = false;
        let mut pinned_attempted = false;
        let mut preserve_output = false;
        let selected =
            std::mem::take(&mut *selected.lock().unwrap_or_else(PoisonError::into_inner));
        for auth_id in &selected {
            preserve_output = false;
            let auth_id = auth_id.trim();
            if auth_id.is_empty() {
                continue;
            }
            auth_id.clone_into(&mut last_attempted);
            selection_observed = true;
            pinned_attempted |= !self.pinned.is_empty() && auth_id == self.pinned;
            let Some(auth) = self.auth(&target, auth_id) else {
                continue;
            };
            attempted = if auth.websockets && is_websocket_provider(&auth.provider) {
                Upstream::Websocket
            } else {
                Upstream::Http
            };
            preserve_output = lite && auth.provider.trim().eq_ignore_ascii_case("codex");
        }
        if !selection_observed {
            attempted = Upstream::Http;
        }

        // A turn on a connection-scoped upstream can't change credentials
        // midway, so a credential's failure has the client replay the turn on
        // a new socket.
        let replay_pinned = native && requires_current && pinned_attempted;
        let suppress_error =
            move |error: &ErrorMessage| replay_pinned && should_replay_pinned_failure(error);
        let keepalive = self.state.settings().config.streaming.keepalive;
        let forwarded = forward(
            &mut self.conn,
            started.items,
            ForwardOptions {
                session_key: &self.key,
                preserve_completion_output: preserve_output,
                turn: turn.as_mut(),
                suppress_error: &suppress_error,
                keepalive,
            },
        )
        .await;

        let (output, response_id, pending_call_ids) = match forwarded {
            Forwarded::Closed => return ControlFlow::Break(()),
            Forwarded::Suppressed(error) => {
                if pinned_attempted && should_release_pinned(&error) {
                    self.forget_pinned();
                }
                if suppress_error(&error) {
                    self.close_for_replay().await;
                    return ControlFlow::Break(());
                }
                return ControlFlow::Continue(());
            }
            Forwarded::Completed {
                output,
                response_id,
                pending_call_ids,
            } => (output, response_id, pending_call_ids),
        };

        if let Some(turn) = turn {
            turn.commit();
        }
        self.pending_prewarm.clear();
        self.upstream = attempted;
        if attempted == Upstream::Websocket {
            last_attempted.clone_into(&mut self.upstream_ws_auth);
            if !last_attempted.is_empty() {
                self.remember_pinned(&target, &last_attempted);
            }
            self.passthrough_model = model_name;
            self.last_request.clear();
            self.last_output = b"[]".to_vec();
            self.observed = ObservedCompaction::default();
            self.last_response_id.clear();
            self.pending_call_ids.clear();
            return ControlFlow::Continue(());
        }

        self.upstream_ws_auth.clear();
        if let Some(next) = next_last_request {
            self.last_request = next;
        }
        if Val::parse(&output).is_some_and(input_contains_full_transcript) {
            self.observed = ObservedCompaction {
                model: model_name,
                auth_id: last_attempted,
            };
        } else if !self.observed.model.is_empty() {
            let catalog = self.state.catalog();
            let mismatch = resolve_model(catalog, &self.observed.model)
                != resolve_model(catalog, &model_name)
                || (!self.observed.auth_id.is_empty()
                    && !last_attempted.is_empty()
                    && self.observed.auth_id != last_attempted);
            if mismatch {
                self.observed = ObservedCompaction::default();
            }
        }
        self.last_output = output;
        response_id.trim().clone_into(&mut self.last_response_id);
        self.pending_call_ids = pending_call_ids;
        ControlFlow::Continue(())
    }

    /// What the dispatcher knows of `auth_id`, or `None` when it doesn't
    /// know it.
    fn auth(&self, target: &Target, auth_id: &str) -> Option<WebsocketAuth> {
        target.support(&*self.dispatcher, Some(auth_id)).auth
    }

    /// The pinned credential, when there is one and the dispatcher knows it.
    fn pinned_auth(&self, target: &Target) -> Option<WebsocketAuth> {
        if self.pinned.is_empty() {
            return None;
        }
        self.auth(target, &self.pinned)
    }

    /// Drops the pinned credential when it can't serve the request's model,
    /// and pins the one kept for the model's provider when it has only one.
    fn check_pin(&mut self, target: &Target) {
        if !self.pinned.is_empty() {
            let keep = self.pinned_auth(target).is_some_and(|auth| {
                auth.serves_model
                    && self.pinned_by_provider.get(&provider_key(&auth.provider))
                        == Some(&self.pinned)
            });
            if !keep {
                self.pinned.clear();
            }
        }
        if self.pinned.is_empty()
            && let [provider] = target.providers.as_slice()
        {
            let candidate = self.pinned_by_provider.get(provider).cloned();
            match candidate {
                Some(auth_id)
                    if self
                        .auth(target, &auth_id)
                        .is_some_and(|auth| auth.serves_model) =>
                {
                    self.pinned = auth_id;
                }
                _ => {
                    self.pinned_by_provider.remove(provider);
                }
            }
        }
    }

    /// The credential that answered with compaction items, when the request
    /// goes to the same model and that credential may serve it.
    fn observed_compaction_auth(&self, target: &Target) -> Option<String> {
        let observed = &self.observed;
        if observed.model.is_empty()
            || observed.auth_id.is_empty()
            || resolve_model(self.state.catalog(), &observed.model) != target.resolved
        {
            return None;
        }
        let supported = if self.pinned.is_empty() {
            self.auth(target, &observed.auth_id)
                .is_some_and(|auth| auth.serves_model)
        } else {
            self.pinned == observed.auth_id
        };
        supported.then(|| observed.auth_id.clone())
    }

    /// Pins `auth_id`, and keeps it for its provider (`rememberPinnedAuth`).
    fn remember_pinned(&mut self, target: &Target, auth_id: &str) {
        let auth_id = auth_id.trim();
        if auth_id.is_empty() {
            return;
        }
        let Some(auth) = self.auth(target, auth_id) else {
            return;
        };
        auth_id.clone_into(&mut self.pinned);
        let key = provider_key(&auth.provider);
        if !key.is_empty() {
            self.pinned_by_provider.insert(key, auth_id.to_owned());
        }
    }

    /// Unpins the pinned credential, for every provider (`forgetPinnedAuth`).
    fn forget_pinned(&mut self) {
        let pinned = &self.pinned;
        self.pinned_by_provider
            .retain(|_, auth_id| auth_id != pinned);
        self.pinned.clear();
    }

    /// Closes with 1012, so the client replays the turn on a new socket.
    async fn close_for_replay(&mut self) {
        if !self.conn.close_for_upstream_error(&replay_required()).await {
            self.conn.close_without_error();
        }
    }
}

/// A provider as the pinned map keys it.
fn provider_key(provider: &str) -> String {
    go::to_lower(provider.trim())
}

/// Whether `provider`'s upstream can hold a conversation on its WebSocket.
fn is_websocket_provider(provider: &str) -> bool {
    matches!(provider_key(provider).as_str(), "codex" | "xai")
}

/// Whether a request may go on as it is: the last turn went to the upstream
/// socket of the pinned credential, which can still take it
/// (`responsesWebsocketNativePassthroughAllowed`).
pub(super) fn native_passthrough_allowed(
    upstream: Upstream,
    use_upstream: bool,
    pinned: &str,
    upstream_ws_auth: &str,
) -> bool {
    let pinned = pinned.trim();
    upstream == Upstream::Websocket
        && use_upstream
        && !pinned.is_empty()
        && pinned == upstream_ws_auth.trim()
}

/// Whether the request continues the conversation on the upstream socket
/// (`responsesWebsocketRequestRequiresCurrentUpstream`).
pub(super) fn requires_current_upstream(payload: &[u8]) -> bool {
    !str_at(payload, "previous_response_id").trim().is_empty()
        || request_type(payload) == TYPE_APPEND
}

/// A request that starts a transcript over, as the first of one.
fn normalize_replacement(payload: &[u8], last_request: &[u8]) -> Result<Normalized, ErrorMessage> {
    if json::get(payload, "input").is_some_and(|input| !input.is_array()) {
        return Err(input_not_array());
    }
    normalize_create(&transcript_replacement(payload, last_request))
}

/// Whether a Codex client asked for lite responses, whose completion goes
/// out as the upstream wrote it (`IsCodexResponsesLiteRequest`).
pub(super) fn is_lite_request(payload: &[u8], headers: &HeaderMap) -> bool {
    let header = headers
        .get(LITE_HEADER)
        .map(|value| String::from_utf8_lossy(value.as_bytes()).trim().to_owned())
        .unwrap_or_default();
    if header.eq_ignore_ascii_case("true") {
        return true;
    }
    json::get(
        payload,
        "client_metadata.ws_request_header_x_openai_internal_codex_responses_lite",
    )
    .is_some_and(|value| {
        value.raw == b"true"
            || (value.is_string() && value.str().trim().eq_ignore_ascii_case("true"))
    })
}

/// The error that closes the socket with 1012, so the client replays the
/// turn (`responsesWebsocketHTTPReplayRequiredError`).
pub(super) fn replay_required() -> ErrorMessage {
    let mut error = ExecError::upstream(426, REPLAY_REQUIRED_BODY);
    error.ws_close = Some(WsClose::ReplayRequired);
    ErrorMessage::from_exec(error)
}

/// The 409 for a previous response the socket doesn't hold
/// (`responsesWebsocketPreviousResponseNotFoundError`).
pub(super) fn previous_response_not_found() -> ErrorMessage {
    ErrorMessage::new(409, PREVIOUS_RESPONSE_NOT_FOUND_BODY)
}
