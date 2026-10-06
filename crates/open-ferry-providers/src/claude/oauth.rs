// Ported from CLIProxyAPI internal/auth/claude/anthropic_auth.go, errors.go
// and oauth_response.go, and sdk/auth/claude.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Claude's OAuth: the browser login with PKCE, the code exchange and the
//! token refresh, against Anthropic's auth server.
//!
//! This is the flow upstream runs for Claude's OAuth client. Anthropic
//! doesn't publish a reference for it. The authorization URL carries the
//! standard OAuth 2.0 parameters (`client_id`, `response_type=code`,
//! `redirect_uri`, `scope`, `state`) and PKCE's (`code_challenge`,
//! `code_challenge_method=S256`), plus `code=true`, which Anthropic's server
//! takes to show the code on its page for pasting, as `code#state`. None of
//! them describe the caller. The code exchange and refresh send JSON, as
//! Anthropic's token endpoint expects, with only `Content-Type`, `Accept`
//! and our own `User-Agent`, `open-ferry/<version>`.
//!
//! A login takes a function that shows the user the URL, and returns the
//! credential record and its callback server; the caller saves the record
//! with an [`AuthStore`](open_ferry_core::auth::AuthStore), then
//! [finishes](crate::oauth::CallbackServer::finish) the server.
//!
//! Deviations from upstream:
//! - The OAuth calls send no axios headers (`User-Agent: axios/...`, its
//!   `Accept` list, `Accept-Encoding`, `Connection: close`); they send
//!   `Content-Type: application/json`, `Accept: application/json` and our
//!   `User-Agent`. With no `Accept-Encoding` sent, a compressed response
//!   isn't expected: one comes back as an error naming its encoding, where
//!   upstream decodes it.
//! - A login doesn't make a device ID pool (`claude_device_ids`), and the
//!   bundle has none.
//! - After a code exchange or a refresh, upstream asks the OAuth profile
//!   endpoint (and, after an exchange, the `claude_cli` roles endpoint)
//!   for the account, as Claude Code does. Neither is called: the account
//!   and organization come from the token response. A refresh response
//!   without them leaves the stored ones as they are.
//! - The refresh's TLS handshake timeout (10 seconds, for upstream's uTLS
//!   transport) isn't ported; the refresh as a whole still has 30 seconds.
//! - Logins don't open a browser or print; the `present` function shows the
//!   URL. Pasting the callback URL by hand isn't supported yet.
//! - The callback server turns away a callback with the wrong `state` and
//!   keeps waiting (see [`crate::oauth`]), so there's no `invalid_state`
//!   error.
//! - The browser login gives back its callback server, still up, with the
//!   credential, so that the browser's page can say whether the credential
//!   was saved (see [`crate::oauth`]); upstream's login stops the server once
//!   the code is exchanged. A login that fails after the redirect tells the
//!   page why, with the code and the PKCE verifier redacted.
//! - A failed code exchange is logged with the code and the PKCE verifier
//!   redacted from the token endpoint's answer; upstream logs the answer as
//!   it came.
//! - Every endpoint can be changed, through [`Endpoints`].
//! - Response bodies are read up to 1 MiB.
//! - JSON errors after upstream's prefixes (`failed to parse token response:
//!   ` and the like) are this module's own words.
//! - Concurrent refreshes of a token share one request, as upstream's
//!   single-flight group does. The group and the 429 block are keyed by a
//!   hash of the token endpoint and the token, not the token itself.
//! - Waits between refresh attempts stop when the caller drops the future,
//!   where upstream watches its context.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Local, SecondsFormat, TimeDelta, Utc};
use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use http::HeaderMap;
use open_ferry_core::auth::Auth;
use open_ferry_translate::go::json_string;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::client::{Clients, error_chain, read_body};
use super::token::{AuthBundle, TokenData, create_token_storage, credential_file_name, expiry};
use crate::json::{key_of, object_or_null, set_int, set_string};
use crate::oauth::page::{Failure, Outcome};
use crate::oauth::{
    BrowserLogin, CallbackError, CallbackResult, CallbackServer, Pkce, generate_state,
};
use crate::redact::{Policy, Secrets};

/// Claude's OAuth client ID.
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
/// The redirect URI of the browser login. It stays on port 54545 even when
/// the callback server listens elsewhere, as upstream's does; a different
/// port is for forwarding the redirect, as over an SSH tunnel.
pub const REDIRECT_URI: &str = "http://localhost:54545/callback";
/// The scopes a login asks for, and a refresh keeps.
pub const SCOPE: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
/// The callback server's default port.
pub const DEFAULT_CALLBACK_PORT: u16 = 54545;
/// How long before its tokens expire a credential is refreshed (upstream's
/// `RefreshLead`).
pub const REFRESH_LEAD: Duration = Duration::from_secs(4 * 60 * 60);
/// The path the callback server listens on.
const CALLBACK_PATH: &str = "/callback";
/// How long a refresh request may take.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);
/// The shortest and longest a 429 blocks refreshes of a token.
const REFRESH_MIN_BACKOFF: Duration = Duration::from_secs(5);
const REFRESH_MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);
/// The most of an auth server response that is read.
const MAX_RESPONSE_BYTES: usize = 1 << 20;

/// Anthropic's auth server endpoints. Tests point these at a local server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// Where the browser login starts.
    pub authorize_url: String,
    /// Where codes and refresh tokens are exchanged for tokens.
    pub token_url: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            authorize_url: "https://claude.ai/oauth/authorize".to_owned(),
            token_url: "https://platform.claude.com/v1/oauth/token".to_owned(),
        }
    }
}

impl Endpoints {
    /// Upstream's endpoint paths under one `base`, such as
    /// `http://127.0.0.1:PORT` for a test server.
    pub fn with_base(base: &str) -> Self {
        let base = base.trim_end_matches('/');
        Self {
            authorize_url: format!("{base}/oauth/authorize"),
            token_url: format!("{base}/v1/oauth/token"),
        }
    }
}

/// A failed OAuth call. The text may quote an endpoint's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    message: String,
    status: u16,
    retryable: bool,
}

impl Error {
    /// An error without a status from the token endpoint. Retrying it isn't
    /// safe: after a transport or decoding failure, Anthropic may have used
    /// up the single-use refresh token even though its answer was lost, and
    /// a replay would turn that into `invalid_grant`.
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status: 0,
            retryable: false,
        }
    }

    /// The token endpoint's error status (upstream's `refreshHTTPError`).
    fn http(status: u16, body: &str, retryable: bool) -> Self {
        Self {
            message: format!("token refresh failed with status {status}: {body}"),
            status,
            retryable,
        }
    }

    /// What went wrong.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The token endpoint's status, or 0 when it didn't answer with one.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// Whether trying again is safe and may help. Only an error status can
    /// be: 500 or more from a refresh, and also 429 from a code exchange.
    /// A transport or decoding error never is.
    pub fn is_retryable(&self) -> bool {
        self.retryable
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
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

/// Calls Anthropic's auth server for Claude (upstream's `ClaudeAuth`).
#[derive(Clone, Debug)]
pub struct ClaudeAuth {
    client: reqwest::Client,
    endpoints: Endpoints,
}

impl ClaudeAuth {
    /// Calls the real endpoints with `client`.
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            endpoints: Endpoints::default(),
        }
    }

    /// Calls through `proxy_url`: empty for the environment's proxy,
    /// `direct` or `none` for no proxy, or an `http` or `https` proxy URL
    /// (upstream's `NewClaudeAuthWithProxyURL`).
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
            ("code", "true"),
            ("client_id", CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", REDIRECT_URI),
            ("scope", SCOPE),
            ("code_challenge", &pkce.challenge),
            ("code_challenge_method", "S256"),
            ("state", state),
        ]);
        format!("{}?{params}", self.endpoints.authorize_url)
    }

    /// Exchanges a browser login's code for tokens
    /// (`ExchangeCodeForTokens`). The code may come as `code#state`, as
    /// Anthropic's page shows it; the state after `#` then wins over
    /// `state`.
    pub async fn exchange_code_for_tokens(
        &self,
        code: &str,
        state: &str,
        pkce: &Pkce,
    ) -> Result<AuthBundle, Error> {
        let (code, code_state) = parse_code_and_state(code);
        let state = code_state.unwrap_or(state);
        let body = format!(
            "{{\"grant_type\":\"authorization_code\",\"code\":{},\"redirect_uri\":{},\
             \"client_id\":{},\"code_verifier\":{},\"state\":{}}}",
            json_string(code),
            json_string(REDIRECT_URI),
            json_string(CLIENT_ID),
            json_string(&pkce.verifier),
            json_string(state),
        );
        let response = self.json_request(body).send().await.map_err(|e| {
            Error::new(format!(
                "token exchange request failed: {}",
                error_chain(&e)
            ))
        })?;
        let status = response.status().as_u16();
        let body = read_oauth_body(response)
            .await
            .map_err(|e| Error::new(format!("failed to read token response: {e}")))?;
        if status != 200 {
            return Err(Error {
                message: format!(
                    "token exchange failed with status {status}: {}",
                    String::from_utf8_lossy(&body)
                ),
                status,
                retryable: status >= 500 || status == 429,
            });
        }
        let tokens = decode_token_response(&body)
            .map_err(|e| Error::new(format!("failed to parse token response: {e}")))?;
        Ok(AuthBundle {
            api_key: String::new(),
            token_data: tokens.into_token_data(),
            last_refresh: super::token::now_rfc3339(),
        })
    }

    /// Gets new tokens with `refresh_token` (`RefreshTokens`). Calls for the
    /// same token at the same time share one request, which runs to the end
    /// within 30 seconds even if its callers stop waiting. After a 429, the
    /// token isn't sent again until the server's `Retry-After` has passed
    /// (5 seconds to 5 minutes).
    pub async fn refresh_tokens(&self, refresh_token: &str) -> Result<TokenData, Error> {
        if refresh_token.is_empty() {
            return Err(Error::new("refresh token is required"));
        }
        let key: RefreshKey = Sha256::new()
            .chain_update(self.endpoints.token_url.as_bytes())
            .chain_update([0])
            .chain_update(refresh_token.as_bytes())
            .finalize()
            .into();
        check_refresh_block(&key)?;
        let shared = {
            let mut refreshes = REFRESHES.lock().unwrap_or_else(PoisonError::into_inner);
            match refreshes.get(&key) {
                Some(shared) => shared.clone(),
                None => {
                    let auth = self.clone();
                    let token = refresh_token.to_owned();
                    let task = tokio::spawn(async move {
                        let result = auth.refresh_once(&key, &token).await;
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

    async fn refresh_once(
        &self,
        key: &RefreshKey,
        refresh_token: &str,
    ) -> Result<TokenData, Error> {
        check_refresh_block(key)?;
        // Keys in the order Go writes a map.
        let body = format!(
            "{{\"client_id\":{},\"grant_type\":\"refresh_token\",\"refresh_token\":{},\"scope\":{}}}",
            json_string(CLIENT_ID),
            json_string(refresh_token),
            json_string(SCOPE),
        );
        let response = self
            .json_request(body)
            .timeout(REFRESH_TIMEOUT)
            .send()
            .await
            .map_err(|e| {
                Error::new(format!("token refresh request failed: {}", error_chain(&e)))
            })?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body = read_oauth_body(response)
            .await
            .map_err(|e| Error::new(format!("failed to read refresh response: {e}")))?;
        if status != 200 {
            let message = String::from_utf8_lossy(&body);
            if status == 429 {
                let until = Local::now() + retry_after(&headers, Utc::now());
                REFRESH_BLOCKS
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(*key, until);
                return Err(Error::http(status, &message, false));
            }
            return Err(Error::http(status, &message, status >= 500));
        }
        let mut tokens = decode_token_response(&body)
            .map_err(|e| Error::new(format!("failed to parse token response: {e}")))?;
        REFRESH_BLOCKS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(key);
        if tokens.refresh_token.trim().is_empty() {
            refresh_token.clone_into(&mut tokens.refresh_token);
        }
        Ok(tokens.into_token_data())
    }

    /// [`refresh_tokens`](Self::refresh_tokens), tried up to `max_retries`
    /// times, waiting a second more before each retry. Only a 5xx status is
    /// tried again. Any other error stops it, including a lost or unreadable
    /// answer, after which the token may already be used up
    /// (`RefreshTokensWithRetry`).
    pub async fn refresh_tokens_with_retry(
        &self,
        refresh_token: &str,
        max_retries: u32,
    ) -> Result<TokenData, Error> {
        let mut last_error: Option<Error> = None;
        for attempt in 0..max_retries {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(attempt.into())).await;
            }
            match self.refresh_tokens(refresh_token).await {
                Ok(data) => return Ok(data),
                Err(error) => {
                    tracing::warn!("Token refresh attempt {} failed: {error}", attempt + 1);
                    let retryable = error.retryable;
                    last_error = Some(error);
                    if !retryable {
                        break;
                    }
                }
            }
        }
        let (last, status) = last_error.map_or_else(
            || ("%!w(<nil>)".to_owned(), 0),
            |error| (error.message, error.status),
        );
        Err(Error {
            message: format!("token refresh failed after {max_retries} attempts: {last}"),
            status,
            retryable: false,
        })
    }

    fn json_request(&self, body: String) -> reqwest::RequestBuilder {
        self.client
            .post(&self.endpoints.token_url)
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(http::header::ACCEPT, "application/json")
            .body(body)
    }
}

type RefreshKey = [u8; 32];
type SharedRefresh = Shared<BoxFuture<'static, Result<TokenData, Error>>>;

/// Refreshes in flight, by a hash of the token endpoint and refresh token.
static REFRESHES: LazyLock<Mutex<HashMap<RefreshKey, SharedRefresh>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Until when a token's refreshes are blocked after a 429, by the same key.
static REFRESH_BLOCKS: LazyLock<Mutex<HashMap<RefreshKey, DateTime<Local>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The error for a token whose refreshes are blocked, if they are.
fn check_refresh_block(key: &RefreshKey) -> Result<(), Error> {
    let until = REFRESH_BLOCKS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(key)
        .copied();
    match until {
        Some(until) if until > Local::now() => Err(Error::http(
            429,
            &format!(
                "refresh temporarily blocked until {}",
                until.to_rfc3339_opts(SecondsFormat::Secs, true)
            ),
            false,
        )),
        _ => Ok(()),
    }
}

/// How long a 429 blocks refreshes: `Retry-After` in seconds or as a date,
/// else `Retry-After-Ms`, else 5 seconds, kept within 5 seconds and 5
/// minutes (`parseClaudeRetryAfter`).
fn retry_after(headers: &HeaderMap, now: DateTime<Utc>) -> TimeDelta {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .unwrap_or_default()
    };
    let raw = header("retry-after");
    if !raw.is_empty() {
        if let Some(seconds) = go_duration_secs(raw, 1.0) {
            return clamp_backoff(seconds);
        }
        if let Some(when) = super::ratelimit::parse_http_date(raw) {
            return clamp_backoff((when - now).as_seconds_f64());
        }
    }
    let raw = header("retry-after-ms");
    if !raw.is_empty()
        && let Some(seconds) = go_duration_secs(raw, 0.001)
    {
        return clamp_backoff(seconds);
    }
    to_delta(REFRESH_MIN_BACKOFF)
}

fn clamp_backoff(seconds: f64) -> TimeDelta {
    let min = REFRESH_MIN_BACKOFF.as_secs_f64();
    let max = REFRESH_MAX_BACKOFF.as_secs_f64();
    let seconds = if seconds.is_nan() {
        min
    } else {
        seconds.clamp(min, max)
    };
    to_delta(Duration::from_secs_f64(seconds))
}

fn to_delta(duration: Duration) -> TimeDelta {
    TimeDelta::from_std(duration).unwrap_or(TimeDelta::MAX)
}

/// Go's `time.ParseDuration` of a number with a unit appended, in seconds:
/// an optional sign, then digits with at most one `.`.
fn go_duration_secs(raw: &str, unit_secs: f64) -> Option<f64> {
    let digits = raw.strip_prefix(['+', '-']).unwrap_or(raw);
    let valid = !digits.is_empty()
        && digits != "."
        && digits.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && digits.bytes().filter(|&b| b == b'.').count() <= 1;
    if !valid {
        return None;
    }
    let value: f64 = raw.parse().ok()?;
    Some(value * unit_secs)
}

/// Splits `code#state` (`parseCodeAndState`).
fn parse_code_and_state(code: &str) -> (&str, Option<&str>) {
    let mut parts = code.split('#');
    let code = parts.next().unwrap_or_default();
    let state = parts.next().filter(|state| !state.is_empty());
    (code, state)
}

/// Reads an auth server response (`readClaudeOAuthResponseBody`).
async fn read_oauth_body(response: reqwest::Response) -> Result<Vec<u8>, String> {
    let encodings: Vec<String> = response
        .headers()
        .get_all(http::header::CONTENT_ENCODING)
        .iter()
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
        .collect();
    let body = read_body(response, MAX_RESPONSE_BYTES)
        .await
        .map_err(|e| e.to_string())?;
    let joined = encodings.join(",");
    for encoding in joined.split(',').rev() {
        let encoding = crate::json::lower_trim(encoding);
        if encoding.is_empty() || encoding == "identity" {
            continue;
        }
        return Err(format!(
            "decode Claude OAuth response: unsupported content encoding {}",
            open_ferry_translate::go::quote(&encoding)
        ));
    }
    Ok(body)
}

/// The token endpoint's answer (upstream's `tokenResponse`).
#[derive(Default)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    token_type: String,
    expires_in: i64,
    organization_uuid: String,
    organization_name: String,
    account_uuid: String,
    account_email: String,
}

impl TokenResponse {
    fn into_token_data(self) -> TokenData {
        TokenData {
            expire: expiry(self.expires_in),
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            email: self.account_email,
            account_uuid: self.account_uuid,
            organization_uuid: self.organization_uuid,
            organization_name: self.organization_name,
        }
    }
}

/// Decodes the token endpoint's answer as Go's `json.Unmarshal` would.
fn decode_token_response(body: &[u8]) -> Result<TokenResponse, String> {
    let value: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let mut tokens = TokenResponse::default();
    let Some(object) = object_or_null(&value, "tokenResponse")? else {
        return Ok(tokens);
    };
    for (key, value) in object {
        let fields = [
            "access_token",
            "refresh_token",
            "token_type",
            "expires_in",
            "organization",
            "account",
        ];
        let Some(field) = key_of(key, &fields) else {
            continue;
        };
        let path = format!("tokenResponse.{field}");
        match field {
            "access_token" => set_string(&mut tokens.access_token, value, &path)?,
            "refresh_token" => set_string(&mut tokens.refresh_token, value, &path)?,
            "token_type" => set_string(&mut tokens.token_type, value, &path)?,
            "expires_in" => set_int(&mut tokens.expires_in, value, &path)?,
            "organization" => {
                let Some(organization) = object_or_null(value, &path)? else {
                    continue;
                };
                decode_pair(
                    organization,
                    &path,
                    ("uuid", &mut tokens.organization_uuid),
                    ("name", &mut tokens.organization_name),
                )?;
            }
            _ => {
                let Some(account) = object_or_null(value, &path)? else {
                    continue;
                };
                decode_pair(
                    account,
                    &path,
                    ("uuid", &mut tokens.account_uuid),
                    ("email_address", &mut tokens.account_email),
                )?;
            }
        }
    }
    Ok(tokens)
}

/// Decodes a nested struct of two string fields.
fn decode_pair(
    object: &Map<String, Value>,
    path: &str,
    first: (&'static str, &mut String),
    second: (&'static str, &mut String),
) -> Result<(), String> {
    let names = [first.0, second.0];
    let (first_target, second_target) = (first.1, second.1);
    for (key, value) in object {
        let Some(field) = key_of(key, &names) else {
            continue;
        };
        let target = if field == names[0] {
            &mut *first_target
        } else {
            &mut *second_target
        };
        set_string(target, value, &format!("{path}.{field}"))?;
    }
    Ok(())
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
/// `ClaudeAuthenticator.Login`).
///
/// The callback server stays up for the browser's success page: the caller
/// saves the credential and then [finishes](CallbackServer::finish) the
/// server with how that went. A login that fails after the redirect
/// finishes the server itself, with the reason, redacted.
pub async fn login<F>(
    auth: &ClaudeAuth,
    options: LoginOptions,
    present: F,
) -> Result<BrowserLogin, LoginError>
where
    F: FnOnce(&AuthorizationPrompt),
{
    let pkce = Pkce::generate();
    let state = generate_state();
    let mut server = CallbackServer::start(options.callback_port, CALLBACK_PATH, &state, "Claude")
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
    tracing::debug!("Claude authorization code received; exchanging for tokens");
    let bundle = match auth.exchange_code_for_tokens(&code, &state, &pkce).await {
        Ok(bundle) => bundle,
        Err(error) => {
            // The token endpoint's answer may quote what it was sent.
            let (code_part, _) = parse_code_and_state(&code);
            let escaped = query_escape(&code);
            let secrets = Secrets::from_iter([
                code.as_str(),
                code_part,
                escaped.as_str(),
                pkce.verifier.as_str(),
            ]);
            tracing::error!(
                "Token exchange failed: {}",
                secrets.str(error.message(), Policy::Disk)
            );
            let reason = secrets.str(error.message(), Policy::Client);
            let failure = Failure::unfinished("Claude", &reason);
            server.finish(Outcome::Failed(failure)).await;
            return Err(LoginError::authentication(
                AuthenticationErrorKind::CodeExchangeFailed,
                error.message,
            ));
        }
    };
    match build_auth_record(&bundle) {
        Ok(auth) => Ok(BrowserLogin { auth, server }),
        Err(error) => {
            let failure = Failure::unfinished("Claude", error.message());
            server.finish(Outcome::Failed(failure)).await;
            Err(LoginError::Other(error))
        }
    }
}

/// The credential record for a login's tokens: named by
/// [`credential_file_name`], with the file's JSON as its metadata.
pub fn build_auth_record(bundle: &AuthBundle) -> Result<Auth, Error> {
    let mut storage = create_token_storage(bundle);
    if storage.email.is_empty() {
        return Err(Error::new(
            "claude token storage missing account information",
        ));
    }
    let file_name = credential_file_name(
        &storage.email,
        &storage.organization_uuid,
        &storage.account_uuid,
    );
    let mut extra = Map::new();
    extra.insert("email".to_owned(), Value::from(storage.email.as_str()));
    let optional = [
        ("account_uuid", &storage.account_uuid),
        ("organization_uuid", &storage.organization_uuid),
        ("organization_name", &storage.organization_name),
    ];
    for (key, value) in optional {
        if !value.is_empty() {
            extra.insert(key.to_owned(), Value::from(value.as_str()));
        }
    }
    storage.metadata = extra;
    tracing::info!("Claude authentication successful");
    Ok(Auth {
        id: file_name.clone(),
        provider: "claude".to_owned(),
        file_name,
        metadata: storage.to_json(),
        ..Auth::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::extract::State;
    use axum::http::{HeaderMap as AxumHeaders, StatusCode};
    use axum::routing::post;
    use serde_json::json;
    use tokio::sync::Notify;

    use crate::oauth::tests::{assert_page, fetch};

    /// What the mock token endpoint saw.
    type Seen = Arc<Mutex<Option<(AxumHeaders, Value)>>>;

    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{address}")
    }

    fn auth_for(base: &str) -> ClaudeAuth {
        ClaudeAuth::with_proxy_url("direct").with_endpoints(Endpoints::with_base(base))
    }

    const TOKEN_BODY: &str = r#"{
        "access_token":"access",
        "refresh_token":"refresh",
        "token_type":"Bearer",
        "expires_in":3600,
        "account":{"uuid":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","email_address":"user@example.com"},
        "organization":{"uuid":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","name":"Example Org"}
    }"#;

    #[test]
    fn auth_url_has_upstreams_parameters() {
        let pkce = Pkce {
            verifier: "v".into(),
            challenge: "chal-lenge_~".into(),
        };
        let url = ClaudeAuth::new(reqwest::Client::new()).generate_auth_url("st ate", &pkce);
        assert_eq!(
            url,
            "https://claude.ai/oauth/authorize?client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e\
             &code=true&code_challenge=chal-lenge_~&code_challenge_method=S256\
             &redirect_uri=http%3A%2F%2Flocalhost%3A54545%2Fcallback&response_type=code\
             &scope=user%3Aprofile+user%3Ainference+user%3Asessions%3Aclaude_code\
             +user%3Amcp_servers+user%3Afile_upload&state=st+ate"
        );
    }

    #[test]
    fn splits_code_and_state() {
        assert_eq!(parse_code_and_state("abc#xyz"), ("abc", Some("xyz")));
        assert_eq!(parse_code_and_state("abc"), ("abc", None));
        assert_eq!(parse_code_and_state("abc#"), ("abc", None));
        assert_eq!(parse_code_and_state("a#b#c"), ("a", Some("b")));
    }

    #[test]
    fn decodes_token_responses_like_go() {
        let tokens = decode_token_response(TOKEN_BODY.as_bytes()).unwrap();
        assert_eq!(tokens.access_token, "access");
        assert_eq!(tokens.expires_in, 3600);
        assert_eq!(tokens.account_email, "user@example.com");
        assert_eq!(tokens.organization_name, "Example Org");
        let tokens =
            decode_token_response(br#"{"ACCESS_TOKEN":"a","account":null,"x":[]}"#).unwrap();
        assert_eq!(tokens.access_token, "a");
        assert!(decode_token_response(br#"{"expires_in":1.5}"#).is_err());
        assert!(decode_token_response(br#"{"account":{"uuid":1}}"#).is_err());
        assert!(decode_token_response(br#"{"organization":"x"}"#).is_err());
        assert!(decode_token_response(b"null").is_ok());
        assert!(decode_token_response(b"[]").is_err());
    }

    #[test]
    fn retry_after_parses_seconds_dates_and_milliseconds() {
        let now = Utc::now();
        let headers = |pairs: &[(&'static str, &str)]| {
            let mut map = HeaderMap::new();
            for (name, value) in pairs {
                map.insert(*name, value.parse().unwrap());
            }
            map
        };
        assert_eq!(
            retry_after(&headers(&[("retry-after", "60")]), now).num_seconds(),
            60
        );
        assert_eq!(
            retry_after(&headers(&[("retry-after", "1")]), now).num_seconds(),
            5
        );
        assert_eq!(
            retry_after(&headers(&[("retry-after", "9999")]), now).num_seconds(),
            300
        );
        assert_eq!(
            retry_after(&headers(&[("retry-after-ms", "120000")]), now).num_seconds(),
            120
        );
        let date = (now + TimeDelta::seconds(90)).format("%a, %d %b %Y %H:%M:%S GMT");
        let delta = retry_after(&headers(&[("retry-after", &date.to_string())]), now);
        assert!((88..=90).contains(&delta.num_seconds()), "{delta}");
        assert_eq!(
            retry_after(&headers(&[("retry-after", "soon")]), now).num_seconds(),
            5
        );
        assert_eq!(retry_after(&HeaderMap::new(), now).num_seconds(), 5);
    }

    #[tokio::test]
    async fn exchange_sends_json_and_keeps_the_account() {
        let seen = Arc::new(Mutex::new(None::<(AxumHeaders, Value)>));
        let router = Router::new()
            .route(
                "/v1/oauth/token",
                post(
                    |State(seen): State<Seen>, headers: AxumHeaders, body: String| async move {
                        let body: Value = serde_json::from_str(&body).unwrap();
                        *seen.lock().unwrap() = Some((headers, body));
                        TOKEN_BODY
                    },
                ),
            )
            .with_state(seen.clone());
        let auth = auth_for(&serve(router).await);
        let pkce = Pkce {
            verifier: "verifier".into(),
            challenge: "challenge".into(),
        };
        let bundle = auth
            .exchange_code_for_tokens("the-code#fragment-state", "state", &pkce)
            .await
            .unwrap();
        assert_eq!(bundle.token_data.access_token, "access");
        assert_eq!(bundle.token_data.refresh_token, "refresh");
        assert_eq!(
            bundle.token_data.account_uuid,
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
        );
        assert_eq!(bundle.token_data.email, "user@example.com");
        assert_eq!(
            bundle.token_data.organization_uuid,
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
        );
        assert_eq!(bundle.token_data.organization_name, "Example Org");
        assert!(!bundle.token_data.expire.is_empty());

        let (headers, body) = seen.lock().unwrap().take().unwrap();
        assert_eq!(
            body,
            json!({
                "grant_type": "authorization_code",
                "code": "the-code",
                "redirect_uri": REDIRECT_URI,
                "client_id": CLIENT_ID,
                "code_verifier": "verifier",
                "state": "fragment-state",
            })
        );
        assert_eq!(headers["content-type"], "application/json");
        assert_eq!(headers["user-agent"], super::super::client::USER_AGENT);
        assert!(headers.get("accept-encoding").is_none());
        assert!(headers.get("x-app").is_none());

        let record = build_auth_record(&bundle).unwrap();
        assert_eq!(record.id, "claude-00f765af-user@example.com.json");
        assert_eq!(record.file_name, record.id);
        assert_eq!(record.provider, "claude");
        assert_eq!(record.metadata["type"], "claude");
        assert_eq!(record.metadata["access_token"], "access");
        assert_eq!(record.metadata["organization_name"], "Example Org");
        assert!(!record.metadata.contains_key("claude_device_ids"));
    }

    #[tokio::test]
    async fn exchange_reports_status_errors() {
        let router = Router::new().route(
            "/v1/oauth/token",
            post(|| async { (StatusCode::BAD_REQUEST, r#"{"error":"invalid_grant"}"#) }),
        );
        let auth = auth_for(&serve(router).await);
        let pkce = Pkce::generate();
        let error = auth
            .exchange_code_for_tokens("code", "state", &pkce)
            .await
            .unwrap_err();
        assert_eq!(
            error.message(),
            r#"token exchange failed with status 400: {"error":"invalid_grant"}"#
        );
        assert_eq!(error.status(), 400);
    }

    #[test]
    fn login_needs_an_email() {
        let error = build_auth_record(&AuthBundle::default()).unwrap_err();
        assert_eq!(
            error.message(),
            "claude token storage missing account information"
        );
        let legacy = build_auth_record(&AuthBundle {
            token_data: TokenData {
                email: "a@b.c".into(),
                ..TokenData::default()
            },
            ..AuthBundle::default()
        })
        .unwrap();
        assert_eq!(legacy.id, "claude-a@b.c.json");
        assert!(!legacy.metadata.contains_key("account_uuid"));
    }

    #[tokio::test]
    async fn refresh_429_blocks_immediate_replay() {
        let calls = Arc::new(AtomicUsize::new(0));
        let router = Router::new()
            .route(
                "/v1/oauth/token",
                post(|State(calls): State<Arc<AtomicUsize>>| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        [("retry-after", "60")],
                        r#"{"error":"rate_limited"}"#,
                    )
                }),
            )
            .with_state(calls.clone());
        let auth = auth_for(&serve(router).await);
        let error = auth
            .refresh_tokens_with_retry("dummy_refresh_token", 3)
            .await
            .unwrap_err();
        assert!(error.message().contains("status 429"), "{error}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let error = auth
            .refresh_tokens_with_retry("dummy_refresh_token", 3)
            .await
            .unwrap_err();
        assert!(
            error
                .message()
                .contains("refresh temporarily blocked until"),
            "{error}"
        );
        assert_eq!(error.status(), 429);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn refresh_retries_server_errors_only() {
        let calls = Arc::new(AtomicUsize::new(0));
        let router = Router::new()
            .route(
                "/v1/oauth/token",
                post(|State(calls): State<Arc<AtomicUsize>>| async move {
                    let call = calls.fetch_add(1, Ordering::SeqCst);
                    if call == 0 {
                        (StatusCode::INTERNAL_SERVER_ERROR, "boom")
                    } else {
                        (StatusCode::BAD_REQUEST, "bad")
                    }
                }),
            )
            .with_state(calls.clone());
        let auth = auth_for(&serve(router).await);
        let error = auth
            .refresh_tokens_with_retry("rt-retry", 3)
            .await
            .unwrap_err();
        assert_eq!(
            error.message(),
            "token refresh failed after 3 attempts: token refresh failed with status 400: bad"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            auth.refresh_tokens("").await.unwrap_err().message(),
            "refresh token is required"
        );
    }

    /// A loopback token endpoint that writes `response` to each connection
    /// and closes it, counting connections.
    async fn serve_raw(response: &'static [u8]) -> (String, Arc<AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                counted.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    if !response.is_empty() {
                        // Read the request head before answering.
                        let mut buffer = [0_u8; 4096];
                        let _ = stream.read(&mut buffer).await;
                        let _ = stream.write_all(response).await;
                    }
                    // Dropping the stream closes the connection.
                });
            }
        });
        (format!("http://{address}"), calls)
    }

    // Ports TestRefreshTokensWithRetry_DoesNotReplayAfterResponseReadError:
    // the body ends before its Content-Length.
    #[tokio::test]
    async fn refresh_does_not_replay_after_a_response_read_error() {
        let (base, calls) =
            serve_raw(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{\"access").await;
        let error = auth_for(&base)
            .refresh_tokens_with_retry("rt-single-use-read", 3)
            .await
            .unwrap_err();
        assert!(
            error.message().contains("failed to read refresh response"),
            "{error}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    // Ports TestRefreshTokensWithRetry_DoesNotReplayAfterJSONDecodeError.
    #[tokio::test]
    async fn refresh_does_not_replay_after_a_json_decode_error() {
        let calls = Arc::new(AtomicUsize::new(0));
        let router = Router::new()
            .route(
                "/v1/oauth/token",
                post(|State(calls): State<Arc<AtomicUsize>>| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    "invalid json payload"
                }),
            )
            .with_state(calls.clone());
        let error = auth_for(&serve(router).await)
            .refresh_tokens_with_retry("rt-single-use-json", 3)
            .await
            .unwrap_err();
        assert!(
            error.message().contains("failed to parse token response"),
            "{error}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    // Ports TestRefreshTokensWithRetry_DoesNotReplayAfterTransportError: the
    // endpoint takes the connection and closes it without an answer.
    #[tokio::test]
    async fn refresh_does_not_replay_after_a_transport_error() {
        let (base, calls) = serve_raw(b"").await;
        let error = auth_for(&base)
            .refresh_tokens_with_retry("rt-single-use-transport", 3)
            .await
            .unwrap_err();
        assert!(
            error.message().contains("token refresh request failed"),
            "{error}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn refresh_deduplicates_concurrent_calls_and_keeps_the_account() {
        #[derive(Clone)]
        struct Shared {
            calls: Arc<AtomicUsize>,
            release: Arc<Notify>,
            body: Arc<Mutex<Option<Value>>>,
        }
        let shared = Shared {
            calls: Arc::new(AtomicUsize::new(0)),
            release: Arc::new(Notify::new()),
            body: Arc::new(Mutex::new(None)),
        };
        let router = Router::new()
            .route(
                "/v1/oauth/token",
                post(|State(shared): State<Shared>, body: String| async move {
                    shared.calls.fetch_add(1, Ordering::SeqCst);
                    *shared.body.lock().unwrap() = serde_json::from_str(&body).ok();
                    shared.release.notified().await;
                    r#"{
                        "access_token":"new-access",
                        "refresh_token":"",
                        "token_type":"Bearer",
                        "expires_in":3600,
                        "account":{"uuid":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","email_address":"shared@example.com"},
                        "organization":{"uuid":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","name":"Shared Org"}
                    }"#
                }),
            )
            .with_state(shared.clone());
        let auth = auth_for(&serve(router).await);
        let first = tokio::spawn({
            let auth = auth.clone();
            async move { auth.refresh_tokens("shared-refresh-token").await }
        });
        let second = tokio::spawn({
            let auth = auth.clone();
            async move { auth.refresh_tokens("shared-refresh-token").await }
        });
        while shared.calls.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(shared.calls.load(Ordering::SeqCst), 1);
        shared.release.notify_waiters();
        for task in [first, second] {
            let data = task.await.unwrap().unwrap();
            assert_eq!(data.access_token, "new-access");
            // A blank refresh token keeps the old one.
            assert_eq!(data.refresh_token, "shared-refresh-token");
            assert_eq!(data.account_uuid, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
            assert_eq!(data.email, "shared@example.com");
            assert_eq!(
                data.organization_uuid,
                "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
            );
            assert_eq!(data.organization_name, "Shared Org");
        }
        assert_eq!(shared.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            shared.body.lock().unwrap().take().unwrap(),
            json!({
                "client_id": CLIENT_ID,
                "grant_type": "refresh_token",
                "refresh_token": "shared-refresh-token",
                "scope": SCOPE,
            })
        );
    }

    #[tokio::test]
    async fn refresh_runs_on_after_its_caller_stops_waiting() {
        let calls = Arc::new(AtomicUsize::new(0));
        let router = Router::new()
            .route(
                "/v1/oauth/token",
                post(|State(calls): State<Arc<AtomicUsize>>| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    (StatusCode::BAD_REQUEST, r#"{"error":"probe"}"#)
                }),
            )
            .with_state(calls.clone());
        let auth = auth_for(&serve(router).await);
        let dropped = tokio::time::timeout(
            Duration::from_millis(10),
            auth.refresh_tokens("independent-timeout-token"),
        )
        .await;
        assert!(dropped.is_err());
        // The shared request is still in flight; a new caller joins it.
        let error = auth
            .refresh_tokens("independent-timeout-token")
            .await
            .unwrap_err();
        assert_eq!(
            error.message(),
            r#"token refresh failed with status 400: {"error":"probe"}"#
        );
        assert!(!error.is_retryable());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn compressed_responses_are_reported() {
        let router = Router::new().route(
            "/v1/oauth/token",
            post(|| async { ([("content-encoding", "identity, GZIP")], "x") }),
        );
        let auth = auth_for(&serve(router).await);
        let error = auth.refresh_tokens("rt-gzip").await.unwrap_err();
        assert_eq!(
            error.message(),
            "failed to read refresh response: decode Claude OAuth response: \
             unsupported content encoding \"gzip\""
        );
    }

    #[tokio::test]
    async fn login_reports_a_taken_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let auth = auth_for("http://127.0.0.1:9");
        let options = LoginOptions {
            callback_port: port,
            callback_timeout: Duration::from_millis(10),
        };
        let error = login(&auth, options, |_| panic!("no prompt")).await;
        match error {
            Err(LoginError::Authentication(error)) => {
                assert_eq!(error.kind, AuthenticationErrorKind::PortInUse);
                assert_eq!(
                    error.to_string(),
                    format!(
                        "port_in_use: OAuth callback port is already in use \
                         (caused by: port {port} is already in use)"
                    )
                );
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn login_times_out_without_a_callback() {
        let auth = auth_for("http://127.0.0.1:9");
        let options = LoginOptions {
            callback_port: 0,
            callback_timeout: Duration::from_millis(20),
        };
        let mut prompt = None;
        let error = login(&auth, options, |p| prompt = Some(p.clone()))
            .await
            .unwrap_err();
        let prompt = prompt.unwrap();
        assert!(
            prompt
                .url
                .starts_with("http://127.0.0.1:9/oauth/authorize?")
        );
        assert!(prompt.url.contains("code_challenge_method=S256"));
        assert_ne!(prompt.callback_port, 0);
        match error {
            LoginError::Authentication(error) => {
                assert_eq!(error.kind, AuthenticationErrorKind::CallbackTimeout);
                assert_eq!(error.kind.code(), 408);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// Starts a browser login against `auth` in the background, and gives
    /// it, its callback port and its state.
    async fn start_login(
        auth: ClaudeAuth,
    ) -> (
        tokio::task::JoinHandle<Result<BrowserLogin, LoginError>>,
        u16,
        String,
    ) {
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
        let prompt: AuthorizationPrompt = receiver.await.unwrap();
        let state = url::Url::parse(&prompt.url)
            .unwrap()
            .query_pairs()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| value.into_owned())
            .unwrap();
        (login, prompt.callback_port, state)
    }

    /// Sends the provider's redirect with `code` to the login's callback
    /// server on `port`, and gives where the server sends the browser on.
    async fn redirect(port: u16, code: &str, state: &str) -> String {
        let answer = fetch(port, &format!("/callback?code={code}&state={state}")).await;
        assert_eq!(answer.status, 302);
        answer.header("location").unwrap().to_owned()
    }

    // Not upstream's: the login exchanges the redirect's code, and the
    // browser, which follows the redirect to the success page once the login
    // has the code, finds the server still up and is told the sign-in
    // worked. (The login used to stop the server as soon as it had the code,
    // and the browser found the port closed.)
    #[tokio::test]
    async fn login_exchanges_the_callback_code() {
        let exchanging = Arc::new(Notify::new());
        let answer = Arc::new(Notify::new());
        let router = Router::new().route(
            "/v1/oauth/token",
            post({
                let (exchanging, answer) = (Arc::clone(&exchanging), Arc::clone(&answer));
                move |body: String| async move {
                    let body: Value = serde_json::from_str(&body).unwrap();
                    assert_eq!(body["code"], "the-code");
                    exchanging.notify_one();
                    answer.notified().await;
                    TOKEN_BODY
                }
            }),
        );
        let (login, port, state) = start_login(auth_for(&serve(router).await)).await;
        let location = redirect(port, "the-code", &state).await;
        // The login is done waiting for the redirect, and exchanging its
        // code, when the browser follows it.
        exchanging.notified().await;
        let page = tokio::spawn(async move { fetch(port, &location).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!page.is_finished(), "the page didn't wait for the login");
        answer.notify_one();
        let BrowserLogin {
            auth: record,
            server,
        } = login.await.unwrap().unwrap();
        assert_eq!(record.id, "claude-00f765af-user@example.com.json");
        assert_eq!(record.metadata["refresh_token"], "refresh");
        assert_eq!(record.metadata["email"], "user@example.com");
        server.finish(Outcome::SignedIn).await;
        assert_page(&page.await.unwrap(), 200, "Signed in to Claude");
    }

    // Not upstream's: a failed code exchange ends the login, and the success
    // page the browser follows the redirect to says why, without the code
    // the token endpoint quoted.
    #[tokio::test]
    async fn login_reports_a_failed_exchange_on_its_page() {
        let exchanging = Arc::new(Notify::new());
        let router = Router::new().route(
            "/v1/oauth/token",
            post({
                let exchanging = Arc::clone(&exchanging);
                move |body: String| async move {
                    exchanging.notify_one();
                    let body: Value = serde_json::from_str(&body).unwrap();
                    let code = body["code"].as_str().unwrap().to_owned();
                    let error = json!({
                        "error": "invalid_grant",
                        "error_description": format!("unknown code {code}"),
                    });
                    (StatusCode::BAD_REQUEST, error.to_string())
                }
            }),
        );
        let (login, port, state) = start_login(auth_for(&serve(router).await)).await;
        let location = redirect(port, "the-code-0123456789", &state).await;
        exchanging.notified().await;
        let page = fetch(port, &location).await;
        match login.await.unwrap() {
            Err(LoginError::Authentication(error)) => {
                assert_eq!(error.kind, AuthenticationErrorKind::CodeExchangeFailed);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_page(&page, 400, "The sign-in didn&#39;t finish");
        assert!(
            page.body.contains("token exchange failed with status 400"),
            "{}",
            page.body
        );
        assert!(
            page.body.contains("unknown code [redacted]"),
            "{}",
            page.body
        );
        assert!(!page.body.contains("0123456789"), "{}", page.body);
    }

    #[test]
    fn error_texts_match_upstream() {
        let error = OAuthError {
            code: "access_denied".into(),
            description: String::new(),
            status: 400,
        };
        assert_eq!(error.to_string(), "OAuth error: access_denied");
        let error = OAuthError {
            description: "nope".into(),
            ..error
        };
        assert_eq!(error.to_string(), "OAuth error access_denied: nope");
        let error = AuthenticationError {
            kind: AuthenticationErrorKind::CodeExchangeFailed,
            cause: None,
        };
        assert_eq!(
            error.to_string(),
            "code_exchange_failed: Failed to exchange authorization code for tokens"
        );
    }
}
