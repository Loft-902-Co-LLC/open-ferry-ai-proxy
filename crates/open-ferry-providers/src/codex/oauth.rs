// Ported from CLIProxyAPI internal/auth/codex/openai_auth.go and errors.go,
// sdk/auth/codex.go and sdk/auth/codex_device.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Codex's OAuth: the browser login with PKCE, the device-code login, the
//! code exchange and the token refresh, against OpenAI's auth server.
//!
//! These are OpenAI's OAuth flows for the Codex client ID, as upstream runs
//! them. The authorization URL carries the standard OAuth and PKCE
//! parameters, plus three that OpenAI's auth server defines for this client:
//! `prompt=login` (OpenID Connect: always show the sign-in), and
//! `id_token_add_organizations=true` and `codex_cli_simplified_flow=true`,
//! which ask for the account's organizations in the `id_token` and for
//! OpenAI's sign-in page for Codex. They choose what the auth server shows
//! and returns; none of them describe the caller. Requests carry our own
//! `User-Agent` (see [`super::client::USER_AGENT`]).
//!
//! A login takes a function that shows the user the URL or device code, and
//! returns the credential record; the caller saves it with an
//! [`AuthStore`](open_ferry_core::auth::AuthStore).
//!
//! Deviations from upstream:
//! - Logins don't open a browser or print; the `present` function shows the
//!   URL or code. Pasting the callback URL by hand isn't supported yet.
//! - The callback server turns away a callback with the wrong `state` and
//!   keeps waiting (see [`crate::oauth`]), so there's no `invalid_state`
//!   error. There's no `browser_open_failed` error either.
//! - Every endpoint can be changed, through [`Endpoints`].
//! - Response bodies are read up to 1 MiB.
//! - JSON errors after upstream's prefixes (`failed to parse token response:
//!   ` and the like) are this module's own words.
//! - Concurrent refreshes of a token share one request, as upstream's
//!   single-flight group does; the group is keyed by a hash of the token
//!   endpoint and the token, not the token itself.
//! - Retries between refresh attempts and device-code polls stop when the
//!   caller drops the future, where upstream watches its context.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::Duration;

use chrono::{Local, SecondsFormat, TimeDelta};
use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use open_ferry_core::auth::Auth;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::client::{Clients, error_chain, read_body};
use super::jwt;
use super::jwt::{DEFAULT_PLAN_TYPE, parse_jwt_token, plan_type_or_default};
use super::token::{
    AuthBundle, TokenData, create_token_storage, credential_file_name, now_rfc3339,
};
use crate::oauth::{CallbackError, CallbackResult, CallbackServer, Pkce, generate_state};

/// OpenAI's OAuth client ID for Codex.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// The redirect URI of the browser login. It stays on port 1455 even when
/// the callback server listens elsewhere, as upstream's does; a different
/// port is for forwarding the redirect, as over an SSH tunnel.
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
/// The redirect URI the device login exchanges its code with.
pub const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
/// The callback server's default port.
pub const DEFAULT_CALLBACK_PORT: u16 = 1455;
/// The path the callback server listens on.
const CALLBACK_PATH: &str = "/auth/callback";
/// How long a refresh request may take.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the device login waits for the user.
const DEVICE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// The device login's poll interval when the server names none.
const DEVICE_DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// The most of an auth server response that is read.
const MAX_RESPONSE_BYTES: usize = 1 << 20;

/// OpenAI's auth server endpoints. Tests point these at a local server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// Where the browser login starts.
    pub authorize_url: String,
    /// Where codes and refresh tokens are exchanged for tokens.
    pub token_url: String,
    /// Where the device login gets its code.
    pub device_user_code_url: String,
    /// Where the device login polls for the authorization code.
    pub device_token_url: String,
    /// The page where the user enters the device code.
    pub device_verification_url: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self::with_base("https://auth.openai.com")
    }
}

impl Endpoints {
    /// Upstream's endpoint paths under `base`, such as
    /// `http://127.0.0.1:PORT` for a test server.
    pub fn with_base(base: &str) -> Self {
        let base = base.trim_end_matches('/');
        Self {
            authorize_url: format!("{base}/oauth/authorize"),
            token_url: format!("{base}/oauth/token"),
            device_user_code_url: format!("{base}/api/accounts/deviceauth/usercode"),
            device_token_url: format!("{base}/api/accounts/deviceauth/token"),
            device_verification_url: format!("{base}/codex/device"),
        }
    }
}

/// A failed OAuth call. The text never holds a token or code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error(String);

impl Error {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    /// What went wrong.
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// What kind of login failure an [`AuthenticationError`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthenticationErrorKind {
    /// The authorization code couldn't be exchanged.
    CodeExchangeFailed,
    /// The callback server couldn't start.
    ServerStartFailed,
    /// The callback port is taken.
    PortInUse,
    /// No callback came in time.
    CallbackTimeout,
}

impl AuthenticationErrorKind {
    /// Upstream's `type`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CodeExchangeFailed => "code_exchange_failed",
            Self::ServerStartFailed => "server_start_failed",
            Self::PortInUse => "port_in_use",
            Self::CallbackTimeout => "callback_timeout",
        }
    }

    /// Upstream's message.
    pub fn message(self) -> &'static str {
        match self {
            Self::CodeExchangeFailed => "Failed to exchange authorization code for tokens",
            Self::ServerStartFailed => "Failed to start OAuth callback server",
            Self::PortInUse => "OAuth callback port is already in use",
            Self::CallbackTimeout => "Timeout waiting for OAuth callback",
        }
    }

    /// Upstream's code: an HTTP status, or 13 (an exit code) for a port in
    /// use.
    pub fn code(self) -> u16 {
        match self {
            Self::CodeExchangeFailed => 400,
            Self::ServerStartFailed => 500,
            Self::PortInUse => 13,
            Self::CallbackTimeout => 408,
        }
    }
}

/// A login failure (upstream's `AuthenticationError`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthenticationError {
    /// What failed.
    pub kind: AuthenticationErrorKind,
    /// The error behind it.
    pub cause: Option<String>,
}

impl fmt::Display for AuthenticationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kind, message) = (self.kind.as_str(), self.kind.message());
        match &self.cause {
            Some(cause) => write!(f, "{kind}: {message} (caused by: {cause})"),
            None => write!(f, "{kind}: {message}"),
        }
    }
}

impl std::error::Error for AuthenticationError {}

/// An error the auth server sent to the callback (upstream's `OAuthError`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthError {
    /// The `error` parameter.
    pub code: String,
    /// The `error_description`, or empty.
    pub description: String,
    /// The status upstream gives it: 400.
    pub status: u16,
}

impl fmt::Display for OAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.description.is_empty() {
            write!(f, "OAuth error: {}", self.code)
        } else {
            write!(f, "OAuth error {}: {}", self.code, self.description)
        }
    }
}

impl std::error::Error for OAuthError {}

/// Why a login failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoginError {
    /// The login itself failed.
    Authentication(AuthenticationError),
    /// The auth server sent an error to the callback.
    OAuth(OAuthError),
    /// Anything else.
    Other(Error),
}

impl fmt::Display for LoginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Authentication(error) => error.fmt(f),
            Self::OAuth(error) => error.fmt(f),
            Self::Other(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for LoginError {}

impl LoginError {
    fn authentication(kind: AuthenticationErrorKind, cause: impl Into<String>) -> Self {
        Self::Authentication(AuthenticationError {
            kind,
            cause: Some(cause.into()),
        })
    }
}

/// Calls OpenAI's auth server for Codex (upstream's `CodexAuth`).
#[derive(Clone, Debug)]
pub struct CodexAuth {
    client: reqwest::Client,
    endpoints: Endpoints,
}

impl CodexAuth {
    /// Calls the real endpoints with `client`.
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            endpoints: Endpoints::default(),
        }
    }

    /// Calls through `proxy_url`: empty for the environment's proxy,
    /// `direct` or `none` for no proxy, or an `http` or `https` proxy URL
    /// (upstream's `NewCodexAuthWithProxyURL`).
    pub fn with_proxy_url(proxy_url: &str) -> Self {
        Self::new(Clients::new(proxy_url).get(""))
    }

    /// Calls `endpoints` instead.
    pub fn with_endpoints(mut self, endpoints: Endpoints) -> Self {
        self.endpoints = endpoints;
        self
    }

    /// The endpoints it calls.
    pub fn endpoints(&self) -> &Endpoints {
        &self.endpoints
    }

    /// The URL that starts the browser login (`GenerateAuthURL`).
    pub fn generate_auth_url(&self, state: &str, pkce: &Pkce) -> String {
        let params = encode_form(&[
            ("client_id", CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", REDIRECT_URI),
            ("scope", "openid email profile offline_access"),
            ("state", state),
            ("code_challenge", &pkce.challenge),
            ("code_challenge_method", "S256"),
            ("prompt", "login"),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
        ]);
        format!("{}?{params}", self.endpoints.authorize_url)
    }

    /// Exchanges a browser login's code for tokens
    /// (`ExchangeCodeForTokens`).
    pub async fn exchange_code_for_tokens(
        &self,
        code: &str,
        pkce: &Pkce,
    ) -> Result<AuthBundle, Error> {
        self.exchange_code_for_tokens_with_redirect(code, REDIRECT_URI, pkce)
            .await
    }

    /// Exchanges a code that was issued for `redirect_uri`
    /// (`ExchangeCodeForTokensWithRedirect`).
    pub async fn exchange_code_for_tokens_with_redirect(
        &self,
        code: &str,
        redirect_uri: &str,
        pkce: &Pkce,
    ) -> Result<AuthBundle, Error> {
        let redirect_uri = redirect_uri.trim();
        if redirect_uri.is_empty() {
            return Err(Error::new("redirect URI is required for token exchange"));
        }
        let form = encode_form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", &pkce.verifier),
        ]);
        let response = self.form_request(form).send().await.map_err(|e| {
            Error::new(format!(
                "token exchange request failed: {}",
                error_chain(&e)
            ))
        })?;
        let status = response.status().as_u16();
        let body = read_body(response, MAX_RESPONSE_BYTES)
            .await
            .map_err(|e| Error::new(format!("failed to read token response: {e}")))?;
        if status != 200 {
            return Err(Error::new(format!(
                "token exchange failed with status {status}: {}",
                String::from_utf8_lossy(&body)
            )));
        }
        let tokens = decode_token_response(&body)
            .map_err(|e| Error::new(format!("failed to parse token response: {e}")))?;
        let claims = match parse_jwt_token(&tokens.id_token) {
            Ok(claims) => Some(claims),
            Err(error) => {
                tracing::warn!("Failed to parse ID token: {error}");
                None
            }
        };
        let data = TokenData {
            account_id: claims
                .as_ref()
                .map(|claims| claims.account_id().to_owned())
                .unwrap_or_default(),
            email: claims
                .as_ref()
                .map(|claims| claims.user_email().to_owned())
                .unwrap_or_default(),
            plan_type: plan_type_or_default(claims.as_ref()),
            expire: expiry(tokens.expires_in),
            id_token: tokens.id_token,
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
        };
        Ok(AuthBundle {
            api_key: String::new(),
            token_data: data,
            last_refresh: now_rfc3339(),
        })
    }

    /// Gets new tokens with `refresh_token` (`RefreshTokens`). Calls for the
    /// same token at the same time share one request, which runs to the end
    /// within 30 seconds even if its callers stop waiting.
    pub async fn refresh_tokens(&self, refresh_token: &str) -> Result<TokenData, Error> {
        if refresh_token.is_empty() {
            return Err(Error::new("refresh token is required"));
        }
        let key: [u8; 32] = Sha256::new()
            .chain_update(self.endpoints.token_url.as_bytes())
            .chain_update([0])
            .chain_update(refresh_token.as_bytes())
            .finalize()
            .into();
        let shared = {
            let mut refreshes = REFRESHES.lock().unwrap_or_else(PoisonError::into_inner);
            match refreshes.get(&key) {
                Some(shared) => shared.clone(),
                None => {
                    let auth = self.clone();
                    let token = refresh_token.to_owned();
                    let task = tokio::spawn(async move {
                        let result = auth.refresh_once(&token).await;
                        REFRESHES
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .remove(&key);
                        result
                    });
                    let shared = async move {
                        task.await.unwrap_or_else(|_| {
                            Err(Error::new(
                                "token refresh failed: invalid single-flight result",
                            ))
                        })
                    }
                    .boxed()
                    .shared();
                    refreshes.insert(key, shared.clone());
                    shared
                }
            }
        };
        shared.await
    }

    async fn refresh_once(&self, refresh_token: &str) -> Result<TokenData, Error> {
        let form = encode_form(&[
            ("client_id", CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("scope", "openid profile email"),
        ]);
        let response = self
            .form_request(form)
            .timeout(REFRESH_TIMEOUT)
            .send()
            .await
            .map_err(|e| {
                Error::new(format!("token refresh request failed: {}", error_chain(&e)))
            })?;
        let status = response.status().as_u16();
        let body = read_body(response, MAX_RESPONSE_BYTES)
            .await
            .map_err(|e| Error::new(format!("failed to read refresh response: {e}")))?;
        if status != 200 {
            return Err(Error::new(format!(
                "token refresh failed with status {status}: {}",
                String::from_utf8_lossy(&body)
            )));
        }
        let tokens = decode_token_response(&body)
            .map_err(|e| Error::new(format!("failed to parse refresh response: {e}")))?;
        let claims = match parse_jwt_token(&tokens.id_token) {
            Ok(claims) => Some(claims),
            Err(error) => {
                tracing::warn!("Failed to parse refreshed ID token: {error}");
                None
            }
        };
        Ok(TokenData {
            account_id: claims
                .as_ref()
                .map(|claims| claims.account_id().to_owned())
                .unwrap_or_default(),
            email: claims
                .as_ref()
                .map(|claims| claims.email.clone())
                .unwrap_or_default(),
            plan_type: plan_type_or_default(claims.as_ref()),
            expire: expiry(tokens.expires_in),
            id_token: tokens.id_token,
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
        })
    }

    /// [`refresh_tokens`](Self::refresh_tokens), tried up to `max_retries`
    /// times, waiting a second more before each retry. A reused refresh
    /// token isn't retried (`RefreshTokensWithRetry`).
    pub async fn refresh_tokens_with_retry(
        &self,
        refresh_token: &str,
        max_retries: u32,
    ) -> Result<TokenData, Error> {
        let mut last_error = None;
        for attempt in 0..max_retries {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(attempt.into())).await;
            }
            match self.refresh_tokens(refresh_token).await {
                Ok(data) => return Ok(data),
                Err(error) if is_non_retryable_refresh_error(&error) => {
                    tracing::warn!(
                        "Token refresh attempt {} failed with non-retryable error: {error}",
                        attempt + 1
                    );
                    return Err(error);
                }
                Err(error) => {
                    tracing::warn!("Token refresh attempt {} failed: {error}", attempt + 1);
                    last_error = Some(error);
                }
            }
        }
        let last = last_error.map_or_else(|| "%!w(<nil>)".to_owned(), |error| error.0);
        Err(Error::new(format!(
            "token refresh failed after {max_retries} attempts: {last}"
        )))
    }

    fn form_request(&self, form: String) -> reqwest::RequestBuilder {
        self.client
            .post(&self.endpoints.token_url)
            .header(
                http::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .header(http::header::ACCEPT, "application/json")
            .body(form)
    }

    async fn post_json(
        &self,
        url: &str,
        body: String,
    ) -> Result<reqwest::Response, reqwest::Error> {
        self.client
            .post(url)
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(http::header::ACCEPT, "application/json")
            .body(body)
            .send()
            .await
    }
}

type SharedRefresh = Shared<BoxFuture<'static, Result<TokenData, Error>>>;

/// Refreshes in flight, by a hash of the token endpoint and refresh token.
static REFRESHES: LazyLock<Mutex<HashMap<[u8; 32], SharedRefresh>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn is_non_retryable_refresh_error(error: &Error) -> bool {
    error.0.to_lowercase().contains("refresh_token_reused")
}

/// The token endpoint's answer.
#[derive(Default)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    id_token: String,
    expires_in: i64,
}

/// Decodes the token endpoint's answer as Go's `json.Unmarshal` would.
fn decode_token_response(body: &[u8]) -> Result<TokenResponse, String> {
    let value: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let mut tokens = TokenResponse::default();
    let mut token_type = String::new();
    let Some(object) = jwt::object_or_null(&value, "tokenResponse")? else {
        return Ok(tokens);
    };
    for (key, value) in object {
        let field = format!(".{key}");
        let field = field.as_str();
        match jwt::key_of(
            key,
            &[
                "access_token",
                "refresh_token",
                "id_token",
                "token_type",
                "expires_in",
            ],
        ) {
            Some("access_token") => jwt::set_string(&mut tokens.access_token, value, field)?,
            Some("refresh_token") => jwt::set_string(&mut tokens.refresh_token, value, field)?,
            Some("id_token") => jwt::set_string(&mut tokens.id_token, value, field)?,
            Some("token_type") => jwt::set_string(&mut token_type, value, field)?,
            Some("expires_in") => jwt::set_int(&mut tokens.expires_in, value, field)?,
            _ => {}
        }
    }
    Ok(tokens)
}

/// When tokens that last `expires_in` seconds from now expire, in RFC 3339.
fn expiry(expires_in: i64) -> String {
    let now = Local::now();
    TimeDelta::try_seconds(expires_in)
        .and_then(|delta| now.checked_add_signed(delta))
        .unwrap_or(now)
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Go's `url.Values.Encode`: keys sorted, each pair `QueryEscape`d.
fn encode_form(pairs: &[(&str, &str)]) -> String {
    let mut pairs = pairs.to_vec();
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    pairs
        .iter()
        .map(|(key, value)| format!("{}={}", query_escape(key), query_escape(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Go's `url.QueryEscape`.
fn query_escape(text: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(text.len());
    for &byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte));
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(char::from(HEX[usize::from(byte >> 4)]));
                out.push(char::from(HEX[usize::from(byte & 0x0f)]));
            }
        }
    }
    out
}

/// How a browser login runs.
#[derive(Clone, Debug)]
pub struct LoginOptions {
    /// The port the callback server listens on, or 0 for any free port.
    pub callback_port: u16,
    /// How long to wait for the callback.
    pub callback_timeout: Duration,
}

impl Default for LoginOptions {
    fn default() -> Self {
        Self {
            callback_port: DEFAULT_CALLBACK_PORT,
            callback_timeout: Duration::from_secs(5 * 60),
        }
    }
}

/// What to show the user to start a browser login.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizationPrompt {
    /// The URL to open.
    pub url: String,
    /// The port the callback server listens on, for tunnelling the redirect
    /// to it when the browser is on another machine.
    pub callback_port: u16,
}

/// Runs the browser login: starts the callback server, has `present` show
/// the URL, waits for the redirect and exchanges its code (upstream's
/// `CodexAuthenticator.Login`).
pub async fn login<F>(
    auth: &CodexAuth,
    options: LoginOptions,
    present: F,
) -> Result<Auth, LoginError>
where
    F: FnOnce(&AuthorizationPrompt),
{
    let pkce = Pkce::generate();
    let state = generate_state();
    let mut server = CallbackServer::start(options.callback_port, CALLBACK_PATH, &state)
        .await
        .map_err(|error| {
            if error.kind() == io::ErrorKind::AddrInUse {
                LoginError::authentication(
                    AuthenticationErrorKind::PortInUse,
                    format!("port {} is already in use", options.callback_port),
                )
            } else {
                LoginError::authentication(
                    AuthenticationErrorKind::ServerStartFailed,
                    format!("server failed to start: {error}"),
                )
            }
        })?;
    present(&AuthorizationPrompt {
        url: auth.generate_auth_url(&state, &pkce),
        callback_port: server.port(),
    });
    let code = match server.wait(options.callback_timeout).await {
        Ok(CallbackResult::Code(code)) => code,
        Ok(CallbackResult::Error(code)) => {
            return Err(LoginError::OAuth(OAuthError {
                code,
                description: String::new(),
                status: 400,
            }));
        }
        Err(error @ CallbackError::Timeout) => {
            return Err(LoginError::authentication(
                AuthenticationErrorKind::CallbackTimeout,
                error.to_string(),
            ));
        }
        Err(error @ CallbackError::Closed) => {
            return Err(LoginError::Other(Error::new(error.to_string())));
        }
    };
    drop(server);
    tracing::debug!("Codex authorization code received; exchanging for tokens");
    let bundle = auth
        .exchange_code_for_tokens(&code, &pkce)
        .await
        .map_err(|error| {
            LoginError::authentication(AuthenticationErrorKind::CodeExchangeFailed, error.0)
        })?;
    build_auth_record(&bundle).map_err(LoginError::Other)
}

/// What to show the user during a device login.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceCode {
    /// The page to open.
    pub verification_url: String,
    /// The code to enter there.
    pub user_code: String,
}

/// Runs the device login: gets a code, has `present` show it, polls until
/// the user has entered it, and exchanges the result (upstream's
/// `loginWithDeviceFlow`).
pub async fn login_with_device_code<F>(auth: &CodexAuth, present: F) -> Result<Auth, LoginError>
where
    F: FnOnce(&DeviceCode),
{
    let user_code = request_device_user_code(auth)
        .await
        .map_err(LoginError::Other)?;
    let device_code = match user_code.user_code.trim() {
        "" => user_code.user_code_alt.trim(),
        code => code,
    };
    let device_auth_id = user_code.device_auth_id.trim();
    if device_code.is_empty() || device_auth_id.is_empty() {
        return Err(LoginError::Other(Error::new(
            "codex device flow did not return required fields",
        )));
    }
    present(&DeviceCode {
        verification_url: auth.endpoints.device_verification_url.clone(),
        user_code: device_code.to_owned(),
    });
    let token = poll_device_token(auth, device_auth_id, device_code, user_code.interval)
        .await
        .map_err(LoginError::Other)?;
    let code = token.authorization_code.trim();
    let verifier = token.code_verifier.trim();
    let challenge = token.code_challenge.trim();
    if code.is_empty() || verifier.is_empty() || challenge.is_empty() {
        return Err(LoginError::Other(Error::new(
            "codex device flow token response missing required fields",
        )));
    }
    let pkce = Pkce {
        verifier: verifier.to_owned(),
        challenge: challenge.to_owned(),
    };
    let bundle = auth
        .exchange_code_for_tokens_with_redirect(code, DEVICE_REDIRECT_URI, &pkce)
        .await
        .map_err(|error| {
            LoginError::authentication(AuthenticationErrorKind::CodeExchangeFailed, error.0)
        })?;
    build_auth_record(&bundle).map_err(LoginError::Other)
}

struct DeviceUserCode {
    device_auth_id: String,
    user_code: String,
    user_code_alt: String,
    interval: Duration,
}

async fn request_device_user_code(auth: &CodexAuth) -> Result<DeviceUserCode, Error> {
    let body = format!(
        "{{\"client_id\":{}}}",
        open_ferry_translate::go::json_string(CLIENT_ID)
    );
    let response = auth
        .post_json(&auth.endpoints.device_user_code_url, body)
        .await
        .map_err(|e| {
            Error::new(format!(
                "failed to request codex device code: {}",
                error_chain(&e)
            ))
        })?;
    let status = response.status().as_u16();
    let body = read_body(response, MAX_RESPONSE_BYTES)
        .await
        .map_err(|e| Error::new(format!("failed to read codex device code response: {e}")))?;
    if !(200..300).contains(&status) {
        if status == 404 {
            return Err(Error::new(format!(
                "codex device endpoint is unavailable (status {status})"
            )));
        }
        return Err(Error::new(format!(
            "codex device code request failed with status {status}: {}",
            trimmed_or_empty(&body)
        )));
    }
    decode_device_user_code(&body)
        .map_err(|e| Error::new(format!("failed to decode codex device code response: {e}")))
}

fn decode_device_user_code(body: &[u8]) -> Result<DeviceUserCode, String> {
    let value: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let mut code = DeviceUserCode {
        device_auth_id: String::new(),
        user_code: String::new(),
        user_code_alt: String::new(),
        interval: DEVICE_DEFAULT_POLL_INTERVAL,
    };
    let Some(object) = jwt::object_or_null(&value, "codexDeviceUserCodeResponse")? else {
        return Ok(code);
    };
    for (key, value) in object {
        let field = format!("codexDeviceUserCodeResponse.{key}");
        let field = field.as_str();
        match jwt::key_of(
            key,
            &["device_auth_id", "user_code", "usercode", "interval"],
        ) {
            Some("device_auth_id") => jwt::set_string(&mut code.device_auth_id, value, field)?,
            Some("user_code") => jwt::set_string(&mut code.user_code, value, field)?,
            Some("usercode") => jwt::set_string(&mut code.user_code_alt, value, field)?,
            Some("interval") => code.interval = poll_interval(value),
            _ => {}
        }
    }
    Ok(code)
}

/// The poll interval: whole seconds, as a number or a string, or 5 seconds
/// (`parseCodexDevicePollInterval`).
fn poll_interval(value: &Value) -> Duration {
    let seconds = match value {
        Value::String(text) => text.trim().parse::<i64>().ok(),
        Value::Number(number) => number.to_string().parse::<i64>().ok(),
        _ => None,
    };
    match seconds {
        Some(seconds) if seconds > 0 => Duration::from_secs(seconds.unsigned_abs()),
        _ => DEVICE_DEFAULT_POLL_INTERVAL,
    }
}

#[derive(Default)]
struct DeviceToken {
    authorization_code: String,
    code_verifier: String,
    code_challenge: String,
}

async fn poll_device_token(
    auth: &CodexAuth,
    device_auth_id: &str,
    user_code: &str,
    interval: Duration,
) -> Result<DeviceToken, Error> {
    let deadline = tokio::time::Instant::now() + DEVICE_TIMEOUT;
    let body = format!(
        "{{\"device_auth_id\":{},\"user_code\":{}}}",
        open_ferry_translate::go::json_string(device_auth_id),
        open_ferry_translate::go::json_string(user_code)
    );
    loop {
        if tokio::time::Instant::now() > deadline {
            return Err(Error::new(
                "codex device authentication timed out after 15 minutes",
            ));
        }
        let response = auth
            .post_json(&auth.endpoints.device_token_url, body.clone())
            .await
            .map_err(|e| {
                Error::new(format!(
                    "failed to poll codex device token: {}",
                    error_chain(&e)
                ))
            })?;
        let status = response.status().as_u16();
        let response_body = read_body(response, MAX_RESPONSE_BYTES)
            .await
            .map_err(|e| Error::new(format!("failed to read codex device poll response: {e}")))?;
        match status {
            200..=299 => {
                return decode_device_token(&response_body).map_err(|e| {
                    Error::new(format!("failed to decode codex device token response: {e}"))
                });
            }
            403 | 404 => tokio::time::sleep(interval).await,
            _ => {
                return Err(Error::new(format!(
                    "codex device token polling failed with status {status}: {}",
                    trimmed_or_empty(&response_body)
                )));
            }
        }
    }
}

fn decode_device_token(body: &[u8]) -> Result<DeviceToken, String> {
    let value: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let mut token = DeviceToken::default();
    let Some(object) = jwt::object_or_null(&value, "codexDeviceTokenResponse")? else {
        return Ok(token);
    };
    for (key, value) in object {
        let field = format!("codexDeviceTokenResponse.{key}");
        let field = field.as_str();
        match jwt::key_of(
            key,
            &["authorization_code", "code_verifier", "code_challenge"],
        ) {
            Some("authorization_code") => {
                jwt::set_string(&mut token.authorization_code, value, field)?;
            }
            Some("code_verifier") => jwt::set_string(&mut token.code_verifier, value, field)?,
            Some("code_challenge") => jwt::set_string(&mut token.code_challenge, value, field)?,
            _ => {}
        }
    }
    Ok(token)
}

fn trimmed_or_empty(body: &[u8]) -> String {
    match String::from_utf8_lossy(body).trim() {
        "" => "empty response body".to_owned(),
        text => text.to_owned(),
    }
}

/// The credential record for a login's tokens: named for the email, the
/// account and the plan, with the token file's JSON as its metadata
/// (upstream's `buildAuthRecord`).
pub fn build_auth_record(bundle: &AuthBundle) -> Result<Auth, Error> {
    let mut storage = create_token_storage(bundle);
    if storage.email.is_empty() {
        return Err(Error::new(
            "codex token storage missing account information",
        ));
    }
    let mut plan_type = storage.plan_type.clone();
    let mut hash_account_id = String::new();
    if !storage.id_token.is_empty()
        && let Ok(claims) = parse_jwt_token(&storage.id_token)
    {
        let plan = claims.codex_auth_info.chatgpt_plan_type.trim();
        if !plan.is_empty() {
            plan.clone_into(&mut plan_type);
        }
        let account_id = claims.codex_auth_info.chatgpt_account_id.trim();
        if !account_id.is_empty() {
            hash_account_id = Sha256::digest(account_id.as_bytes())
                .iter()
                .take(4)
                .map(|byte| format!("{byte:02x}"))
                .collect();
        }
    }
    if plan_type.is_empty() {
        DEFAULT_PLAN_TYPE.clone_into(&mut plan_type);
    }
    storage.plan_type.clone_from(&plan_type);
    let file_name = credential_file_name(&storage.email, &plan_type, &hash_account_id, true);
    let mut extra = Map::new();
    extra.insert("email".to_owned(), Value::from(storage.email.as_str()));
    extra.insert("plan_type".to_owned(), Value::from(plan_type.as_str()));
    storage.metadata = extra;
    tracing::info!("Codex authentication successful");
    Ok(Auth {
        id: file_name.clone(),
        provider: "codex".to_owned(),
        file_name,
        metadata: storage.to_json(),
        attributes: [("plan_type".to_owned(), plan_type)].into_iter().collect(),
        ..Auth::default()
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::post;
    use serde_json::json;
    use tokio::sync::Notify;

    use super::*;
    use crate::codex::jwt::tests::make_test_jwt;

    /// A mock auth server on 127.0.0.1 and its base URL.
    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{address}")
    }

    fn auth_for(base: &str) -> CodexAuth {
        CodexAuth::with_proxy_url("direct").with_endpoints(Endpoints::with_base(base))
    }

    fn parse_form(body: &str) -> HashMap<String, String> {
        url::form_urlencoded::parse(body.as_bytes())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    }

    fn test_jwt(plan_type: &str, account_id: &str) -> String {
        let mut info = json!({"chatgpt_account_id": account_id});
        if !plan_type.is_empty() {
            info["chatgpt_plan_type"] = json!(plan_type);
        }
        make_test_jwt(&json!({
            "email": "user@example.com",
            "https://api.openai.com/auth": info,
        }))
    }

    #[test]
    fn auth_url_has_upstreams_parameters() {
        let pkce = Pkce {
            verifier: "v".into(),
            challenge: "chal-lenge_~".into(),
        };
        let url = CodexAuth::new(reqwest::Client::new()).generate_auth_url("st ate", &pkce);
        assert_eq!(
            url,
            "https://auth.openai.com/oauth/authorize?client_id=app_EMoamEEZ73f0CkXaXp7hrann\
             &code_challenge=chal-lenge_~&code_challenge_method=S256\
             &codex_cli_simplified_flow=true&id_token_add_organizations=true&prompt=login\
             &redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback&response_type=code\
             &scope=openid+email+profile+offline_access&state=st+ate"
        );
        assert_eq!(query_escape("a*b/c"), "a%2Ab%2Fc");
    }

    #[test]
    fn decodes_token_responses_like_go() {
        let tokens = decode_token_response(
            br#"{"ACCESS_TOKEN":"a","refresh_token":null,"expires_in":3600,"x":[]}"#,
        )
        .unwrap();
        assert_eq!(tokens.access_token, "a");
        assert_eq!(tokens.refresh_token, "");
        assert_eq!(tokens.expires_in, 3600);
        assert!(decode_token_response(br#"{"expires_in":1.5}"#).is_err());
        assert!(decode_token_response(br#"{"access_token":1}"#).is_err());
        assert!(decode_token_response(b"null").is_ok());
        assert!(decode_token_response(b"[]").is_err());
    }

    #[tokio::test]
    async fn refresh_non_retryable_only_attempts_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let router = Router::new()
            .route(
                "/oauth/token",
                post(|State(calls): State<Arc<AtomicUsize>>| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::BAD_REQUEST,
                        r#"{"error":"invalid_grant","code":"refresh_token_reused"}"#,
                    )
                }),
            )
            .with_state(calls.clone());
        let auth = auth_for(&serve(router).await);
        let error = auth
            .refresh_tokens_with_retry("dummy_refresh_token", 3)
            .await
            .unwrap_err();
        assert!(
            error
                .message()
                .to_lowercase()
                .contains("refresh_token_reused")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn refresh_retries_then_reports_the_last_error() {
        let calls = Arc::new(AtomicUsize::new(0));
        let router = Router::new()
            .route(
                "/oauth/token",
                post(|State(calls): State<Arc<AtomicUsize>>| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::INTERNAL_SERVER_ERROR, "boom")
                }),
            )
            .with_state(calls.clone());
        let auth = auth_for(&serve(router).await);
        let error = auth
            .refresh_tokens_with_retry("rt-retry", 2)
            .await
            .unwrap_err();
        assert_eq!(
            error.message(),
            "token refresh failed after 2 attempts: token refresh failed with status 500: boom"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[derive(Clone)]
    struct Blocking {
        calls: Arc<AtomicUsize>,
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[tokio::test]
    async fn refresh_deduplicates_concurrent_calls_across_instances() {
        let state = Blocking {
            calls: Arc::new(AtomicUsize::new(0)),
            started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
        };
        let router = Router::new()
            .route(
                "/oauth/token",
                post(|State(state): State<Blocking>, body: String| async move {
                    state.calls.fetch_add(1, Ordering::SeqCst);
                    let form = parse_form(&body);
                    assert_eq!(form["grant_type"], "refresh_token");
                    assert_eq!(form["refresh_token"], "shared-refresh-token");
                    assert_eq!(form["client_id"], CLIENT_ID);
                    assert_eq!(form["scope"], "openid profile email");
                    state.started.notify_one();
                    state.release.notified().await;
                    r#"{"access_token":"new-access","refresh_token":"new-refresh","token_type":"Bearer","expires_in":3600}"#
                }),
            )
            .with_state(state.clone());
        let base = serve(router).await;
        let auth_a = auth_for(&base);
        let auth_b = auth_for(&base);

        let first =
            tokio::spawn(async move { auth_a.refresh_tokens("shared-refresh-token").await });
        state.started.notified().await;
        let second =
            tokio::spawn(async move { auth_b.refresh_tokens("shared-refresh-token").await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(state.calls.load(Ordering::SeqCst), 1);
        state.release.notify_one();

        for task in [first, second] {
            let data = task.await.unwrap().unwrap();
            assert_eq!(data.access_token, "new-access");
            assert_eq!(data.refresh_token, "new-refresh");
        }
        assert_eq!(state.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn refresh_outlives_a_caller_that_stops_waiting() {
        let state = Blocking {
            calls: Arc::new(AtomicUsize::new(0)),
            started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
        };
        let router = Router::new()
            .route(
                "/oauth/token",
                post(|State(state): State<Blocking>| async move {
                    state.calls.fetch_add(1, Ordering::SeqCst);
                    state.started.notify_one();
                    state.release.notified().await;
                    r#"{"access_token":"late","expires_in":60}"#
                }),
            )
            .with_state(state.clone());
        let auth = auth_for(&serve(router).await);
        let caller = {
            let auth = auth.clone();
            tokio::spawn(async move { auth.refresh_tokens("abandoned-token").await })
        };
        state.started.notified().await;
        caller.abort();
        let joined = {
            let auth = auth.clone();
            tokio::spawn(async move { auth.refresh_tokens("abandoned-token").await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        state.release.notify_one();
        assert_eq!(joined.await.unwrap().unwrap().access_token, "late");
        assert_eq!(state.calls.load(Ordering::SeqCst), 1);
    }

    async fn refresh_with_id_token(id_token: String) -> TokenData {
        let router = Router::new().route(
            "/oauth/token",
            post(move || async move {
                json!({
                    "access_token": "at-1",
                    "refresh_token": "rt-1",
                    "id_token": id_token,
                    "token_type": "Bearer",
                    "expires_in": 3600,
                })
                .to_string()
            }),
        );
        auth_for(&serve(router).await)
            .refresh_tokens("dummy-refresh")
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn refresh_plan_type_defaults_to_free_when_missing() {
        let data = refresh_with_id_token(test_jwt("", "acc-12345")).await;
        assert_eq!(data.plan_type, "free");
        assert_eq!(data.account_id, "acc-12345");
        assert_eq!(data.email, "user@example.com");
    }

    #[tokio::test]
    async fn refresh_extracts_plan_type_when_present() {
        let data = refresh_with_id_token(test_jwt("pro", "acc-12345")).await;
        assert_eq!(data.plan_type, "pro");
        assert!(!data.expire.is_empty());
    }

    #[tokio::test]
    async fn refresh_requires_a_token() {
        let auth = auth_for("http://127.0.0.1:9");
        assert_eq!(
            auth.refresh_tokens("").await.unwrap_err().message(),
            "refresh token is required"
        );
    }

    #[test]
    fn auth_record_plan_type_defaults_to_free_when_missing() {
        let bundle = AuthBundle {
            token_data: TokenData {
                id_token: test_jwt("", "acc-12345"),
                access_token: "mock-access-token".into(),
                refresh_token: "mock-refresh-token".into(),
                email: "user@example.com".into(),
                ..TokenData::default()
            },
            ..AuthBundle::default()
        };
        let auth = build_auth_record(&bundle).unwrap();
        assert_eq!(auth.attribute("plan_type"), Some("free"));
        assert_eq!(auth.metadata_str("plan_type"), Some("free"));
        assert!(auth.file_name.ends_with("-free.json"), "{}", auth.file_name);
        assert_eq!(auth.id, auth.file_name);
        assert_eq!(auth.provider, "codex");
        assert_eq!(auth.metadata_str("type"), Some("codex"));
        assert_eq!(auth.metadata_str("access_token"), Some("mock-access-token"));
    }

    #[test]
    fn auth_record_plan_type_extracted_when_present() {
        let bundle = AuthBundle {
            token_data: TokenData {
                id_token: test_jwt("plus", "acc-12345"),
                access_token: "mock-access-token".into(),
                refresh_token: "mock-refresh-token".into(),
                email: "user@example.com".into(),
                plan_type: "plus".into(),
                ..TokenData::default()
            },
            ..AuthBundle::default()
        };
        let auth = build_auth_record(&bundle).unwrap();
        assert_eq!(auth.attribute("plan_type"), Some("plus"));
        assert_eq!(auth.metadata_str("plan_type"), Some("plus"));
        assert!(auth.file_name.ends_with("-plus.json"), "{}", auth.file_name);
        // The account hash is the first 8 hex digits of its SHA-256.
        let hash: String = Sha256::digest(b"acc-12345")
            .iter()
            .take(4)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(
            auth.file_name,
            format!("codex-{hash}-user@example.com-plus.json")
        );
    }

    #[test]
    fn auth_record_needs_an_email() {
        let error = build_auth_record(&AuthBundle::default()).unwrap_err();
        assert_eq!(
            error.message(),
            "codex token storage missing account information"
        );
    }

    #[tokio::test]
    async fn browser_login_exchanges_the_code() {
        let id_token = test_jwt("team", "acc-1");
        let token_body = json!({
            "access_token": "at",
            "refresh_token": "rt",
            "id_token": id_token,
            "expires_in": 3600,
        })
        .to_string();
        let router = Router::new().route(
            "/oauth/token",
            post(move |headers: HeaderMap, body: String| async move {
                let form = parse_form(&body);
                assert_eq!(form["grant_type"], "authorization_code");
                assert_eq!(form["code"], "the-code");
                assert_eq!(form["redirect_uri"], REDIRECT_URI);
                assert_eq!(form["client_id"], CLIENT_ID);
                assert!(!form["code_verifier"].is_empty());
                assert_eq!(headers["content-type"], "application/x-www-form-urlencoded");
                assert_eq!(headers["accept"], "application/json");
                assert!(
                    headers["user-agent"]
                        .to_str()
                        .unwrap()
                        .starts_with("open-ferry/")
                );
                token_body.clone()
            }),
        );
        let auth = auth_for(&serve(router).await);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let login = tokio::spawn(async move {
            let options = LoginOptions {
                callback_port: 0,
                callback_timeout: Duration::from_secs(10),
            };
            login(&auth, options, |prompt| {
                let _ = sender.send(prompt.clone());
            })
            .await
        });
        let prompt = receiver.await.unwrap();
        let query: HashMap<String, String> = url::Url::parse(&prompt.url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        assert_eq!(query["code_challenge_method"], "S256");
        let callback = format!(
            "http://127.0.0.1:{}/auth/callback?code=the-code&state={}",
            prompt.callback_port, query["state"]
        );
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
            .get(callback)
            .send()
            .await
            .unwrap();
        let record = login.await.unwrap().unwrap();
        let hash: String = Sha256::digest(b"acc-1")
            .iter()
            .take(4)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(
            record.file_name,
            format!("codex-{hash}-user@example.com-team.json")
        );
        assert_eq!(record.metadata_str("refresh_token"), Some("rt"));
        assert_eq!(record.metadata_str("account_id"), Some("acc-1"));
        assert_eq!(record.metadata_str("email"), Some("user@example.com"));
    }

    #[tokio::test]
    async fn browser_login_reports_errors() {
        let auth = auth_for("http://127.0.0.1:9");
        let options = LoginOptions {
            callback_port: 0,
            callback_timeout: Duration::from_millis(20),
        };
        let error = login(&auth, options, |_| {}).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "callback_timeout: Timeout waiting for OAuth callback \
             (caused by: timeout waiting for OAuth callback)"
        );

        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let options = LoginOptions {
            callback_port: taken.local_addr().unwrap().port(),
            callback_timeout: Duration::from_millis(20),
        };
        let error = login(&auth, options, |_| {}).await.unwrap_err();
        assert!(
            matches!(
                &error,
                LoginError::Authentication(AuthenticationError {
                    kind: AuthenticationErrorKind::PortInUse,
                    ..
                })
            ),
            "{error}"
        );

        let (sender, receiver) = tokio::sync::oneshot::channel();
        let login = tokio::spawn(async move {
            let options = LoginOptions {
                callback_port: 0,
                callback_timeout: Duration::from_secs(10),
            };
            login(&auth, options, |prompt| {
                let _ = sender.send(prompt.callback_port);
            })
            .await
        });
        let port = receiver.await.unwrap();
        reqwest::get(format!(
            "http://127.0.0.1:{port}/auth/callback?error=access_denied"
        ))
        .await
        .unwrap();
        let error = login.await.unwrap().unwrap_err();
        assert_eq!(error.to_string(), "OAuth error: access_denied");
    }

    #[derive(Clone)]
    struct DeviceServer {
        polls: Arc<AtomicUsize>,
        id_token: String,
    }

    #[tokio::test]
    async fn device_login_polls_then_exchanges() {
        let state = DeviceServer {
            polls: Arc::new(AtomicUsize::new(0)),
            id_token: test_jwt("plus", "acc-9"),
        };
        let router = Router::new()
            .route(
                "/api/accounts/deviceauth/usercode",
                post(|body: String| async move {
                    assert_eq!(body, format!("{{\"client_id\":\"{CLIENT_ID}\"}}"));
                    r#"{"device_auth_id":" dev-1 ","usercode":"ABCD-1234","interval":"1"}"#
                }),
            )
            .route(
                "/api/accounts/deviceauth/token",
                post(
                    |State(state): State<DeviceServer>, body: String| async move {
                        assert_eq!(
                            body,
                            r#"{"device_auth_id":"dev-1","user_code":"ABCD-1234"}"#
                        );
                        if state.polls.fetch_add(1, Ordering::SeqCst) == 0 {
                            return (StatusCode::FORBIDDEN, String::new()).into_response();
                        }
                        json!({
                            "authorization_code": "dev-code",
                            "code_verifier": "dev-verifier",
                            "code_challenge": "dev-challenge",
                        })
                        .to_string()
                        .into_response()
                    },
                ),
            )
            .route(
                "/oauth/token",
                post(
                    |State(state): State<DeviceServer>, body: String| async move {
                        let form = parse_form(&body);
                        assert_eq!(form["code"], "dev-code");
                        assert_eq!(form["code_verifier"], "dev-verifier");
                        assert_eq!(form["redirect_uri"], DEVICE_REDIRECT_URI);
                        json!({"access_token": "at", "id_token": state.id_token, "expires_in": 10})
                            .to_string()
                    },
                ),
            )
            .with_state(state.clone());
        let base = serve(router).await;
        let auth = auth_for(&base);
        let mut shown = None;
        let record = login_with_device_code(&auth, |code| shown = Some(code.clone()))
            .await
            .unwrap();
        let shown = shown.unwrap();
        assert_eq!(shown.user_code, "ABCD-1234");
        assert_eq!(shown.verification_url, format!("{base}/codex/device"));
        assert_eq!(state.polls.load(Ordering::SeqCst), 2);
        assert_eq!(record.attribute("plan_type"), Some("plus"));
    }

    #[tokio::test]
    async fn device_login_reports_errors() {
        let router = Router::new().route(
            "/api/accounts/deviceauth/usercode",
            post(|| async { (StatusCode::NOT_FOUND, "") }),
        );
        let auth = auth_for(&serve(router).await);
        let error = login_with_device_code(&auth, |_| {}).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "codex device endpoint is unavailable (status 404)"
        );

        let router = Router::new().route(
            "/api/accounts/deviceauth/usercode",
            post(|| async { (StatusCode::BAD_GATEWAY, "  ") }),
        );
        let auth = auth_for(&serve(router).await);
        let error = login_with_device_code(&auth, |_| {}).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "codex device code request failed with status 502: empty response body"
        );

        let router = Router::new()
            .route(
                "/api/accounts/deviceauth/usercode",
                post(|| async { r#"{"device_auth_id":"d","user_code":"u"}"# }),
            )
            .route(
                "/api/accounts/deviceauth/token",
                post(|| async { (StatusCode::BAD_REQUEST, " denied ") }),
            );
        let auth = auth_for(&serve(router).await);
        let error = login_with_device_code(&auth, |_| {}).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "codex device token polling failed with status 400: denied"
        );

        let router = Router::new().route(
            "/api/accounts/deviceauth/usercode",
            post(|| async { r#"{"device_auth_id":"d"}"# }),
        );
        let auth = auth_for(&serve(router).await);
        let error = login_with_device_code(&auth, |_| {}).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "codex device flow did not return required fields"
        );
    }

    #[test]
    fn device_poll_interval() {
        assert_eq!(poll_interval(&json!(" 7 ")), Duration::from_secs(7));
        assert_eq!(poll_interval(&json!(3)), Duration::from_secs(3));
        for value in [json!(0), json!(-1), json!("x"), json!(2.5), Value::Null] {
            assert_eq!(poll_interval(&value), DEVICE_DEFAULT_POLL_INTERVAL);
        }
    }
}
