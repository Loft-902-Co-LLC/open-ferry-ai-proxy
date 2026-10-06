// Ported from Go's mime/multipart (multipart.go, formdata.go, writer.go),
// mime/quotedprintable/reader.go, mime/mediatype.go (ParseMediaType,
// FormatMediaType), net/textproto/reader.go (readMIMEHeader,
// readContinuedLineSlice, CanonicalMIMEHeaderKey) and path/filepath (Base)
// (go1.26, BSD-3-Clause, see licenses/Go-LICENSE), as CLIProxyAPI v8.0.15
// (MIT) reads and writes the forms of its image and video endpoints
// (sdk/api/handlers/openai/openai_images_handlers.go,
// internal/runtime/executor/helps/payload_media.go,
// internal/runtime/executor/codex_openai_images.go,
// internal/runtime/executor/openai_compat_executor.go). sniff.rs is from
// Go's net/http/sniff.go (DetectContentType).
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/golang/go

//! `multipart/form-data` bodies, read and written as Go's `mime/multipart`
//! reads and writes them, for the image and video endpoints and the
//! executors that send their forms on.
//!
//! A body is read whole, from memory: [`Reader`] gives its parts one at a
//! time (Go's `NextPart`), each with its headers and its data, and
//! [`Reader::read_form`] reads them all into a [`Form`] within Go's limits
//! (Go's `ReadForm`). [`Writer`] writes a form, with a random boundary as
//! Go's does. [`parse_media_type`] and [`format_media_type`] read and write
//! a `Content-Type` or `Content-Disposition` value. Errors read as Go's.
//! [`detect_content_type`] gives the type of a file sent without one, and
//! [`FileHeader::data_url`] a file as a data URL.
//!
//! Nothing here logs, and the `Debug` of a part, a form or a file shows
//! names and sizes only: a form holds prompts and images.
//!
//! Deviations from Go:
//! - A form is never written to disk. Go's `ReadForm` keeps a file over
//!   its memory limit in a temporary file; here it stays in memory, bounded
//!   by the body limit of the route that read the body, and counts against
//!   the limits as Go counts a file on disk.
//! - A form's values and files are kept by name in name order; Go's maps
//!   give them in random order.
//! - A field or file name, which Go keeps as bytes, is text here, each byte
//!   that isn't part of a valid UTF-8 character read as U+FFFD.
//! - A file name is what follows the last `/` or `\` of the `filename`
//!   parameter on every system, as the management API's uploads read it;
//!   Go splits at `/` alone outside Windows.
//! - A quoted-printable part that doesn't decode leaves the reader at the
//!   end of the part; Go's stops partway, where the decoder stopped.
//! - A folded header line that takes a part's header over its limit fails
//!   with `multipart: message too large`; Go stops reading the header
//!   partway through the line.

mod header;
mod media_type;
mod quoted_printable;
mod reader;
mod sniff;
#[cfg(test)]
mod tests;
mod writer;

use std::fmt;

pub use header::{Header, canonical_key};
pub use media_type::{MediaType, ParseError, base_name, format_media_type, parse_media_type};
pub use reader::{FileHeader, Form, Part, Reader};
pub use sniff::detect_content_type;
pub use writer::{Writer, escape_quotes, file_content_disposition};

/// The memory limit upstream's handlers and executors read a form with
/// (Go's `c.MultipartForm()` default, and upstream's
/// `openAICompatMultipartMemory`): 32 MiB.
pub const MAX_FORM_MEMORY: i64 = 32 << 20;

/// Why a body couldn't be read as a form. Its `Display` is Go's error
/// text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// Go's `multipart.ErrMessageTooLarge`: too many parts or headers, or
    /// more than the memory limit.
    TooLarge,
    /// Go's `io.ErrUnexpectedEOF`: a part's data ends without a boundary.
    UnexpectedEof,
    /// Any other error, as Go words it.
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::TooLarge => f.write_str("multipart: message too large"),
            Error::UnexpectedEof => f.write_str("unexpected EOF"),
            Error::Other(text) => f.write_str(text),
        }
    }
}

impl std::error::Error for Error {}

/// A size in bytes, shown in place of the bytes.
struct ByteCount(usize);

impl fmt::Debug for ByteCount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} bytes", self.0)
    }
}

/// `bytes` as text, each byte that isn't part of a valid character read as
/// U+FFFD, as Go's `range` over a string and its JSON encoder read them.
pub fn lossy(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        out.push_str(chunk.valid());
        for _ in chunk.invalid() {
            out.push(char::REPLACEMENT_CHARACTER);
        }
    }
    out
}
