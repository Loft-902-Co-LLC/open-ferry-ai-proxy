// Ported from CLIProxyAPI internal/api/handlers/management/
// auth_files_provider_oauth.go (RequestAnthropicToken, RequestCodexToken,
// CancelAuthSession, GetAuthStatus), auth_files_v8.go (StartOAuthV8) and
// auth_files_oauth_callback.go (isWebUIRequest, managementCallbackURL)
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Starting a Claude or Codex login, and following it: the routes that
//! give a login's authorization URL, tell its status and cancel it.
//!
//! A login started here answers `{"state":...,"status":"ok","url":...}`
//! at once, and then waits up to 5 minutes for its callback, which a
//! callback route hands it. It then exchanges the code for tokens with the
//! provider's official OAuth endpoints, through the config's `proxy-url`,
//! and saves the credential in the auth directory, where the running
//! service takes it up; the session then completes. A login that fails
//! sets the session's status to say why. Cancelling the session stops the
//! wait at once, and a login cancelled while it exchanges the code saves
//! nothing.
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
use tokio::sync::oneshot;

use super::Provider;
use super::forwarder::Started;
use super::sessions::{Callback, error_with_cause, is_valid_state};
use crate::go::lossy;
use crate::json::{self, Json};
use crate::query::Query;
use crate::state::ManagementState;
use crate::token_record::save_token_record;

/// How long a login waits for its callback.
pub(super) const WAIT_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// The status of a login whose code couldn't be exchanged.
const EXCHANGE_FAILED: &str = "Failed to exchange authorization code for tokens";

/// The status of a login whose credential couldn't be saved.
const SAVE_FAILED: &str = "Failed to save authentication tokens";

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

    let Some(callback) = sessions.store().register(&oauth_state, provider.name()) else {
        return json::error(StatusCode::TOO_MANY_REQUESTS, "too many oauth sessions");
    };

    let mut forwarder = None;
    if is_web_ui(query) {
        let Some(target) = callback_url(&config, provider.page()) else {
            tracing::error!("Can't forward the {provider} OAuth callback: no server port");
            sessions.store().cancel(&oauth_state);
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
                sessions.store().cancel(&oauth_state);
                return json::error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "failed to start callback server",
                );
            }
        }
    }

    tokio::spawn(wait(
        state.clone(),
        Login {
            client,
            state: oauth_state.clone(),
            pkce,
        },
        callback,
        forwarder,
    ));
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
    /// there is none.
    async fn credential(&self, code: &str, state: &str, pkce: &Pkce) -> Result<Auth, String> {
        match self {
            Self::Claude(auth) => {
                // Claude may give the code with `#` and the state after it.
                let code = code.split_once('#').map_or(code, |(code, _)| code);
                let bundle = auth
                    .exchange_code_for_tokens(code, state, pkce)
                    .await
                    .map_err(|error| {
                        tracing::error!("{EXCHANGE_FAILED}: {error}");
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
                        tracing::error!("{EXCHANGE_FAILED}: {error}");
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
/// otherwise; `forwarder` stops when this returns.
async fn wait(
    state: ManagementState,
    login: Login,
    receiver: oneshot::Receiver<Callback>,
    forwarder: Option<Started>,
) {
    let _forwarder = forwarder;
    let store = state.oauth_sessions().store();
    let provider = login.client.provider();
    let callback = tokio::select! {
        callback = receiver => callback,
        () = tokio::time::sleep(state.oauth_sessions().wait_timeout()) => {
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
        tracing::error!(
            "The {provider} OAuth callback reported an error: {:?}",
            callback.error
        );
        store.set_error(&login.state, provider.callback_error());
        return;
    }
    let record = match login
        .client
        .credential(&callback.code, &login.state, &login.pkce)
        .await
    {
        Ok(record) => record,
        Err(status) => {
            store.set_error(&login.state, &status);
            return;
        }
    };
    // A login cancelled while it exchanged the code saves nothing.
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
