//! Not upstream's: what the executors' tests of the secrets an attempt sends
//! share. An upstream or proxy that echoes what it was sent in its error
//! must not get any of it back to the client: the credential headers after
//! the custom ones, each cookie, the URL's credentials and the proxy's
//! password. The mocks listen on ephemeral ports of 127.0.0.1. [`Logs`]
//! captures what a test logs, for the tests that keep secrets out of logs.

use std::cell::RefCell;
use std::fmt::{self, Write as _};
use std::sync::{Arc, Mutex, Once, PoisonError};

use axum::Router;
use axum::http::Uri;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method};
use open_ferry_core::auth::Auth;
use open_ferry_core::exec::ExecError;
use open_ferry_core::observe::{Observation, RequestContext, Tap};
use tracing::subscriber::Interest;

use crate::codex::websocket::mock::{Answer, Server};
use crate::redact::REDACTED;

/// The client's key, which the credential forwards upstream.
const FORWARDED_KEY: &str = "forwarded-key-0123456789";
/// The client's cookie, which the credential forwards upstream.
const COOKIE: &str = "cookie-secret-0123456789";
/// The password in the base URL's user info.
const URL_SECRET: &str = "url-secret-0123456789";
/// The proxy's password.
const PROXY_SECRET: &str = "proxy-secret-0123456789";

/// A mock that answers every request with one status and an error whose
/// message echoes the request: its target, every header, and each `Basic`
/// credential decoded.
struct Echo {
    addr: String,
    echoed: Arc<Mutex<Vec<String>>>,
}

impl Echo {
    async fn start(status: u16) -> Self {
        let echoed = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&echoed);
        let app = Router::new().fallback(move |method: Method, uri: Uri, headers: HeaderMap| {
            let recorder = Arc::clone(&recorder);
            async move {
                let echo = echo_of(&method, &uri, &headers);
                recorder
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(echo.clone());
                let body = serde_json::json!({
                    "type": "error",
                    "error": {"type": "authentication_error", "message": echo},
                });
                axum::response::Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { addr, echoed }
    }

    fn echoed(&self) -> String {
        self.echoed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .join("\n")
    }
}

/// The request as the mock echoes it.
fn echo_of(method: &Method, uri: &Uri, headers: &HeaderMap) -> String {
    let mut parts = vec![format!("{method} {uri}")];
    for (name, value) in headers {
        let value = String::from_utf8_lossy(value.as_bytes());
        if let Some(credential) = value.strip_prefix("Basic ")
            && let Ok(decoded) = STANDARD.decode(credential.trim())
        {
            parts.push(format!(
                "{name} decoded: {}",
                String::from_utf8_lossy(&decoded)
            ));
        }
        parts.push(format!("{name}: {value}"));
    }
    parts.join(" | ")
}

/// A call to make and check: its credential, the client's headers, and
/// what answers it.
pub(crate) struct Case {
    /// The credential, forwarding the client's key and cookie.
    pub(crate) auth: Arc<Auth>,
    /// The client's headers: its key and cookie.
    pub(crate) headers: HeaderMap,
    /// The secrets the answer must echo, proving they were sent.
    sent: Vec<&'static str>,
    echo: Echo,
}

impl Case {
    /// Asserts that the answer echoed the secrets sent, and that `error`
    /// quotes no secret at all, only [`REDACTED`].
    pub(crate) fn check(&self, error: &ExecError) {
        self.check_text(&error.message);
    }

    /// [`Self::check`] for what the client gets as text.
    pub(crate) fn check_text(&self, text: &str) {
        let echoed = self.echo.echoed();
        for secret in &self.sent {
            assert!(echoed.contains(secret), "{secret} wasn't sent: {echoed}");
        }
        for secret in secrets() {
            assert!(!text.contains(&secret), "{secret} in {text}");
        }
        assert!(text.contains(REDACTED), "{text}");
    }
}

/// A tap that keeps the body of each answer as it was read, for a test that
/// what the client gets is redacted while the taps, which redact for the
/// disk themselves, read what the upstream sent.
#[derive(Default)]
pub(crate) struct Raw(Mutex<Vec<u8>>);

impl Tap for Raw {
    fn chunk(&self, chunk: &Bytes) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(chunk);
    }
}

impl Raw {
    /// The observation of a call that the new recorder sees, and the
    /// recorder.
    pub(crate) fn observe() -> (Arc<Observation>, Arc<Self>) {
        let raw = Arc::new(Self::default());
        let context = Arc::new(RequestContext::new(Method::POST, "/v1/test".into()));
        let observation = Arc::new(Observation::new(context, vec![raw.clone()]));
        (observation, raw)
    }

    /// What the taps have read of the answers so far.
    pub(crate) fn seen(&self) -> String {
        let body = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&body).into_owned()
    }
}

/// Every secret the cases send, and the `Basic` credentials made of them.
fn secrets() -> Vec<String> {
    let mut secrets: Vec<String> = [FORWARDED_KEY, COOKIE, URL_SECRET, PROXY_SECRET]
        .map(str::to_owned)
        .into();
    for password in [URL_SECRET, PROXY_SECRET] {
        secrets.push(STANDARD.encode(format!("user:{password}")));
    }
    secrets
}

/// The two calls an executor's test makes, with the credential `auth`
/// gives for a base URL: to an upstream that answers 401, at a base URL
/// with user info, with the credential forwarding the client's key and
/// cookie; and through a proxy, with a password, that answers 407 for a
/// plain HTTP upstream.
pub(crate) async fn cases(auth: impl Fn(&str) -> Auth) -> [Case; 2] {
    let forwarding = |mut auth: Auth| {
        auth.attributes
            .insert("header:X-Upstream-Key".into(), "$X-Client-Key".into());
        auth.attributes
            .insert("header:Cookie".into(), "$Cookie".into());
        auth
    };
    let mut headers = HeaderMap::new();
    headers.insert("x-client-key", HeaderValue::from_static(FORWARDED_KEY));
    headers.insert(
        "cookie",
        HeaderValue::from_str(&format!("sid={COOKIE}; theme=dark")).unwrap(),
    );

    let upstream = Echo::start(401).await;
    let direct = forwarding(auth(&format!("http://user:{URL_SECRET}@{}", upstream.addr)));
    let proxy = Echo::start(407).await;
    let proxied = Auth {
        proxy_url: format!("http://user:{PROXY_SECRET}@{}", proxy.addr),
        ..forwarding(auth("http://127.0.0.1:9"))
    };
    [
        Case {
            auth: Arc::new(direct),
            headers: headers.clone(),
            sent: vec![FORWARDED_KEY, COOKIE],
            echo: upstream,
        },
        Case {
            auth: Arc::new(proxied),
            headers,
            sent: vec![PROXY_SECRET, FORWARDED_KEY],
            echo: proxy,
        },
    ]
}

/// The custom key header a WebSocket test's credential sets.
pub(crate) const KEY_HEADER: &str = "header:X-Upstream-Key";
/// The key header's value for the first turn of a WebSocket session.
pub(crate) const OLD_KEY: &str = "old-header-secret-0123";
/// The key header's value by the second turn.
pub(crate) const NEW_KEY: &str = "new-header-secret-0123";

/// A WebSocket server answering each message on a connection with a delta
/// and a completed response (`resp-1`, `resp-2`, ... on the connection),
/// both saying `key <value>` for the `X-Upstream-Key` its handshake sent.
pub(crate) async fn echoing_the_handshake() -> Server {
    Server::start(|_| {
        Answer::accept(|mut peer| async move {
            let key = peer
                .handshake()
                .header("x-upstream-key")
                .unwrap_or_default()
                .to_owned();
            let mut turn = 0;
            while peer.recv().await.is_some() {
                turn += 1;
                peer.send(&format!(
                    r#"{{"type":"response.output_text.delta","item_id":"msg-{turn}","output_index":0,"content_index":0,"delta":"key {key}"}}"#
                ))
                .await;
                peer.send(&format!(
                    r#"{{"type":"response.completed","response":{{"id":"resp-{turn}","status":"completed","output":[{{"type":"message","id":"msg-{turn}","role":"assistant","content":[{{"type":"output_text","text":"key {key}"}}]}}],"usage":{{"input_tokens":0,"output_tokens":0,"total_tokens":0}}}}}}"#
                ))
                .await;
            }
        })
    })
    .await
}

/// Checks the chunks of a turn on a connection opened with [`OLD_KEY`]
/// don't have it, and have it redacted.
pub(crate) fn assert_old_key_redacted(turn: &str, chunks: &[String]) {
    for chunk in chunks {
        assert!(
            !chunk.contains(OLD_KEY),
            "{turn}: the key reached the client: {chunk}"
        );
    }
    assert!(
        chunks
            .iter()
            .any(|chunk| chunk.contains(&format!("key {REDACTED}"))),
        "{turn}: {chunks:?}"
    );
}

thread_local! {
    /// Where this thread's logs go while a test captures them.
    static CAPTURED: RefCell<Option<Arc<Mutex<String>>>> = const { RefCell::new(None) };
}

/// What a test logs on its thread, one event a line: the message, then
/// any other field as ` name=value` (as `open-ferry-management`'s OAuth
/// tests capture them). Shared by every test of the crate that reads its
/// logs, as a test binary has one global subscriber.
#[derive(Clone, Default)]
pub(crate) struct Logs(Arc<Mutex<String>>);

impl Logs {
    /// Captures what this thread logs until the guard is dropped. A
    /// `#[tokio::test]` runs its tasks on its thread, so their logs too.
    ///
    /// The subscriber is the global one, for every thread: a scoped one
    /// misses events whose callsite another thread registered first.
    pub(crate) fn capture() -> (Self, Capturing) {
        static INSTALL: Once = Once::new();
        INSTALL.call_once(|| {
            let _ = tracing::subscriber::set_global_default(Capture);
            tracing::callsite::rebuild_interest_cache();
        });
        let logs = Self::default();
        CAPTURED.with(|captured| *captured.borrow_mut() = Some(Arc::clone(&logs.0)));
        (logs, Capturing)
    }

    /// What was captured so far.
    pub(crate) fn text(&self) -> String {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Ends this thread's capture when dropped.
pub(crate) struct Capturing;

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
        let mut text = text.lock().unwrap_or_else(PoisonError::into_inner);
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
