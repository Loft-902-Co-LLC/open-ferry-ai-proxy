//! Preserve transport types before an executor flattens an error for the manager.

use std::error::Error;
use std::io::ErrorKind;

use open_ferry_core::exec::{ExecError, TransportFault};

#[cfg(test)]
pub(crate) mod tests;

/// Keep unambiguous EOF and socket failures independent of hyper's or Windows'
/// wording. The manager still applies its existing text checks.
///
/// h2 loses a preceding GOAWAY when it reports an I/O error on an active stream.
/// Go remembers it and doesn't classify that EOF as a lifecycle drop. Therefore
/// only response-body decode errors without an h2 cause get the new EOF marker;
/// response-future EOFs and HTTP/2 body EOFs keep their existing classification.
pub(crate) fn classify(error: &(dyn Error + 'static), mut failure: ExecError) -> ExecError {
    if failure.status != 0 {
        return failure;
    }
    let mut current = Some(error);
    let mut response_body = false;
    let mut http2 = false;
    let mut eof = false;
    let mut transient = false;
    while let Some(error) = current {
        if error
            .downcast_ref::<hyper::Error>()
            .is_some_and(|error| error.is_closed() || error.is_incomplete_message())
        {
            return failure.with_transport(TransportFault::Lifecycle);
        }
        if let Some(error) = error.downcast_ref::<reqwest::Error>() {
            response_body |= error.is_decode();
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
    if eof && response_body && !http2 {
        failure.transport = Some(TransportFault::Lifecycle);
    } else if transient {
        failure.transport = Some(TransportFault::Transient);
    }
    failure
}
