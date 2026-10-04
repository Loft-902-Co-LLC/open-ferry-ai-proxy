//! The credential's secret kept out of a connection's failures: the call's
//! error and the logs. None of these is upstream's: upstream passes close
//! reasons and connection errors on, and logs them, as they came.

use std::cell::RefCell;
use std::fmt::{self, Write as _};
use std::sync::{Arc, Mutex, Once};

use open_ferry_core::auth::Auth;
use open_ferry_core::executor::ProviderExecutor;
use tracing::subscriber::Interest;

use super::super::mock::{Answer, Server};
use super::{
    HELLO, auth_with, collect, executor, refused, request, with_header, within, ws_options,
};
use crate::redact::REDACTED;

/// The credential's secret, as Codex or a proxy might quote it.
const TOKEN: &str = "sk-review-fake-token";

// Not upstream's: a close reason that quotes the token is redacted in the
// call's error and in the log of the disconnect.
#[tokio::test]
async fn a_close_reason_has_the_token_redacted() {
    let (logs, _capturing) = Logs::capture();
    let server = Server::start(|_| {
        Answer::accept(|mut peer| async move {
            if peer.recv().await.is_some() {
                peer.close(1008, &format!("invalid token: {TOKEN}")).await;
                peer.hold().await;
            }
        })
    })
    .await;
    let response = executor()
        .execute_stream(
            Arc::new(auth_with(&server.url, &[("api_key", TOKEN)])),
            request("gpt-5-codex", HELLO),
            ws_options("closing-with-token"),
        )
        .await
        .unwrap();
    let (_, error) = collect(response).await;
    let error = error.expect("the stream ended without an error");
    assert_eq!(
        error.message,
        format!("websocket: close 1008 (policy violation): invalid token: {REDACTED}")
    );
    server.wait_closed(1).await;
    let logs = logs.text();
    assert!(
        logs.contains("codex websockets: upstream disconnected"),
        "{logs}"
    );
    assert!(logs.contains(REDACTED), "{logs}");
    assert!(!logs.contains(TOKEN), "{logs}");
}

// Not upstream's: a refused handshake whose body quotes the token fails the
// call with the token redacted, and logs nothing of it.
#[tokio::test]
async fn a_refused_handshake_has_the_token_redacted() {
    let (logs, _capturing) = Logs::capture();
    let server = Server::refusing(401, &format!(r#"{{"error":"invalid token {TOKEN}"}}"#)).await;
    let error = refused(
        within(
            "the call",
            executor().execute_stream(
                Arc::new(auth_with(&server.url, &[("api_key", TOKEN)])),
                request("gpt-5-codex", HELLO),
                ws_options(""),
            ),
        )
        .await,
    );
    assert!(error.message.contains(REDACTED), "{error:?}");
    assert!(!error.message.contains(TOKEN), "{error:?}");
    assert!(!logs.text().contains(TOKEN), "{}", logs.text());
}

// Not upstream's: a proxy that refuses the `CONNECT` with a reason quoting
// the token fails the call with the token redacted.
#[tokio::test]
async fn a_refused_connect_has_the_token_redacted() {
    let (logs, _capturing) = Logs::capture();
    let proxy = Server::start(|_| {
        Answer::Raw(format!("HTTP/1.1 407 invalid token {TOKEN}\r\n\r\n").into_bytes())
    })
    .await;
    let auth = Auth {
        proxy_url: proxy.url.clone(),
        ..auth_with("http://127.0.0.1:9", &[("api_key", TOKEN)])
    };
    let error = refused(
        within(
            "the call",
            executor().execute_stream(
                Arc::new(auth),
                request("gpt-5-codex", HELLO),
                ws_options(""),
            ),
        )
        .await,
    );
    assert_eq!(
        error.message,
        format!("codex websockets executor: proxy CONNECT failed: 407 invalid token {REDACTED}")
    );
    assert_eq!(proxy.record().handshakes.len(), 1);
    assert!(!logs.text().contains(TOKEN), "{}", logs.text());
}

/// The client's key, which the credential forwards upstream.
const FORWARDED_KEY: &str = "forwarded-key-0123456789";
/// The client's cookie, which the credential forwards upstream.
const COOKIE: &str = "cookie-secret-0123456789";
/// The proxy's password.
const PROXY_SECRET: &str = "proxy-secret-0123456789";

/// A credential for `base_url` forwarding the client's key and cookie.
fn forwarding(base_url: &str) -> Auth {
    auth_with(
        base_url,
        &[
            ("api_key", TOKEN),
            ("header:X-Upstream-Key", "$X-Client-Key"),
            ("header:Cookie", "$Cookie"),
        ],
    )
}

/// Options of a client on the WebSocket that sends its key and cookie.
fn client_options() -> open_ferry_core::exec::Options {
    with_header(
        with_header(ws_options(""), "x-client-key", FORWARDED_KEY),
        "cookie",
        &format!("sid={COOKIE}"),
    )
}

// Not upstream's: a refused handshake whose body quotes the headers the
// custom ones set, the forwarded cookie among them, fails the call with
// them redacted.
#[tokio::test]
async fn a_refused_handshake_has_every_secret_sent_redacted() {
    let server = Server::refusing(
        401,
        &format!(r#"{{"error":"invalid {FORWARDED_KEY} for {COOKIE} with {TOKEN}"}}"#),
    )
    .await;
    let error = refused(
        within(
            "the call",
            executor().execute_stream(
                Arc::new(forwarding(&server.url)),
                request("gpt-5-codex", HELLO),
                client_options(),
            ),
        )
        .await,
    );
    let handshake = server.record().handshakes[0].clone();
    assert_eq!(handshake.header("x-upstream-key"), Some(FORWARDED_KEY));
    assert_eq!(
        handshake.header("cookie"),
        Some(format!("sid={COOKIE}").as_str())
    );
    for secret in [FORWARDED_KEY, COOKIE, TOKEN] {
        assert!(!error.message.contains(secret), "{error:?}");
    }
    assert!(
        error.message.contains(&format!(
            "invalid {REDACTED} for {REDACTED} with {REDACTED}"
        )),
        "{error:?}"
    );
}

// Not upstream's: a proxy that refuses the `CONNECT` with a reason quoting
// its password and the `Proxy-Authorization` it was sent fails the call
// with both redacted.
#[tokio::test]
async fn a_refused_connect_has_the_proxys_password_redacted() {
    let credential = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        format!("user:{PROXY_SECRET}"),
    );
    let reason = format!(
        "HTTP/1.1 407 bad {PROXY_SECRET} {credential}

"
    );
    let proxy = Server::start(move |_| Answer::Raw(reason.clone().into_bytes())).await;
    let auth = Auth {
        proxy_url: proxy
            .url
            .replacen("http://", &format!("http://user:{PROXY_SECRET}@"), 1),
        ..forwarding("http://127.0.0.1:9")
    };
    let error = refused(
        within(
            "the call",
            executor().execute_stream(
                Arc::new(auth),
                request("gpt-5-codex", HELLO),
                client_options(),
            ),
        )
        .await,
    );
    let handshake = proxy.record().handshakes[0].clone();
    assert_eq!(
        handshake.header("proxy-authorization"),
        Some(format!("Basic {credential}").as_str())
    );
    assert_eq!(
        error.message,
        format!("codex websockets executor: proxy CONNECT failed: 407 bad {REDACTED} {REDACTED}")
    );
}

thread_local! {
    /// Where this thread's logs go while a test captures them.
    static CAPTURED: RefCell<Option<Arc<Mutex<String>>>> = const { RefCell::new(None) };
}

/// What a test logs on its thread, one event a line: the message, then
/// any other field as ` name=value` (as `open-ferry-management`'s OAuth
/// tests capture them).
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
