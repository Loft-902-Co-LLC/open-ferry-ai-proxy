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
//! Deviations from upstream: the whole module. Upstream's executors publish
//! their usage and request-log records themselves, and a context's end
//! tells them the client went away.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

use bytes::Bytes;
use futures_core::Stream;

use super::{Observation, Outcome};
use crate::exec::{ChunkStream, ExecError, Options, StreamResponse};

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
        if let Some(observation) = self.observation.take() {
            match result {
                Ok(_) => observation.finish(Outcome::Completed),
                Err(error) => {
                    observation.error(error);
                    observation.finish(Outcome::Failed);
                }
            }
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
        let Some(observation) = self.observation.take() else {
            return result;
        };
        match result {
            Ok(response) => Ok(StreamResponse {
                headers: response.headers,
                chunks: Box::pin(ObservedChunks {
                    inner: response.chunks,
                    observation: Some(observation),
                }),
            }),
            Err(error) => {
                observation.error(&error);
                observation.finish(Outcome::Failed);
                Err(error)
            }
        }
    }
}

impl Drop for CallReport {
    fn drop(&mut self) {
        if let Some(observation) = self.observation.take() {
            observation.finish(Outcome::Canceled);
        }
    }
}

/// A stream's chunks, reporting how it ends.
struct ObservedChunks {
    inner: ChunkStream,
    /// `None` once the end is reported.
    observation: Option<Arc<Observation>>,
}

impl Stream for ObservedChunks {
    type Item = Result<Bytes, ExecError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        let item = ready!(this.inner.as_mut().poll_next(cx));
        match &item {
            Some(Ok(_)) => {}
            Some(Err(error)) => {
                if let Some(observation) = this.observation.take() {
                    observation.error(error);
                    observation.finish(Outcome::Failed);
                }
            }
            None => {
                if let Some(observation) = this.observation.take() {
                    observation.finish(Outcome::Completed);
                }
            }
        }
        Poll::Ready(item)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl Drop for ObservedChunks {
    fn drop(&mut self) {
        if let Some(observation) = self.observation.take() {
            observation.finish(Outcome::Canceled);
        }
    }
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
