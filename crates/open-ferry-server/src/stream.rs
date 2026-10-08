// Ported from CLIProxyAPI sdk/api/handlers/stream_forwarder.go and
// StartNonStreamingKeepAlive in sdk/api/handlers/handlers.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Writing results to clients: streams, with keep-alives and a terminal
//! error, and non-streaming results that take a while.
//!
//! A stream's bytes go out as its body gives them; nothing here writes or
//! flushes them. When a write to the client fails, the server drops the
//! body, which drops the call's stream and so cancels the call at once, as
//! upstream's handlers cancel it on a failed write or flush. A stream that
//! goes well ends with its provider's stream, not at its completion event:
//! upstream ends a Responses stream at `response.completed` only for a
//! provider that acknowledges delivery, which only its Antigravity executor
//! does.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use axum::body::{Body, BodyDataStream};
use axum::response::Response;
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use futures_util::stream;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use tokio::time::{Instant, Interval, MissedTickBehavior};

use crate::errors::ErrorMessage;
use crate::exec::HandlerStream;
use crate::headers::write_upstream_headers;

/// How an endpoint writes a stream (upstream's `StreamForwardOptions`).
pub(crate) trait StreamWriter: Send + 'static {
    /// Writes a payload.
    fn write_chunk(&mut self, chunk: Bytes, out: &mut BytesMut);

    /// After a payload, an error that ends the stream with nothing more
    /// written.
    fn chunk_error(&mut self) -> Option<ErrorMessage> {
        None
    }

    /// Rewrites an error before it is written.
    fn normalize_terminal_error(&mut self, error: ErrorMessage) -> ErrorMessage {
        error
    }

    /// Writes the error that ends the stream.
    fn write_terminal_error(&mut self, error: &ErrorMessage, out: &mut BytesMut);

    /// When the payloads end, an error to end the stream with instead of
    /// [`StreamWriter::write_done`].
    fn close_error(&mut self) -> Option<ErrorMessage> {
        None
    }

    /// Writes the end of a stream that went well.
    fn write_done(&mut self, _out: &mut BytesMut) {}

    /// Writes a keep-alive.
    fn write_keep_alive(&mut self, out: &mut BytesMut) {
        out.extend_from_slice(b": keep-alive\n\n");
    }
}

/// The body that writes `items` with `writer`, and a keep-alive every
/// `keepalive` (upstream's `ForwardStream`). Dropping the body drops the
/// stream, which cancels the call.
pub(crate) fn forward<W: StreamWriter>(
    items: HandlerStream,
    writer: W,
    keepalive: Option<Duration>,
) -> Body {
    let ticker = keepalive.filter(|d| !d.is_zero()).map(ticker);
    let pump = Pump {
        items,
        writer,
        ticker,
        done: false,
    };
    Body::from_stream(stream::unfold(pump, |mut pump| async move {
        let out = pump.next().await?;
        Some((Ok::<_, Infallible>(out), pump))
    }))
}

struct Pump<W> {
    items: HandlerStream,
    writer: W,
    ticker: Option<Interval>,
    done: bool,
}

impl<W: StreamWriter> Pump<W> {
    /// The next bytes to write, or `None` at the end.
    async fn next(&mut self) -> Option<Bytes> {
        let mut out = BytesMut::new();
        while out.is_empty() && !self.done {
            tokio::select! {
                biased;
                item = self.items.next() => match item {
                    Some(Ok(chunk)) => {
                        self.writer.write_chunk(chunk, &mut out);
                        if let Some(error) = self.writer.chunk_error() {
                            let error = self.writer.normalize_terminal_error(error);
                            tracing::debug!(status = error.status, "stream stopped: {}", error.text);
                            self.stop();
                        }
                    }
                    Some(Err(error)) => {
                        let error = self.writer.normalize_terminal_error(error);
                        self.writer.write_terminal_error(&error, &mut out);
                        self.stop();
                    }
                    None => {
                        match self.writer.close_error() {
                            Some(error) => self.writer.write_terminal_error(&error, &mut out),
                            None => self.writer.write_done(&mut out),
                        }
                        self.stop();
                    }
                },
                () = tick(&mut self.ticker) => self.writer.write_keep_alive(&mut out),
            }
        }
        (!out.is_empty()).then(|| out.freeze())
    }

    /// Ends the stream, dropping the payloads now, which cancels the call,
    /// rather than once the client has read what is left.
    fn stop(&mut self) {
        self.done = true;
        self.items = stream::empty().boxed();
    }
}

/// A ticker whose first tick comes after `period`, like Go's
/// `time.NewTicker`.
pub(crate) fn ticker(period: Duration) -> Interval {
    let mut ticker = tokio::time::interval_at(Instant::now() + period, period);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    ticker
}

/// The ticker's next tick, or never.
pub(crate) async fn tick(ticker: &mut Option<Interval>) {
    match ticker {
        Some(ticker) => {
            ticker.tick().await;
        }
        None => std::future::pending().await,
    }
}

/// How a stream started.
pub(crate) enum Peeked {
    /// It failed before its first payload.
    Failed(ErrorMessage),
    /// It ended with nothing.
    Closed,
    /// Its first payload, and the rest.
    First(Bytes, HandlerStream),
}

/// Reads a stream's first item.
pub(crate) async fn peek(mut items: HandlerStream) -> Peeked {
    match items.next().await {
        None => Peeked::Closed,
        Some(Err(error)) => Peeked::Failed(error),
        Some(Ok(first)) => Peeked::First(first, items),
    }
}

/// A 200 event stream: the SSE headers, then the provider's headers that
/// don't clash, then `body`.
pub(crate) fn sse_response(upstream: &HeaderMap, body: Body) -> Response {
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    write_upstream_headers(headers, upstream);
    response
}

/// A 200 JSON response: `body`, with the provider's headers.
pub(crate) fn json_response(upstream: &HeaderMap, body: Bytes) -> Response {
    let mut response = Response::new(Body::from(body));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    write_upstream_headers(headers, upstream);
    response
}

/// Runs `call` and answers with `respond(output)`. If the call takes longer
/// than `interval`, the response starts as a 200 JSON stream instead: a
/// newline every `interval` while the call runs, then the body `respond`
/// gives, whatever its status and headers (upstream's
/// `StartNonStreamingKeepAlive`, which writes to a response that has
/// started).
pub(crate) async fn keep_alive<T, F, R>(interval: Option<Duration>, call: F, respond: R) -> Response
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
    R: FnOnce(T) -> Response + Send + 'static,
{
    let Some(interval) = interval.filter(|d| !d.is_zero()) else {
        return respond(call.await);
    };
    let mut call = Box::pin(call);
    let mut ticker = ticker(interval);
    tokio::select! {
        biased;
        output = &mut call => return respond(output),
        _ = ticker.tick() => {}
    }

    let phase = Phase::Waiting {
        call,
        ticker,
        respond,
        tick_due: true,
    };
    let body = stream::unfold(phase, |phase| async move {
        match phase {
            Phase::Waiting {
                mut call,
                mut ticker,
                respond,
                tick_due,
            } => {
                if !tick_due {
                    tokio::select! {
                        biased;
                        output = &mut call => {
                            let mut body = respond(output).into_body().into_data_stream();
                            let item = body.next().await?;
                            return Some((item, Phase::Body(body)));
                        }
                        _ = ticker.tick() => {}
                    }
                }
                let next = Phase::Waiting {
                    call,
                    ticker,
                    respond,
                    tick_due: false,
                };
                Some((Ok(Bytes::from_static(b"\n")), next))
            }
            Phase::Body(mut body) => {
                let item = body.next().await?;
                Some((item, Phase::Body(body)))
            }
        }
    });
    let mut response = Response::new(Body::from_stream(body));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

/// Where a keep-alive response is.
enum Phase<F, R> {
    /// The call is running. `tick_due` says a tick has come that hasn't
    /// been written.
    Waiting {
        call: Pin<Box<F>>,
        ticker: Interval,
        respond: R,
        tick_due: bool,
    },
    /// The call is done, and its body is being written.
    Body(BodyDataStream),
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    struct Lines;

    impl StreamWriter for Lines {
        fn write_chunk(&mut self, chunk: Bytes, out: &mut BytesMut) {
            out.extend_from_slice(b"data: ");
            out.extend_from_slice(&chunk);
            out.extend_from_slice(b"\n\n");
        }

        fn write_terminal_error(&mut self, error: &ErrorMessage, out: &mut BytesMut) {
            out.extend_from_slice(format!("error: {}\n\n", error.text).as_bytes());
        }

        fn write_done(&mut self, out: &mut BytesMut) {
            out.extend_from_slice(b"data: [DONE]\n\n");
        }
    }

    async fn collect(body: Body) -> String {
        String::from_utf8(body.collect().await.unwrap().to_bytes().to_vec()).unwrap()
    }

    #[tokio::test]
    async fn forwards_chunks_then_done_or_an_error() {
        let items =
            stream::iter([Ok(Bytes::from_static(b"1")), Ok(Bytes::from_static(b"2"))]).boxed();
        assert_eq!(
            collect(forward(items, Lines, None)).await,
            "data: 1\n\ndata: 2\n\ndata: [DONE]\n\n"
        );
        let items = stream::iter([
            Ok(Bytes::from_static(b"1")),
            Err(ErrorMessage::new(502, "broken")),
            Ok(Bytes::from_static(b"never")),
        ])
        .boxed();
        assert_eq!(
            collect(forward(items, Lines, None)).await,
            "data: 1\n\nerror: broken\n\n"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn writes_keep_alives_while_waiting() {
        let items = stream::once(async {
            tokio::time::sleep(Duration::from_millis(2500)).await;
            Ok(Bytes::from_static(b"late"))
        })
        .boxed();
        assert_eq!(
            collect(forward(items, Lines, Some(Duration::from_secs(1)))).await,
            ": keep-alive\n\n: keep-alive\n\ndata: late\n\ndata: [DONE]\n\n"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn keep_alive_starts_the_response_when_a_call_is_slow() {
        let respond = |body: &'static str| {
            let mut response = Response::new(Body::from(body));
            *response.status_mut() = StatusCode::BAD_GATEWAY;
            response
        };
        let quick = keep_alive(Some(Duration::from_secs(1)), async { "quick" }, respond).await;
        assert_eq!(quick.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(collect(quick.into_body()).await, "quick");

        let slow = async {
            tokio::time::sleep(Duration::from_millis(2500)).await;
            "slow"
        };
        let started = keep_alive(Some(Duration::from_secs(1)), slow, respond).await;
        assert_eq!(started.status(), StatusCode::OK);
        assert_eq!(started.headers()[header::CONTENT_TYPE], "application/json");
        assert_eq!(collect(started.into_body()).await, "\n\nslow");

        let off = keep_alive(None, async { "off" }, respond).await;
        assert_eq!(off.status(), StatusCode::BAD_GATEWAY);
    }
}
