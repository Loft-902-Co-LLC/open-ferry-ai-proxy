// Ported from CLIProxyAPI internal/api/handlers/management/
// auth_files_provider_oauth.go (RequestAnthropicToken, RequestCodexToken,
// CancelAuthSession, GetAuthStatus), auth_files_v8.go (StartOAuthV8) and
// auth_files_oauth_callback.go (isWebUIRequest, managementCallbackURL)
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Starting a Claude or Codex login, and following it: the routes that
//! give a login's authorization URL, tell its status and cancel it.
//!
//! A login started here answers `{"state":...,"status":"ok","url":...}`
//! at once, and then waits up to 5 minutes for its callback, which a
//! callback route hands it. It then exchanges the code for tokens with the
//! provider's official OAuth endpoints, through the config's `proxy-url`,
//! within 60 seconds, and saves the credential in the auth directory,
//! where the running service takes it up; the session then completes. A
//! login that fails sets the session's status to say why.
//!
//! Cancelling the session stops its login at once, wherever it is, except
//! while it saves: a login cancelled before its exchange sends no token
//! request, and one cancelled during it drops the request and saves
//! nothing. When the service shuts down, every login stops. However a
//! login ends, its forwarder stops, and its session is dropped unless the
//! login completed or failed it.
//!
//! No log or answer holds a login's code, PKCE verifier or tokens. A
//! failed exchange is logged with a fixed message and the token endpoint's
//! status, and a callback's error with a fixed message and the error, only
//! when it is one RFC 6749 defines.
//!
//! With `is_webui` set (to `1`, `true`, `yes` or `on`), a callback
//! forwarder is started on the port of the provider's redirect URI, which
//! sends the browser on to the main server's callback page, over `https`
//! when the config enables TLS.
//!
//! Deviations from upstream:
//! - Only Claude and Codex logins are served. The v8 `oauth/auth-url`
//!   answers 404 `{"error":"provider_not_found"}` for every other provider,
//!   as upstream answers one it doesn't know, and the v0 `*-auth-url` of
//!   the others are unported.
//! - The `state` is 32 random bytes in unpadded URL-safe base64; upstream's
//!   is 16 in hex.
//! - A login that can't start its forwarder, or can't tell where the main
//!   server listens, drops its session; upstream leaves it pending, with
//!   nothing waiting on it.
//! - A login cancelled before its exchange sends no token request, and one
//!   cancelled during it drops the request at once. Upstream's runs the
//!   exchange to its end, then saves nothing.
//! - The exchange, until the credential is made, has 60 seconds: past
//!   that, the session fails with `Timeout exchanging authorization code
//!   for tokens`. Upstream's has no deadline.
//! - When the service shuts down, the logins stop and drop their sessions,
//!   and a login can't start after that: it answers 503 `{"error":"server
//!   shutting down"}`. Upstream's logins run until the process exits.
//! - A failed exchange is logged with a fixed message and the token
//!   endpoint's status, where upstream logs the error, which quotes the
//!   endpoint's answer, and so may hold the code. A Codex session's status
//!   keeps upstream's wording, the answer included, but with the login's
//!   code and PKCE verifier replaced by `[redacted]`.
//! - A login can't start while 1024 sessions are kept: it answers 429
//!   `{"error":"too many oauth sessions"}`.
//! - A credential without an email isn't saved: the session fails with
//!   `Failed to save authentication tokens:` and the reason. Upstream saves
//!   it under a name without one. A Codex account ID is hashed for the file
//!   name after trimming.
//! - A Claude login saves no device IDs: open-ferry never reads or sends
//!   them.
//! - A callback can't carry another login's state, so there is no `State
//!   code error`.
//! - The status of a login doesn't poll a plugin.

use std::fmt::Write as _;
use std::time::Duration;

use axum::extract::{RawQuery, State};
use axum::response::Response;
use http::StatusCode;
use open_ferry_core::auth::Auth;
use open_ferry_core::config::Config;
use open_ferry_providers::claude::oauth::{self as claude, ClaudeAuth};
use open_ferry_providers::codex::oauth::{self as codex, CodexAuth};
use open_ferry_providers::oauth::{Pkce, generate_state};
use open_ferry_translate::go::{to_lower, trim_space};

use super::Provider;
use super::forwarder::Started;
use super::sessions::{Registration, error_with_cause, is_valid_state};
use crate::go::lossy;
use crate::json::{self, Json};
use crate::query::Query;
use crate::state::ManagementState;
use crate::token_record::save_token_record;

/// How long a login waits for its callback.
pub(super) const WAIT_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// How long a login's code exchange may take, until its credential is made.
pub(super) const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(60);

/// The status of a login whose code couldn't be exchanged.
const EXCHANGE_FAILED: &str = "Failed to exchange authorization code for tokens";

/// The status of a login whose code exchange took too long.
const EXCHANGE_TIMED_OUT: &str = "Timeout exchanging authorization code for tokens";

/// The status of a login whose credential couldn't be saved.
const SAVE_FAILED: &str = "Failed to save authentication tokens";

/// What stands in a session's status for a secret.
const REDACTED: &str = "[redacted]";

/// The errors RFC 6749 (section 4.1.2.1) lets an authorization server send
/// to the redirect URI: the only callback errors logged as they came.
const CALLBACK_ERRORS: [&str; 7] = [
    "invalid_request",
    "unauthorized_client",
    "access_denied",
    "unsupported_response_type",
    "invalid_scope",
    "server_error",
    "temporarily_unavailable",
];

/// `GET /v0/management/anthropic-auth-url` (`RequestAnthropicToken`).
pub(super) async fn anthropic_auth_url(
    State(state): State<ManagementState>,
    RawQuery(query): RawQuery,
) -> Response {
    start(&state, Provider::Claude, &Query::parse(query.as_deref())).await
}

/// `GET /v0/management/codex-auth-url` (`RequestCodexToken`).
pub(super) async fn codex_auth_url(
    State(state): State<ManagementState>,
    RawQuery(query): RawQuery,
) -> Response {
    start(&state, Provider::Codex, &Query::parse(query.as_deref())).await
}

/// `GET /v8/management/oauth/auth-url?provider=...` (`StartOAuthV8`): a
/// `claude` or `codex` login.
pub(super) async fn auth_url(
    State(state): State<ManagementState>,
    RawQuery(query): RawQuery,
) -> Response {
    let query = Query::parse(query.as_deref());
    let provider = to_lower(&lossy(trim_space(query.value("provider"))));
    match provider.as_str() {
        "" => json::error(StatusCode::BAD_REQUEST, "provider is required"),
        "claude" => start(&state, Provider::Claude, &query).await,
        "codex" => start(&state, Provider::Codex, &query).await,
        _ => json::error(StatusCode::NOT_FOUND, "provider_not_found"),
    }
}

/// Starts a `provider` login, and answers with its authorization URL and
/// state.
async fn start(state: &ManagementState, provider: Provider, query: &Query) -> Response {
    let sessions = state.oauth_sessions();
    let config = state.config();
    let pkce = Pkce::generate();
    let oauth_state = generate_state();
    let client = Client::new(state, provider, &config);
    let url = client.auth_url(&oauth_state, &pkce);

    let Some(registration) = sessions.store().register(&oauth_state, provider.name()) else {
        return json::error(StatusCode::TOO_MANY_REQUESTS, "too many oauth sessions");
    };
    // From here, a login that doesn't start drops its session.
    let login = Login {
        client,
        state: oauth_state.clone(),
        pkce,
        _lease: Lease {
            management: state.clone(),
            state: oauth_state.clone(),
            id: registration.id,
        },
    };

    let mut forwarder = None;
    if is_web_ui(query) {
        let Some(target) = callback_url(&config, provider.page()) else {
            tracing::error!("Can't forward the {provider} OAuth callback: no server port");
            return json::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "callback server unavailable",
            );
        };
        let port = sessions.callback_port(provider);
        match sessions.forwarders().start(port, target).await {
            Ok(started) => forwarder = Some(started),
            Err(error) => {
                tracing::error!("Failed to start the {provider} OAuth callback forwarder: {error}");
                return json::error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "failed to start callback server",
                );
            }
        }
    }

    if !sessions.spawn(run(state.clone(), login, registration, forwarder)) {
        return json::error(StatusCode::SERVICE_UNAVAILABLE, "server shutting down");
    }
    json::response(
        StatusCode::OK,
        &Json::map([
            ("status", Json::Str("ok".into())),
            ("url", Json::Str(url)),
            ("state", Json::Str(oauth_state)),
        ]),
    )
}

/// Whether the login was started from the web UI: `is_webui` is `1`,
/// `true`, `yes` or `on`, after trimming and in any case
/// (`isWebUIRequest`).
fn is_web_ui(query: &Query) -> bool {
    let value = to_lower(&lossy(trim_space(query.value("is_webui"))));
    matches!(value.as_str(), "1" | "true" | "yes" | "on")
}

/// The URL of the main server's `path` on 127.0.0.1, or `None` when the
/// config has no port (`managementCallbackURL`).
fn callback_url(config: &Config, path: &str) -> Option<String> {
    if config.port <= 0 {
        return None;
    }
    let scheme = if config.tls.enable { "https" } else { "http" };
    Some(format!("{scheme}://127.0.0.1:{}{path}", config.port))
}

/// A login waiting for its callback.
struct Login {
    client: Client,
    /// The session's state.
    state: String,
    pkce: Pkce,
    _lease: Lease,
}

/// A login's hold on its session: when the login ends, however it ends,
/// the session is dropped if it is still pending.
struct Lease {
    management: ManagementState,
    /// The session's state.
    state: String,
    /// The session's registration.
    id: u64,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.management
            .oauth_sessions()
            .store()
            .release(&self.state, self.id);
    }
}

/// What calls the provider's OAuth endpoints.
enum Client {
    Claude(ClaudeAuth),
    Codex(CodexAuth),
}

impl Client {
    /// A client for `provider` that calls through the config's
    /// `proxy-url`.
    fn new(state: &ManagementState, provider: Provider, config: &Config) -> Self {
        let sessions = state.oauth_sessions();
        match provider {
            Provider::Claude => Self::Claude(
                ClaudeAuth::with_proxy_url(&config.proxy_url)
                    .with_endpoints(sessions.claude_endpoints()),
            ),
            Provider::Codex => Self::Codex(
                CodexAuth::with_proxy_url(&config.proxy_url)
                    .with_endpoints(sessions.codex_endpoints()),
            ),
        }
    }

    fn provider(&self) -> Provider {
        match self {
            Self::Claude(_) => Provider::Claude,
            Self::Codex(_) => Provider::Codex,
        }
    }

    /// The URL that starts the login in the browser.
    fn auth_url(&self, state: &str, pkce: &Pkce) -> String {
        match self {
            Self::Claude(auth) => auth.generate_auth_url(state, pkce),
            Self::Codex(auth) => auth.generate_auth_url(state, pkce),
        }
    }

    /// The credential `code` gives, or the session's status saying why
    /// there is none, which may quote the token endpoint's answer.
    async fn credential(&self, code: &str, state: &str, pkce: &Pkce) -> Result<Auth, String> {
        let provider = self.provider();
        match self {
            Self::Claude(auth) => {
                // Claude may give the code with `#` and the state after it.
                let code = code.split_once('#').map_or(code, |(code, _)| code);
                let bundle = auth
                    .exchange_code_for_tokens(code, state, pkce)
                    .await
                    .map_err(|error| {
                        log_exchange_failure(provider, error.status());
                        EXCHANGE_FAILED.to_owned()
                    })?;
                claude::build_auth_record(&bundle)
                    .map_err(|error| error_with_cause(SAVE_FAILED, error.message()))
            }
            Self::Codex(auth) => {
                let bundle = auth
                    .exchange_code_for_tokens(code, pkce)
                    .await
                    .map_err(|error| {
                        log_exchange_failure(provider, error.status());
                        error_with_cause(EXCHANGE_FAILED, error.message())
                    })?;
                codex::build_auth_record(&bundle)
                    .map_err(|error| error_with_cause(SAVE_FAILED, error.message()))
            }
        }
    }
}

/// Waits for `login`'s callback, then makes and saves its credential and
/// completes its session, or sets the session's status saying why it
/// failed. Returns at once when the session is cancelled or ends
/// otherwise, except while it saves; `forwarder` stops when this returns.
async fn run(
    state: ManagementState,
    login: Login,
    registration: Registration,
    forwarder: Option<Started>,
) {
    let _forwarder = forwarder;
    let Registration {
        callback,
        mut lifeline,
        ..
    } = registration;
    let sessions = state.oauth_sessions();
    let store = sessions.store();
    let provider = login.client.provider();
    let callback = tokio::select! {
        callback = callback => callback,
        () = tokio::time::sleep(sessions.wait_timeout()) => {
            tracing::error!("Timed out waiting for the {provider} OAuth callback");
            store.set_error(&login.state, "Timeout waiting for OAuth callback");
            return;
        }
    };
    // Without a callback, the session was cancelled, ended or replaced.
    let Ok(callback) = callback else {
        return;
    };
    if !callback.error.is_empty() {
        log_callback_error(provider, &callback.error);
        store.set_error(&login.state, provider.callback_error());
        return;
    }
    let exchange = tokio::time::timeout(
        sessions.exchange_timeout(),
        login
            .client
            .credential(&callback.code, &login.state, &login.pkce),
    );
    let record = tokio::select! {
        // The lifeline first: a login whose session ended since its callback
        // came sends no token request, and one whose session ends meanwhile
        // drops it.
        biased;
        () = lifeline.cut() => return,
        exchanged = exchange => match exchanged {
            Ok(Ok(record)) => record,
            Ok(Err(status)) => {
                // Codex's status quotes the token endpoint's answer, which
                // may quote the request: the code, as is or as sent.
                let code = &callback.code;
                let secrets = [code, &query_escape(code), &login.pkce.verifier];
                store.set_error(&login.state, &redact(&status, &secrets));
                return;
            }
            Err(_) => {
                tracing::error!("Timed out exchanging the {provider} authorization code for tokens");
                store.set_error(&login.state, EXCHANGE_TIMED_OUT);
                return;
            }
        },
    };
    // A login whose session ended as the exchange did saves nothing.
    if !store.is_pending(&login.state, provider.name()) {
        return;
    }
    match save_token_record(&state, record).await {
        Ok(path) => {
            tracing::info!("{provider} authentication successful; credential saved to {path}");
            store.complete(&login.state);
        }
        Err(error) => {
            tracing::error!("{SAVE_FAILED}: {error}");
            store.set_error(&login.state, SAVE_FAILED);
        }
    }
}

/// Logs that `provider`'s code exchange failed, with the token endpoint's
/// `status` if it answered with one, but nothing it said: its answer may
/// quote the code.
fn log_exchange_failure(provider: Provider, status: u16) {
    if status == 0 {
        tracing::error!("{EXCHANGE_FAILED} ({provider})");
    } else {
        tracing::error!("{EXCHANGE_FAILED} ({provider}): the token endpoint answered {status}");
    }
}

/// Logs that `provider`'s OAuth callback reported `error`, naming it only
/// when RFC 6749 defines it: anyone may send the callback anything.
fn log_callback_error(provider: Provider, error: &str) {
    if CALLBACK_ERRORS.contains(&error) {
        tracing::error!("The {provider} OAuth callback reported an error: {error}");
    } else {
        tracing::error!("The {provider} OAuth callback reported an error");
    }
}

/// `text` with each of `secrets` that isn't empty replaced by
/// `[redacted]`, the longest first.
fn redact(text: &str, secrets: &[&String]) -> String {
    let mut secrets: Vec<&str> = secrets
        .iter()
        .map(|secret| secret.as_str())
        .filter(|secret| !secret.is_empty())
        .collect();
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    let mut text = text.to_owned();
    for secret in secrets {
        text = text.replace(secret, REDACTED);
    }
    text
}

/// `text` as Go's `url.QueryEscape` gives it, as the Codex exchange sends
/// the code.
fn query_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for &byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte));
            }
            b' ' => out.push('+'),
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// `GET /v0/management/get-auth-status` (`GetAuthStatus`): `ok` once the
/// login of `state` completed, `wait` while it is pending, else `error`
/// with why.
pub(super) async fn status(
    State(state): State<ManagementState>,
    RawQuery(query): RawQuery,
) -> Response {
    let query = Query::parse(query.as_deref());
    let oauth_state = lossy(trim_space(query.value("state")));
    if oauth_state.is_empty() {
        return answer("ok", None);
    }
    if !is_valid_state(&oauth_state) {
        return json::response(
            StatusCode::BAD_REQUEST,
            &outcome("error", Some("invalid state")),
        );
    }
    match state.oauth_sessions().store().get(&oauth_state) {
        None => answer("error", Some("unknown or expired state")),
        Some(session) if session.completed => answer("ok", None),
        Some(session) if !session.status.is_empty() => answer("error", Some(&session.status)),
        Some(_) => answer("wait", None),
    }
}

/// `DELETE /v0/management/oauth-session` (`CancelAuthSession`): cancels
/// the pending login of `state`.
pub(super) async fn cancel(
    State(state): State<ManagementState>,
    RawQuery(query): RawQuery,
) -> Response {
    let query = Query::parse(query.as_deref());
    let oauth_state = lossy(trim_space(query.value("state")));
    if oauth_state.is_empty() {
        return json::response(
            StatusCode::BAD_REQUEST,
            &outcome("error", Some("missing state")),
        );
    }
    if !is_valid_state(&oauth_state) {
        return json::response(
            StatusCode::BAD_REQUEST,
            &outcome("error", Some("invalid state")),
        );
    }
    let cancelled = state.oauth_sessions().store().cancel(&oauth_state);
    json::response(
        StatusCode::OK,
        &Json::map([
            ("status", Json::Str("ok".into())),
            ("cancelled", Json::Bool(cancelled)),
        ]),
    )
}

/// `{"status":status}`, with `"error":error` if given, with 200.
fn answer(status: &str, error: Option<&str>) -> Response {
    json::response(StatusCode::OK, &outcome(status, error))
}

/// `{"status":status}`, with `"error":error` if given.
fn outcome(status: &str, error: Option<&str>) -> Json {
    match error {
        Some(error) => Json::map([
            ("status", Json::Str(status.to_owned())),
            ("error", Json::Str(error.to_owned())),
        ]),
        None => Json::map([("status", Json::Str(status.to_owned()))]),
    }
}
