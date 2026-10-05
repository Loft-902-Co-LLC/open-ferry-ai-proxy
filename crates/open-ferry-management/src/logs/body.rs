// Ported from CLIProxyAPI internal/api/handlers/management/logs.go
// (writeLogsResponse, logAccumulator.addLine, logAccumulator.append)
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The body of `GET /v0/management/logs` (upstream's `writeLogsResponse`):
//! gin's `c.JSON` of the page, its keys sorted, its lines read again from
//! the handles the first read kept. The body is written on the blocking
//! pool and sent in chunks of about [`CHUNK`] bytes, at most [`QUEUE`] of
//! them waiting to be sent; an answer that fits one chunk is sent whole,
//! with its length.
//!
//! Deviations from upstream:
//! - The lines aren't gathered in memory, where upstream holds every line
//!   it answers (all of every file without `limit` or `after`) before it
//!   writes any: the files are read twice, through the same handles, the
//!   second time only those holding the answer's lines, which alone stay
//!   open, and only a few chunks of the body, and the line being written,
//!   are held. A file changed between the reads gives the lines it then
//!   has in the range the first read found, never more lines than it
//!   counted; `line-count`, `latest-timestamp` and `next-cursor` are the
//!   first read's.
//! - A read that fails once the body has started ends it early, where
//!   upstream would have answered 500 before sending any of it; one that
//!   fails before answers 500 `failed to read log files: <error>`.
//! - A client that stops reading holds a blocking thread until it goes
//!   away.

use std::fmt::Write as _;
use std::io;

use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use futures_util::{StreamExt as _, future, stream};
use http::{HeaderValue, StatusCode, header};
use tokio::sync::mpsc;

use super::Page;
use super::timestamp::parse_timestamp;
use crate::json;

/// About how many bytes of the body are sent at a time.
const CHUNK: usize = 64 * 1024;

/// How many chunks may wait to be sent.
const QUEUE: usize = 4;

/// A piece of the body, and whether it is the last.
type Piece = io::Result<(Bytes, bool)>;

/// The answer with `page`: whole when it fits one chunk, streamed
/// otherwise.
pub(super) async fn respond(page: Page) -> Response {
    let (sender, mut receiver) = mpsc::channel::<Piece>(QUEUE);
    let task = tokio::task::spawn_blocking(move || {
        let mut out = Out {
            sender,
            buf: String::with_capacity(CHUNK),
        };
        let written = page.write(&mut out);
        out.finish(written);
    });
    let Some(first) = receiver.recv().await else {
        // The writer always sends a last piece unless it panicked.
        if let Err(error) = task.await
            && let Ok(panic) = error.try_into_panic()
        {
            std::panic::resume_unwind(panic);
        }
        return read_failed(&io::Error::other("the read ended early"));
    };
    match first {
        Ok((bytes, true)) => json_body(Body::from(bytes)),
        Ok((bytes, false)) => {
            let rest = stream::unfold(receiver, |mut receiver| async move {
                let piece = receiver.recv().await?;
                Some((piece.map(|(bytes, _)| bytes), receiver))
            });
            json_body(Body::from_stream(
                stream::once(future::ready(Ok::<_, io::Error>(bytes))).chain(rest),
            ))
        }
        Err(error) => read_failed(&error),
    }
}

/// The answer to a read that failed before the body started.
fn read_failed(error: &io::Error) -> Response {
    json::error(
        StatusCode::INTERNAL_SERVER_ERROR,
        &format!("failed to read log files: {error}"),
    )
}

/// A 200 with `body`, typed as gin's `c.JSON` types it.
fn json_body(body: Body) -> Response {
    let mut response = Response::new(body);
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
}

/// Where the body is written: a chunk being filled, and the queue it is
/// sent on.
struct Out {
    sender: mpsc::Sender<Piece>,
    buf: String,
}

impl Out {
    /// Sends the chunk once it is full; fails once the answer is no longer
    /// wanted.
    fn flush(&mut self) -> io::Result<()> {
        if self.buf.len() < CHUNK {
            return Ok(());
        }
        let chunk = std::mem::replace(&mut self.buf, String::with_capacity(CHUNK));
        self.sender
            .blocking_send(Ok((Bytes::from(chunk), false)))
            .map_err(|_| io::Error::other("the answer is no longer wanted"))
    }

    /// Sends the rest of the body, or the error that ended it.
    fn finish(self, written: io::Result<()>) {
        let piece = written.map(|()| (Bytes::from(self.buf), true));
        // A receiver gone has nothing left to tell.
        let _ = self.sender.blocking_send(piece);
    }
}

impl Page {
    /// Writes the page as gin's `c.JSON` of upstream's `gin.H` writes it,
    /// reading its lines again.
    fn write(self, out: &mut Out) -> io::Result<()> {
        let Self {
            segments,
            mut filter,
            mut skip,
            mut wanted,
            line_count,
            latest,
            next_cursor,
            cursor_reset,
        } = self;
        if cursor_reset {
            out.buf.push_str("{\"cursor-reset\":true,");
        } else {
            out.buf.push('{');
        }
        let _ = write!(
            out.buf,
            "\"latest-timestamp\":{latest},\"line-count\":{line_count},\"lines\":["
        );
        let mut first = true;
        for segment in segments {
            segment.lines(|line| {
                let timestamp = if filter.cutoff == 0 {
                    0
                } else {
                    parse_timestamp(line)
                };
                if !filter.admits(timestamp) {
                    return Ok(());
                }
                if skip > 0 {
                    skip -= 1;
                    return Ok(());
                }
                if wanted == 0 {
                    return Ok(());
                }
                wanted -= 1;
                if !first {
                    out.buf.push(',');
                }
                first = false;
                json::write_string(&mut out.buf, line);
                out.flush()
            })?;
        }
        out.buf.push_str("],\"next-cursor\":");
        json::write_string(&mut out.buf, &next_cursor);
        out.buf.push('}');
        Ok(())
    }
}
