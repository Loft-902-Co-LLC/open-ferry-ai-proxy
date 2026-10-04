//! What a call's taps see of its upstream attempts: each request as it is
//! sent, with the credential's headers set, then the answer's head, and its
//! body as it is read (the `RecordAPIRequest`, `RecordAPIResponseMetadata`
//! and `AppendAPIResponseChunk` calls of upstream's executors).
//!
//! A send site is given its call's [`Attempt`]. When a tap sees the call,
//! it tells them the request with [`announce`] before it sends; once the
//! answer comes, it hands it to [`response`], which tells the taps its head
//! and marks it, so the body readers give each chunk they read to the
//! [`BodyTap`] they find on it. Without taps, each step is one branch, and
//! the answer isn't marked. OAuth and token calls aren't tapped, as
//! upstream doesn't record them.
//!
//! Deviations from upstream: the whole module (see
//! [`open_ferry_core::observe`]).

use std::sync::Arc;

use bytes::Bytes;
use http::{HeaderMap, Method};
use open_ferry_core::auth::Auth;
use open_ferry_core::exec::{Format, Options};
use open_ferry_core::observe::{AttemptKind, AttemptRequest, Observation};

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
    /// sending `secrets` somewhere in them.
    pub(crate) fn request<'b>(
        &'b self,
        method: &'b Method,
        url: &'b str,
        headers: &'b HeaderMap,
        body: &'b Bytes,
        secrets: &'b [&'b str],
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

    /// Gives the taps the next part of the body: a chunk off the wire, or
    /// a WebSocket message.
    pub(crate) fn chunk(&self, chunk: &Bytes) {
        self.0.chunk(chunk);
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
                request.url, request.model, request.secrets[0]
            ));
        }

        fn response_head(&self, status: u16, _headers: &HeaderMap) {
            self.push(format!("head {status}"));
        }

        fn chunk(&self, chunk: &Bytes) {
            self.push(format!("chunk {}", String::from_utf8_lossy(chunk)));
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
                    &["sk-secret"],
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
}
