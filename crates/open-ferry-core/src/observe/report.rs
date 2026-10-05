//! How the manager tells a call's taps that an executor call ended: a
//! [`CallReport`] made before the executor is called, which reports the end
//! of a non-streaming call ([`CallReport::finish`]) or of a stream
//! ([`CallReport::stream`]).
//!
//! A call with no taps is left as it is: no report is kept and the stream
//! isn't wrapped. A wrapped stream passes its chunks, its wake-ups and its
//! size hint through unchanged, and dropping it drops the executor's
//! stream, so backpressure and cancellation reach the executor as before.
//!
//! The report exists before the executor is awaited and moves into the
//! stream it returns, so a call dropped at any point, even before the
//! executor answers, tells its taps once that it was canceled.
//!
//! A stream dropped before its end first looks at what its executor already
//! has ready: it takes, without waiting, up to 16 more items, and stops at
//! the first that isn't ready. An error among them ends the call as a
//! failure, as it would have had the client read on; the chunks before it
//! go nowhere. So a client that leaves just after an `apply_patch` failure's
//! `response.failed` frame, before the 502 the executor queued behind it,
//! leaves the 502 recorded. Otherwise the call was canceled.
//!
//! Deviations from upstream: the whole module. Upstream's executors publish
//! their usage and request-log records themselves, and a context's end
//! tells them the client went away. They record an `apply_patch` failure
//! before they send it (`RecordApplyPatchStreamFailure`); here a failure
//! the executor has queued is recorded when the stream is dropped. A client
//! that leaves before the executor has queued it ends the call as canceled,
//! where upstream's reader, if it is waiting on the body at that moment,
//! reads a failed body, finalizes the translator and records the patch 502.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker, ready};

use bytes::Bytes;
use futures_core::Stream;

use super::{Observation, Outcome};
use crate::exec::{ChunkStream, ExecError, Options, StreamResponse};

/// How many items a stream dropped before its end takes, at most, to find
/// an error its executor has already queued. An executor queues an error
/// right behind the chunks made from the same line or from the stream's
/// end: an `apply_patch` failure behind its one `response.failed` frame, a
/// read error behind the translator's closing events and `[DONE]`, a few
/// for each output item still open. Sixteen covers those, and bounds what
/// is read and translated for a client that is gone.
const READY_ITEMS: usize = 16;

/// Reports an executor call's end to its taps. Dropped before it is
/// finished, as when the call's future is dropped, it reports the call
/// canceled.
#[derive(Debug)]
#[must_use = "a report dropped at once says the call was canceled"]
pub struct CallReport {
    observation: Option<Arc<Observation>>,
}

impl CallReport {
    /// A report for a call made with `options`. Without taps it reports
    /// nothing.
    pub fn start(options: &Options) -> Self {
        Self::observing(options.observation.as_ref())
    }

    /// A report to `observation`'s taps. Without taps it reports nothing.
    pub fn observing(observation: Option<&Arc<Observation>>) -> Self {
        Self {
            observation: observation
                .filter(|observation| observation.is_tapped())
                .cloned(),
        }
    }

    /// Reports a non-streaming call's `result`: its error and a failure, or
    /// that it completed.
    pub fn finish<T>(mut self, result: &Result<T, ExecError>) {
        match result {
            Ok(_) => self.end(Outcome::Completed),
            Err(error) => self.fail(error),
        }
    }

    /// Reports a streaming call's `result`, the start of its stream: an
    /// error at once, and otherwise how the stream ends. The report moves
    /// into the stream, which ends it at its first error or its end;
    /// dropped before then, it reports the call canceled. Without taps,
    /// `result` as it is.
    pub fn stream(
        mut self,
        result: Result<StreamResponse, ExecError>,
    ) -> Result<StreamResponse, ExecError> {
        if self.observation.is_none() {
            return result;
        }
        match result {
            Ok(response) => Ok(StreamResponse {
                headers: response.headers,
                chunks: Box::pin(ObservedChunks {
                    inner: response.chunks,
                    report: self,
                }),
            }),
            Err(error) => {
                self.fail(&error);
                Err(error)
            }
        }
    }

    /// Reports the call's failure with `error`, unless its end is reported.
    fn fail(&mut self, error: &ExecError) {
        if let Some(observation) = self.observation.take() {
            observation.error(error);
            observation.finish(Outcome::Failed);
        }
    }

    /// Reports that the call ended with `outcome`, unless its end is
    /// reported.
    fn end(&mut self, outcome: Outcome) {
        if let Some(observation) = self.observation.take() {
            observation.finish(outcome);
        }
    }
}

impl Drop for CallReport {
    fn drop(&mut self) {
        self.end(Outcome::Canceled);
    }
}

/// A stream's chunks, reporting how it ends.
struct ObservedChunks {
    inner: ChunkStream,
    /// Reports the end, once. Dropped before then, as when looking at what
    /// is ready panics, it reports the call canceled.
    report: CallReport,
}

impl Stream for ObservedChunks {
    type Item = Result<Bytes, ExecError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        let item = ready!(this.inner.as_mut().poll_next(cx));
        match &item {
            Some(Ok(_)) => {}
            Some(Err(error)) => this.report.fail(error),
            None => this.report.end(Outcome::Completed),
        }
        Poll::Ready(item)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl Drop for ObservedChunks {
    fn drop(&mut self) {
        // Nothing is polled while a panic unwinds, which a second panic
        // would turn into an abort.
        if self.report.observation.is_some()
            && !std::thread::panicking()
            && let Some(error) = ready_error(&mut self.inner)
        {
            self.report.fail(&error);
        }
        self.report.end(Outcome::Canceled);
    }
}

/// The first error among the next [`READY_ITEMS`] items `chunks` already
/// has, taken without waiting: polled with a waker that does nothing, it
/// stops at an item that isn't ready, and at its end. The chunks before
/// the error go nowhere.
fn ready_error(chunks: &mut ChunkStream) -> Option<ExecError> {
    let mut cx = Context::from_waker(Waker::noop());
    for _ in 0..READY_ITEMS {
        match chunks.as_mut().poll_next(&mut cx) {
            Poll::Ready(Some(Ok(_))) => {}
            Poll::Ready(Some(Err(error))) => return Some(error),
            Poll::Ready(None) | Poll::Pending => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use futures_util::StreamExt;
    use futures_util::stream;
    use http::{HeaderMap, Method};

    use super::*;
    use crate::exec::{ErrorKind, Format};
    use crate::observe::{RequestContext, Tap};

    /// Records what it is told.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl Tap for Recorder {
        fn error(&self, error: &ExecError) {
            self.0
                .lock()
                .unwrap()
                .push(format!("error {}", error.message));
        }

        fn finish(&self, outcome: Outcome) {
            self.0.lock().unwrap().push(format!("{outcome:?}"));
        }
    }

    impl Recorder {
        fn take(&self) -> Vec<String> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }
    }

    fn tapped() -> (Options, Arc<Recorder>) {
        let recorder = Arc::new(Recorder::default());
        let context = Arc::new(RequestContext::new(Method::POST, "/v1/x".to_owned()));
        let mut options = Options::new(Format::from_static("openai"));
        options.observation = Some(Arc::new(Observation::new(
            context,
            vec![Arc::clone(&recorder) as Arc<dyn Tap>],
        )));
        (options, recorder)
    }

    fn failure() -> ExecError {
        ExecError::new(ErrorKind::Upstream, "boom")
    }

    fn chunks(items: Vec<Result<Bytes, ExecError>>) -> StreamResponse {
        StreamResponse {
            headers: HeaderMap::new(),
            chunks: stream::iter(items).boxed(),
        }
    }

    // Not upstream's: a call's end reaches its taps once.
    #[test]
    fn reports_how_a_call_ended() {
        let (options, recorder) = tapped();
        CallReport::start(&options).finish(&Ok::<(), ExecError>(()));
        assert_eq!(recorder.take(), ["Completed"]);
        CallReport::start(&options).finish(&Err::<(), _>(failure()));
        assert_eq!(recorder.take(), ["error boom", "Failed"]);
        drop(CallReport::start(&options));
        assert_eq!(recorder.take(), ["Canceled"]);
    }

    // Not upstream's: a call dropped before its executor answers, whether
    // it makes a unary call or opens a stream, reports itself canceled once;
    // one that answered leaves the report to the stream, which reports once.
    #[tokio::test]
    async fn reports_a_call_dropped_before_its_executor_answers() {
        use futures_util::FutureExt as _;

        let (options, recorder) = tapped();
        let report = CallReport::start(&options);
        let mut opening = Box::pin(async move {
            std::future::pending::<()>().await;
            report.stream(Err(failure()))
        });
        assert!(opening.as_mut().now_or_never().is_none());
        assert!(recorder.take().is_empty());
        drop(opening);
        assert_eq!(recorder.take(), ["Canceled"]);

        let report = CallReport::start(&options);
        let response = report.stream(Ok(chunks(vec![]))).unwrap();
        assert!(recorder.take().is_empty());
        drop(response);
        assert_eq!(recorder.take(), ["Canceled"]);
    }

    // Not upstream's: a stream reports its first error, its end, or being
    // dropped, and passes its chunks through.
    #[tokio::test]
    async fn reports_how_a_stream_ended() {
        let (options, recorder) = tapped();
        let error = CallReport::start(&options)
            .stream(Err(failure()))
            .unwrap_err();
        assert_eq!(error.message, "boom");
        assert_eq!(recorder.take(), ["error boom", "Failed"]);

        let ok = || Ok(Bytes::from_static(b"a"));
        let mut response = CallReport::start(&options)
            .stream(Ok(chunks(vec![ok(), ok()])))
            .unwrap();
        assert_eq!(response.chunks.size_hint(), (2, Some(2)));
        assert!(response.chunks.next().await.unwrap().is_ok());
        assert!(recorder.take().is_empty());
        assert!(response.chunks.next().await.unwrap().is_ok());
        assert!(response.chunks.next().await.is_none());
        drop(response);
        assert_eq!(recorder.take(), ["Completed"]);

        let mut response = CallReport::start(&options)
            .stream(Ok(chunks(vec![ok(), Err(failure())])))
            .unwrap();
        while response.chunks.next().await.is_some() {}
        drop(response);
        assert_eq!(recorder.take(), ["error boom", "Failed"]);

        let mut response = CallReport::start(&options)
            .stream(Ok(chunks(vec![ok(), ok()])))
            .unwrap();
        assert!(response.chunks.next().await.is_some());
        drop(response);
        assert_eq!(recorder.take(), ["Canceled"]);
    }

    fn ok() -> Result<Bytes, ExecError> {
        Ok(Bytes::from_static(b"a"))
    }

    // Not upstream's: a stream dropped with an error its executor already
    // queued, as the 502 behind an `apply_patch` failure's frame, reports
    // that error; one further back than READY_ITEMS is not looked at.
    #[tokio::test]
    async fn reports_an_error_ready_when_a_stream_is_dropped() {
        let (options, recorder) = tapped();
        let mut response = CallReport::start(&options)
            .stream(Ok(chunks(vec![ok(), Err(failure())])))
            .unwrap();
        assert!(response.chunks.next().await.unwrap().is_ok());
        assert!(recorder.take().is_empty());
        drop(response);
        assert_eq!(recorder.take(), ["error boom", "Failed"]);

        let mut items = vec![ok(); READY_ITEMS];
        items[READY_ITEMS - 1] = Err(failure());
        let response = CallReport::start(&options)
            .stream(Ok(chunks(items)))
            .unwrap();
        drop(response);
        assert_eq!(recorder.take(), ["error boom", "Failed"]);

        let mut items = vec![ok(); READY_ITEMS + 1];
        items[READY_ITEMS] = Err(failure());
        let response = CallReport::start(&options)
            .stream(Ok(chunks(items)))
            .unwrap();
        drop(response);
        assert_eq!(recorder.take(), ["Canceled"]);
    }

    // Not upstream's: a stream dropped while its executor is still waiting
    // was canceled, whatever would have come next; nothing waits for it.
    #[tokio::test]
    async fn a_stream_dropped_while_waiting_was_canceled() {
        let (options, recorder) = tapped();
        let mut response = CallReport::start(&options)
            .stream(Ok(StreamResponse {
                headers: HeaderMap::new(),
                chunks: stream::iter([ok()]).chain(stream::pending()).boxed(),
            }))
            .unwrap();
        assert!(response.chunks.next().await.unwrap().is_ok());
        drop(response);
        assert_eq!(recorder.take(), ["Canceled"]);

        // An error behind an item that isn't ready yet isn't reached.
        let mut polls = 0;
        let mut response = CallReport::start(&options)
            .stream(Ok(StreamResponse {
                headers: HeaderMap::new(),
                chunks: stream::poll_fn(move |_| {
                    polls += 1;
                    match polls {
                        1 => Poll::Ready(Some(ok())),
                        2 => Poll::Pending,
                        _ => Poll::Ready(Some(Err(failure()))),
                    }
                })
                .boxed(),
            }))
            .unwrap();
        assert!(response.chunks.next().await.unwrap().is_ok());
        drop(response);
        assert_eq!(recorder.take(), ["Canceled"]);
    }

    /// A stream with one chunk ready, that panics when polled after it.
    fn panicking() -> StreamResponse {
        let mut polled = false;
        StreamResponse {
            headers: HeaderMap::new(),
            chunks: stream::poll_fn(move |_| {
                assert!(!std::mem::replace(&mut polled, true), "polled again");
                Poll::Ready(Some(ok()))
            })
            .boxed(),
        }
    }

    // Not upstream's: a stream that panics when looked at on its drop still
    // reports the call canceled, and one dropped while a panic unwinds isn't
    // looked at, which would abort.
    #[tokio::test]
    async fn a_dropped_stream_that_panics_was_canceled() {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        let (options, recorder) = tapped();
        let mut response = CallReport::start(&options).stream(Ok(panicking())).unwrap();
        assert!(response.chunks.next().await.unwrap().is_ok());
        assert!(catch_unwind(AssertUnwindSafe(move || drop(response))).is_err());
        assert_eq!(recorder.take(), ["Canceled"]);

        let mut response = CallReport::start(&options).stream(Ok(panicking())).unwrap();
        assert!(response.chunks.next().await.unwrap().is_ok());
        let unwound = catch_unwind(AssertUnwindSafe(move || {
            let _response = response;
            panic!("leaving");
        }));
        assert!(unwound.is_err());
        assert_eq!(recorder.take(), ["Canceled"]);
    }

    // Not upstream's: without taps nothing is wrapped or reported.
    #[test]
    fn untapped_calls_are_left_alone() {
        let mut options = Options::new(Format::from_static("openai"));
        assert!(CallReport::start(&options).observation.is_none());
        options.observation = Some(Arc::new(Observation::new(
            Arc::new(RequestContext::new(Method::GET, "/".to_owned())),
            Vec::new(),
        )));
        assert!(CallReport::start(&options).observation.is_none());
        assert!(CallReport::start(&options).stream(Err(failure())).is_err());
    }
}
