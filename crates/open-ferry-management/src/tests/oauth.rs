// Ported from CLIProxyAPI internal/api/handlers/management/
// oauth_sessions_test.go, oauth_callback_test.go and
// oauth_codex_concurrency_test.go, and the OAuth checks of
// internal/api/server_management_v8_test.go
// (TestManagementV8IndependentContract) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests of the routes of `crate::oauth`: the login sessions, the callback
//! routes, the callback forwarders and the logins themselves.
//!
//! The provider endpoints a login calls are servers on 127.0.0.1 ephemeral
//! ports (an unused local port unless a test sets one), and so are the
//! forwarders unless a test gives them a port. No test follows a
//! forwarder's redirect.
//!
//! Deviations from upstream:
//! - A callback reaches its login through its session instead of a file,
//!   so the tests that read the callback file check what the login
//!   received instead: `TestPostOAuthCallbackCreatesMissingAuthDir` checks
//!   that no directory was made, and calls back a Codex login, as
//!   Antigravity logins aren't served;
//!   `TestWriteOAuthCallbackFileForPendingSessionCreatesMissingAuthDirForCallbackProviders`
//!   keeps its Claude and Codex cases.
//! - `TestGetOAuthCallbackWritesPluginProviderCallback` and
//!   `TestGetOAuthCallbackDoesNotAliasPluginProvider` are dropped: plugin
//!   sessions aren't ported.
//! - `TestOAuthSessionStoreCompleteProviderSkipsCompletedSessions` is
//!   dropped: `CompleteProvider`, which only the plugin host and embedders
//!   call, isn't ported.
//! - The store tests use a store of their own, and the handler tests the
//!   store of their [`Api`], instead of replacing a global store.
//! - The Codex login tests answer the token exchange from a local server
//!   instead of replacing the OAuth service, so the ID token they return
//!   carries the email, which the credential takes from it; the
//!   concurrency test also checks the first login completed.
//! - The v8 contract's OAuth checks are ported here rather than with the
//!   rest of `TestManagementV8IndependentContract`.

use std::cell::RefCell;
use std::fmt::{self, Write as _};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http::{Method, StatusCode};
use open_ferry_core::config::Config;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tracing::subscriber::Interest;

use super::{
    Answer, Api, AuthDir, LOCAL, Upstream, http_response, keyed, keyed_config, read_request,
    request_from,
};
use crate::oauth::Provider;
use crate::oauth::sessions::{Callback, MAX_SESSIONS, NotPending, Session, Store};

const STATUS: &str = "/v0/management/get-auth-status";
const SESSION: &str = "/v0/management/oauth-session";
const CALLBACK: &str = "/v0/management/oauth-callback";
const CODEX_AUTH_URL: &str = "/v0/management/codex-auth-url";
const CLAUDE_AUTH_URL: &str = "/v0/management/anthropic-auth-url";
const REMOTE: &str = "203.0.113.7:4000";

/// The main server's port in the tests' configs: nothing listens there, as
/// no test follows a forwarder's redirect.
const SERVER_PORT: i64 = 28_765;

/// What Anthropic's token endpoint answers in the tests.
const CLAUDE_TOKENS: &str = r#"{"access_token":"access-claude","refresh_token":"refresh-claude","expires_in":3600,"account":{"uuid":"acct-1","email_address":"claude-user@example.test"},"organization":{"uuid":"org-1","name":"Org"}}"#;

/// The page the main server's callback routes answer with.
const SUCCESS_PAGE: &str = concat!(
    r#"<html><head><meta charset="utf-8"><title>Authentication successful</title>"#,
    "<script>setTimeout(function(){window.close();},5000);</script></head>",
    "<body><h1>Authentication successful!</h1><p>You can close this window.</p>",
    "<p>This window will close automatically in 5 seconds.</p></body></html>",
);

/// A store whose sessions last a minute, as upstream's tests make.
fn store() -> Store {
    Store::new(Duration::from_secs(60))
}

/// Starts a pending session in `store`, and returns where its callback
/// arrives.
fn register(store: &Store, state: &str, provider: &str) -> oneshot::Receiver<Callback> {
    store.register(state, provider).expect("a session").callback
}

/// `{"error":message,"status":"error"}`.
fn failed(message: &str) -> String {
    json!({ "error": message, "status": "error" }).to_string()
}

/// A callback with `code`.
fn code(code: &str) -> Callback {
    Callback {
        code: code.to_owned(),
        error: String::new(),
    }
}

/// [`keyed_config`] with [`SERVER_PORT`], so that a login from the web UI
/// can tell where to send the browser.
fn served_config() -> Config {
    let mut config = keyed_config();
    config.port = SERVER_PORT;
    config
}

/// Starts a login with `GET path` and returns its state.
async fn start_login(api: &Api, path: &str) -> String {
    let body = api.get(path).await.expect(StatusCode::OK);
    assert_eq!(body["status"], "ok", "{path}: {body}");
    assert!(
        body["url"].as_str().is_some_and(|url| !url.is_empty()),
        "{path}: {body}"
    );
    body["state"].as_str().unwrap().to_owned()
}

/// Hands `callback` to the pending `provider` login of `state`.
fn deliver(api: &Api, state: &str, provider: &str, callback: Callback) {
    let store = api.state.oauth_sessions().store();
    store.deliver(state, provider, callback).unwrap();
}

/// Waits up to 5 seconds for the session of `state` to stop being pending,
/// and returns it if it is still kept.
async fn settled(api: &Api, state: &str) -> Option<Session> {
    let store = api.state.oauth_sessions().store();
    let deadline = Instant::now() + Duration::from_secs(5);
    while store.is_pending(state, "") {
        assert!(Instant::now() < deadline, "session {state} stayed pending");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    store.get(state)
}

/// Waits up to 5 seconds for the session of `state` to complete.
async fn completed(api: &Api, state: &str) {
    let session = settled(api, state).await.expect("a session");
    assert!(session.completed, "{state}: {:?}", session.status);
}

/// Waits up to 5 seconds for the session of `state` to fail, and returns
/// why it did.
async fn failure(api: &Api, state: &str) -> String {
    let session = settled(api, state).await.expect("a session");
    assert!(!session.completed, "{state} completed");
    session.status
}

/// The API over `auth_dir`, its logins calling the provider endpoints at
/// `base`.
fn login_api(auth_dir: &AuthDir, base: &str) -> Api {
    let api = Api::over(auth_dir);
    api.state.oauth_sessions().overrides().endpoints = Some(base.to_owned());
    api
}

/// The names of the files in `auth_dir`.
fn files(auth_dir: &AuthDir) -> Vec<String> {
    std::fs::read_dir(auth_dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect()
}

/// The name of the one `.json` file in `auth_dir` named with `prefix`.
fn saved_file(auth_dir: &AuthDir, prefix: &str) -> String {
    let names: Vec<String> = files(auth_dir)
        .into_iter()
        .filter(|name| name.starts_with(prefix) && name.ends_with(".json"))
        .collect();
    assert_eq!(names.len(), 1, "{names:?}");
    names[0].clone()
}

/// An unsigned ID token whose claims carry an email, a Codex account and,
/// unless empty, `plan_type` (`makeOAuthTestJWT`).
fn codex_id_token(email: Option<&str>, plan_type: &str) -> String {
    let mut auth = json!({ "chatgpt_account_id": "acc-oauth-test" });
    if !plan_type.is_empty() {
        auth["chatgpt_plan_type"] = json!(plan_type);
    }
    let mut claims = json!({ "https://api.openai.com/auth": auth });
    if let Some(email) = email {
        claims["email"] = json!(email);
    }
    format!(
        "{}.{}.",
        URL_SAFE_NO_PAD.encode(r#"{"alg":"none","typ":"JWT"}"#),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    )
}

/// What OpenAI's token endpoint answers, with `id_token`.
fn codex_tokens(id_token: &str) -> Vec<u8> {
    let body = json!({
        "access_token": "access-codex",
        "refresh_token": "refresh-codex",
        "id_token": id_token,
        "expires_in": 3600,
    });
    let headers = [("Content-Type", "application/json")];
    http_response("200 OK", &headers, body.to_string().as_bytes())
}

/// What Anthropic's token endpoint answers.
fn claude_tokens() -> Vec<u8> {
    let headers = [("Content-Type", "application/json")];
    http_response("200 OK", &headers, CLAUDE_TOKENS.as_bytes())
}

/// The body of an HTTP request read as text.
fn request_body(request: &str) -> &str {
    request.split_once("\r\n\r\n").map_or("", |(_, body)| body)
}

/// What a forwarder answered.
#[derive(Debug)]
struct Forwarded {
    status_line: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Forwarded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Sends `method target` to the forwarder at `addr`, on a connection of its
/// own, and reads the answer; `None` when nothing answers.
async fn forward(addr: SocketAddr, method: &str, target: &str) -> Option<Forwarded> {
    let mut stream = TcpStream::connect(addr).await.ok()?;
    let request = format!(
        "{method} {target} HTTP/1.1\r\nHost: localhost:1455\r\nContent-Length: 0\r\n\
         Connection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.ok()?;
    let mut data = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut data))
        .await
        .ok()?
        .ok()?;
    let text = String::from_utf8(data).ok()?;
    let (head, body) = text.split_once("\r\n\r\n")?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next()?.to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_owned(), value.trim().to_owned()))
        .collect();
    Some(Forwarded {
        status_line,
        headers,
        body: body.to_owned(),
    })
}

/// The address of `provider`'s forwarder, which must run.
fn forwarder(api: &Api, provider: Provider) -> SocketAddr {
    let addr = api.state.oauth_sessions().forwarder_addr(provider);
    addr.expect("a running forwarder")
}

/// Waits up to 10 seconds for `provider`'s forwarder, at `addr`, to stop:
/// to be dropped, and to answer no more.
async fn stopped(api: &Api, provider: Provider, addr: SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while api.state.oauth_sessions().forwarder_addr(provider) == Some(addr) {
        assert!(Instant::now() < deadline, "the forwarder on {addr} runs on");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    while forward(addr, "GET", "/").await.is_some() {
        assert!(Instant::now() < deadline, "the forwarder on {addr} answers");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Checks `answer` is the main server's callback page.
fn assert_page(answer: &Answer, what: &str) {
    assert_eq!(
        (answer.status, answer.body.as_str()),
        (StatusCode::OK, SUCCESS_PAGE),
        "{what}"
    );
    assert_eq!(
        answer.header("content-type"),
        Some("text/html; charset=utf-8"),
        "{what}"
    );
}

/// Waits up to 5 seconds for every login of `api` to end.
async fn ended(api: &Api) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while api.state.oauth_sessions().running() > 0 {
        assert!(Instant::now() < deadline, "a login runs on");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Waits up to 5 seconds for `count` to reach `want`.
async fn reaches(count: &AtomicUsize, want: usize, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while count.load(Ordering::SeqCst) < want {
        assert!(Instant::now() < deadline, "{what}: not {want}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(count.load(Ordering::SeqCst), want, "{what}");
}

/// A token endpoint on a 127.0.0.1 ephemeral port that never answers: it
/// reads each request, then holds its connection until the client closes
/// it.
struct Stall {
    /// `http://127.0.0.1:<port>`.
    url: String,
    /// How many requests it read.
    requests: Arc<AtomicUsize>,
    /// How many of their connections the client closed.
    closed: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Stall {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicUsize::new(0));
        let (read, ended) = (Arc::clone(&requests), Arc::clone(&closed));
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            while let Ok((mut stream, _)) = listener.accept().await {
                let (read, ended) = (Arc::clone(&read), Arc::clone(&ended));
                connections.spawn(async move {
                    if read_request(&mut stream).await.is_some() {
                        read.fetch_add(1, Ordering::SeqCst);
                    }
                    let mut rest = [0; 1024];
                    while matches!(stream.read(&mut rest).await, Ok(n) if n > 0) {}
                    ended.fetch_add(1, Ordering::SeqCst);
                });
            }
        });
        Self {
            url,
            requests,
            closed,
            task,
        }
    }
}

impl Drop for Stall {
    fn drop(&mut self) {
        self.task.abort();
    }
}

thread_local! {
    /// Where this thread's logs go while a test captures them.
    static CAPTURED: RefCell<Option<Arc<Mutex<String>>>> = const { RefCell::new(None) };
}

/// What a test logs on its thread, one event a line: the message, then
/// any other field as ` name=value`.
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<String>>);

impl Logs {
    /// Captures what this thread logs until the guard is dropped. A
    /// `#[tokio::test]` runs its tasks on its thread, so their logs too.
    ///
    /// The subscriber is the global one, for every thread: a scoped one
    /// misses events whose callsite another thread registered first.
    fn capture() -> (Self, Capturing) {
        static INSTALL: Once = Once::new();
        INSTALL.call_once(|| {
            let _ = tracing::subscriber::set_global_default(Capture);
            tracing::callsite::rebuild_interest_cache();
        });
        let logs = Self::default();
        CAPTURED.with(|captured| *captured.borrow_mut() = Some(Arc::clone(&logs.0)));
        (logs, Capturing)
    }

    fn text(&self) -> String {
        self.0.lock().unwrap().clone()
    }
}

/// Ends this thread's capture when dropped.
struct Capturing;

impl Drop for Capturing {
    fn drop(&mut self) {
        CAPTURED.with(|captured| captured.borrow_mut().take());
    }
}

/// The subscriber that keeps the events of a thread that captures them.
struct Capture;

impl tracing::Subscriber for Capture {
    fn register_callsite(&self, _: &'static tracing::Metadata<'static>) -> Interest {
        Interest::sometimes()
    }

    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        CAPTURED.with(|captured| captured.borrow().is_some())
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let Some(text) = CAPTURED.with(|captured| captured.borrow().clone()) else {
            return;
        };
        let mut line = String::new();
        event.record(&mut Fields(&mut line));
        let mut text = text.lock().unwrap();
        text.push_str(&line);
        text.push('\n');
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

/// Writes an event's fields as [`Logs`] keeps them.
struct Fields<'a>(&'a mut String);

impl tracing::field::Visit for Fields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }
}

#[test]
fn oauth_session_store_complete_keeps_short_lived_session() {
    let store = store();
    let _callback = register(&store, "completed-state", "codex");

    store.complete("completed-state");

    assert!(
        store.get("completed-state").is_some(),
        "completed OAuth session was deleted instead of kept"
    );
    assert!(!store.is_pending("completed-state", "codex"));
}

#[test]
fn oauth_session_store_complete_does_not_extend_completed_session() {
    let store = store();
    let _callback = register(&store, "completed-state", "codex");
    store.complete("completed-state");
    let before = store.get("completed-state").unwrap();

    store.set_completed_ttl(Duration::from_secs(120));
    store.complete("completed-state");
    let after = store.get("completed-state").unwrap();
    assert_eq!(after.expires_at, before.expires_at);
}

#[test]
fn get_oauth_session_hides_completed_session() {
    let store = store();
    let _callback = register(&store, "completed-state", "codex");
    store.complete("completed-state");

    assert_eq!(store.active("completed-state"), None);
    assert!(
        store
            .get("completed-state")
            .is_some_and(|session| session.completed)
    );
}

#[tokio::test]
async fn get_auth_status_rejects_unknown_state_and_accepts_completed_state() {
    let api = Api::new();
    let store = api.state.oauth_sessions().store();

    api.get(&format!("{STATUS}?state=unknown-state"))
        .await
        .assert(StatusCode::OK, &failed("unknown or expired state"));

    let _callback = register(store, "completed-state", "codex");
    store.complete("completed-state");
    api.get(&format!("{STATUS}?state=completed-state"))
        .await
        .assert(StatusCode::OK, r#"{"status":"ok"}"#);
}

#[tokio::test]
async fn oauth_callback_rejects_completed_session() {
    let api = Api::new();
    let store = api.state.oauth_sessions().store();
    let _callback = register(store, "completed-state", "codex");
    store.complete("completed-state");

    let body = r#"{"provider":"codex","state":"completed-state","code":"test-code"}"#;
    api.send(request_from(LOCAL, Method::POST, CALLBACK, body))
        .await
        .assert(
            StatusCode::CONFLICT,
            &failed("oauth flow is already completed"),
        );
}

#[test]
fn oauth_session_store_cancel_removes_pending_session() {
    let store = store();
    let _callback = register(&store, "pending-state", "xai");

    assert!(store.cancel("pending-state"));
    assert!(!store.is_pending("pending-state", "xai"));
    assert_eq!(store.get("pending-state"), None);
    assert!(!store.cancel("pending-state"));
}

#[test]
fn oauth_session_store_cancel_ignores_completed_and_unknown() {
    let store = store();
    let _callback = register(&store, "completed-state", "codex");
    store.complete("completed-state");

    assert!(!store.cancel("completed-state"));
    assert!(store.get("completed-state").is_some());
    assert!(!store.cancel("missing-state"));
}

#[test]
fn oauth_session_store_cancel_ignores_error_session() {
    let store = store();
    let _callback = register(&store, "error-state", "kimi");
    store.set_error("error-state", "Authentication failed");

    assert!(!store.is_pending("error-state", "kimi"));
    assert!(!store.cancel("error-state"));
}

#[test]
fn cancel_oauth_session_and_callback_reject_after_cancel() {
    let store = store();
    let _callback = register(&store, "callback-state", "anthropic");

    assert!(store.cancel("callback-state"));
    assert!(!store.is_pending("callback-state", "anthropic"));
    assert_eq!(
        store.deliver("callback-state", "anthropic", code("code")),
        Err(NotPending)
    );
}

#[test]
fn guard_oauth_session_pending_for_save() {
    let store = store();
    for provider in [
        "anthropic",
        "codex",
        "antigravity",
        "xai",
        "kimi",
        "kimi-ai",
        "kimi.ai",
        "meta",
    ] {
        let state = format!("{provider}-save-guard");
        let _callback = register(&store, &state, provider);
        assert!(store.is_pending(&state, provider), "{provider}");
        assert!(store.cancel(&state), "{provider}");
        assert!(
            !store.is_pending(&state, provider),
            "{provider} after cancel"
        );
    }

    // Completed and failed sessions refuse the save too.
    let _completed = register(&store, "completed-save", "codex");
    store.complete("completed-save");
    assert!(!store.is_pending("completed-save", "codex"));

    let _failed = register(&store, "error-save", "anthropic");
    store.set_error("error-save", "Authentication failed");
    assert!(!store.is_pending("error-save", "anthropic"));
}

#[tokio::test]
async fn cancel_auth_session_handler() {
    let api = Api::new();
    let store = api.state.oauth_sessions().store();
    let _callback = register(store, "device-state", "xai");
    let cancel = |state: &str| {
        let path = match state {
            "" => SESSION.to_owned(),
            state => format!("{SESSION}?state={state}"),
        };
        api.send(keyed(Method::DELETE, &path, ""))
    };

    cancel("")
        .await
        .assert(StatusCode::BAD_REQUEST, &failed("missing state"));
    cancel("bad/state")
        .await
        .assert(StatusCode::BAD_REQUEST, &failed("invalid state"));
    cancel("device-state")
        .await
        .assert(StatusCode::OK, r#"{"cancelled":true,"status":"ok"}"#);
    assert!(!store.is_pending("device-state", "xai"));
    cancel("device-state")
        .await
        .assert(StatusCode::OK, r#"{"cancelled":false,"status":"ok"}"#);

    // The status after a cancel doesn't report success.
    api.get(&format!("{STATUS}?state=device-state"))
        .await
        .assert(StatusCode::OK, &failed("unknown or expired state"));
}

#[tokio::test]
async fn post_oauth_callback_hands_the_redirect_to_the_pending_login() {
    let dir = tempfile::tempdir().unwrap();
    let auth_dir = dir.path().join("missing-auth");
    let mut config = keyed_config();
    config.auth_dir = auth_dir.to_str().unwrap().to_owned();
    let api = Api::with(config, None);
    let state = "test-codex-state";
    let mut callback = register(api.state.oauth_sessions().store(), state, "codex");

    let redirect = format!("http://localhost:1455/auth/callback?state={state}&code=test-code");
    let body = json!({ "provider": "codex", "redirect_url": redirect }).to_string();
    api.send(request_from(LOCAL, Method::POST, CALLBACK, &body))
        .await
        .assert(StatusCode::OK, r#"{"status":"ok"}"#);

    assert_eq!(callback.try_recv(), Ok(code("test-code")));
    assert!(!auth_dir.exists(), "a callback made the auth directory");
}

#[test]
fn deliver_hands_the_callback_to_pending_sessions_of_callback_providers() {
    for provider in ["anthropic", "codex"] {
        let store = store();
        let state = format!("{provider}-state");
        let mut callback = register(&store, &state, provider);

        let sent = code(&format!("code-{provider}"));
        store.deliver(&state, provider, sent.clone()).unwrap();
        assert_eq!(callback.try_recv(), Ok(sent), "{provider}");
    }
}

#[tokio::test]
async fn request_codex_token_completion_keeps_concurrent_session_pending() {
    let id_token = codex_id_token(Some("oauth-user@example.test"), "");
    let upstream = Upstream::answering(codex_tokens(&id_token)).await;
    let auth_dir = AuthDir::new();
    let api = login_api(&auth_dir, &upstream.url);
    let store = api.state.oauth_sessions().store();

    let first = start_login(&api, CODEX_AUTH_URL).await;
    let second = start_login(&api, CODEX_AUTH_URL).await;
    deliver(&api, &first, "codex", code("first-code"));

    completed(&api, &first).await;
    assert!(
        store.is_pending(&second, "codex"),
        "concurrent codex session {second} didn't stay pending after {first} completed"
    );
    let requests = upstream.requests();
    assert_eq!(requests.len(), 1);
    assert!(request_body(&requests[0]).contains("code=first-code"));
    assert!(store.cancel(&second));
}

#[tokio::test]
async fn request_codex_token_plan_type_saved_to_auth_file() {
    for (claim_plan, want) in [("", "free"), ("pro", "pro")] {
        let id_token = codex_id_token(Some("oauth-user@example.test"), claim_plan);
        let upstream = Upstream::answering(codex_tokens(&id_token)).await;
        let auth_dir = AuthDir::new();
        let api = login_api(&auth_dir, &upstream.url);

        let state = start_login(&api, CODEX_AUTH_URL).await;
        deliver(&api, &state, "codex", code("test-code"));
        completed(&api, &state).await;

        let saved = auth_dir.read_json(&saved_file(&auth_dir, "codex-"));
        assert_eq!(saved["plan_type"], want, "{claim_plan:?}");
        assert_eq!(saved["email"], "oauth-user@example.test");
        assert_eq!(saved["access_token"], "access-codex");
    }
}

#[tokio::test]
async fn management_v8_oauth_contract() {
    let api = Api::new();
    let store = api.state.oauth_sessions().store();
    for (path, status, body) in [
        (
            "/v8/management/oauth/auth-url",
            StatusCode::BAD_REQUEST,
            r#"{"error":"provider is required"}"#,
        ),
        (
            "/v8/management/oauth/auth-url?provider=%20",
            StatusCode::BAD_REQUEST,
            r#"{"error":"provider is required"}"#,
        ),
        (
            "/v8/management/oauth/auth-url?provider=unknown",
            StatusCode::NOT_FOUND,
            r#"{"error":"provider_not_found"}"#,
        ),
    ] {
        let answer = api.get(path).await;
        assert_eq!(
            (answer.status, answer.body.as_str()),
            (status, body),
            "{path}"
        );
    }
    for (path, provider) in [
        ("/v8/management/oauth/auth-url?provider=codex", "codex"),
        (
            "/v8/management/oauth/auth-url?provider=%20CLAUDE%20",
            "anthropic",
        ),
        (CODEX_AUTH_URL, "codex"),
        (CLAUDE_AUTH_URL, "anthropic"),
    ] {
        let state = start_login(&api, path).await;
        assert!(store.is_pending(&state, provider), "{path}");
        api.get(&format!("/v8/management/oauth/status?state={state}"))
            .await
            .assert(StatusCode::OK, r#"{"status":"wait"}"#);
        let path = format!("/v8/management/oauth/session?state={state}");
        let answer = api.send(keyed(Method::DELETE, &path, "")).await;
        assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
        assert!(!store.is_pending(&state, provider), "{path}: not cancelled");
    }
}

// Not upstream's: the logins of other providers aren't served: the v8
// route answers as upstream answers a provider it doesn't know, and the v0
// routes and callback pages are unported.
#[tokio::test]
async fn other_providers_logins_are_not_served() {
    let api = Api::new();
    for provider in ["antigravity", "xai", "kimi", "gemini-cli", "anthropic"] {
        let path = format!("/v8/management/oauth/auth-url?provider={provider}");
        api.get(&path)
            .await
            .assert(StatusCode::NOT_FOUND, r#"{"error":"provider_not_found"}"#);
    }
    for path in [
        "/v0/management/antigravity-auth-url",
        "/v0/management/kimi-auth-url",
        "/v8/management/codex-auth-url",
    ] {
        api.get(path).await.assert(StatusCode::NOT_FOUND, "");
    }
    // Left to the server, which answers them with its 404.
    for path in ["/antigravity/callback", "/devin/callback", "/callback"] {
        let answer = api.send(request_from(LOCAL, Method::GET, path, "")).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{path}");
    }
    assert_eq!(api.state.oauth_sessions().store().count(), 0);
}

// Not upstream's: the callback routes check a callback in upstream's
// order, from anyone while a key is set, and hand it to its login.
#[tokio::test]
async fn oauth_callback_checks_and_hands_over_the_callback() {
    let api = Api::new();
    let store = api.state.oauth_sessions().store();
    let mut codex = register(store, "codex-state", "codex");
    let _claude = register(store, "claude-state", "anthropic");
    let get = |path: &str| {
        let path = format!("{CALLBACK}?{path}");
        api.send(request_from(REMOTE, Method::GET, &path, ""))
    };
    let post = |body: &str| {
        let path = "/v8/management/oauth/callback";
        api.send(request_from(REMOTE, Method::POST, path, body))
    };

    post("")
        .await
        .assert(StatusCode::BAD_REQUEST, &failed("invalid body"));
    post(r#"{"state":1}"#)
        .await
        .assert(StatusCode::BAD_REQUEST, &failed("invalid body"));
    post(r#"{"redirect_url":"http://[::1/cb?state=codex-state&code=a"}"#)
        .await
        .assert(StatusCode::BAD_REQUEST, &failed("invalid redirect_url"));
    get("code=a")
        .await
        .assert(StatusCode::BAD_REQUEST, &failed("state is required"));
    get("state=bad/state&code=a")
        .await
        .assert(StatusCode::BAD_REQUEST, &failed("invalid state"));
    get("state=codex-state").await.assert(
        StatusCode::BAD_REQUEST,
        &failed("code or error is required"),
    );
    get("state=other-state&code=a")
        .await
        .assert(StatusCode::NOT_FOUND, &failed("unknown or expired state"));
    get("provider=no%20such&state=codex-state&code=a")
        .await
        .assert(StatusCode::BAD_REQUEST, &failed("unsupported provider"));
    get("provider=claude&state=codex-state&code=a")
        .await
        .assert(
            StatusCode::BAD_REQUEST,
            &failed("provider does not match state"),
        );
    get("provider=gemini-cli&state=codex-state&code=a")
        .await
        .assert(
            StatusCode::BAD_REQUEST,
            &failed("provider does not match state"),
        );
    assert!(
        codex.try_recv().is_err(),
        "a refused callback was handed over"
    );

    // `openai` names Codex; the error, from `error_description` when
    // `error` is empty, goes with the code.
    get("provider=%20OpenAI%20&state=%20codex-state%20&code=%20the-code%20&error=&error_description=denied")
        .await
        .assert(StatusCode::OK, r#"{"status":"ok"}"#);
    assert_eq!(
        codex.try_recv(),
        Ok(Callback {
            code: "the-code".into(),
            error: "denied".into(),
        })
    );

    // A failed login answers that it failed, but not why.
    store.set_error("claude-state", "Bad request");
    let body = json!({ "state": "claude-state", "code": "a" }).to_string();
    post(&body)
        .await
        .assert(StatusCode::CONFLICT, &failed("oauth flow failed"));

    // Without a key set, the routes are unported.
    let api = Api::with(Config::default(), None);
    for method in [Method::GET, Method::POST] {
        let path = format!("{CALLBACK}?state=codex-state&code=a");
        let answer = api.send(request_from(LOCAL, method, &path, "{}")).await;
        answer.assert(StatusCode::NOT_FOUND, "");
    }
}

// Not upstream's: the main server's callback pages hand the callback to
// their provider's pending login of the state named, if there is one, and
// answer with upstream's page whatever happened, to anyone, even without a
// key set.
#[tokio::test]
async fn callback_pages_hand_over_the_callback() {
    let api = Api::with(Config::default(), None);
    let store = api.state.oauth_sessions().store();
    let mut claude = register(store, "claude-state", "anthropic");
    let mut codex = register(store, "codex-state", "codex");
    let page = |path: &str| api.send(request_from(REMOTE, Method::GET, path, ""));

    for path in [
        "/anthropic/callback",
        "/anthropic/callback?code=a",
        "/codex/callback?state=claude-state&code=a",
        "/anthropic/callback?state=codex-state&code=a",
        "/anthropic/callback?state=other-state&code=a",
        "/anthropic/callback?state=claude-state%2F&code=a",
        "/anthropic/callback?state=%20claude-state&code=a",
        "/anthropic/callback?state=..&code=a",
    ] {
        assert_page(&page(path).await, path);
    }
    assert!(claude.try_recv().is_err(), "a page handed over a callback");
    assert!(codex.try_recv().is_err(), "a page handed over a callback");

    let path = "/anthropic/callback?state=claude-state&code=%20the-code%23claude-state";
    assert_page(&page(path).await, path);
    assert_eq!(claude.try_recv(), Ok(code("the-code#claude-state")));

    let path = "/codex/callback?state=codex-state&error=&error_description=access_denied";
    assert_page(&page(path).await, path);
    assert_eq!(
        codex.try_recv(),
        Ok(Callback {
            code: String::new(),
            error: "access_denied".into(),
        })
    );

    // Other methods aren't served.
    let answer = api
        .send(request_from(REMOTE, Method::POST, "/codex/callback", ""))
        .await;
    answer.assert(StatusCode::NOT_FOUND, "404 page not found");
}

// Not upstream's: a Claude code may come as `code#state`, as Anthropic's
// page shows it: the code before the `#` is exchanged with the session's
// state, through open-ferry's own client, and the credential is saved.
#[tokio::test]
async fn claude_login_exchanges_the_code_before_the_hash() {
    let upstream = Upstream::answering(claude_tokens()).await;
    let auth_dir = AuthDir::new();
    let api = login_api(&auth_dir, &upstream.url);

    let state = start_login(&api, "/v8/management/oauth/auth-url?provider=claude").await;
    let body = json!({ "provider": "claude", "state": state, "code": format!("the-code#{state}") });
    let path = "/v8/management/oauth/callback";
    api.send(request_from(LOCAL, Method::POST, path, &body.to_string()))
        .await
        .assert(StatusCode::OK, r#"{"status":"ok"}"#);
    completed(&api, &state).await;
    api.get(&format!("{STATUS}?state={state}"))
        .await
        .assert(StatusCode::OK, r#"{"status":"ok"}"#);

    let requests = upstream.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert!(
        request.starts_with("POST /v1/oauth/token HTTP/1.1\r\n"),
        "{request}"
    );
    let head = request.to_ascii_lowercase();
    assert!(head.contains("\r\nuser-agent: open-ferry/"), "{request}");
    let sent: Value = serde_json::from_str(request_body(request)).unwrap();
    assert_eq!(sent["code"], "the-code");
    assert_eq!(sent["state"], state.as_str());

    let saved = auth_dir.read_json(&saved_file(&auth_dir, "claude-"));
    assert_eq!(saved["email"], "claude-user@example.test");
    assert_eq!(saved["access_token"], "access-claude");
    assert_eq!(saved["type"], "claude");
}

// Not upstream's: the exchange goes through the config's `proxy-url`.
#[tokio::test]
async fn the_exchange_goes_through_the_config_proxy() {
    let id_token = codex_id_token(Some("oauth-user@example.test"), "");
    let proxy = Upstream::answering(codex_tokens(&id_token)).await;
    let auth_dir = AuthDir::new();
    let mut config = auth_dir.config();
    config.proxy_url.clone_from(&proxy.url);
    let api = Api::over_with(&auth_dir, config, None);
    api.state.oauth_sessions().overrides().endpoints = Some("http://token.invalid".to_owned());

    let state = start_login(&api, CODEX_AUTH_URL).await;
    deliver(&api, &state, "codex", code("proxied-code"));
    completed(&api, &state).await;

    let requests = proxy.requests();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].starts_with("POST http://token.invalid/oauth/token HTTP/1.1\r\n"),
        "{}",
        requests[0]
    );
}

// Not upstream's: a login fails, saying why, when the provider reports an
// error, when the code can't be exchanged, and when the credential has no
// email.
#[tokio::test]
async fn failed_logins_say_why() {
    // The provider's error.
    let api = Api::new();
    for (path, provider, status) in [
        (CLAUDE_AUTH_URL, "anthropic", "Bad request"),
        (CODEX_AUTH_URL, "codex", "Bad Request"),
    ] {
        let state = start_login(&api, path).await;
        let body = json!({ "state": state, "error": "access_denied" }).to_string();
        api.send(request_from(LOCAL, Method::POST, CALLBACK, &body))
            .await
            .assert(StatusCode::OK, r#"{"status":"ok"}"#);
        assert_eq!(failure(&api, &state).await, status, "{provider}");
        api.get(&format!("{STATUS}?state={state}"))
            .await
            .assert(StatusCode::OK, &failed(status));
        // A failed session can't be cancelled, and refuses callbacks.
        assert!(!api.state.oauth_sessions().store().cancel(&state));
        api.send(request_from(LOCAL, Method::POST, CALLBACK, &body))
            .await
            .assert(StatusCode::CONFLICT, &failed("oauth flow failed"));
    }

    // The exchange's error.
    let refused = http_response("400 Bad Request", &[], br#"{"error":"invalid_grant"}"#);
    let upstream = Upstream::answering(refused).await;
    let auth_dir = AuthDir::new();
    let api = login_api(&auth_dir, &upstream.url);
    let state = start_login(&api, CLAUDE_AUTH_URL).await;
    deliver(&api, &state, "anthropic", code("bad-code"));
    assert_eq!(
        failure(&api, &state).await,
        "Failed to exchange authorization code for tokens"
    );
    let state = start_login(&api, CODEX_AUTH_URL).await;
    deliver(&api, &state, "codex", code("bad-code"));
    assert_eq!(
        failure(&api, &state).await,
        "Failed to exchange authorization code for tokens: \
         token exchange failed with status 400: {\"error\":\"invalid_grant\"}"
    );

    // A credential without an email.
    let upstream = Upstream::answering(codex_tokens(&codex_id_token(None, "pro"))).await;
    let api = login_api(&auth_dir, &upstream.url);
    let state = start_login(&api, CODEX_AUTH_URL).await;
    deliver(&api, &state, "codex", code("test-code"));
    assert_eq!(
        failure(&api, &state).await,
        "Failed to save authentication tokens: codex token storage missing account information"
    );
    assert_eq!(files(&auth_dir), Vec::<String>::new());
}

// Not upstream's: a login waits for its callback for the time set, then
// fails.
#[tokio::test]
async fn logins_time_out() {
    let api = Api::new();
    api.state.oauth_sessions().overrides().wait = Some(Duration::from_millis(50));
    let state = start_login(&api, CODEX_AUTH_URL).await;
    assert_eq!(
        failure(&api, &state).await,
        "Timeout waiting for OAuth callback"
    );
    api.get(&format!("{STATUS}?state={state}")).await.assert(
        StatusCode::OK,
        &failed("Timeout waiting for OAuth callback"),
    );
}

// Not upstream's: a login cancelled while it exchanges the code saves
// nothing.
#[tokio::test]
async fn a_login_cancelled_during_the_exchange_saves_nothing() {
    let auth_dir = AuthDir::new();
    let api = Api::over_with(
        &auth_dir,
        {
            let mut config = auth_dir.config();
            config.port = SERVER_PORT;
            config
        },
        None,
    );
    let state = api.state.clone();
    let upstream = Upstream::start(move |_, request| {
        let sent: Value = serde_json::from_str(request_body(request)).unwrap();
        let oauth_state = sent["state"].as_str().unwrap();
        assert!(state.oauth_sessions().store().cancel(oauth_state));
        claude_tokens()
    })
    .await;
    api.state.oauth_sessions().overrides().endpoints = Some(upstream.url.clone());

    // The login's forwarder stops when its wait ends.
    let state = start_login(&api, &format!("{CLAUDE_AUTH_URL}?is_webui=1")).await;
    let addr = forwarder(&api, Provider::Claude);
    deliver(&api, &state, "anthropic", code("the-code"));
    stopped(&api, Provider::Claude, addr).await;

    assert_eq!(upstream.requests().len(), 1);
    assert_eq!(api.state.oauth_sessions().store().get(&state), None);
    assert_eq!(files(&auth_dir), Vec::<String>::new());
    assert!(api.sync.calls().is_empty());
}

// Not upstream's: cancelling a login ends its wait at once, which stops
// its forwarder.
#[tokio::test]
async fn cancelling_a_login_ends_its_wait() {
    let api = Api::with(served_config(), None);
    let state = start_login(&api, &format!("{CODEX_AUTH_URL}?is_webui=%20Yes%20")).await;
    let addr = forwarder(&api, Provider::Codex);
    assert!(addr.ip().is_loopback());

    let path = format!("{SESSION}?state={state}");
    api.send(keyed(Method::DELETE, &path, ""))
        .await
        .assert(StatusCode::OK, r#"{"cancelled":true,"status":"ok"}"#);
    stopped(&api, Provider::Codex, addr).await;
}

// Not upstream's: a forwarder answers every request as Go's
// `http.Redirect` does, sending the browser to the main server's callback
// page with the query.
#[tokio::test]
async fn forwarders_send_the_browser_to_the_callback_page() {
    let api = Api::with(served_config(), None);
    let page = format!("http://127.0.0.1:{SERVER_PORT}/codex/callback");

    // Only a login from the web UI has one.
    let state = start_login(&api, CODEX_AUTH_URL).await;
    assert_eq!(
        api.state.oauth_sessions().forwarder_addr(Provider::Codex),
        None
    );
    assert!(api.state.oauth_sessions().store().cancel(&state));

    let state = start_login(&api, &format!("{CODEX_AUTH_URL}?is_webui=true")).await;
    let addr = forwarder(&api, Provider::Codex);

    let answer = forward(addr, "GET", "/auth/callback?code=c&state=s")
        .await
        .unwrap();
    let location = format!("{page}?code=c&state=s");
    assert_eq!(answer.status_line, "HTTP/1.1 302 Found");
    assert_eq!(answer.header("location"), Some(location.as_str()));
    assert_eq!(answer.header("cache-control"), Some("no-store"));
    assert_eq!(
        answer.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(
        answer.body,
        format!("<a href=\"{page}?code=c&amp;state=s\">Found</a>.\n\n")
    );

    let answer = forward(addr, "HEAD", "/?code=c").await.unwrap();
    assert_eq!(answer.status_line, "HTTP/1.1 302 Found");
    assert_eq!(
        answer.header("location"),
        Some(format!("{page}?code=c").as_str())
    );
    assert_eq!(
        answer.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(answer.body, "");

    let answer = forward(addr, "POST", "/any/path").await.unwrap();
    assert_eq!(answer.status_line, "HTTP/1.1 302 Found");
    assert_eq!(answer.header("location"), Some(page.as_str()));
    assert_eq!(answer.header("cache-control"), Some("no-store"));
    assert_eq!(answer.header("content-type"), None);
    assert_eq!(answer.body, "");

    assert!(api.state.oauth_sessions().store().cancel(&state));
    stopped(&api, Provider::Codex, addr).await;
}

// Not upstream's: with TLS on, the browser is sent to the callback page
// over https; without a server port, a login from the web UI can't start.
#[tokio::test]
async fn forwarders_follow_the_server_config() {
    let mut config = served_config();
    config.tls.enable = true;
    let api = Api::with(config, None);
    let state = start_login(&api, &format!("{CLAUDE_AUTH_URL}?is_webui=on")).await;
    let addr = forwarder(&api, Provider::Claude);
    let answer = forward(addr, "GET", "/callback?code=c&state=s")
        .await
        .unwrap();
    let location = format!("https://127.0.0.1:{SERVER_PORT}/anthropic/callback?code=c&state=s");
    assert_eq!(answer.header("location"), Some(location.as_str()));
    assert!(api.state.oauth_sessions().store().cancel(&state));
    stopped(&api, Provider::Claude, addr).await;

    let api = Api::new();
    api.get(&format!("{CLAUDE_AUTH_URL}?is_webui=1"))
        .await
        .assert(
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":"callback server unavailable"}"#,
        );
    assert_eq!(api.state.oauth_sessions().store().count(), 0);
}

// Not upstream's: while the port of the provider's redirect URI is held
// elsewhere, a login from the web UI fails and leaves no session.
#[tokio::test]
async fn a_held_callback_port_fails_the_login() {
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().port();
    let api = Api::with(served_config(), None);
    api.state.oauth_sessions().overrides().ports = Some((port, port));

    for path in [
        format!("{CODEX_AUTH_URL}?is_webui=1"),
        "/v8/management/oauth/auth-url?provider=claude&is_webui=1".to_owned(),
    ] {
        api.get(&path).await.assert(
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":"failed to start callback server"}"#,
        );
    }
    assert_eq!(api.state.oauth_sessions().store().count(), 0);
    drop(held);
}

// Not upstream's: a login started on a port that has a forwarder replaces
// it, and the end of the replaced forwarder's login leaves the new one
// running.
#[tokio::test]
async fn a_new_login_replaces_the_forwarder_on_its_port() {
    let port = {
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        free.local_addr().unwrap().port()
    };
    let api = Api::with(served_config(), None);
    api.state.oauth_sessions().overrides().ports = Some((port, port));
    let store = api.state.oauth_sessions().store();

    let first = start_login(&api, &format!("{CLAUDE_AUTH_URL}?is_webui=1")).await;
    let second = start_login(
        &api,
        "/v8/management/oauth/auth-url?provider=claude&is_webui=1",
    )
    .await;
    let addr = forwarder(&api, Provider::Claude);
    assert_eq!(addr.port(), port);
    assert!(store.is_pending(&first, "anthropic"));

    assert!(store.cancel(&first));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        api.state.oauth_sessions().forwarder_addr(Provider::Claude),
        Some(addr)
    );
    let answer = forward(addr, "GET", "/callback?code=x").await.unwrap();
    let location = format!("http://127.0.0.1:{SERVER_PORT}/anthropic/callback?code=x");
    assert_eq!(answer.header("location"), Some(location.as_str()));

    assert!(store.cancel(&second));
    stopped(&api, Provider::Claude, addr).await;
}

// Not upstream's: at most `MAX_SESSIONS` sessions are kept, so a login
// can't start while that many are.
#[tokio::test]
async fn logins_are_refused_while_the_sessions_are_full() {
    let api = Api::new();
    let store = api.state.oauth_sessions().store();
    let _callbacks: Vec<_> = (0..MAX_SESSIONS)
        .map(|n| register(store, &format!("state-{n}"), "codex"))
        .collect();

    api.get(CODEX_AUTH_URL).await.assert(
        StatusCode::TOO_MANY_REQUESTS,
        r#"{"error":"too many oauth sessions"}"#,
    );
    assert!(store.register("state-new", "codex").is_none());
    // A kept session's state can start again.
    assert!(store.register("state-0", "anthropic").is_some());

    assert!(store.cancel("state-1"));
    let state = start_login(&api, CODEX_AUTH_URL).await;
    assert!(store.cancel(&state));
}

// Not upstream's: sessions are dropped once they expire, completed ones
// sooner, which frees their places.
#[test]
fn expired_sessions_are_dropped() {
    let store = Store::new(Duration::from_millis(300));
    let _pending = register(&store, "pending", "codex");
    let _failed = register(&store, "failed", "codex");
    store.set_error("failed", " ");
    assert_eq!(store.get("failed").unwrap().status, "Authentication failed");
    let _callbacks: Vec<_> = (2..MAX_SESSIONS)
        .map(|n| register(&store, &format!("state-{n}"), "codex"))
        .collect();
    assert!(store.register("one-more", "codex").is_none());

    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(store.get("pending"), None);
    assert_eq!(store.get("failed"), None);
    assert!(store.register("one-more", "codex").is_some());
    assert_eq!(store.count(), 1);

    let store = Store::new(Duration::from_secs(60));
    store.set_completed_ttl(Duration::from_millis(50));
    let _completed = register(&store, "completed", "codex");
    store.complete("completed");
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(store.get("completed"), None);
}

// Not upstream's: a failed exchange is logged with a fixed message and the
// token endpoint's status, and `oauth-callback` answers anyone that the
// login failed, not why, though the endpoint's answer quotes the code and
// the PKCE verifier; the status a key reads keeps upstream's wording with
// both redacted. A login that succeeds logs no token.
#[tokio::test]
async fn exchange_failures_keep_secrets_out_of_answers_and_logs() {
    let (logs, _guard) = Logs::capture();
    let echo = Upstream::start(|_, request| {
        let body = format!("invalid_grant: {}", request_body(request));
        http_response("400 Bad Request", &[], body.as_bytes())
    })
    .await;
    let auth_dir = AuthDir::new();
    let api = login_api(&auth_dir, &echo.url);
    let mut verifiers = Vec::new();
    let mut statuses = Vec::new();
    for (n, path) in [CLAUDE_AUTH_URL, CODEX_AUTH_URL].into_iter().enumerate() {
        let state = start_login(&api, path).await;
        let callback = format!("{CALLBACK}?state={state}&code=MARKER-code%2Fx%20y");
        api.send(request_from(REMOTE, Method::GET, &callback, ""))
            .await
            .assert(StatusCode::OK, r#"{"status":"ok"}"#);
        let status = failure(&api, &state).await;

        // The token endpoint was sent the code and the verifier, and
        // answered with both.
        let request = echo.requests()[n].clone();
        let body = request_body(&request);
        let (verifier, want) = if path == CLAUDE_AUTH_URL {
            let sent: Value = serde_json::from_str(body).unwrap();
            assert_eq!(sent["code"], "MARKER-code/x y");
            let want = "Failed to exchange authorization code for tokens".to_owned();
            (sent["code_verifier"].as_str().unwrap().to_owned(), want)
        } else {
            assert!(body.contains("code=MARKER-code%2Fx+y&"), "{body}");
            let verifier = url::form_urlencoded::parse(body.as_bytes())
                .find(|(name, _)| name == "code_verifier")
                .unwrap()
                .1
                .into_owned();
            let want = "Failed to exchange authorization code for tokens: token exchange \
                        failed with status 400: invalid_grant: \
                        client_id=app_EMoamEEZ73f0CkXaXp7hrann&code=[redacted]\
                        &code_verifier=[redacted]&grant_type=authorization_code\
                        &redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback";
            (verifier, want.to_owned())
        };
        assert!(!verifier.is_empty());
        assert_eq!(status, want, "{path}");

        // A key reads the status; anyone calling back learns only that the
        // login failed.
        let answer = api.get(&format!("{STATUS}?state={state}")).await;
        let body = answer.expect(StatusCode::OK);
        assert_eq!(body, json!({ "error": status, "status": "error" }));
        api.send(request_from(REMOTE, Method::GET, &callback, ""))
            .await
            .assert(StatusCode::CONFLICT, &failed("oauth flow failed"));
        let body = json!({ "state": state, "code": "a" }).to_string();
        api.send(request_from(REMOTE, Method::POST, CALLBACK, &body))
            .await
            .assert(StatusCode::CONFLICT, &failed("oauth flow failed"));
        verifiers.push(verifier);
        statuses.push(status);
    }

    // Logins that succeed.
    let id_token = codex_id_token(Some("oauth-user@example.test"), "");
    let upstream = Upstream::answering(codex_tokens(&id_token)).await;
    let api = login_api(&auth_dir, &upstream.url);
    let state = start_login(&api, CODEX_AUTH_URL).await;
    deliver(&api, &state, "codex", code("MARKER-good-code"));
    completed(&api, &state).await;
    let upstream = Upstream::answering(claude_tokens()).await;
    let api = login_api(&auth_dir, &upstream.url);
    let state = start_login(&api, CLAUDE_AUTH_URL).await;
    deliver(&api, &state, "anthropic", code("MARKER-good-code"));
    completed(&api, &state).await;

    let logs = logs.text();
    for provider in ["Claude", "Codex"] {
        let failed = format!(
            "Failed to exchange authorization code for tokens ({provider}): \
             the token endpoint answered 400\n"
        );
        assert!(logs.contains(&failed), "{logs}");
        let saved = format!("{provider} authentication successful; credential saved to ");
        assert!(logs.contains(&saved), "{logs}");
    }
    let tokens = [
        "access-codex",
        "refresh-codex",
        &id_token,
        "access-claude",
        "refresh-claude",
    ];
    for secret in verifiers.iter().map(String::as_str).chain(tokens) {
        assert!(!logs.contains(secret), "{secret}: {logs}");
        for status in &statuses {
            assert!(!status.contains(secret), "{secret}: {status}");
        }
    }
    assert!(!logs.contains("MARKER"), "{logs}");
    assert!(!statuses.concat().contains("MARKER"), "{statuses:?}");
}

// Not upstream's: a callback's error is logged with a fixed message,
// naming the error only when RFC 6749 defines it, and never with its
// description or code; the session's status keeps upstream's wording.
#[tokio::test]
async fn callback_errors_are_logged_without_what_they_carry() {
    let (logs, _guard) = Logs::capture();
    let api = Api::new();
    let api = &api;
    let page = |path: String| async move {
        let answer = api.send(request_from(REMOTE, Method::GET, &path, "")).await;
        assert_page(&answer, &path);
    };

    let state = start_login(api, CODEX_AUTH_URL).await;
    page(format!(
        "/codex/callback?state={state}&code=MARKER-code&error=access_denied\
         &error_description=MARKER-description"
    ))
    .await;
    assert_eq!(failure(api, &state).await, "Bad Request");

    let state = start_login(api, CLAUDE_AUTH_URL).await;
    page(format!(
        "/anthropic/callback?state={state}&code=MARKER-code\
         &error_description=MARKER-description%20MARKER-code"
    ))
    .await;
    assert_eq!(failure(api, &state).await, "Bad request");

    let state = start_login(api, CODEX_AUTH_URL).await;
    let path = format!("{CALLBACK}?state={state}&code=MARKER-code&error=MARKER-error");
    api.send(request_from(REMOTE, Method::GET, &path, ""))
        .await
        .assert(StatusCode::OK, r#"{"status":"ok"}"#);
    assert_eq!(failure(api, &state).await, "Bad Request");

    let logs = logs.text();
    let reported: Vec<&str> = logs
        .lines()
        .filter(|line| line.contains("OAuth callback reported"))
        .collect();
    assert_eq!(
        reported,
        [
            "The Codex OAuth callback reported an error: access_denied",
            "The Claude OAuth callback reported an error",
            "The Codex OAuth callback reported an error",
        ]
    );
    assert!(!logs.contains("MARKER"), "{logs}");
}

// Not upstream's: what a login holds doesn't show in `Debug`.
#[test]
fn debug_hides_what_logins_hold() {
    let callback = Callback {
        code: "MARKER-code".into(),
        error: "MARKER-error".into(),
    };
    assert_eq!(
        format!("{callback:?}"),
        r#"Callback { code: "[redacted]", error: "[redacted]" }"#
    );
    assert_eq!(
        format!("{:?}", Callback::default()),
        r#"Callback { code: "", error: "" }"#
    );

    let store = store();
    let _callback = register(&store, "MARKER-state", "codex");
    store.deliver("MARKER-state", "codex", callback).unwrap();
    assert_eq!(format!("{store:?}"), "Store { .. }");
}

// Not upstream's: a login cancelled after its callback came, before its
// exchange, sends no token request.
#[tokio::test]
async fn a_login_cancelled_before_its_exchange_sends_no_request() {
    let upstream = Upstream::answering(claude_tokens()).await;
    let auth_dir = AuthDir::new();
    let api = login_api(&auth_dir, &upstream.url);
    let store = api.state.oauth_sessions().store();

    for (path, provider) in [(CLAUDE_AUTH_URL, "anthropic"), (CODEX_AUTH_URL, "codex")] {
        let state = start_login(&api, path).await;
        // The login runs on this thread: it can't run between these.
        deliver(&api, &state, provider, code("the-code"));
        assert!(store.cancel(&state));
        ended(&api).await;
        assert_eq!(store.get(&state), None);
    }
    assert_eq!(upstream.requests().len(), 0);
    assert_eq!(files(&auth_dir), Vec::<String>::new());
    assert!(api.sync.calls().is_empty());
}

// Not upstream's: a login cancelled while the token endpoint keeps it
// waiting drops its request at once, stops its forwarder and saves
// nothing.
#[tokio::test]
async fn a_login_cancelled_during_a_stalled_exchange_drops_it() {
    let stall = Stall::start().await;
    let auth_dir = AuthDir::new();
    let mut config = auth_dir.config();
    config.port = SERVER_PORT;
    let api = Api::over_with(&auth_dir, config, None);
    api.state.oauth_sessions().overrides().endpoints = Some(stall.url.clone());
    let store = api.state.oauth_sessions().store();

    let state = start_login(&api, &format!("{CODEX_AUTH_URL}?is_webui=1")).await;
    let addr = forwarder(&api, Provider::Codex);
    deliver(&api, &state, "codex", code("the-code"));
    reaches(&stall.requests, 1, "requests").await;

    let path = format!("{SESSION}?state={state}");
    api.send(keyed(Method::DELETE, &path, ""))
        .await
        .assert(StatusCode::OK, r#"{"cancelled":true,"status":"ok"}"#);
    ended(&api).await;
    stopped(&api, Provider::Codex, addr).await;
    reaches(&stall.closed, 1, "closed connections").await;
    assert_eq!(store.get(&state), None);
    assert_eq!(files(&auth_dir), Vec::<String>::new());
    assert!(api.sync.calls().is_empty());
}

// Not upstream's: an exchange that takes too long fails the login.
#[tokio::test]
async fn exchanges_time_out() {
    let stall = Stall::start().await;
    let auth_dir = AuthDir::new();
    let api = login_api(&auth_dir, &stall.url);
    api.state.oauth_sessions().overrides().exchange = Some(Duration::from_millis(200));

    for (path, provider) in [(CLAUDE_AUTH_URL, "anthropic"), (CODEX_AUTH_URL, "codex")] {
        let state = start_login(&api, path).await;
        deliver(&api, &state, provider, code("the-code"));
        let timed_out = "Timeout exchanging authorization code for tokens";
        assert_eq!(failure(&api, &state).await, timed_out, "{provider}");
        api.get(&format!("{STATUS}?state={state}"))
            .await
            .assert(StatusCode::OK, &failed(timed_out));
    }
    ended(&api).await;
    reaches(&stall.requests, 2, "requests").await;
    reaches(&stall.closed, 2, "closed connections").await;
    assert_eq!(files(&auth_dir), Vec::<String>::new());
}

// Not upstream's: shutting down stops every login, waiting for its callback
// or exchanging its code, which stops their forwarders and drops their
// sessions; no login starts after that.
#[tokio::test]
async fn shutting_down_stops_the_logins() {
    let stall = Stall::start().await;
    let auth_dir = AuthDir::new();
    let mut config = auth_dir.config();
    config.port = SERVER_PORT;
    let api = Api::over_with(&auth_dir, config, None);
    api.state.oauth_sessions().overrides().endpoints = Some(stall.url.clone());
    let store = api.state.oauth_sessions().store();

    let waiting = start_login(&api, &format!("{CODEX_AUTH_URL}?is_webui=1")).await;
    let addr = forwarder(&api, Provider::Codex);
    let exchanging = start_login(&api, CLAUDE_AUTH_URL).await;
    deliver(&api, &exchanging, "anthropic", code("the-code"));
    reaches(&stall.requests, 1, "requests").await;
    assert_eq!(api.state.oauth_sessions().running(), 2);

    tokio::time::timeout(Duration::from_secs(5), api.state.shutdown())
        .await
        .expect("the logins stop");
    assert_eq!(api.state.oauth_sessions().running(), 0);
    stopped(&api, Provider::Codex, addr).await;
    reaches(&stall.closed, 1, "closed connections").await;
    assert_eq!(store.get(&waiting), None);
    assert_eq!(store.get(&exchanging), None);

    api.get(CODEX_AUTH_URL).await.assert(
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"error":"server shutting down"}"#,
    );
    assert_eq!(store.count(), 0);
    assert_eq!(api.state.oauth_sessions().running(), 0);
    assert_eq!(files(&auth_dir), Vec::<String>::new());
    assert!(api.sync.calls().is_empty());
}
