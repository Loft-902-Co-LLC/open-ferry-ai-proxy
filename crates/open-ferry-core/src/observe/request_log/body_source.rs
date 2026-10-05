// Ported from CLIProxyAPI internal/api/middleware/request_logging.go
// (deferredRequestBodyCapture, its Read and statusMarker) and
// internal/logging/request_logger_body_source.go (FileBodySource's role)
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! What the request log keeps of the bodies that pass by: the part of the
//! client's body a handler reads, kept for an error log, and the answer's
//! body.
//!
//! Deviations from upstream: the bodies are kept in memory up to
//! [`CAPTURE_LIMIT`] bytes each, where upstream spools them to temporary
//! files in the log directory. What an answer's body has past the limit is
//! counted and left out, and the log says so in one line.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;

/// The most bytes of one body the request log keeps (upstream's
/// `maxDeferredErrorRequestBodyBytes`).
pub const CAPTURE_LIMIT: usize = 32 << 20;

/// The part of the client's body a handler reads, kept for the log of a
/// request that fails, as the handler reads it (upstream's
/// `deferredRequestBodyCapture`). The server's body wrapper feeds it.
/// Cloning gives another handle to the same capture.
#[derive(Clone)]
pub struct DeferredCapture {
    inner: Arc<Mutex<Deferred>>,
}

struct Deferred {
    captured: Vec<u8>,
    content_length: Option<u64>,
    bytes_read: u64,
    saw_eof: bool,
    truncated: bool,
}

impl DeferredCapture {
    /// A capture for a body of `content_length` bytes, `None` when unknown.
    pub fn new(content_length: Option<u64>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Deferred {
                captured: Vec::new(),
                content_length,
                bytes_read: 0,
                saw_eof: false,
                truncated: false,
            })),
        }
    }

    /// Keeps what fits of `chunk`, the next part of the body the handler
    /// read.
    pub fn record(&self, chunk: &[u8]) {
        let mut deferred = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        deferred.bytes_read = deferred.bytes_read.saturating_add(chunk.len() as u64);
        let room = CAPTURE_LIMIT.saturating_sub(deferred.captured.len());
        if chunk.len() > room {
            deferred.truncated = true;
        }
        let kept = chunk.get(..room.min(chunk.len())).unwrap_or_default();
        deferred.captured.extend_from_slice(kept);
    }

    /// Notes that the handler read the body to its end.
    pub fn end(&self) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .saw_eof = true;
    }

    /// What was kept, and the markers saying what is missing (upstream's
    /// `Bytes` and `statusMarker`).
    pub(crate) fn snapshot(&self) -> (Vec<u8>, String) {
        let deferred = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let mut markers = Vec::new();
        if deferred.truncated {
            markers.push(format!(
                "[REQUEST BODY TRUNCATED: captured first {} bytes]",
                deferred.captured.len()
            ));
        }
        let complete = deferred.saw_eof
            || deferred
                .content_length
                .is_some_and(|length| deferred.bytes_read >= length);
        if !complete {
            markers.push(match deferred.content_length {
                Some(length) => format!(
                    "[REQUEST BODY CAPTURE INCOMPLETE: consumed {} of {length} bytes]",
                    deferred.bytes_read
                ),
                None => format!(
                    "[REQUEST BODY CAPTURE INCOMPLETE: consumed {} bytes from an unknown-length body]",
                    deferred.bytes_read
                ),
            });
        }
        (deferred.captured.clone(), markers.join("\n"))
    }
}

impl fmt::Debug for DeferredCapture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let deferred = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        f.debug_struct("DeferredCapture")
            .field("captured", &deferred.captured.len())
            .field("content_length", &deferred.content_length)
            .field("bytes_read", &deferred.bytes_read)
            .field("saw_eof", &deferred.saw_eof)
            .finish_non_exhaustive()
    }
}

/// The answer's body as it was sent, up to [`CAPTURE_LIMIT`] bytes.
#[derive(Default)]
pub struct ResponseCapture {
    chunks: Vec<Bytes>,
    len: usize,
    dropped: usize,
}

impl ResponseCapture {
    /// Keeps what fits of `chunk`, the next part of the body sent.
    pub fn push(&mut self, chunk: &Bytes) {
        let room = CAPTURE_LIMIT.saturating_sub(self.len);
        let kept = chunk.len().min(room);
        if kept > 0 {
            self.chunks.push(chunk.slice(..kept));
            self.len += kept;
        }
        self.dropped = self.dropped.saturating_add(chunk.len() - kept);
    }

    /// The body kept, in one piece.
    pub(crate) fn concat(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.len);
        for chunk in &self.chunks {
            out.extend_from_slice(chunk);
        }
        out
    }

    /// How many bytes were left out.
    pub(crate) fn dropped(&self) -> usize {
        self.dropped
    }
}

impl fmt::Debug for ResponseCapture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResponseCapture")
            .field("len", &self.len)
            .field("dropped", &self.dropped)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: a body read to its end has no marker; one read in
    // part says how much was read.
    #[test]
    fn marks_incomplete_captures() {
        let capture = DeferredCapture::new(Some(10));
        capture.record(b"hello");
        assert_eq!(
            capture.snapshot(),
            (
                b"hello".to_vec(),
                "[REQUEST BODY CAPTURE INCOMPLETE: consumed 5 of 10 bytes]".to_owned()
            )
        );
        capture.record(b"world");
        assert_eq!(capture.snapshot().1, "");

        let unknown = DeferredCapture::new(None);
        unknown.record(b"abc");
        assert_eq!(
            unknown.snapshot().1,
            "[REQUEST BODY CAPTURE INCOMPLETE: consumed 3 bytes from an unknown-length body]"
        );
        unknown.end();
        assert_eq!(unknown.snapshot().1, "");
    }

    // Not upstream's: a body past the limit is cut, and says so.
    #[test]
    fn truncates_large_captures() {
        let capture = DeferredCapture::new(None);
        let chunk = vec![b'x'; CAPTURE_LIMIT - 1];
        capture.record(&chunk);
        capture.record(b"yz");
        capture.end();
        let (body, marker) = capture.snapshot();
        assert_eq!(body.len(), CAPTURE_LIMIT);
        assert_eq!(
            marker,
            format!("[REQUEST BODY TRUNCATED: captured first {CAPTURE_LIMIT} bytes]")
        );

        let mut response = ResponseCapture::default();
        response.push(&Bytes::from(chunk));
        response.push(&Bytes::from_static(b"abc"));
        assert_eq!(response.concat().len(), CAPTURE_LIMIT);
        assert_eq!(response.dropped(), 2);
    }
}
