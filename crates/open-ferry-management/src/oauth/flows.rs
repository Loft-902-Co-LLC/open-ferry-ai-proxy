// Ported from CLIProxyAPI internal/api/handlers/management/
// auth_files_provider_oauth.go (RequestAnthropicToken, RequestCodexToken,
// CancelAuthSession, GetAuthStatus), auth_files_v8.go (StartOAuthV8) and
// auth_files_oauth_callback.go (isWebUIRequest, managementCallbackURL)
// (v8.0.38, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Starting a Claude or Codex login, and following it: the routes that
//! give a login's authorization URL, tell its status and cancel it.
//!
//! A login started here answers `{"state":...,"status":"ok","url":...}`
//! at once, and then waits up to 5 minutes for its callback, which a
//! callback route hands it. It then exchanges the code for tokens with the
//! provider's official OAuth endpoints, through its explicit session proxy
//! or the existing client defaults when none was supplied, within 60 seconds,
//! and saves the credential in the auth directory,
//! where the running service takes it up; the session then completes. A
//! login that fails sets the session's status to say why.
//!
//! Cancelling the session stops its login at once, wherever it is, except
//! while it saves: a login cancelled before its exchange sends no token
//! request, and one cancelled during it drops the request and saves
//! nothing. Shutdown stops login owners after a bounded grace for saving
//! binds. Claimed saves and their finalizers remain gate-owned until they
//! record their durable outcome, while the process stays alive. However a
//! login owner ends, its forwarder stops; only an unclaimed pending session
//! is dropped.
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
//! - When the service shuts down, saving SIWC binds have a ten-second grace
//!   before ordinary login owners stop. Post-fence collectors survive that
//!   abort while the process stays alive. New logins answer 503
//!   `{"error":"server shutting down"}`. Upstream's logins run until exit.
//! - A failed exchange is logged with a fixed message and the token
//!   endpoint's status, where upstream logs the error, which quotes the
//!   endpoint's answer, and so may hold the code. A Codex session's status
//!   keeps upstream's wording, the answer included, but with the login's
//!   code and PKCE verifier replaced by `[redacted]`.
//! - A saved credential is logged, at info, with its file name's email
//!   masked (`claude-j***@e***.com.json`) and without its directory;
//!   upstream prints the full path to standard output.
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
use http::{Method, StatusCode};
use open_ferry_core::auth::Auth;
use open_ferry_core::config::Config;
use open_ferry_providers::claude::oauth::{self as claude, ClaudeAuth};
use open_ferry_providers::codex::oauth::{self as codex, CodexAuth};
use open_ferry_providers::oauth::{CALLBACK_ERRORS, Pkce, generate_state};
use open_ferry_translate::go::{to_lower, trim_space};

use super::Provider;
use super::forwarder::Started;
use super::sessions::{Callback, Lifeline, Registration, error_with_cause, is_valid_state};
use crate::go::lossy;
use crate::json::{self, Json};
use crate::query::Query;
use crate::state::ManagementState;
use crate::token_record::{apply_session_proxy, save_token_record};

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

/// `GET` or `POST /v8/management/oauth/auth-url` (`StartOAuthV8`): a
/// `claude` or `codex` login. A session proxy is accepted only in a POST body.
pub(crate) async fn auth_url(
    State(state): State<ManagementState>,
    RawQuery(query): RawQuery,
    method: Method,
    body: axum::body::Body,
) -> Response {
    let query = Query::parse(query.as_deref());
    if query.get("proxy_url").is_some() || query.get("proxy-url").is_some() {
        return json::error(
            StatusCode::BAD_REQUEST,
            "proxy_url must be provided in the request body",
        );
    }
    let body = match crate::bind::read_body(body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let options = match StartOptions::parse(&method, &query, &body) {
        Ok(options) => options,
        Err(error) => return json::error(StatusCode::BAD_REQUEST, error),
    };
    let provider = to_lower(&options.provider);
    let mode = query.value("mode");
    if !mode.is_empty() && mode != b"siwc-bind" {
        return json::error(StatusCode::BAD_REQUEST, "unsupported oauth mode");
    }
    if mode == b"siwc-bind" && provider != "codex" {
        return json::error(StatusCode::BAD_REQUEST, "SIWC bind requires codex");
    }
    if mode == b"siwc-bind" && !options.proxy_url.is_empty() {
        return json::error(
            StatusCode::BAD_REQUEST,
            "session proxy is not supported for SIWC bind",
        );
    }
    match provider.as_str() {
        "" => json::error(StatusCode::BAD_REQUEST, "provider is required"),
        "claude" => start_with_options(&state, Provider::Claude, options, None).await,
        "codex" if query.value("mode") == b"siwc-bind" => {
            if state.credential_store().is_err() {
                return json::error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "credential store unavailable",
                );
            }
            let name = lossy(query.value("name"));
            let _mutation = state.credential_lock().lock().await;
            match super::siwc_bind::prepare(&state, &name).await {
                Ok(bind) => start_with_options(&state, Provider::Codex, options, Some(bind)).await,
                Err(_) => json::error(StatusCode::BAD_REQUEST, "invalid SIWC bind target"),
            }
        }
        "codex" => start_with_options(&state, Provider::Codex, options, None).await,
        _ => json::error(StatusCode::NOT_FOUND, "provider_not_found"),
    }
}

/// `POST /v8/management/oauth/siwc/bind`, without a server-side callback listener.
pub(super) async fn siwc_bind(
    State(state): State<ManagementState>,
    body: axum::body::Body,
) -> Response {
    let body = match crate::bind::read_body(body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let value = match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(value) if value.is_object() => value,
        _ => return json::error(StatusCode::BAD_REQUEST, "invalid request body"),
    };
    let Some(name) = value.get("name").and_then(serde_json::Value::as_str) else {
        return json::error(StatusCode::BAD_REQUEST, "name is required");
    };
    let name = name.trim();
    if name.is_empty() {
        return json::error(StatusCode::BAD_REQUEST, "name is required");
    }
    if state.credential_store().is_err() {
        return json::error(
            StatusCode::SERVICE_UNAVAILABLE,
            "credential store unavailable",
        );
    }
    let _mutation = state.credential_lock().lock().await;
    if !state
        .manager()
        .list()
        .iter()
        .any(|a| a.id == name || a.file_name == name)
    {
        return json::error(StatusCode::NOT_FOUND, "auth file not found");
    }
    match super::siwc_bind::prepare(&state, name).await {
        Ok(bind) => start_with_bind(&state, Provider::Codex, &Query::default(), Some(bind)).await,
        Err(_) => json::error(StatusCode::BAD_REQUEST, "invalid SIWC bind target"),
    }
}

/// Starts a `provider` login, and answers with its authorization URL and
/// state.
async fn start(state: &ManagementState, provider: Provider, query: &Query) -> Response {
    start_with_bind(state, provider, query, None).await
}

async fn start_with_bind(
    state: &ManagementState,
    provider: Provider,
    query: &Query,
    bind: Option<super::siwc_bind::Bind>,
) -> Response {
    start_with_options(state, provider, StartOptions::from_query(query), bind).await
}

async fn start_with_options(
    state: &ManagementState,
    provider: Provider,
    options: StartOptions,
    bind: Option<super::siwc_bind::Bind>,
) -> Response {
    let sessions = state.oauth_sessions();
    let config = state.config();
    let pkce = Pkce::generate();
    let oauth_state = generate_state();
    let client = match bind {
        Some(bind) => Client::Siwc(Box::new(bind)),
        None => match Client::for_session(state, provider, &config, &options.proxy_url) {
            Ok(client) => client,
            Err(()) => {
                return json::error(
                    StatusCode::BAD_GATEWAY,
                    "failed to create OAuth proxy transport",
                );
            }
        },
    };
    let redirect = match provider {
        Provider::Codex => {
            open_ferry_providers::codex::siwc::redirect_uri(sessions.callback_port(provider))
        }
        Provider::Claude => String::new(),
    };
    let url = match &client {
        Client::Siwc(bind) => match bind.auth_url(&oauth_state, &redirect, &pkce) {
            Ok(url) => url,
            Err(_) => {
                return json::error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "failed to generate authorization URL",
                );
            }
        },
        _ => match client.auth_url(&oauth_state, &pkce) {
            Ok(url) => url,
            Err(reason) => return json::error(StatusCode::INTERNAL_SERVER_ERROR, reason),
        },
    };

    let registration = if options.proxy_url.is_empty() {
        sessions.store().register(&oauth_state, provider.name())
    } else {
        sessions
            .store()
            .register_with_proxy(&oauth_state, provider.name(), &options.proxy_url)
    };
    let Some(registration) = registration else {
        return json::error(StatusCode::TOO_MANY_REQUESTS, "too many oauth sessions");
    };
    if let Client::Siwc(bind) = &client
        && !sessions
            .store()
            .mark_bind_target(&oauth_state, bind.target.mutation_path(), &redirect)
    {
        sessions.store().release(&oauth_state, registration.id);
        return json::error(
            StatusCode::CONFLICT,
            "a SIWC binding is already pending for this credential",
        );
    }
    let is_bind = matches!(client, Client::Siwc(_));
    // From here, a login that doesn't start drops its session.
    let login = Login {
        client,
        state: oauth_state.clone(),
        pkce,
        redirect,
        proxy_url: options.proxy_url,
        _lease: Lease {
            management: state.clone(),
            state: oauth_state.clone(),
            id: registration.id,
        },
    };

    let mut forwarder = None;
    if options.is_webui {
        let Some(target) = callback_url(
            &config,
            if is_bind {
                "/v8/management/oauth/callback"
            } else {
                provider.page()
            },
        ) else {
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
    let mut response = vec![
        ("status", Json::Str("ok".into())),
        ("url", Json::Str(url)),
        ("state", Json::Str(oauth_state)),
    ];
    if is_bind {
        response.extend([
            ("flow", Json::Str("siwc_bind".into())),
            ("expires_in", Json::Int(300)),
        ]);
    }
    json::response(StatusCode::OK, &Json::Struct(response))
}

/// Native login options; deliberately no `Debug`, as a proxy can carry credentials.
struct StartOptions {
    provider: String,
    proxy_url: String,
    is_webui: bool,
}

impl StartOptions {
    fn from_query(query: &Query) -> Self {
        Self {
            provider: lossy(trim_space(query.value("provider"))),
            proxy_url: String::new(),
            is_webui: is_web_ui(query),
        }
    }

    fn parse(method: &Method, query: &Query, body: &[u8]) -> Result<Self, &'static str> {
        let mut options = Self::from_query(query);
        if trim_space(body).is_empty() {
            return Ok(options);
        }
        if method != Method::POST {
            return Err("request body requires POST");
        }
        let value: serde_json::Value =
            serde_json::from_slice(body).map_err(|_| "invalid request body")?;
        let object = value.as_object().ok_or("invalid request body")?;
        let string = |key, error| -> Result<&str, &'static str> {
            match object.get(key) {
                None | Some(serde_json::Value::Null) => Ok(""),
                Some(serde_json::Value::String(value)) => Ok(value.trim()),
                _ => Err(error),
            }
        };
        let provider = string("provider", "invalid provider")?;
        if !options.provider.is_empty()
            && !provider.is_empty()
            && !crate::go::equal_fold(&options.provider, provider)
        {
            return Err("conflicting provider parameter");
        }
        if options.provider.is_empty() {
            provider.clone_into(&mut options.provider);
        }
        let snake = string("proxy_url", "invalid proxy_url")?;
        let kebab = string("proxy-url", "invalid proxy_url")?;
        if !snake.is_empty() && !kebab.is_empty() && snake != kebab {
            return Err("invalid proxy_url");
        }
        let proxy = if snake.is_empty() { kebab } else { snake };
        validate_session_proxy(proxy)?;
        proxy.clone_into(&mut options.proxy_url);
        // These aliases are unused by native providers, but their types are
        // checked before dispatch, with domain taking precedence upstream.
        if object.get("domain").is_some_and(|value| !value.is_null()) {
            string("domain", "invalid domain")?;
        } else {
            string("channel", "invalid channel")?;
        }
        let body_webui = match object.get("is_webui") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::Bool(value)) => Some(*value),
            Some(serde_json::Value::String(value)) => match to_lower(value.trim()).as_str() {
                "1" | "true" | "yes" | "on" => Some(true),
                "0" | "false" | "no" | "off" | "" => Some(false),
                _ => return Err("invalid is_webui"),
            },
            _ => return Err("invalid is_webui"),
        };
        if trim_space(query.value("is_webui")).is_empty()
            && let Some(webui) = body_webui
        {
            options.is_webui = webui;
        }
        Ok(options)
    }
}

/// Accepts only proxy endpoints, not direct selectors or URLs with routing data.
fn validate_session_proxy(raw: &str) -> Result<(), &'static str> {
    if raw.is_empty() {
        return Ok(());
    }
    let invalid = "invalid proxy_url";
    let go = crate::go_url::parse(raw.as_bytes()).ok_or(invalid)?;
    let url = url::Url::parse(raw).map_err(|_| invalid)?;
    if !matches!(go.scheme.as_str(), "http" | "https" | "socks5" | "socks5h")
        || url.host_str().is_none_or(str::is_empty)
        || go.host.ends_with(b":")
        || (go.path != b"/" && !go.path.is_empty())
        || url.query().is_some_and(|query| !query.is_empty())
        || url.fragment().is_some_and(|fragment| !fragment.is_empty())
    {
        return Err(invalid);
    }
    // Check the original authority: WHATWG parsing drops default ports.
    let host = lossy(&go.host);
    let port = if host.starts_with('[') {
        host.split_once(']')
            .and_then(|(_, tail)| tail.strip_prefix(':'))
    } else {
        host.rsplit_once(':').map(|(_, port)| port)
    };
    if port.is_some_and(|port| port.parse::<u16>().map_or(true, |port| port == 0))
        || (matches!(go.scheme.as_str(), "socks5" | "socks5h") && port.is_none())
    {
        return Err(invalid);
    }
    // This build has no SOCKS transport. Reqwest accepts its URLs even without
    // that feature, so reject it here rather than misroute an explicit override.
    if !matches!(go.scheme.as_str(), "http" | "https") {
        return Err(invalid);
    }
    reqwest::Proxy::all(raw).map_err(|_| invalid)?;
    Ok(())
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
    redirect: String,
    proxy_url: String,
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
    Siwc(Box<super::siwc_bind::Bind>),
}

enum Credential {
    Login(Box<Auth>),
    Bind(Box<open_ferry_providers::codex::token::SiwcTokenData>),
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

    /// An explicit session proxy gets a fallible, isolated transport. Unlike
    /// the default constructors, this path never falls back to another route.
    fn for_session(
        state: &ManagementState,
        provider: Provider,
        config: &Config,
        proxy_url: &str,
    ) -> Result<Self, ()> {
        if proxy_url.is_empty() {
            return Ok(Self::new(state, provider, config));
        }
        let proxy = reqwest::Proxy::all(proxy_url).map_err(|_| ())?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .proxy(proxy)
            .user_agent(concat!("open-ferry/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 10 {
                    attempt.error("too many OAuth redirects")
                } else if attempt
                    .previous()
                    .first()
                    .is_some_and(|first| first.origin() == attempt.url().origin())
                {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .build()
            .map_err(|_| ())?;
        let sessions = state.oauth_sessions();
        Ok(match provider {
            Provider::Claude => {
                Self::Claude(ClaudeAuth::new(client).with_endpoints(sessions.claude_endpoints()))
            }
            Provider::Codex => {
                Self::Codex(CodexAuth::new(client).with_endpoints(sessions.codex_endpoints()))
            }
        })
    }

    fn provider(&self) -> Provider {
        match self {
            Self::Claude(_) => Provider::Claude,
            Self::Codex(_) | Self::Siwc(_) => Provider::Codex,
        }
    }

    /// The URL that starts the login in the browser.
    fn auth_url(&self, state: &str, pkce: &Pkce) -> Result<String, &'static str> {
        match self {
            Self::Claude(auth) => Ok(auth.generate_auth_url(state, pkce)),
            Self::Codex(auth) => Ok(auth.generate_auth_url(state, pkce)),
            Self::Siwc(_) => Err("bind URL requires a redirect URI"),
        }
    }

    /// The credential `code` gives, or the session's status saying why
    /// there is none, which may quote the token endpoint's answer.
    async fn credential(
        &self,
        callback: &Callback,
        state: &str,
        redirect: &str,
        pkce: &Pkce,
    ) -> Result<Credential, String> {
        let code = callback.code.as_str();
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
                    .map(|record| Credential::Login(Box::new(record)))
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
                    .map(|record| Credential::Login(Box::new(record)))
                    .map_err(|error| error_with_cause(SAVE_FAILED, error.message()))
            }
            Self::Siwc(bind) => {
                if callback.client_id.trim().is_empty() {
                    return Err("callback did not contain an issued client ID".into());
                }
                bind.client
                    .exchange_code_for_tokens(
                        code,
                        &callback.client_id,
                        redirect,
                        &bind.target.device_id,
                        pkce,
                    )
                    .await
                    .map(|tokens| Credential::Bind(Box::new(tokens)))
                    .map_err(|error| {
                        log_exchange_failure(provider, error.status());
                        EXCHANGE_FAILED.to_owned()
                    })
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
        callback, lifeline, ..
    } = registration;
    let sessions = state.oauth_sessions();
    let store = sessions.store();
    let provider = login.client.provider();
    let callback = tokio::select! {
        callback = callback => callback,
        () = tokio::time::sleep(if matches!(login.client, Client::Siwc(_)) { sessions.wait_timeout().min(Duration::from_secs(300)) } else { sessions.wait_timeout() }) => {
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
    // The wait above holds nothing of the gate. From the callback on, the
    // login is one operation of it: asked for here, so a switch that has
    // begun refuses it and the session says so, and run as a task the gate
    // owns, so a switch waits for the exchange and the save.
    let gate = state.gate();
    let Ok(operation) = gate.begin_operation() else {
        store.set_error(&login.state, crate::config_write::SWITCHING);
        return;
    };
    let session = login.state.clone();
    let work = complete(state.clone(), login, callback, lifeline);
    let Ok(task) = gate.spawn(&operation, work) else {
        return;
    };
    drop(operation);
    // Stop unclaimed work with its owner, but keep a claimed save and its
    // finalizer gate-owned after shutdown aborts that owner.
    let mut task = StopOnDrop {
        task,
        management: state.clone(),
        session,
    };
    if let Err(error) = (&mut task.task).await {
        tracing::error!("The {provider} login's task failed: {error}");
    }
}

/// Abort with the login owner, except a claimed save during shutdown.
struct StopOnDrop {
    task: tokio::task::JoinHandle<()>,
    management: ManagementState,
    session: String,
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        // Claimed saves and their finalizers remain gate-owned while the
        // process stays alive. The shutdown cutoff, claim_login_commit and
        // admit_bind share the login mutex: no new claim can follow this check.
        let sessions = self.management.oauth_sessions();
        if sessions.is_shut_down() && sessions.store().is_committing(&self.session) {
            return;
        }
        self.task.abort();
    }
}

/// Exchanges `callback`'s code for tokens, saves the credential and
/// completes the session, or sets the session's status saying why not: the
/// part of a login that is an operation of the active gate.
async fn complete(
    state: ManagementState,
    login: Login,
    callback: Callback,
    mut lifeline: Lifeline,
) {
    let sessions = state.oauth_sessions();
    let store = sessions.store();
    let provider = login.client.provider();
    let is_bind = matches!(login.client, Client::Siwc(_));
    if is_bind && let Err(error) = sessions.check_bind_worker() {
        bind_error(store, &login.state, error);
        return;
    }
    // Advisory only: another exchange may consume the last permit; the
    // non-waiting acquisition immediately before spawn is authoritative.
    let exchange = tokio::time::timeout(
        sessions.exchange_timeout(),
        login
            .client
            .credential(&callback, &login.state, &login.redirect, &login.pkce),
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
    // Shutdown keeps the production JoinSet/StopOnDrop ownership chain alive
    // until this guard drops, after the session has its durable outcome.
    let _completion = if is_bind {
        let Some(completion) = sessions.bind_completion() else {
            bind_error(
                store,
                &login.state,
                super::siwc_bind::SaveError::ShuttingDown,
            );
            return;
        };
        Some(completion)
    } else {
        None
    };
    let saved = match (record, login.client) {
        (Credential::Login(mut record), Client::Claude(_) | Client::Codex(_)) => {
            use crate::token_record::SaveError;
            apply_session_proxy(&mut record, &login.proxy_url);
            if !sessions.claim_login_commit(&login.state, provider.name(), login._lease.id) {
                return;
            }
            #[cfg(test)]
            if let Some(claimed) = sessions.overrides().login_commit_claimed.clone() {
                claimed.notify_one();
            }
            match save_token_record(&state, *record).await {
                Ok(path) => Ok((path, true)),
                Err(SaveError::Sync { path, .. }) => Ok((path, false)),
                Err(error) => {
                    tracing::error!("{SAVE_FAILED}: {error}");
                    store.fail_commit(&login.state, SAVE_FAILED);
                    return;
                }
            }
        }
        (Credential::Bind(tokens), Client::Siwc(bind)) => super::siwc_bind::save(
            &state,
            bind.target,
            *tokens,
            login.state.clone(),
            login._lease.id,
            &mut lifeline,
        )
        .await
        .map(|outcome| {
            (
                String::new(),
                outcome == super::siwc_bind::SaveOutcome::Synchronized,
            )
        }),
        _ => {
            tracing::error!("{SAVE_FAILED}: OAuth credential kind mismatch");
            store.set_error(&login.state, SAVE_FAILED);
            return;
        }
    };
    match saved {
        Ok((path, synchronized)) => {
            if is_bind {
                if synchronized {
                    tracing::info!("SIWC credential bound and synchronized");
                } else {
                    tracing::warn!(
                        "SIWC binding saved, runtime sync unavailable; the saved credential will load on restart"
                    );
                }
            } else if synchronized {
                tracing::info!(
                    "{provider} authentication successful; credential saved as {}",
                    saved_name(&path)
                );
            } else {
                tracing::warn!(
                    "{provider} authentication saved as {}; runtime sync unavailable; the saved credential will load on restart",
                    saved_name(&path)
                );
            }
            #[cfg(any(test, feature = "test-support"))]
            if is_bind {
                let pause = sessions.overrides().bind_before_complete.clone();
                if let Some(pause) = pause {
                    pause.0.notify_one();
                    pause.1.notified().await;
                }
            }
            store.complete(&login.state);
            #[cfg(test)]
            if is_bind && let Some(events) = sessions.overrides().bind_shutdown_events.clone() {
                events.lock().unwrap().push("collector finalized");
            }
        }
        Err(error) => bind_error(store, &login.state, error),
    }
}

const BIND_BUSY: &str = "SIWC bind workers are busy; start a new binding";

fn bind_error(store: &super::sessions::Store, session: &str, error: super::siwc_bind::SaveError) {
    use super::siwc_bind::SaveError;
    match error {
        SaveError::Busy => {
            if store.set_pending_error(session, BIND_BUSY) {
                tracing::info!("{BIND_BUSY}");
            }
        }
        SaveError::ShuttingDown => {
            if store.set_pending_error(session, "server shutting down") {
                tracing::info!("SIWC bind cancelled: server shutting down");
            }
        }
        SaveError::Cancelled(cause) => {
            // Keep the previous non-shutdown diagnostic, and retain a status
            // already set by target invalidation, expiry or cancellation.
            tracing::error!("{SAVE_FAILED}: SIWC binding failed (Other, OS None)");
            if cause == super::siwc_bind::CancelCause::LifelineDropped {
                store.set_pending_error(session, SAVE_FAILED);
            }
        }
        SaveError::Failed(error) => {
            tracing::error!(
                "{SAVE_FAILED}: SIWC binding failed ({:?}, OS {:?})",
                error.kind(),
                error.raw_os_error()
            );
            store.fail_commit(session, SAVE_FAILED);
        }
    }
}

/// The name a log shows for the credential file saved at `path`: the file
/// name with the account's email masked ([`mask_emails`]), such as
/// `claude-j***@e***.com.json`, and without the directory.
///
/// [`mask_emails`]: open_ferry_core::observe::mask::mask_emails
fn saved_name(path: &str) -> String {
    let name = std::path::Path::new(path)
        .file_name()
        .map_or_else(|| path.into(), |name| name.to_string_lossy());
    open_ferry_core::observe::mask::mask_emails(&name).into_owned()
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

#[cfg(test)]
mod round7_tests {
    use super::*;
    use crate::oauth::{Sessions, mutation_path::BindMutationPath, sessions::Registration};
    use crate::tests::capture::Logs;

    fn ended_session() -> (Sessions, Registration) {
        let sessions = Sessions::default();
        let registration = sessions.store().register("round7-ended", "codex").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let target = BindMutationPath::capture(dir.path().join("bind.json"));
        assert!(
            sessions
                .store()
                .mark_bind_target("round7-ended", target.clone(), "")
        );
        sessions.store().invalidate_bind_target(&target);
        (sessions, registration)
    }

    async fn refused(shutdown: bool) {
        let (logs, _capture) = Logs::capture();
        let (sessions, _registration) = ended_session();
        if shutdown {
            sessions.shutdown().await;
        } else {
            sessions.bind_workers.limit_for_test(0);
        }
        // Drive the real pre-worker check and shared error arm directly:
        // this synchronous window has no scheduling hook and needs no sleep.
        bind_error(
            sessions.store(),
            "round7-ended",
            sessions.check_bind_worker().unwrap_err(),
        );
        assert_eq!(
            sessions.store().get("round7-ended").unwrap().status,
            "SIWC binding target changed; start a new binding"
        );
        assert!(
            logs.text().is_empty(),
            "an ended session was logged as a new refusal"
        );

        // The same refusal must still change and log a pending session once.
        let _pending = sessions
            .store()
            .register("round7-pending", "codex")
            .unwrap();
        bind_error(
            sessions.store(),
            "round7-pending",
            sessions.check_bind_worker().unwrap_err(),
        );
        let expected = if shutdown {
            "server shutting down"
        } else {
            BIND_BUSY
        };
        assert_eq!(
            sessions.store().get("round7-pending").unwrap().status,
            expected
        );
        assert!(logs.text().contains(expected));
        assert_eq!(logs.levels(), [tracing::Level::INFO]);
    }

    #[tokio::test]
    async fn round7_shutdown_refusal_preserves_an_ended_status() {
        refused(true).await;
    }

    #[tokio::test]
    async fn round7_busy_refusal_preserves_an_ended_status() {
        refused(false).await;
    }

    #[tokio::test]
    async fn round7_admit_bind_prioritizes_an_ended_session_over_shutdown() {
        let (sessions, registration) = ended_session();
        sessions.shutdown().await;
        assert_eq!(
            sessions.admit_bind("round7-ended", registration.id),
            Err(crate::oauth::siwc_bind::CancelCause::SessionEnded)
        );
    }
}
