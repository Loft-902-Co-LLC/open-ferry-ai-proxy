// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_execution.go (Execute,
// ExecuteCount, ExecuteStream, executeMixedOnce, executeCountMixedOnce,
// executeStreamMixedOnce and their helpers), conductor_stream.go,
// sanitizeDownstreamWebsocketFallbackRequest in conductor_home_execution.go,
// and filterExecutionModels, nextModelPoolOffset and the force-mapped
// response rewrites in conductor_models.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Running a call: pick a credential, call its executor, record the outcome,
//! and on failure move on to the next credential, or to another round.
//!
//! A round tries credentials until one succeeds, a request-scoped rule or a
//! request error ends the call, `max_retry_credentials` is reached, or none
//! is left. Within a credential, each upstream model of its pool is tried in
//! turn, and a 401 refreshes the credential once and tries again. A stream
//! is returned once its first non-empty chunk arrives; a failure before then
//! counts like a failed call, so the next credential is tried.
//!
//! When a round fails, the error the caller sees is the last one an
//! upstream gave, rather than the manager's own "no auth available".
//!
//! The manager doesn't translate: the executor reads the call's
//! [`Options::source_format`](crate::exec::Options::source_format). So a call
//! from the image, video or speech endpoints (`openai-image`,
//! `openai-video`, `openai-speech`) reaches the executor with its format,
//! request path and payload as they came, a multipart form included, and its
//! response comes back as the executor gave it. Upstream's `requestToFormat`
//! keeps those formats as the client sent them for the request interceptors,
//! which aren't ported.
//!
//! Deviations from upstream:
//! - Every executor error, stream bootstrap error and empty stream counts as
//!   an upstream attempt; upstream asks the executor whether it reached the
//!   provider.
//! - Home dispatch, request preparation, request interceptors, API-key
//!   capability metadata and the Antigravity credits fallback aren't
//!   ported. Upstream's `Enrich` isn't either: with session affinity on,
//!   each round works out the call's session for the picks and results
//!   (see `affinity`), and writes nothing into the call's metadata.
//! - There are no context checks: dropping the call's future or stream stops
//!   it where it is, so the Claude OAuth cancellation checks aren't needed.
//! - A stream always has a source, so upstream's "upstream stream has no
//!   source" error can't happen.
//! - Removing `generate` from a WebSocket fallback payload re-serializes the
//!   JSON (keeping key order and numbers), where sjson edits it in place.
//! - When the client drops a stream, its task stops reading at once and
//!   marks no further result, as upstream's goroutine discards the rest
//!   once its context ends. What the taps are told is in the next point.
//! - Each executor call's end is reported to the request's taps here (see
//!   [`CallReport`]), where upstream's executors publish their usage and
//!   request-log records themselves. A stream's report is made before its
//!   executor is awaited, at the first attempt and at the retry after a
//!   refresh, so a client that leaves while the executor is still connecting
//!   ends the call for the taps once, as canceled. A stream the client drops
//!   reports a failure its executor has already queued, such as the 502
//!   behind an `apply_patch` failure's `response.failed` frame, which
//!   upstream's executors record before they send it; otherwise it ends the
//!   call as canceled, where upstream's reader, if it is waiting on the body
//!   at that moment, reads a failed body, finalizes the translator and
//!   records the patch 502.
//! - With open-ferry's `routing.quota.check-after` set, a pick may let the
//!   call check a capped quota rest; the call holds that check until its
//!   outcome is recorded, a stream until its task ends (see `quota_check`).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use http::HeaderMap;
use open_ferry_translate::json::exact;
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::Instrument as _;

use super::affinity::Session;
use super::classify::{
    CODE_FORCE_COOLDOWN, ErrView, is_count_tokens_endpoint_not_found_error,
    is_credential_scoped_error, is_request_invalid_error,
    is_responses_compact_availability_neutral_error, is_responses_compact_request_fault_error,
    result_error_from_error, retry_after_from_error,
};
use super::cooldown::CallResult;
use super::credential::websockets_enabled;
use super::models::{AliasResult, OAuthAliasTable, Resolver};
use super::policy::Eligibility;
use super::quota_check::{self, CheckClaim};
use super::retry::{
    RetryQuery, request_retry_round_exclusions, should_retry_after_error, wait_for_cooldown,
};
use super::rewrite::{StreamRewriter, rewrite_model_in_response};
use super::scoped::{ScopedAction, match_request_scoped_error_action};
use super::select::{PickArgs, Selection, is_auth_blocked_for_model, normalize_providers};
use super::settings::Settings;
use super::text::go_lower;
use super::{Manager, lock};
use crate::auth::Auth;
use crate::exec::{ChunkStream, ErrorKind, ExecError, Options, Request, Response, StreamResponse};
use crate::executor::ProviderExecutor;
use crate::observe::{CallReport, SelectedAuth};
use crate::session::Payload;

/// The pool offset at which the counter starts again (upstream's
/// `nextModelPoolOffset` wrap).
const MODEL_POOL_OFFSET_WRAP: usize = 2_147_483_640;

/// Which unary call to make.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CallKind {
    /// `Execute`.
    Execute,
    /// `ExecuteCount`.
    CountTokens,
}

/// The retry settings read once per call (upstream's `retrySettings`).
#[derive(Clone, Copy)]
struct RetrySettings {
    request_retry: usize,
    max_credentials: usize,
    max_wait: Duration,
}

/// A failed attempt, with the marks upstream keeps by wrapping the error.
#[derive(Clone)]
struct Failure {
    err: ExecError,
    /// Whether the call reached the provider (upstream's
    /// `upstreamExecutionAttemptError`).
    attempted: bool,
    /// Whether a request-scoped rule ended the call (upstream's
    /// `requestStopError`).
    stop: bool,
    /// The stream's headers, when it failed before its first chunk
    /// (upstream's `streamBootstrapError`).
    bootstrap: Option<HeaderMap>,
}

impl Failure {
    /// The manager's own error, from before any provider was reached.
    fn local(err: ExecError) -> Self {
        Self {
            err,
            attempted: false,
            stop: false,
            bootstrap: None,
        }
    }

    /// An executor's error.
    fn upstream(err: ExecError) -> Self {
        Self {
            err,
            attempted: true,
            stop: false,
            bootstrap: None,
        }
    }

    /// A stream's error before its first chunk.
    fn bootstrap(err: ExecError, headers: HeaderMap) -> Self {
        Self {
            err,
            attempted: true,
            stop: false,
            bootstrap: Some(headers),
        }
    }

    /// An error a request-scoped rule says ends the call.
    fn stop(err: ExecError) -> Self {
        Self {
            err,
            attempted: true,
            stop: true,
            bootstrap: None,
        }
    }
}

/// The error to report: the last upstream error when there is one, unless
/// the call itself was canceled (upstream's `preferredExecutionAttemptError`).
fn preferred(fallback: Failure, upstream: Option<Failure>) -> Failure {
    if matches!(
        fallback.err.kind,
        ErrorKind::Canceled | ErrorKind::DeadlineExceeded
    ) {
        return fallback;
    }
    match upstream {
        Some(mut upstream) => {
            upstream.attempted = true;
            upstream
        }
        None => fallback,
    }
}

/// The providers a call runs over: only the forced provider when the call
/// names one, else `providers`, lower-cased, trimmed and deduplicated
/// (upstream's `providersForExecution` with a `ForcedProvider`, then
/// `normalizeProviders`).
///
/// A forced provider that `providers` leave out is a conflict, which gets
/// upstream's 400 for a model router that picked another provider: this
/// port has no model router, so the providers the caller routed to stand in
/// for its choice.
fn execution_providers(providers: &[String], opts: &Options) -> Result<Vec<String>, ExecError> {
    let normalized = normalize_providers(providers);
    let forced = opts
        .metadata
        .forced_provider
        .as_deref()
        .map(|provider| go_lower(provider.trim()))
        .filter(|provider| !provider.is_empty());
    let Some(forced) = forced else {
        if normalized.is_empty() {
            return Err(ExecError::new(
                ErrorKind::ProviderNotFound,
                "no provider supplied",
            ));
        }
        return Ok(normalized);
    };
    if !normalized.is_empty() && !normalized.contains(&forced) {
        return Err(ExecError::new(
            ErrorKind::Upstream,
            "agent is only supported for native interactions execution",
        )
        .with_status(400));
    }
    Ok(vec![forced])
}

/// The model credentials are picked by: `auth_selection_model` when set,
/// else `fallback` (upstream's `authSelectionModelFromOptions`).
fn auth_selection_model(opts: &Options, fallback: &str) -> String {
    if let Some(model) = &opts.metadata.auth_selection_model {
        let model = model.trim();
        if !model.is_empty() {
            return model.to_owned();
        }
    }
    fallback.trim().to_owned()
}

/// The model to send upstream when credentials are picked by another one,
/// and whether there is such a model (upstream's
/// `executionModelForAuthSelection`).
fn execution_model_for_auth_selection(opts: &Options, model: &str) -> (String, bool) {
    let model = model.trim();
    if model.is_empty() {
        return (String::new(), false);
    }
    if auth_selection_model(opts, model) == model {
        return (String::new(), false);
    }
    (model.to_owned(), true)
}

/// Sets `requested_model` to the route model unless the caller set one
/// (upstream's `ensureRequestedModelMetadata`).
fn ensure_requested_model(opts: &mut Options, route_model: &str) {
    let route_model = route_model.trim();
    if route_model.is_empty() || !opts.metadata.requested_model.trim().is_empty() {
        return;
    }
    opts.metadata.requested_model = route_model.to_owned();
}

/// The credential the call is pinned to, or empty (upstream's
/// `pinnedAuthIDFromMetadata`).
fn pinned_auth_id(opts: &Options) -> String {
    opts.metadata
        .pinned_auth_id
        .as_deref()
        .map(str::trim)
        .unwrap_or_default()
        .to_owned()
}

/// The credentials picked in one round, as the tracker records them.
type AttemptedSet = Arc<Mutex<HashSet<String>>>;

/// Options whose `selected_auth` callback also records each credential
/// picked, before calling the caller's (upstream's
/// `withAttemptedAuthTracker`).
fn with_attempted_auth_tracker(opts: &Options) -> (Options, AttemptedSet) {
    let attempted: AttemptedSet = Arc::default();
    let mut opts = opts.clone();
    let previous = opts.metadata.selected_auth.take();
    let set = attempted.clone();
    opts.metadata.selected_auth = Some(Arc::new(move |auth_id: &str| {
        if !auth_id.trim().is_empty() {
            lock(&set).insert(auth_id.to_owned());
        }
        if let Some(previous) = &previous {
            previous(auth_id);
        }
    }));
    (opts, attempted)
}

/// Tells the caller which credential the call was given (upstream's
/// `publishSelectedAuthMetadata`, without the metadata keys), and the
/// request's observers, for its trace ID and logs (upstream's auth index
/// callback).
fn publish_selected(opts: &Options, auth: &Arc<Auth>) {
    if let Some(observation) = &opts.observation {
        observation
            .context()
            .select(SelectedAuth::new(Arc::clone(auth)));
    }
    let auth_id = auth.id.trim();
    if auth_id.is_empty() {
        return;
    }
    if let Some(callback) = &opts.metadata.selected_auth {
        callback(auth_id);
    }
}

/// The rotation for a model pool, advancing it (upstream's
/// `nextModelPoolOffset`).
pub(super) fn next_model_pool_offset(
    offsets: &mut HashMap<String, usize>,
    key: &str,
    size: usize,
) -> usize {
    if size <= 1 {
        return 0;
    }
    let key = key.trim();
    if key.is_empty() {
        return 0;
    }
    let slot = offsets.entry(key.to_owned()).or_insert(0);
    let mut offset = *slot;
    if offset >= MODEL_POOL_OFFSET_WRAP {
        offset = 0;
    }
    *slot = offset + 1;
    offset % size
}

/// A WebSocket client's call going over HTTP drops `generate`, which only
/// the WebSocket transport takes (upstream's
/// `sanitizeDownstreamWebsocketFallbackRequest`).
fn sanitize_downstream_websocket_fallback_request(
    opts: &Options,
    auth: &Auth,
    req: &Request,
) -> Request {
    let mut req = req.clone();
    if !opts.downstream_websocket || websockets_enabled(auth) || req.payload.is_empty() {
        return req;
    }
    let Ok(Value::Object(mut body)) = exact::from_slice(&req.payload) else {
        return req;
    };
    if body.shift_remove("generate").is_none() {
        return req;
    }
    if let Ok(updated) = serde_json::to_vec(&Value::Object(body)) {
        req.payload = Bytes::from(updated);
    }
    req
}

/// Rewrites the response's model to the alias the client asked for, when
/// the alias says so (upstream's `rewriteForceMappedResponse`).
pub(super) fn rewrite_force_mapped_response(resp: &mut Response, alias: &AliasResult) {
    if !alias.force_mapping || alias.original_alias.trim().is_empty() {
        return;
    }
    resp.payload = Bytes::from(rewrite_model_in_response(
        &resp.payload,
        &alias.original_alias,
    ));
}

/// A failed call's result, with the request-scoped rule's code applied.
/// `credential_scope` says whether the credential-scope flag is taken from
/// the error at all.
fn failure_result(
    attempt: &Attempt<'_>,
    auth: &Auth,
    err: &ExecError,
    action: Option<ScopedAction>,
    credential_scope: bool,
) -> CallResult {
    let mut error = result_error_from_error(ErrView::Exec(err));
    if let Some(action) = action {
        action.apply_to_result(&mut error);
    }
    CallResult {
        auth_id: auth.id.clone(),
        provider: attempt.provider.to_owned(),
        model: attempt.result_model.clone(),
        route_model: attempt.route_model.to_owned(),
        success: false,
        error: Some(error),
        retry_after: retry_after_from_error(ErrView::Exec(err)),
        credential_scope: credential_scope && is_credential_scoped_error(ErrView::Exec(err)),
        response_headers: err.headers.clone(),
        credential_version: auth.credential_version,
        registration_epoch: auth.registration_epoch,
        ..CallResult::default()
    }
}

/// The headers of a stream that failed after it opened: the stream's, with
/// those the error carries in their place, as upstream merges a Codex error
/// event's headers into the response's.
fn stream_failure_headers(stream: &HeaderMap, err: &ExecError) -> HeaderMap {
    let mut headers = stream.clone();
    for name in err.headers.keys() {
        headers.remove(name);
    }
    for (name, value) in &err.headers {
        headers.append(name.clone(), value.clone());
    }
    headers
}

/// What one model attempt records results under.
struct Attempt<'a> {
    provider: &'a str,
    route_model: &'a str,
    result_model: String,
    /// The call's session, while session affinity is on.
    session: Option<&'a Arc<Session>>,
}

impl Attempt<'_> {
    /// The call's session, while session affinity is on.
    fn session(&self) -> Option<&Session> {
        self.session.map(Arc::as_ref)
    }
}

/// A picked credential with the upstream models to try.
struct Prepared {
    auth: Arc<Auth>,
    executor: Arc<dyn ProviderExecutor>,
    provider: String,
    models: Vec<String>,
    pooled: bool,
    alias: AliasResult,
    /// The quota check this call makes, if it was let through to make one
    /// (open-ferry's; see `quota_check`). Held until the outcome is
    /// recorded.
    check: Option<Arc<CheckClaim>>,
}

/// How a stream began (upstream's `readStreamBootstrap`).
enum Bootstrap {
    /// The first non-empty chunk arrived.
    Open { first: Bytes, rest: ChunkStream },
    /// The stream ended first, after empty chunks or none.
    Closed { saw_chunk: bool },
    /// The stream failed first.
    Failed(ExecError),
}

async fn read_stream_bootstrap(mut chunks: ChunkStream) -> Bootstrap {
    let mut saw_chunk = false;
    loop {
        match chunks.next().await {
            None => return Bootstrap::Closed { saw_chunk },
            Some(Err(err)) => return Bootstrap::Failed(err),
            Some(Ok(payload)) if payload.is_empty() => saw_chunk = true,
            Some(Ok(first)) => {
                return Bootstrap::Open {
                    first,
                    rest: chunks,
                };
            }
        }
    }
}

/// A stream that yields only `err`, with the failed stream's headers
/// (upstream's `streamErrorResult`).
fn stream_error_result(headers: HeaderMap, err: ExecError) -> StreamResponse {
    StreamResponse {
        headers,
        chunks: Box::pin(futures_util::stream::iter([Err(err)])),
    }
}

impl Manager {
    fn retry_settings(&self) -> RetrySettings {
        let settings = self.settings();
        RetrySettings {
            request_retry: settings.request_retry,
            max_credentials: settings.max_retry_credentials,
            max_wait: settings.max_retry_interval,
        }
    }

    /// The settings and OAuth aliases, for resolving models outside the lock.
    pub(crate) fn resolver_parts(&self) -> (Arc<Settings>, Arc<OAuthAliasTable>) {
        let state = self.lock();
        (state.settings.clone(), state.oauth.clone())
    }

    /// The model a call's outcome is recorded under (upstream's
    /// `stateModelForExecution`).
    fn state_model_for_execution(
        &self,
        auth: &Auth,
        route_model: &str,
        upstream_model: &str,
        pooled: bool,
    ) -> String {
        let (settings, oauth) = self.resolver_parts();
        Resolver {
            settings: &settings,
            oauth: &oauth,
        }
        .state_model_for_execution(auth, route_model, upstream_model, pooled)
    }

    /// The alias behind one attempt's upstream model (upstream's
    /// `resolveAttemptAliasResult`).
    fn attempt_alias(
        &self,
        auth: &Auth,
        route_model: &str,
        upstream_model: &str,
        fallback: &AliasResult,
    ) -> AliasResult {
        let (settings, oauth) = self.resolver_parts();
        Resolver {
            settings: &settings,
            oauth: &oauth,
        }
        .resolve_attempt_alias_result(auth, route_model, upstream_model, fallback)
    }

    /// Picks the next credential and its upstream models, leaving out the
    /// models it is blocked for (upstream's `pickNextMixed` followed by
    /// `preparedExecutionModelsWithAlias`). With session affinity on, the
    /// pick keeps `session` on its credential.
    #[allow(clippy::too_many_arguments)]
    fn pick_prepared(
        &self,
        providers: &[String],
        route_model: &str,
        pinned: &str,
        downstream_websocket: bool,
        eligibility: Eligibility,
        tried: &HashSet<String>,
        session: Option<&Session>,
    ) -> Result<Prepared, ExecError> {
        let now = self.now();
        let mut guard = self.lock();
        let state = &mut *guard;
        let settings = state.settings.clone();
        let oauth = state.oauth.clone();
        let resolver = Resolver {
            settings: &settings,
            oauth: &oauth,
        };
        let selection = Selection {
            auths: &state.auths,
            executors: &state.executors,
            models: self.models(),
            resolver,
            strategy: settings.routing_strategy,
            now,
        };
        let args = PickArgs {
            model: route_model,
            pinned,
            downstream_websocket,
            tried,
            eligibility,
        };
        let picked = match state.affinity.as_mut() {
            Some(affinity) => selection.pick_next_mixed_sticky(
                &mut state.selector,
                affinity,
                providers,
                &args,
                session,
            )?,
            None => selection.pick_next_mixed(&mut state.selector, providers, &args)?,
        };
        let offsets = &mut state.pool_offsets;
        let (candidates, pooled, alias) = resolver.execution_model_candidates_with_alias(
            &picked.auth,
            route_model,
            |key, size| next_model_pool_offset(offsets, key, size),
        );
        let models: Vec<String> = candidates
            .into_iter()
            .filter(|model| {
                let state_model =
                    resolver.state_model_for_execution(&picked.auth, route_model, model, pooled);
                !is_auth_blocked_for_model(&picked.auth, &state_model, now).0
            })
            .collect();
        let check = if models.is_empty() {
            None
        } else {
            let keys: Vec<String> = models
                .iter()
                .map(|model| {
                    resolver.state_model_for_execution(&picked.auth, route_model, model, pooled)
                })
                .collect();
            quota_check::claim(state, &self.shared, &picked.auth.id, keys, now)
        };
        Ok(Prepared {
            auth: picked.auth,
            executor: picked.executor,
            provider: picked.provider,
            models,
            pooled,
            alias,
            check,
        })
    }

    /// The session a call binds under, while session affinity is on
    /// (upstream's `Enrich` and `extractSessionIDs`): read from the client's
    /// headers, its body as it arrived (else the request's) and its
    /// WebSocket session.
    fn call_session(&self, req: &Request, opts: &Options) -> Option<Arc<Session>> {
        // Without affinity, no session is read.
        self.lock().affinity.as_ref()?;
        let body = if opts.original_request.is_empty() {
            &req.payload
        } else {
            &opts.original_request
        };
        let payload = Payload::parse(body);
        Session::of(
            &opts.headers,
            &payload,
            opts.metadata
                .execution_session_id
                .as_deref()
                .unwrap_or_default(),
            opts.source_format.as_str(),
            &opts.metadata.caller_scope,
        )
        .map(Arc::new)
    }

    /// Whether to start another round after `err`, and the wait first.
    #[allow(clippy::too_many_arguments)]
    fn retry_wait(
        &self,
        providers: &[String],
        model: &str,
        pinned: &str,
        eligibility: Eligibility,
        attempt: usize,
        retry: RetrySettings,
        attempted: &HashSet<String>,
        err: &ExecError,
    ) -> Option<Duration> {
        let now = self.now();
        let state = self.lock();
        let selection = Selection {
            auths: &state.auths,
            executors: &state.executors,
            models: self.models(),
            resolver: Resolver {
                settings: &state.settings,
                oauth: &state.oauth,
            },
            strategy: state.settings.routing_strategy,
            now,
        };
        let query = RetryQuery {
            providers,
            model,
            pinned,
            attempt,
            default_retry: retry.request_retry,
            attempted,
            eligibility,
        };
        should_retry_after_error(&selection, &query, err, retry.max_wait)
    }

    /// Runs a non-streaming call or a token count over the providers, in
    /// rounds (upstream's `Execute` and `ExecuteCount`).
    pub(crate) async fn execute_unary(
        &self,
        kind: CallKind,
        providers: &[String],
        req: Request,
        opts: Options,
    ) -> Result<Response, ExecError> {
        let normalized = execution_providers(providers, &opts)?;
        let retry = self.retry_settings();
        let retry_model = auth_selection_model(&opts, &req.model);
        let pinned = pinned_auth_id(&opts);
        let mut preferred_upstream: Option<Failure> = None;
        let mut attempt = 0;
        let last = loop {
            let (round_opts, round_attempted) = with_attempted_auth_tracker(&opts);
            let failure = match self
                .unary_once(kind, &normalized, &req, round_opts, retry, attempt)
                .await
            {
                Ok(resp) => return Ok(resp),
                Err(failure) => failure,
            };
            if failure.stop {
                return Err(failure.err);
            }
            if failure.attempted {
                preferred_upstream = Some(failure.clone());
            }
            let attempted = lock(&round_attempted).clone();
            let wait = self.retry_wait(
                &normalized,
                &retry_model,
                &pinned,
                Eligibility::for_request(&opts),
                attempt,
                retry,
                &attempted,
                &failure.err,
            );
            let Some(wait) = wait else {
                break failure;
            };
            wait_for_cooldown(wait, retry.max_wait).await;
            attempt += 1;
        };
        Err(preferred(last, preferred_upstream).err)
    }

    /// Runs a streaming call over the providers, in rounds (upstream's
    /// `ExecuteStream`). A stream that failed before its first chunk comes
    /// back as a stream of that one error, with the provider's headers.
    pub(crate) async fn execute_streaming(
        &self,
        providers: &[String],
        req: Request,
        opts: Options,
    ) -> Result<StreamResponse, ExecError> {
        let normalized = execution_providers(providers, &opts)?;
        let retry = self.retry_settings();
        let retry_model = auth_selection_model(&opts, &req.model);
        let pinned = pinned_auth_id(&opts);
        let mut preferred_upstream: Option<Failure> = None;
        let mut attempt = 0;
        let last = loop {
            let (round_opts, round_attempted) = with_attempted_auth_tracker(&opts);
            let failure = match self
                .stream_once(&normalized, &req, round_opts, retry, attempt)
                .await
            {
                Ok(stream) => return Ok(stream),
                Err(failure) => failure,
            };
            if failure.attempted {
                preferred_upstream = Some(failure.clone());
            }
            if failure.stop {
                return Err(failure.err);
            }
            let attempted = lock(&round_attempted).clone();
            let wait = self.retry_wait(
                &normalized,
                &retry_model,
                &pinned,
                Eligibility::for_request(&opts),
                attempt,
                retry,
                &attempted,
                &failure.err,
            );
            let Some(wait) = wait else {
                break failure;
            };
            wait_for_cooldown(wait, retry.max_wait).await;
            attempt += 1;
        };
        let failure = preferred(last, preferred_upstream);
        match failure.bootstrap {
            Some(headers) => Ok(stream_error_result(headers, failure.err)),
            None => Err(failure.err),
        }
    }

    async fn call_unary(
        kind: CallKind,
        executor: &dyn ProviderExecutor,
        auth: Arc<Auth>,
        req: Request,
        opts: Options,
    ) -> Result<Response, ExecError> {
        let report = CallReport::start(&opts);
        let result = match kind {
            CallKind::Execute => executor.execute(auth, req, opts).await,
            CallKind::CountTokens => executor.count_tokens(auth, req, opts).await,
        };
        report.finish(&result);
        result
    }

    /// One executor call that opens a stream, reported to the taps from
    /// before the executor is awaited: a call dropped while the executor
    /// is still connecting reports itself canceled, and the report moves
    /// into the stream the executor returns.
    async fn call_stream(
        executor: &dyn ProviderExecutor,
        auth: Arc<Auth>,
        req: Request,
        opts: Options,
    ) -> Result<StreamResponse, ExecError> {
        let report = CallReport::start(&opts);
        let result = executor.execute_stream(auth, req, opts).await;
        report.stream(result)
    }

    /// One round of a unary call (upstream's `executeMixedOnce` and
    /// `executeCountMixedOnce`).
    async fn unary_once(
        &self,
        kind: CallKind,
        providers: &[String],
        req: &Request,
        mut opts: Options,
        retry: RetrySettings,
        round: usize,
    ) -> Result<Response, Failure> {
        let route_model = auth_selection_model(&opts, &req.model);
        let (execution_model, restore_execution_model) =
            execution_model_for_auth_selection(&opts, &req.model);
        ensure_requested_model(&mut opts, &route_model);
        let pinned = pinned_auth_id(&opts);
        let session = self.call_session(req, &opts);
        let mut tried =
            request_retry_round_exclusions(&self.lock().auths, round, retry.request_retry);
        let mut attempted: HashSet<String> = HashSet::new();
        let mut last: Option<Failure> = None;
        let mut upstream: Option<Failure> = None;
        loop {
            if retry.max_credentials > 0 && attempted.len() >= retry.max_credentials {
                return Err(match last {
                    Some(last) => preferred(last, upstream),
                    None => Failure::local(ExecError::auth_not_found()),
                });
            }
            let prepared = match self.pick_prepared(
                providers,
                &route_model,
                &pinned,
                opts.downstream_websocket,
                Eligibility::for_request(&opts),
                &tried,
                session.as_deref(),
            ) {
                Ok(prepared) => prepared,
                Err(err) => {
                    return Err(match last {
                        Some(last) => preferred(last, upstream),
                        None => Failure::local(err),
                    });
                }
            };
            publish_selected(&opts, &prepared.auth);
            tried.insert(prepared.auth.id.clone());
            if prepared.models.is_empty() {
                continue;
            }
            attempted.insert(prepared.auth.id.clone());
            let Prepared {
                mut auth,
                executor,
                provider,
                models,
                pooled,
                alias,
                check: _check,
            } = prepared;
            let mut auth_err: Option<ExecError> = None;
            let mut did_refresh = false;
            for upstream_model in &models {
                let attempt = Attempt {
                    provider: &provider,
                    route_model: &route_model,
                    result_model: self.state_model_for_execution(
                        &auth,
                        &route_model,
                        upstream_model,
                        pooled,
                    ),
                    session: session.as_ref(),
                };
                let mut exec_req = req.clone();
                exec_req.model = if restore_execution_model {
                    execution_model.clone()
                } else {
                    upstream_model.clone()
                };
                let exec_opts = opts.clone();
                let mut outcome = Self::call_unary(
                    kind,
                    &*executor,
                    auth.clone(),
                    exec_req.clone(),
                    exec_opts.clone(),
                )
                .await;
                if let Err(err) = &outcome {
                    upstream = Some(Failure::upstream(err.clone()));
                    if let Some(refreshed) = self
                        .try_refresh_after_unauthorized(&auth, err, did_refresh)
                        .await
                    {
                        auth = refreshed;
                        did_refresh = true;
                        outcome = Self::call_unary(
                            kind,
                            &*executor,
                            auth.clone(),
                            exec_req.clone(),
                            exec_opts.clone(),
                        )
                        .await;
                        if let Err(err) = &outcome {
                            upstream = Some(Failure::upstream(err.clone()));
                        }
                    }
                }
                let err = match outcome {
                    Ok(mut resp) => {
                        let session = attempt.session.map(Arc::as_ref);
                        self.mark_call_result(
                            &CallResult {
                                auth_id: auth.id.clone(),
                                provider: provider.clone(),
                                model: attempt.result_model,
                                route_model: route_model.clone(),
                                success: true,
                                response_headers: resp.headers.clone(),
                                // A token count says nothing of the quota.
                                skip_quota_observation: kind == CallKind::CountTokens,
                                credential_version: auth.credential_version,
                                registration_epoch: auth.registration_epoch,
                                ..CallResult::default()
                            },
                            session,
                        );
                        let attempt_alias =
                            self.attempt_alias(&auth, &route_model, upstream_model, &alias);
                        rewrite_force_mapped_response(&mut resp, &attempt_alias);
                        return Ok(resp);
                    }
                    Err(err) => err,
                };
                let settings = self.settings();
                let action = match_request_scoped_error_action(&auth, &err, &settings);
                let view = ErrView::Exec(&err);
                let credential_scope = match kind {
                    CallKind::Execute => {
                        let result = failure_result(&attempt, &auth, &err, action, true);
                        if is_responses_compact_availability_neutral_error(
                            &exec_opts,
                            view,
                            result.error.as_ref(),
                        ) {
                            self.record_availability_neutral_result(&result);
                        } else {
                            self.mark_call_result(&result, attempt.session());
                        }
                        result.credential_scope
                    }
                    CallKind::CountTokens => {
                        // A count_tokens route the upstream lacks is recorded
                        // without suspending the model, which still serves
                        // messages.
                        let mut result = failure_result(&attempt, &auth, &err, action, false);
                        result.skip_quota_observation = true;
                        if is_count_tokens_endpoint_not_found_error(view, &exec_req.model)
                            && result
                                .error
                                .as_ref()
                                .is_none_or(|e| e.code != CODE_FORCE_COOLDOWN)
                        {
                            self.record_availability_neutral_result(&result);
                        } else {
                            result.credential_scope = is_credential_scoped_error(view);
                            self.mark_call_result(&result, attempt.session());
                        }
                        result.credential_scope
                    }
                };
                if let Some(action) = action {
                    if action.is_stop() {
                        return Err(Failure::stop(err));
                    }
                    auth_err = Some(err);
                    if credential_scope {
                        break;
                    }
                    continue;
                }
                if (kind == CallKind::Execute
                    && is_responses_compact_request_fault_error(&exec_opts, view))
                    || is_request_invalid_error(view)
                {
                    return Err(Failure::upstream(err));
                }
                auth_err = Some(err);
                if credential_scope {
                    break;
                }
            }
            if let Some(err) = auth_err {
                let settings = self.settings();
                if let Some(action) = match_request_scoped_error_action(&auth, &err, &settings) {
                    if action.is_stop() {
                        return Err(Failure::stop(err));
                    }
                    last = Some(Failure::upstream(err));
                    continue;
                }
                let view = ErrView::Exec(&err);
                if (kind == CallKind::Execute
                    && is_responses_compact_request_fault_error(&opts, view))
                    || is_request_invalid_error(view)
                {
                    return Err(Failure::upstream(err));
                }
                last = Some(Failure::upstream(err));
            }
        }
    }

    /// One round of a streaming call (upstream's `executeStreamMixedOnce`).
    async fn stream_once(
        &self,
        providers: &[String],
        req: &Request,
        mut opts: Options,
        retry: RetrySettings,
        round: usize,
    ) -> Result<StreamResponse, Failure> {
        let route_model = auth_selection_model(&opts, &req.model);
        let (execution_model, restore_execution_model) =
            execution_model_for_auth_selection(&opts, &req.model);
        ensure_requested_model(&mut opts, &route_model);
        let pinned = pinned_auth_id(&opts);
        let session = self.call_session(req, &opts);
        let mut tried =
            request_retry_round_exclusions(&self.lock().auths, round, retry.request_retry);
        let mut attempted: HashSet<String> = HashSet::new();
        let mut last: Option<Failure> = None;
        let mut upstream: Option<Failure> = None;
        loop {
            if retry.max_credentials > 0 && attempted.len() >= retry.max_credentials {
                return Err(match last {
                    Some(last) => preferred(last, upstream),
                    None => Failure::local(ExecError::auth_not_found()),
                });
            }
            let prepared = match self.pick_prepared(
                providers,
                &route_model,
                &pinned,
                opts.downstream_websocket,
                Eligibility::for_request(&opts),
                &tried,
                session.as_deref(),
            ) {
                Ok(prepared) => prepared,
                Err(err) => {
                    return Err(match last {
                        Some(last) => preferred(last, upstream),
                        None => Failure::local(err),
                    });
                }
            };
            publish_selected(&opts, &prepared.auth);
            tried.insert(prepared.auth.id.clone());
            if prepared.models.is_empty() {
                continue;
            }
            attempted.insert(prepared.auth.id.clone());
            let exec_req =
                sanitize_downstream_websocket_fallback_request(&opts, &prepared.auth, req);
            let stream_execution_model = if restore_execution_model {
                execution_model.as_str()
            } else {
                ""
            };
            let failure = match self
                .stream_with_model_pool(
                    &prepared,
                    exec_req,
                    &opts,
                    &route_model,
                    stream_execution_model,
                    session.as_ref(),
                )
                .await
            {
                Ok(stream) => return Ok(stream),
                Err(failure) => failure,
            };
            if failure.attempted {
                upstream = Some(failure.clone());
            }
            if failure.stop {
                return Err(failure);
            }
            let settings = self.settings();
            if let Some(action) =
                match_request_scoped_error_action(&prepared.auth, &failure.err, &settings)
            {
                if action.is_stop() {
                    return Err(Failure {
                        stop: true,
                        ..failure
                    });
                }
                last = Some(failure);
                continue;
            }
            if is_request_invalid_error(ErrView::Exec(&failure.err)) {
                return Err(failure);
            }
            last = Some(failure);
        }
    }

    /// Opens a stream on one credential, trying each upstream model of its
    /// pool until one gives a first chunk (upstream's
    /// `executeStreamWithModelPool`).
    async fn stream_with_model_pool(
        &self,
        prepared: &Prepared,
        req: Request,
        opts: &Options,
        route_model: &str,
        execution_model: &str,
        session: Option<&Arc<Session>>,
    ) -> Result<StreamResponse, Failure> {
        let executor = &prepared.executor;
        let mut auth = prepared.auth.clone();
        let mut last: Option<Failure> = None;
        let mut upstream: Option<Failure> = None;
        let mut did_refresh = false;
        for (idx, model) in prepared.models.iter().enumerate() {
            let is_last_model = idx + 1 >= prepared.models.len();
            let attempt = Attempt {
                provider: &prepared.provider,
                route_model,
                result_model: self.state_model_for_execution(
                    &auth,
                    route_model,
                    model,
                    prepared.pooled,
                ),
                session,
            };
            let mut exec_req = req.clone();
            exec_req.model = if execution_model.is_empty() {
                model.clone()
            } else {
                execution_model.to_owned()
            };
            let exec_opts = opts.clone();
            let mut outcome = Self::call_stream(
                &**executor,
                auth.clone(),
                exec_req.clone(),
                exec_opts.clone(),
            )
            .await;
            if let Err(err) = &outcome {
                upstream = Some(Failure::upstream(err.clone()));
                if let Some(refreshed) = self
                    .try_refresh_after_unauthorized(&auth, err, did_refresh)
                    .await
                {
                    auth = refreshed;
                    publish_selected(&exec_opts, &auth);
                    did_refresh = true;
                    outcome = Self::call_stream(
                        &**executor,
                        auth.clone(),
                        exec_req.clone(),
                        exec_opts.clone(),
                    )
                    .await;
                    if let Err(err) = &outcome {
                        upstream = Some(Failure::upstream(err.clone()));
                    }
                }
            }
            let stream = match outcome {
                Ok(stream) => stream,
                Err(err) => {
                    let settings = self.settings();
                    let action = match_request_scoped_error_action(&auth, &err, &settings);
                    let result = failure_result(&attempt, &auth, &err, action, true);
                    self.mark_call_result(&result, attempt.session());
                    if action.is_some_and(ScopedAction::is_stop) {
                        return Err(Failure::stop(err));
                    }
                    if action.is_none() && is_request_invalid_error(ErrView::Exec(&err)) {
                        return Err(Failure::upstream(err));
                    }
                    let current = Failure::upstream(err);
                    last = Some(current.clone());
                    if result.credential_scope {
                        return Err(preferred(current, upstream));
                    }
                    continue;
                }
            };
            let StreamResponse {
                mut headers,
                chunks,
            } = stream;
            let mut bootstrap = read_stream_bootstrap(chunks).await;
            if let Bootstrap::Failed(err) = &bootstrap {
                upstream = Some(Failure::bootstrap(err.clone(), headers.clone()));
                if let Some(refreshed) = self
                    .try_refresh_after_unauthorized(&auth, err, did_refresh)
                    .await
                {
                    auth = refreshed;
                    publish_selected(&exec_opts, &auth);
                    did_refresh = true;
                    match Self::call_stream(
                        &**executor,
                        auth.clone(),
                        exec_req.clone(),
                        exec_opts.clone(),
                    )
                    .await
                    {
                        Err(retry_err) => {
                            headers = HeaderMap::new();
                            bootstrap = Bootstrap::Failed(retry_err);
                        }
                        Ok(retry) => {
                            headers = retry.headers;
                            bootstrap = read_stream_bootstrap(retry.chunks).await;
                        }
                    }
                }
                if let Bootstrap::Failed(err) = &bootstrap {
                    upstream = Some(Failure::bootstrap(err.clone(), headers.clone()));
                }
            }
            match bootstrap {
                Bootstrap::Failed(err) => {
                    let settings = self.settings();
                    let action = match_request_scoped_error_action(&auth, &err, &settings);
                    let mut result = failure_result(&attempt, &auth, &err, action, true);
                    result.response_headers = stream_failure_headers(&headers, &err);
                    self.mark_call_result(&result, attempt.session());
                    if let Some(action) = action {
                        if action.is_stop() {
                            return Err(Failure::stop(err));
                        }
                        last = Some(Failure::upstream(err.clone()));
                        if result.credential_scope {
                            return Err(preferred(Failure::bootstrap(err, headers), upstream));
                        }
                        continue;
                    }
                    if is_request_invalid_error(ErrView::Exec(&err)) {
                        return Err(Failure::upstream(err));
                    }
                    if !is_last_model {
                        last = Some(Failure::upstream(err.clone()));
                        if result.credential_scope {
                            return Err(preferred(Failure::bootstrap(err, headers), upstream));
                        }
                        continue;
                    }
                    return Err(preferred(Failure::bootstrap(err, headers), upstream));
                }
                Bootstrap::Closed { saw_chunk: false } => {
                    let empty = ExecError::empty_stream();
                    let session = attempt.session.map(Arc::as_ref);
                    self.mark_call_result(
                        &CallResult {
                            auth_id: auth.id.clone(),
                            provider: prepared.provider.clone(),
                            model: attempt.result_model,
                            route_model: route_model.to_owned(),
                            success: false,
                            error: Some(result_error_from_error(ErrView::Exec(&empty))),
                            response_headers: headers.clone(),
                            credential_version: auth.credential_version,
                            registration_epoch: auth.registration_epoch,
                            ..CallResult::default()
                        },
                        session,
                    );
                    let current = Failure::bootstrap(empty.clone(), headers);
                    upstream = Some(current.clone());
                    if !is_last_model {
                        last = Some(Failure::upstream(empty));
                        continue;
                    }
                    return Err(preferred(current, upstream));
                }
                Bootstrap::Closed { saw_chunk: true } => {
                    let alias = self.attempt_alias(&auth, route_model, model, &prepared.alias);
                    return Ok(self.wrap_stream(
                        auth,
                        &attempt,
                        headers,
                        None,
                        Box::pin(futures_util::stream::empty()),
                        &alias,
                        prepared.check.clone(),
                    ));
                }
                Bootstrap::Open { first, rest } => {
                    let alias = self.attempt_alias(&auth, route_model, model, &prepared.alias);
                    return Ok(self.wrap_stream(
                        auth,
                        &attempt,
                        headers,
                        Some(first),
                        rest,
                        &alias,
                        prepared.check.clone(),
                    ));
                }
            }
        }
        let last = last.unwrap_or_else(|| {
            Failure::local(ExecError::new(
                ErrorKind::AuthNotFound,
                "no upstream model available",
            ))
        });
        Err(preferred(last, upstream))
    }

    /// Hands the stream on from its first chunk, rewriting the model when the
    /// alias says so, and records its outcome when it ends (upstream's
    /// `wrapStreamResult`). A quota check the call makes is held until the
    /// stream's outcome is recorded.
    #[allow(clippy::too_many_arguments)]
    fn wrap_stream(
        &self,
        auth: Arc<Auth>,
        attempt: &Attempt<'_>,
        headers: HeaderMap,
        first: Option<Bytes>,
        mut rest: ChunkStream,
        alias: &AliasResult,
        check: Option<Arc<CheckClaim>>,
    ) -> StreamResponse {
        let (tx, rx) = mpsc::channel(1);
        let rewriter = (alias.force_mapping && !alias.original_alias.trim().is_empty())
            .then(|| StreamRewriter::new(alias.original_alias.clone()));
        let mut forwarder = Forwarder {
            manager: self.clone(),
            auth,
            provider: attempt.provider.to_owned(),
            route_model: attempt.route_model.to_owned(),
            result_model: attempt.result_model.clone(),
            headers: headers.clone(),
            session: attempt.session.cloned(),
            rewriter,
            failed: false,
            tx,
            _check: check,
        };
        // The task keeps the request's span, so what the provider's stream
        // logs shows the request's ID.
        let forward = async move {
            if let Some(first) = first
                && !forwarder.emit(Ok(first)).await
            {
                return;
            }
            loop {
                let next = tokio::select! {
                    // A client that left wins over a source that ended at
                    // the same moment.
                    biased;
                    () = forwarder.tx.closed() => return,
                    next = rest.next() => next,
                };
                let Some(chunk) = next else {
                    break;
                };
                if !forwarder.emit(chunk).await {
                    return;
                }
            }
            drop(rest);
            let tail = forwarder
                .rewriter
                .as_mut()
                .map(StreamRewriter::finish)
                .unwrap_or_default();
            if !tail.is_empty() && !forwarder.emit(Ok(Bytes::from(tail))).await {
                return;
            }
            if !forwarder.failed && !forwarder.tx.is_closed() {
                forwarder.manager.mark_call_result(
                    &CallResult {
                        auth_id: forwarder.auth.id.clone(),
                        provider: forwarder.provider.clone(),
                        model: forwarder.result_model.clone(),
                        route_model: forwarder.route_model.clone(),
                        success: true,
                        response_headers: forwarder.headers.clone(),
                        credential_version: forwarder.auth.credential_version,
                        registration_epoch: forwarder.auth.registration_epoch,
                        ..CallResult::default()
                    },
                    forwarder.session.as_deref(),
                );
            }
        };
        tokio::spawn(forward.in_current_span());
        let chunks = futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });
        StreamResponse {
            headers,
            chunks: Box::pin(chunks),
        }
    }
}

/// The task state of a stream being handed on.
struct Forwarder {
    manager: Manager,
    auth: Arc<Auth>,
    provider: String,
    route_model: String,
    result_model: String,
    /// The stream's response headers, for the quota snapshot.
    headers: HeaderMap,
    session: Option<Arc<Session>>,
    rewriter: Option<StreamRewriter>,
    failed: bool,
    tx: mpsc::Sender<Result<Bytes, ExecError>>,
    /// The quota check the stream makes, released when the task ends.
    _check: Option<Arc<CheckClaim>>,
}

impl Forwarder {
    /// Hands one chunk on; false when the client is gone. The first error
    /// is recorded as the call's failure, and still handed on.
    async fn emit(&mut self, chunk: Result<Bytes, ExecError>) -> bool {
        let payload = match chunk {
            Err(err) => {
                if !self.failed {
                    self.failed = true;
                    let settings = self.manager.settings();
                    let action = match_request_scoped_error_action(&self.auth, &err, &settings);
                    let attempt = Attempt {
                        provider: &self.provider,
                        route_model: &self.route_model,
                        result_model: self.result_model.clone(),
                        session: self.session.as_ref(),
                    };
                    let mut result = failure_result(&attempt, &self.auth, &err, action, true);
                    result.response_headers = stream_failure_headers(&self.headers, &err);
                    self.manager.mark_call_result(&result, attempt.session());
                }
                return self.tx.send(Err(err)).await.is_ok();
            }
            Ok(payload) => payload,
        };
        if payload.is_empty() {
            return true;
        }
        // The tail flushed at the end goes through the rewriter again, as
        // upstream's does.
        let payload = match self.rewriter.as_mut() {
            Some(rewriter) => Bytes::from(rewriter.rewrite_stream_chunk(&payload)),
            None => payload,
        };
        if payload.is_empty() {
            return true;
        }
        self.tx.send(Ok(payload)).await.is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::Format;

    fn opts() -> Options {
        Options::new(Format::from("openai"))
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|&name| name.to_owned()).collect()
    }

    // Ports TestProvidersForExecutionForcedGeminiUsesGeminiProvider (the
    // providers half: the model is the server's, which routes an agent to
    // itself), plus the unforced normalization.
    #[test]
    fn forced_provider_is_the_only_provider() {
        let mut o = opts();
        assert_eq!(
            execution_providers(&names(&[" Gemini ", "gemini", ""]), &o).unwrap(),
            ["gemini"]
        );
        let none = execution_providers(&names(&[" "]), &o).unwrap_err();
        assert!(matches!(none.kind, ErrorKind::ProviderNotFound));

        o.metadata.forced_provider = Some(" Gemini ".into());
        assert_eq!(execution_providers(&[], &o).unwrap(), ["gemini"]);
        assert_eq!(
            execution_providers(&names(&["gemini-interactions", "GEMINI"]), &o).unwrap(),
            ["gemini"]
        );
        o.metadata.forced_provider = Some("  ".into());
        assert_eq!(
            execution_providers(&names(&["codex"]), &o).unwrap(),
            ["codex"]
        );
    }

    // Ports TestProvidersForExecutionForcedGeminiRejectsRouterProvider, with
    // the providers the caller routed to standing in for the model router's.
    #[test]
    fn forced_provider_the_providers_leave_out_is_a_conflict() {
        let mut o = opts();
        o.metadata.forced_provider = Some("gemini".into());
        let err = execution_providers(&names(&["claude"]), &o).unwrap_err();
        assert_eq!(err.http_status(), 400);
        assert_eq!(
            err.to_string(),
            "agent is only supported for native interactions execution"
        );
    }

    #[test]
    fn auth_selection_model_prefers_metadata() {
        let mut o = opts();
        assert_eq!(auth_selection_model(&o, " gpt-5 "), "gpt-5");
        o.metadata.auth_selection_model = Some("  ".into());
        assert_eq!(auth_selection_model(&o, "gpt-5"), "gpt-5");
        o.metadata.auth_selection_model = Some(" pool ".into());
        assert_eq!(auth_selection_model(&o, "gpt-5"), "pool");
        assert_eq!(
            execution_model_for_auth_selection(&o, " gpt-5 "),
            ("gpt-5".to_owned(), true)
        );
        assert_eq!(
            execution_model_for_auth_selection(&o, "pool"),
            (String::new(), false)
        );
        assert_eq!(
            execution_model_for_auth_selection(&o, " "),
            (String::new(), false)
        );
    }

    #[test]
    fn requested_model_kept_when_set() {
        let mut o = opts();
        ensure_requested_model(&mut o, " m ");
        assert_eq!(o.metadata.requested_model, "m");
        ensure_requested_model(&mut o, "other");
        assert_eq!(o.metadata.requested_model, "m");
    }

    #[test]
    fn pool_offset_rotates_and_wraps() {
        let mut offsets = HashMap::new();
        assert_eq!(next_model_pool_offset(&mut offsets, "k", 1), 0);
        assert_eq!(next_model_pool_offset(&mut offsets, " ", 3), 0);
        assert_eq!(next_model_pool_offset(&mut offsets, "k", 3), 0);
        assert_eq!(next_model_pool_offset(&mut offsets, "k", 3), 1);
        assert_eq!(next_model_pool_offset(&mut offsets, "k", 3), 2);
        assert_eq!(next_model_pool_offset(&mut offsets, "k", 3), 0);
        offsets.insert("k".into(), MODEL_POOL_OFFSET_WRAP);
        assert_eq!(next_model_pool_offset(&mut offsets, "k", 3), 0);
        assert_eq!(offsets.get("k"), Some(&1));
    }

    #[test]
    fn tracker_records_and_chains() {
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let mut o = opts();
        let sink = seen.clone();
        o.metadata.selected_auth = Some(Arc::new(move |id: &str| lock(&sink).push(id.into())));
        let (tracked, attempted) = with_attempted_auth_tracker(&o);
        let auth = |id: &str| {
            Arc::new(Auth {
                id: id.into(),
                ..Auth::default()
            })
        };
        publish_selected(&tracked, &auth(" a "));
        publish_selected(&tracked, &auth(" "));
        assert!(lock(&attempted).contains("a"));
        assert_eq!(*lock(&seen), vec!["a".to_owned()]);
    }

    #[test]
    fn websocket_fallback_drops_generate() {
        let mut o = opts();
        o.downstream_websocket = true;
        let auth = Auth::default();
        let req = Request {
            model: "m".into(),
            payload: Bytes::from_static(br#"{"a":1,"generate":false,"b":2}"#),
        };
        let out = sanitize_downstream_websocket_fallback_request(&o, &auth, &req);
        assert_eq!(&out.payload[..], br#"{"a":1,"b":2}"#);
        let plain = Request {
            model: "m".into(),
            payload: Bytes::from_static(br#"{"a": 1}"#),
        };
        let out = sanitize_downstream_websocket_fallback_request(&o, &auth, &plain);
        assert_eq!(&out.payload[..], br#"{"a": 1}"#);
        o.downstream_websocket = false;
        let out = sanitize_downstream_websocket_fallback_request(&o, &auth, &req);
        assert_eq!(out.payload, req.payload);
    }

    /// Not upstream's: sjson deletes `generate` in place, so the client's
    /// numbers keep their text.
    #[test]
    fn websocket_fallback_keeps_number_text() {
        let mut o = opts();
        o.downstream_websocket = true;
        let req = Request {
            model: "m".into(),
            payload: Bytes::from_static(br#"{"a":-0,"generate":false,"b":[1E20,1e5]}"#),
        };
        let out = sanitize_downstream_websocket_fallback_request(&o, &Auth::default(), &req);
        assert_eq!(&out.payload[..], br#"{"a":-0,"b":[1E20,1e5]}"#);
    }

    #[test]
    fn preferred_keeps_cancellation() {
        let fallback = Failure::local(ExecError::canceled());
        let upstream = Failure::upstream(ExecError::upstream(500, "boom"));
        assert_eq!(
            preferred(fallback, Some(upstream.clone())).err.kind,
            ErrorKind::Canceled
        );
        let fallback = Failure::local(ExecError::auth_not_found());
        let chosen = preferred(fallback, Some(upstream));
        assert_eq!(chosen.err.status, 500);
        assert!(chosen.attempted);
    }
}
