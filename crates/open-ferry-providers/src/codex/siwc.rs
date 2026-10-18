// Ported from CLIProxyAPI internal/auth/codex/siwc_auth.go (v8.0.32, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Sign in with ChatGPT's dynamic-client OAuth transport.
//!
//! The agent name is always open-ferry; token requests use our own version,
//! OS and architecture. The device UUID goes only in the authorization URL
//! and the nested credential, never in token forms or logs. Endpoints are
//! injectable, response bodies are bounded, and errors don't quote remote
//! bodies or credential values. Shared refresh requests survive cancelled
//! waiters and time out after 30 seconds. Successful rotations are reused for
//! up to ten minutes in a bounded cache. Sets known spent by this process become
//! tombstones, including for outstanding waiters. Removing a successful entry
//! retires its outstanding waiters too. If no waiter has taken the new set before
//! its entry expires (ten minutes after completion) or is evicted (the 256-entry
//! cache is full and it is the oldest completed entry, which one new refresh can
//! cause when the others are in flight), the rotation is lost: the old token is
//! already spent, so the credential needs a reload or a new sign-in.
//! Lost tasks are detected on the next refresh call or by a polling waiter.
//! Runtime shutdown permits one retry only before sending; shutdown or panic
//! after the conservative send mark (before connecting) retires predecessors and
//! fails closed for up to ten minutes from detection or until eviction, then a
//! new request is sent. A full cache of in-flight requests refuses new refreshes
//! rather than duplicating them.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Local, SecondsFormat, TimeDelta};
use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::client::{Clients, read_body, read_body_prefix};
pub use super::token::SiwcTokenData;
use crate::json;
use crate::oauth::Pkce;

/// Upstream `siwc_auth.go:21-35` protocol values (v8.0.32).
pub const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
/// Our fixed agent name, replacing upstream's configurable identifier.
pub const AGENT_NAME_HINT: &str = "open-ferry";
/// The registration authorization endpoint.
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/api/accounts/authorize";
/// The dynamic-client token endpoint.
pub const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
/// The resource authorized by this grant.
pub const RESOURCE: &str = "https://api.openai.com/v1";
/// The default loopback callback port.
pub const DEFAULT_CALLBACK_PORT: u16 = 1455;
/// The loopback callback path.
pub const CALLBACK_PATH: &str = "/auth/callback";
/// The default loopback redirect URI.
pub const REDIRECT_URI: &str = "http://127.0.0.1:1455/auth/callback";
/// The scope required for direct API access.
pub const DIRECT_TOKEN_SCOPE: &str = "chatgpt.tokens.use.direct";
/// The requested scopes, in upstream's order.
pub const SCOPE: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// The margin subtracted from the issued access-token TTL.
pub const EXPIRY_MARGIN: Duration = Duration::from_secs(3 * 60);
/// How early the stored expiry should trigger a refresh.
pub const REFRESH_LEAD: Duration = Duration::from_secs(5 * 60);

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RESPONSE_BYTES: usize = 1 << 20;
const REFRESH_GRACE: Duration = Duration::from_secs(10 * 60);
const MAX_REFRESHES: usize = 256;

/// The identity used for every SIWC token, API and websocket request.
pub fn user_agent() -> String {
    format!(
        "open-ferry/{} ({}; {})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// A redirect URI for the actual loopback listener's port.
pub fn redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}{CALLBACK_PATH}")
}

/// Whether a trimmed value is a canonical UUID, as upstream's regex checks.
pub fn is_valid_uuid(raw: &str) -> bool {
    let raw = raw.trim().as_bytes();
    raw.len() == 36
        && raw.iter().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

/// SIWC endpoints; tests use a loopback base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// Where the browser registration starts.
    pub authorize_url: String,
    /// Where issued codes and refresh tokens are exchanged.
    pub token_url: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            authorize_url: AUTHORIZE_URL.to_owned(),
            token_url: TOKEN_URL.to_owned(),
        }
    }
}

impl Endpoints {
    /// Upstream's paths under `base` (for example, `http://127.0.0.1:PORT`).
    pub fn with_base(base: &str) -> Self {
        let base = base.trim_end_matches('/');
        Self {
            authorize_url: format!("{base}/api/accounts/authorize"),
            token_url: format!("{base}/api/accounts/oauth/token"),
        }
    }
}

/// A failed SIWC operation, without secret values or remote response bodies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    message: String,
    status: u16,
    non_retryable: bool,
}

impl Error {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status: 0,
            non_retryable: false,
        }
    }

    /// What failed.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The token endpoint's error HTTP status, or 0 for local errors.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// Whether retrying this refresh cannot recover the credential.
    pub fn is_non_retryable(&self) -> bool {
        self.non_retryable
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// Calls SIWC's dynamic-client endpoints with the existing proxy-aware clients.
/// Arbitrary clients cannot be supplied by production callers:
/// ```compile_fail
/// use open_ferry_providers::codex::siwc::SiwcAuth;
/// let auth = SiwcAuth::new(reqwest::Client::new());
/// ```
#[derive(Clone)]
pub struct SiwcAuth {
    client: reqwest::Client,
    endpoints: Endpoints,
    // Test seam: false panics before sending, true after the response arrives.
    #[cfg(test)]
    panic_after_send: Option<bool>,
}

impl fmt::Debug for SiwcAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SiwcAuth").finish_non_exhaustive()
    }
}

impl SiwcAuth {
    /// Test seam; production clients must have clean defaults and safe redirects.
    #[cfg(test)]
    pub(crate) fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            endpoints: Endpoints::default(),
            panic_after_send: None,
        }
    }

    /// Uses an environment, direct, or HTTP(S) proxy setting as Codex does.
    pub fn with_proxy_url(proxy_url: &str) -> Self {
        Self {
            client: Clients::new(proxy_url).get(""),
            endpoints: Endpoints::default(),
            #[cfg(test)]
            panic_after_send: None,
        }
    }

    /// Uses injectable endpoints instead.
    pub fn with_endpoints(mut self, endpoints: Endpoints) -> Self {
        self.endpoints = endpoints;
        self
    }

    /// The configured endpoints.
    pub fn endpoints(&self) -> &Endpoints {
        &self.endpoints
    }

    /// The registration URL (`GenerateSIWCAuthURL`). Use
    /// [`crate::oauth::generate_state`] independently for state and nonce.
    pub fn generate_auth_url(
        &self,
        state: &str,
        nonce: &str,
        device_id: &str,
        redirect: &str,
        pkce: &Pkce,
    ) -> Result<String, Error> {
        let device_id = device_id.trim();
        if !is_valid_uuid(device_id) {
            return Err(Error::new(
                "Sign in with ChatGPT requires a valid device UUID",
            ));
        }
        if state.trim().is_empty() {
            return Err(Error::new("state is required"));
        }
        if nonce.trim().is_empty() {
            return Err(Error::new("nonce is required"));
        }
        let host_id = format!("urn:uuid:{}", device_id.to_ascii_lowercase());
        let form = encode_form(&[
            ("client_id", DYNAMIC_CLIENT_ID),
            ("agent_name_hint", AGENT_NAME_HINT),
            ("ext_agent_host_id", &host_id),
            ("response_type", "code"),
            ("redirect_uri", redirect_or_default(redirect)),
            ("resource", RESOURCE),
            ("scope", SCOPE),
            ("state", state.trim()),
            ("code_challenge", &pkce.challenge),
            ("code_challenge_method", "S256"),
            ("nonce", nonce.trim()),
        ]);
        Ok(format!("{}?{form}", self.endpoints.authorize_url))
    }

    /// Exchanges a code with the callback's issued client ID, not the
    /// registration client ID (`ExchangeSIWCCodeForTokens`).
    pub async fn exchange_code_for_tokens(
        &self,
        code: &str,
        issued_client_id: &str,
        redirect: &str,
        device_id: &str,
        pkce: &Pkce,
    ) -> Result<SiwcTokenData, Error> {
        let client_id = issued_client_id.trim();
        if client_id.is_empty() {
            return Err(Error::new(
                "OpenAI OAuth registration callback did not contain an issued client ID",
            ));
        }
        if code.trim().is_empty() {
            return Err(Error::new("authorization code is required"));
        }
        let form = encode_form(&[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("code", code.trim()),
            ("code_verifier", &pkce.verifier),
            ("redirect_uri", redirect_or_default(redirect)),
            ("resource", RESOURCE),
        ]);
        let mut data = self.request_token(form, true, None).await?;
        data.client_id = client_id.to_owned();
        data.device_id = device_id.trim().to_owned();
        Ok(data)
    }

    /// Refreshes the nested credential. Concurrent requests for the same
    /// endpoint, issued client ID and refresh token share a bounded task.
    /// Each waiter keeps its own device UUID and previous ID token.
    /// Successful rotations are reused for up to ten minutes (at most 256
    /// entries). Once this process rotates their successor or observes its
    /// reused-token error, outstanding and later waiters get a local error
    /// until eviction or expiry. Removing a successful entry also retires its
    /// outstanding waiters. Tasks lost to runtime shutdown are retried once only
    /// before sending. Shutdown or panic after the conservative send mark (before
    /// connecting) retires predecessors and fails closed for later callers too,
    /// for up to ten minutes from detection on the next refresh call or by a
    /// polling waiter, or until eviction; after that a new request is sent. Pre-send panics and
    /// ordinary transport errors remain retryable. If all entries are
    /// in flight, new refreshes fail locally with a retryable error. Callers must run
    /// refresh-then-persist in a task that a client disconnect cannot cancel,
    /// checking the persisted token baseline so an older result cannot replace
    /// a newer rotation on disk.
    pub async fn refresh_tokens(&self, previous: &SiwcTokenData) -> Result<SiwcTokenData, Error> {
        self.refresh_tokens_in_cache(previous, MAX_REFRESHES, &REFRESHES)
            .await
    }

    async fn refresh_tokens_in_cache(
        &self,
        previous: &SiwcTokenData,
        limit: usize,
        cache: &'static Mutex<HashMap<[u8; 32], RefreshEntry>>,
    ) -> Result<SiwcTokenData, Error> {
        let token = previous.refresh_token.trim();
        let client_id = previous.client_id.trim();
        if token.is_empty() {
            return Err(Error::new("SIWC refresh token is required"));
        }
        if client_id.is_empty() {
            return Err(Error::new(
                "Stored OpenAI OAuth credential does not contain an issued client ID; reconnect ChatGPT",
            ));
        }
        let key = refresh_key(&self.endpoints.token_url, client_id, token);
        let mut retry_cancelled = true;
        let mut data = loop {
            let (shared, superseded) = {
                let mut refreshes = cache.lock().unwrap_or_else(PoisonError::into_inner);
                prune_refreshes(&mut refreshes);
                match refreshes.get(&key) {
                    Some(entry) => {
                        if entry.superseded.load(Ordering::SeqCst) {
                            return Err(superseded_refresh());
                        }
                        (entry.shared.clone(), entry.superseded.clone())
                    }
                    None => {
                        reserve_refresh(&mut refreshes, limit)?;
                        let superseded = Arc::new(AtomicBool::new(false));
                        let started = Arc::new(AtomicBool::new(false));
                        let task_started = started.clone();
                        let auth = self.clone();
                        let form = encode_form(&[
                            ("grant_type", "refresh_token"),
                            ("client_id", client_id),
                            ("refresh_token", token),
                            ("resource", RESOURCE),
                        ]);
                        let id = Arc::new(());
                        let task_id = id.clone();
                        let task_client = client_id.to_owned();
                        let task = tokio::spawn(async move {
                            let result = auth.request_token(form, false, Some(&task_started)).await;
                            let successor = result.as_ref().ok().map(|data| {
                                refresh_key(
                                    &auth.endpoints.token_url,
                                    &task_client,
                                    data.refresh_token.trim(),
                                )
                            });
                            let spent = successor.is_some()
                                || result.as_ref().is_err_and(|error| error.non_retryable);
                            finish_refresh_in_cache(cache, key, &task_id, successor, spent);
                            result
                        });
                        let task_handle = task.abort_handle();
                        let shared =
                            shared_refresh_in_cache(task, key, id.clone(), started.clone(), cache);
                        refreshes.insert(
                            key,
                            RefreshEntry {
                                shared: shared.clone(),
                                task: Some(task_handle),
                                started,
                                id,
                                superseded: superseded.clone(),
                                completed: None,
                                successor: None,
                            },
                        );
                        (shared, superseded)
                    }
                }
            };
            match shared.await {
                Ok(data) => {
                    if superseded.load(Ordering::SeqCst) {
                        return Err(superseded_refresh());
                    }
                    break data;
                }
                Err(RefreshError::Cancelled) if retry_cancelled => retry_cancelled = false,
                Err(RefreshError::Cancelled) => return Err(invalid_refresh_result()),
                Err(RefreshError::Request(error)) => return Err(error),
            }
        };
        data.client_id = client_id.to_owned();
        data.device_id = previous.device_id.trim().to_owned();
        if data.id_token.trim().is_empty() {
            data.id_token.clone_from(&previous.id_token);
        }
        Ok(data)
    }

    /// Tries at least once and at most `max_retries` times, waiting one
    /// second more before each retry, as upstream actually does. Reused
    /// refresh tokens, superseded sets and post-send interruptions aren't
    /// retried. Cancelling a waiter stops its retries, not its shared task.
    /// Run this call and persistence together in a task that survives a client
    /// disconnect, just as for [`Self::refresh_tokens`].
    pub async fn refresh_tokens_with_retry(
        &self,
        previous: &SiwcTokenData,
        max_retries: u32,
    ) -> Result<SiwcTokenData, Error> {
        let max_retries = max_retries.max(1);
        let mut last = Error::new("SIWC token refresh failed");
        for attempt in 0..max_retries {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(attempt.into())).await;
            }
            match self.refresh_tokens(previous).await {
                Ok(data) => return Ok(data),
                Err(error) if error.non_retryable => return Err(error),
                Err(error) => last = error,
            }
        }
        last.message = format!(
            "SIWC token refresh failed after {max_retries} attempts: {}",
            last.message
        );
        Err(last)
    }

    async fn request_token(
        &self,
        form: String,
        require_id_token: bool,
        started: Option<&AtomicBool>,
    ) -> Result<SiwcTokenData, Error> {
        #[cfg(test)]
        if self.panic_after_send == Some(false) {
            panic!("fake pre-send panic");
        }
        let request = self
            .client
            .post(&self.endpoints.token_url)
            .header(
                http::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .header(http::header::ACCEPT, "application/json")
            .header(http::header::USER_AGENT, user_agent())
            .timeout(REQUEST_TIMEOUT)
            .body(form);
        if let Some(started) = started {
            started.store(true, Ordering::SeqCst);
        }
        let response = request.send().await.map_err(send_error)?;
        #[cfg(test)]
        if self.panic_after_send == Some(true) {
            panic!("fake post-send panic");
        }
        let status = response.status().as_u16();
        if status != 200 {
            let (body, _) = read_body_prefix(response, MAX_RESPONSE_BYTES).await;
            // Upstream's non-retryable marker is retained without quoting
            // the body: it may echo tokens, device IDs or callback values.
            let non_retryable = String::from_utf8_lossy(&body)
                .to_ascii_lowercase()
                .contains("refresh_token_reused");
            return Err(Error {
                message: format!(
                    "OpenAI OAuth token request failed ({status}){}",
                    if non_retryable {
                        ": refresh_token_reused"
                    } else {
                        ""
                    }
                ),
                status,
                non_retryable,
            });
        }
        let body = read_body(response, MAX_RESPONSE_BYTES)
            .await
            .map_err(|_| Error::new("failed to read SIWC token response"))?;
        decode_token_response(&body, require_id_token, Local::now())
    }
}

#[derive(Clone)]
enum RefreshError {
    Request(Error),
    Cancelled,
}

type SharedRefresh = Shared<BoxFuture<'static, Result<SiwcTokenData, RefreshError>>>;
static REFRESHES: LazyLock<Mutex<HashMap<[u8; 32], RefreshEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct RefreshEntry {
    task: Option<tokio::task::AbortHandle>,
    started: Arc<AtomicBool>,
    shared: SharedRefresh,
    id: Arc<()>,
    superseded: Arc<AtomicBool>,
    completed: Option<tokio::time::Instant>,
    successor: Option<[u8; 32]>,
}

fn refresh_key(endpoint: &str, client: &str, token: &str) -> [u8; 32] {
    Sha256::new()
        .chain_update(endpoint)
        .chain_update([0])
        .chain_update(client)
        .chain_update([0])
        .chain_update(token)
        .finalize()
        .into()
}

// Finished tasks must not occupy in-flight capacity just because no waiter polled.
fn reap_lost_refreshes(map: &mut HashMap<[u8; 32], RefreshEntry>) {
    let mut spent = Vec::new();
    map.retain(|key, entry| {
        if entry.completed.is_none() && entry.task.as_ref().is_some_and(|task| task.is_finished()) {
            if !entry.started.load(Ordering::SeqCst) {
                return false;
            }
            spent.push(*key);
            entry
                .completed
                .get_or_insert_with(tokio::time::Instant::now);
            entry.shared =
                futures_util::future::ready(Err(RefreshError::Request(interrupted_refresh())))
                    .boxed()
                    .shared();
        }
        true
    });
    for key in spent {
        retire_predecessors(map, key);
    }
}

fn retire_predecessors(map: &HashMap<[u8; 32], RefreshEntry>, key: [u8; 32]) {
    // Invalidate outstanding waiters too, even if this task's entry was evicted.
    for entry in map.values().filter(|entry| entry.successor == Some(key)) {
        entry.superseded.store(true, Ordering::SeqCst);
    }
}

fn retire_refresh(entry: &RefreshEntry) {
    if entry.successor.is_some() {
        entry.superseded.store(true, Ordering::SeqCst);
    }
}

fn prune_refreshes(map: &mut HashMap<[u8; 32], RefreshEntry>) {
    reap_lost_refreshes(map);
    map.retain(|_, entry| {
        let keep = entry
            .completed
            .is_none_or(|at| at.elapsed() < REFRESH_GRACE);
        if !keep {
            retire_refresh(entry);
        }
        keep
    });
}

// The caller prunes expired entries once before lookup/insertion, under this lock.
fn reserve_refresh(map: &mut HashMap<[u8; 32], RefreshEntry>, limit: usize) -> Result<(), Error> {
    if map.len() >= limit {
        reap_lost_refreshes(map);
    }
    if map.len() >= limit {
        let oldest = map
            .iter()
            .filter_map(|(key, entry)| entry.completed.map(|at| (*key, at)))
            .min_by_key(|(_, at)| *at)
            .map(|(key, _)| key)
            .ok_or_else(|| Error::new("SIWC token refresh cache is full; retry later"))?;
        if let Some(entry) = map.remove(&oldest) {
            retire_refresh(&entry);
        }
    }
    Ok(())
}

#[cfg(test)]
fn insert_refresh(map: &mut HashMap<[u8; 32], RefreshEntry>, key: [u8; 32], entry: RefreshEntry) {
    insert_refresh_with_limit(map, key, entry, MAX_REFRESHES).unwrap();
}

#[cfg(test)]
fn insert_refresh_with_limit(
    map: &mut HashMap<[u8; 32], RefreshEntry>,
    key: [u8; 32],
    entry: RefreshEntry,
    limit: usize,
) -> Result<(), Error> {
    prune_refreshes(map);
    reserve_refresh(map, limit)?;
    map.insert(key, entry);
    Ok(())
}

#[cfg(test)]
fn finish_refresh(key: [u8; 32], id: &Arc<()>, successor: Option<[u8; 32]>, spent: bool) {
    finish_refresh_in_cache(&REFRESHES, key, id, successor, spent);
}

fn finish_refresh_in_cache(
    cache: &Mutex<HashMap<[u8; 32], RefreshEntry>>,
    key: [u8; 32],
    id: &Arc<()>,
    successor: Option<[u8; 32]>,
    spent: bool,
) {
    let mut map = cache.lock().unwrap_or_else(PoisonError::into_inner);
    if spent {
        retire_predecessors(&map, key);
    }
    if let Some(entry) = map.get_mut(&key)
        && Arc::ptr_eq(&entry.id, id)
    {
        if successor.is_some() {
            entry.completed = Some(tokio::time::Instant::now());
            entry.successor = successor;
        } else {
            map.remove(&key);
        }
    }
}

#[cfg(test)]
fn shared_refresh(
    task: tokio::task::JoinHandle<Result<SiwcTokenData, Error>>,
    key: [u8; 32],
    id: Arc<()>,
    started: Arc<AtomicBool>,
) -> SharedRefresh {
    shared_refresh_in_cache(task, key, id, started, &REFRESHES)
}

fn shared_refresh_in_cache(
    task: tokio::task::JoinHandle<Result<SiwcTokenData, Error>>,
    key: [u8; 32],
    id: Arc<()>,
    started: Arc<AtomicBool>,
    cache: &'static Mutex<HashMap<[u8; 32], RefreshEntry>>,
) -> SharedRefresh {
    async move {
        task.await
            .map_err(|error| {
                // Also covers a task dropped before its first poll. The identity
                // check avoids removing a replacement after eviction or failure.
                if started.load(Ordering::SeqCst) {
                    // Keep the interruption for the grace window: replay is unsafe.
                    let mut map = cache.lock().unwrap_or_else(PoisonError::into_inner);
                    retire_predecessors(&map, key);
                    if let Some(entry) = map.get_mut(&key)
                        && Arc::ptr_eq(&entry.id, &id)
                    {
                        entry
                            .completed
                            .get_or_insert_with(tokio::time::Instant::now);
                    }
                    RefreshError::Request(interrupted_refresh())
                } else if error.is_cancelled() {
                    finish_refresh_in_cache(cache, key, &id, None, false);
                    RefreshError::Cancelled
                } else {
                    finish_refresh_in_cache(cache, key, &id, None, false);
                    RefreshError::Request(invalid_refresh_result())
                }
            })?
            .map_err(RefreshError::Request)
    }
    .boxed()
    .shared()
}

fn superseded_refresh() -> Error {
    Error {
        message: "the SIWC refresh token was already rotated; reload the credential".into(),
        status: 0,
        non_retryable: true,
    }
}

fn interrupted_refresh() -> Error {
    Error {
        message: "the SIWC token refresh was interrupted after it was sent; reload the credential"
            .into(),
        status: 0,
        non_retryable: true,
    }
}

fn invalid_refresh_result() -> Error {
    Error::new("SIWC token refresh failed: invalid single-flight result")
}

fn send_error(error: reqwest::Error) -> Error {
    let kind = if error.is_timeout() {
        "timeout"
    } else if is_tls_error(&error) {
        "TLS"
    } else if error.is_connect() {
        "connect"
    } else {
        "other"
    };
    Error::new(format!("SIWC token request failed ({kind})"))
}

fn is_tls_error(error: &(dyn std::error::Error + 'static)) -> bool {
    // Hyper can nest I/O errors; source() skips the wrapped error itself.
    // Inspect each wrapped value without echoing its text or credential URL.
    error.is::<rustls::Error>()
        || error
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::get_ref)
            .is_some_and(|inner| is_tls_error(inner))
        || error.source().is_some_and(is_tls_error)
}

fn redirect_or_default(redirect: &str) -> &str {
    if redirect.trim().is_empty() {
        REDIRECT_URI
    } else {
        redirect.trim()
    }
}

fn encode_form(pairs: &[(&str, &str)]) -> String {
    let mut pairs = pairs.to_vec();
    pairs.sort_by_key(|(key, _)| *key);
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

fn decode_token_response(
    body: &[u8],
    require_id_token: bool,
    now: DateTime<Local>,
) -> Result<SiwcTokenData, Error> {
    let invalid_json = |_| Error::new("failed to parse SIWC token response");
    let value: Value = serde_json::from_slice(body).map_err(|_| invalid_json(()))?;
    let mut data = SiwcTokenData::default();
    let mut expires_in = 0;
    if let Some(object) =
        json::object_or_null(&value, "siwcTokenResponse").map_err(|_| invalid_json(()))?
    {
        for (key, value) in object {
            let result = match json::key_of(
                key,
                &[
                    "access_token",
                    "refresh_token",
                    "id_token",
                    "scope",
                    "expires_in",
                ],
            ) {
                Some("access_token") => {
                    json::set_string(&mut data.access_token, value, "access_token")
                }
                Some("refresh_token") => {
                    json::set_string(&mut data.refresh_token, value, "refresh_token")
                }
                Some("id_token") => json::set_string(&mut data.id_token, value, "id_token"),
                Some("scope") => json::set_string(&mut data.scope, value, "scope"),
                Some("expires_in") => json::set_int(&mut expires_in, value, "expires_in"),
                _ => Ok(()),
            };
            result.map_err(|_| invalid_json(()))?;
        }
    }
    if data.access_token.trim().is_empty() {
        return Err(Error::new(
            "OpenAI OAuth token response has invalid access_token",
        ));
    }
    if data.refresh_token.trim().is_empty() {
        return Err(Error::new(
            "OpenAI OAuth token response has invalid refresh_token",
        ));
    }
    // As upstream, the ID token's signature and nonce aren't verified;
    // no matching of the SIWC account to the outer account is claimed.
    if require_id_token && data.id_token.trim().is_empty() {
        return Err(Error::new(
            "OpenAI OAuth token response did not contain an ID token",
        ));
    }
    if data.scope.trim().is_empty() {
        return Err(Error::new("OpenAI OAuth token response has invalid scope"));
    }
    if expires_in <= 0 {
        return Err(Error::new(
            "OpenAI OAuth token response has invalid expires_in",
        ));
    }
    data.scope = data.scope.trim().to_owned();
    data.scopes = data.scope.split_whitespace().map(str::to_owned).collect();
    if !data.scopes.iter().any(|scope| scope == DIRECT_TOKEN_SCOPE) {
        return Err(Error::new(format!(
            "OpenAI OAuth grant did not include {DIRECT_TOKEN_SCOPE}"
        )));
    }
    let ttl = if expires_in > EXPIRY_MARGIN.as_secs() as i64 {
        expires_in - EXPIRY_MARGIN.as_secs() as i64
    } else {
        expires_in
    };
    let expire = TimeDelta::try_seconds(ttl)
        .and_then(|ttl| now.checked_add_signed(ttl))
        .ok_or_else(|| Error::new("OpenAI OAuth token response has invalid expires_in"))?;
    data.expire = expire.to_rfc3339_opts(SecondsFormat::Secs, true);
    data.last_refresh = now.to_rfc3339_opts(SecondsFormat::Secs, true);
    Ok(data)
}

#[cfg(test)]
#[path = "siwc_tests.rs"]
mod tests;
