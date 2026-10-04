//! Not upstream's: a stream the manager forwards is read by a task of its
//! own, which keeps the span of the call that started the stream, so what
//! the provider's stream logs shows that request's ID.

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures_core::future::BoxFuture;
use futures_util::StreamExt;
use tracing::Instrument as _;

use super::support::*;
use crate::auth::Auth;
use crate::exec::{Dispatcher, ExecError, Options, Request, Response, StreamResponse};
use crate::executor::ProviderExecutor;
use crate::manager::{Settings, lock};

const MODEL: &str = "stream-span-model";

/// An executor whose streams note the span each chunk is read in.
struct Probe {
    inner: Arc<FakeExecutor>,
    seen: Arc<Mutex<Vec<Option<tracing::Id>>>>,
}

impl ProviderExecutor for Probe {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn execute(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        self.inner.execute(auth, request, options)
    }

    fn execute_stream(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<StreamResponse, ExecError>> {
        let seen = Arc::clone(&self.seen);
        let started = self.inner.execute_stream(auth, request, options);
        Box::pin(async move {
            let mut response = started.await?;
            response.chunks = response
                .chunks
                .inspect(move |_| lock(&seen).push(tracing::Span::current().id()))
                .boxed();
            Ok(response)
        })
    }

    fn count_tokens(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        self.inner.count_tokens(auth, request, options)
    }

    fn refresh(&self, auth: Arc<Auth>) -> BoxFuture<'_, Result<Auth, ExecError>> {
        self.inner.refresh(auth)
    }
}

#[tokio::test(start_paused = true)]
async fn forwarded_stream_is_read_in_the_requests_span() {
    let _subscriber = tracing::subscriber::set_default(tracing_subscriber::registry());
    let h = Harness::new(Settings::default());
    h.add(auth("stream-span-auth", "codex"), &[MODEL]);
    let seen = Arc::new(Mutex::new(Vec::new()));
    h.manager.register_executor(Arc::new(Probe {
        inner: FakeExecutor::with("codex", |_: &Call| {
            Reply::chunks(vec![
                Ok(Bytes::from_static(b"one")),
                Ok(Bytes::from_static(b"two")),
                Ok(Bytes::from_static(b"three")),
            ])
        }),
        seen: Arc::clone(&seen),
    }));

    let span = tracing::info_span!("request");
    let id = span.id();
    assert!(id.is_some());
    let mut opts = options();
    opts.stream = true;
    // The caller is in the span while the call starts, as a handler is,
    // and reads the stream outside it, as the response body is.
    let stream = h
        .manager
        .execute_stream(&providers(&["codex"]), request(MODEL), opts)
        .instrument(span)
        .await
        .unwrap_or_else(|err| panic!("execute_stream() error = {err}"));
    let (chunks, end) = collect(stream).await;
    settle().await;

    assert!(end.is_none(), "{end:?}");
    assert_eq!(chunks.concat(), "onetwothree");
    let seen = lock(&seen).clone();
    assert_eq!(seen.len(), 3);
    assert!(seen.iter().all(|read_in| *read_in == id), "{seen:?}");
}
