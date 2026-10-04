//! Not upstream's: what the executors' tests of the secrets an attempt sends
//! share. An upstream or proxy that echoes what it was sent in its error
//! must not get any of it back to the client: the credential headers after
//! the custom ones, each cookie, the URL's credentials and the proxy's
//! password. The mocks listen on ephemeral ports of 127.0.0.1.

use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::http::Uri;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method};
use open_ferry_core::auth::Auth;
use open_ferry_core::exec::ExecError;
use open_ferry_core::observe::{Observation, RequestContext, Tap};

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
