//! What a call's taps see of its upstream attempts: each request as it is
//! sent, with the credential's headers set, then the answer's head, its
//! body as it is read, and what failed after the head (the
//! `RecordAPIRequest`, `RecordAPIResponseMetadata`, `AppendAPIResponseChunk`
//! and `RecordAPIResponseError` calls of upstream's executors). A send that
//! fails is told by the manager, with the call's error.
//!
//! A send site is given its call's [`Attempt`]. When a tap sees the call,
//! it tells them the request with [`announce`] before it sends; once the
//! answer comes, it hands it to [`response`], which tells the taps its head
//! and marks it, so the body readers give each chunk they read to the
//! [`BodyTap`] they find on it. Without taps, each step is one branch, and
//! the answer isn't marked. A send that connects first, as a Codex
//! WebSocket's does, also tells the taps with [`request_sent`] that the
//! request is going out on the open connection. OAuth and token calls
//! aren't tapped, as upstream doesn't record them.
//!
//! Each send site gathers the attempt's [`secrets`] from the request as it
//! is finally sent, tells the taps them with the request, and scrubs them
//! from the errors it makes of the upstream's answer.
//!
//! Deviations from upstream: the whole module (see
//! [`open_ferry_core::observe`]).

use std::fmt;
use std::sync::Arc;

use bytes::Bytes;
use http::{HeaderMap, Method};
use open_ferry_core::auth::Auth;
use open_ferry_core::exec::{Format, Options};
use open_ferry_core::observe::{AttemptKind, AttemptRequest, Observation};

use crate::redact::Secrets;

/// The secrets an attempt sends to `url` with `headers`: those of its
/// credential headers and cookies, of its URL's user info and key-like
/// query parameters, of the proxy setting `proxy` it goes through (the
/// credential's or the global one, as the executor's clients pick it), and
/// `auth`'s own keys and tokens (see [`Secrets`]).
pub(crate) fn secrets(url: &str, headers: &HeaderMap, proxy: &str, auth: &Auth) -> Secrets {
    let mut secrets = Secrets::new();
    secrets.add_headers(headers);
    secrets.add_url(url);
    secrets.add_proxy(proxy);
    secrets.add_auth(auth);
    secrets
}

/// What a send site is told about the attempts of its executor call,
/// besides the request it sends.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Attempt<'a> {
    /// The call's observation, when any tap sees it.
    pub(crate) observation: Option<&'a Arc<Observation>>,
    /// What the attempts are for.
    pub(crate) kind: AttemptKind,
    /// The executor's provider.
    pub(crate) provider: &'a str,
    /// The model sent upstream.
    pub(crate) model: &'a str,
    /// The format of the bodies sent.
    pub(crate) format: &'a Format,
    /// The credential.
    pub(crate) auth: &'a Auth,
}

impl<'a> Attempt<'a> {
    /// The attempts of a call of `kind` made with `options` and `auth`.
    pub(crate) fn new(
        options: &'a Options,
        kind: AttemptKind,
        provider: &'a str,
        model: &'a str,
        format: &'a Format,
        auth: &'a Auth,
    ) -> Self {
        Self {
            observation: options.tapped(),
            kind,
            provider,
            model,
            format,
            auth,
        }
    }

    /// The attempt that sends `body` with `method` to `url` with `headers`,
    /// sending `secrets` somewhere in them (see [`secrets`]).
    pub(crate) fn request<'b>(
        &'b self,
        method: &'b Method,
        url: &'b str,
        headers: &'b HeaderMap,
        body: &'b Bytes,
        secrets: &'b Secrets,
    ) -> AttemptRequest<'b> {
        AttemptRequest {
            kind: self.kind,
            method,
            url,
            headers,
            body,
            provider: self.provider,
            model: self.model,
            format: self.format,
            auth: self.auth,
            secrets,
        }
    }
}

/// Tells `observation`'s taps `request` is about to be sent, and gives
/// what sees its answer.
pub(crate) fn announce(observation: &Arc<Observation>, request: &AttemptRequest<'_>) -> BodyTap {
    observation.attempt_request(request);
    BodyTap(Arc::clone(observation))
}

/// Tells the taps `response` came, when `tap` is given, and marks it so its
/// body is tapped as it is read.
pub(crate) fn response(tap: Option<BodyTap>, response: &mut reqwest::Response) {
    if let Some(tap) = tap {
        tap.0
            .response_head(response.status().as_u16(), response.headers());
        response.extensions_mut().insert(tap);
    }
}

/// What sees an answer's body as it is read.
#[derive(Clone, Debug)]
pub(crate) struct BodyTap(Arc<Observation>);

impl BodyTap {
    /// What [`response`] marked `response` with, if anything.
    pub(crate) fn of(response: &reqwest::Response) -> Option<Self> {
        response.extensions().get::<Self>().cloned()
    }

    /// Tells the taps an answer came with `status` and `headers`, for an
    /// attempt whose answer isn't an HTTP response, as `claude-cli`'s, which
    /// comes from a process.
    pub(crate) fn response_head(&self, status: u16, headers: &HeaderMap) {
        self.0.response_head(status, headers);
    }

    /// Tells the taps the request is about to go out on a connection that
    /// is up (see [`open_ferry_core::observe::Tap::request_sent`]).
    pub(crate) fn request_sent(&self) {
        self.0.request_sent();
    }

    /// Gives the taps the next part of the body: a chunk off the wire, or
    /// a WebSocket message.
    pub(crate) fn chunk(&self, chunk: &Bytes) {
        self.0.chunk(chunk);
    }

    /// Tells the taps the attempt failed with `error` after its head came,
    /// where upstream's executors record it (`RecordAPIResponseError`).
    pub(crate) fn error(&self, error: &dyn fmt::Display) {
        self.0.attempt_error(&error.to_string());
    }
}

/// Tells `tap`, if any, its request is about to go out on a connection that
/// is up (see [`BodyTap::request_sent`]).
pub(crate) fn request_sent(tap: Option<&BodyTap>) {
    if let Some(tap) = tap {
        tap.request_sent();
    }
}

/// Tells `tap`, if any, the attempt failed with `error` (see
/// [`BodyTap::error`]).
pub(crate) fn attempt_error(tap: Option<&BodyTap>, error: &dyn fmt::Display) {
    if let Some(tap) = tap {
        tap.error(error);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use open_ferry_core::observe::{RequestContext, Tap};

    use super::*;

    /// A tap that writes down what it is told.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl Tap for Recorder {
        fn attempt_request(&self, request: &AttemptRequest<'_>) {
            self.push(format!(
                "request {} {} {}",
                request.url,
                request.model,
                request.secrets.iter().collect::<Vec<_>>().join(",")
            ));
        }

        fn response_head(&self, status: u16, _headers: &HeaderMap) {
            self.push(format!("head {status}"));
        }

        fn chunk(&self, chunk: &Bytes) {
            self.push(format!("chunk {}", String::from_utf8_lossy(chunk)));
        }

        fn attempt_error(&self, message: &str) {
            self.push(format!("error {message}"));
        }
    }

    impl Recorder {
        fn push(&self, event: String) {
            self.0.lock().unwrap().push(event);
        }
    }

    /// Not upstream's: the request, the head and the body, read through
    /// the crate's reader, reach the taps in order; without taps nothing is
    /// marked.
    #[tokio::test]
    async fn taps_the_request_the_head_and_the_body() {
        let recorder = Arc::new(Recorder::default());
        let context = Arc::new(RequestContext::new(Method::POST, "/v1/messages".into()));
        let observation = Arc::new(Observation::new(context, vec![recorder.clone()]));
        let mut options = Options::new(Format::CLAUDE);
        options.observation = Some(Arc::clone(&observation));
        let auth = Auth::default();
        let claude = Format::CLAUDE;
        let attempt = Attempt::new(
            &options,
            AttemptKind::Execute,
            "claude",
            "claude-sonnet",
            &claude,
            &auth,
        );
        let body = Bytes::from_static(b"{}");
        let tap = attempt.observation.map(|observation| {
            announce(
                observation,
                &attempt.request(
                    &Method::POST,
                    "https://example.test/v1/messages",
                    &HeaderMap::new(),
                    &body,
                    &Secrets::from_iter(["sk-secret"]),
                ),
            )
        });
        let mut answer = reqwest::Response::from(http::Response::new("hello"));
        response(tap, &mut answer);
        let read = crate::codex::client::read_body(answer, 64).await.unwrap();
        assert_eq!(read, b"hello");
        assert_eq!(
            *recorder.0.lock().unwrap(),
            [
                "request https://example.test/v1/messages claude-sonnet sk-secret",
                "head 200",
                "chunk hello",
            ]
        );

        let untapped = Options::new(Format::CLAUDE);
        let attempt = Attempt::new(
            &untapped,
            AttemptKind::Execute,
            "claude",
            "claude-sonnet",
            &claude,
            &auth,
        );
        assert!(attempt.observation.is_none());
        let mut answer = reqwest::Response::from(http::Response::new("hello"));
        response(None, &mut answer);
        assert!(BodyTap::of(&answer).is_none());
    }

    /// Not upstream's: an attempt's secrets are those of its credential
    /// headers after the custom ones, its cookies, its URL, its proxy and its
    /// credential.
    #[test]
    fn gathers_what_an_attempt_sends() {
        let mut auth = Auth::default();
        auth.attributes
            .insert("api_key".to_owned(), "attribute-secret-1234".to_owned());
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            http::HeaderValue::from_static("Bearer override-secret-5678"),
        );
        headers.insert(
            "cookie",
            http::HeaderValue::from_static("sid=cookie-secret-9012"),
        );
        let found = secrets(
            "https://example.test/v1?api_key=query-secret-5678",
            &headers,
            "http://user:proxy-secret-5678@127.0.0.1:1",
            &auth,
        );
        assert_eq!(
            found.iter().collect::<Vec<_>>(),
            [
                "override-secret-5678",
                "cookie-secret-9012",
                "query-secret-5678",
                "proxy-secret-5678",
                "dXNlcjpwcm94eS1zZWNyZXQtNTY3OA==",
                "attribute-secret-1234",
            ]
        );
    }

    /// Not upstream's: a body that fails to read is told to the taps as the
    /// attempt's error after what was read, as upstream's executors record
    /// a failed `io.ReadAll`; an untapped answer tells nobody.
    #[tokio::test]
    async fn tells_a_failed_read() {
        let recorder = Arc::new(Recorder::default());
        let context = Arc::new(RequestContext::new(Method::POST, "/v1/responses".into()));
        let observation = Arc::new(Observation::new(context, vec![recorder.clone()]));
        let mut answer = reqwest::Response::from(http::Response::new("hello"));
        response(Some(BodyTap(observation)), &mut answer);
        let error = crate::codex::client::read_body(answer, 3)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "response body is larger than 3 bytes");
        assert_eq!(
            *recorder.0.lock().unwrap(),
            [
                "head 200",
                "chunk hello",
                "error response body is larger than 3 bytes",
            ]
        );

        let answer = reqwest::Response::from(http::Response::new("hello"));
        assert!(crate::codex::client::read_body(answer, 3).await.is_err());
    }
}
