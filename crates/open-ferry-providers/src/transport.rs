//! Preserve transport types before an executor flattens an error for the manager.

use std::error::Error;
use std::io::ErrorKind;

use open_ferry_core::exec::{ExecError, TransportFault};

#[cfg(test)]
pub(crate) mod tests;

/// The response facts that survive consuming its body, including in a stream.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ResponseBody {
    version: reqwest::Version,
    success: bool,
}

impl ResponseBody {
    pub(crate) fn new(response: &reqwest::Response) -> Self {
        Self {
            version: response.version(),
            success: response.status().is_success(),
        }
    }

    /// A newly recognized failure after acceptance must not replay the request.
    /// The manager retains any retry that the original error text already allowed.
    pub(crate) fn classify(self, error: &(dyn Error + 'static), failure: ExecError) -> ExecError {
        classify_inner(error, failure, Some(self))
    }
}

/// Preserve closed-channel and socket types before a response has arrived.
pub(crate) fn classify(error: &(dyn Error + 'static), failure: ExecError) -> ExecError {
    classify_inner(error, failure, None)
}

/// h2 may discard both a preceding GOAWAY and its error wrapper on the trailer
/// path. Only a positively identified HTTP/1 response gets a new EOF marker.
fn classify_inner(
    error: &(dyn Error + 'static),
    mut failure: ExecError,
    body: Option<ResponseBody>,
) -> ExecError {
    if failure.status != 0 || body.is_some_and(|body| !body.success) {
        return failure;
    }
    let mut current = Some(error);
    let mut closed = false;
    let mut http2 = false;
    let mut eof = false;
    let mut transient = false;
    while let Some(error) = current {
        if error
            .downcast_ref::<hyper::Error>()
            .is_some_and(|error| error.is_closed() || error.is_incomplete_message())
        {
            closed = true;
            break;
        }
        // h2 body errors expose their I/O cause through get_io(), not source().
        let h2 = error.downcast_ref::<h2::Error>();
        http2 |= h2.is_some();
        let io_error = error
            .downcast_ref::<std::io::Error>()
            .or_else(|| h2.and_then(h2::Error::get_io));
        if let Some(error) = io_error {
            match error.kind() {
                ErrorKind::UnexpectedEof => eof = true,
                ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::BrokenPipe => transient = true,
                _ => {}
            }
        }
        current = error.source();
    }
    let http1_body = body.is_some_and(|body| {
        matches!(
            body.version,
            reqwest::Version::HTTP_09 | reqwest::Version::HTTP_10 | reqwest::Version::HTTP_11
        )
    });
    let fault = if closed || (eof && http1_body && !http2) {
        Some(TransportFault::Lifecycle)
    } else if transient {
        Some(TransportFault::Transient)
    } else {
        None
    };
    if let Some(fault) = fault {
        failure.transport = Some(if body.is_some() {
            TransportFault::ResponseBody
        } else {
            fault
        });
    }
    failure
}

#[cfg(test)]
mod accepted_body_tests {
    use super::*;

    fn context(version: reqwest::Version) -> ResponseBody {
        ResponseBody {
            version,
            success: true,
        }
    }

    #[test]
    fn every_promoted_body_kind_has_the_non_replaying_marker() {
        for kind in [
            ErrorKind::UnexpectedEof,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::BrokenPipe,
        ] {
            let error = std::io::Error::new(kind, "local body failure");
            let failure = context(reqwest::Version::HTTP_11)
                .classify(&error, ExecError::upstream(0, error.to_string()));
            assert_eq!(failure.transport, Some(TransportFault::ResponseBody));
            let before = classify(&error, ExecError::upstream(0, error.to_string()));
            let want = (kind != ErrorKind::UnexpectedEof).then_some(TransportFault::Transient);
            assert_eq!(before.transport, want);
        }
    }

    #[test]
    fn bare_eof_requires_a_positive_http1_response_version() {
        let error = std::io::Error::new(ErrorKind::UnexpectedEof, "local TLS body EOF");
        // This also covers hyper's HTTP/2 trailer path, which drops h2::Error.
        for version in [reqwest::Version::HTTP_2, reqwest::Version::HTTP_3] {
            assert_eq!(
                context(version)
                    .classify(&error, ExecError::upstream(0, error.to_string()))
                    .transport,
                None
            );
        }
        assert_eq!(
            context(reqwest::Version::HTTP_11)
                .classify(&error, ExecError::upstream(0, error.to_string()))
                .transport,
            Some(TransportFault::ResponseBody)
        );
    }

    #[test]
    fn non_success_responses_and_status_errors_keep_their_classification() {
        let error = std::io::Error::new(ErrorKind::ConnectionReset, "local body failure");
        let mut body = context(reqwest::Version::HTTP_11);
        body.success = false;
        assert_eq!(
            body.classify(&error, ExecError::upstream(0, error.to_string()))
                .transport,
            None
        );
        body.success = true;
        assert_eq!(
            body.classify(&error, ExecError::upstream(429, error.to_string()))
                .transport,
            None
        );
    }
}
