// Ported from Go's mime/multipart/writer.go (NewWriter, SetBoundary,
// Boundary, FormDataContentType, randomBoundary, CreatePart,
// CreateFormFile, CreateFormField, FileContentDisposition, WriteField,
// Close, escapeQuotes) (go1.26, BSD-3-Clause, see licenses/Go-LICENSE).
// https://github.com/golang/go

//! Writing a form into memory.
//!
//! Deviations from Go: a part is written whole, header and data at once,
//! so there is no part left open for the next to close.

use std::fmt;

use super::{ByteCount, Error, Header};

/// A form's writer (Go's `multipart.Writer`) into memory. Its `Debug`
/// shows the boundary and the size written.
pub struct Writer {
    out: Vec<u8>,
    boundary: String,
    wrote_part: bool,
}

impl fmt::Debug for Writer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Writer")
            .field("boundary", &self.boundary)
            .field("out", &ByteCount(self.out.len()))
            .finish()
    }
}

impl Default for Writer {
    fn default() -> Self {
        Self::new()
    }
}

impl Writer {
    /// A writer with a random boundary: 30 random bytes in lower-case hex
    /// (Go's `NewWriter`).
    pub fn new() -> Self {
        let bytes: [u8; 30] = rand::random();
        let boundary = bytes.iter().map(|b| format!("{b:02x}")).collect();
        Self {
            out: Vec::new(),
            boundary,
            wrote_part: false,
        }
    }

    /// A writer with the boundary given (Go's `SetBoundary`): 1 to 70
    /// letters, digits, spaces (not last) and ``'()+_,-./:=?``.
    pub fn with_boundary(boundary: &str) -> Result<Self, Error> {
        if boundary.is_empty() || boundary.len() > 70 {
            return Err(Error::Other("mime: invalid boundary length".to_owned()));
        }
        let last = boundary.len() - 1;
        for (i, b) in boundary.bytes().enumerate() {
            let valid = b.is_ascii_alphanumeric()
                || b"'()+_,-./:=?".contains(&b)
                || (b == b' ' && i != last);
            if !valid {
                return Err(Error::Other("mime: invalid boundary character".to_owned()));
            }
        }
        Ok(Self {
            out: Vec::new(),
            boundary: boundary.to_owned(),
            wrote_part: false,
        })
    }

    /// Its boundary (Go's `Boundary`).
    pub fn boundary(&self) -> &str {
        &self.boundary
    }

    /// The `Content-Type` of the form (Go's `FormDataContentType`), the
    /// boundary quoted if it holds a special byte or a space.
    pub fn form_data_content_type(&self) -> String {
        let special = self
            .boundary
            .bytes()
            .any(|b| b"()<>@,;:\\\"/[]?= ".contains(&b));
        if special {
            format!("multipart/form-data; boundary=\"{}\"", self.boundary)
        } else {
            format!("multipart/form-data; boundary={}", self.boundary)
        }
    }

    /// Writes a part with `header`, its names in name order, and `data`
    /// (Go's `CreatePart` and a write).
    pub fn write_part(&mut self, header: &Header, data: &[u8]) {
        if self.wrote_part {
            self.out.extend_from_slice(b"\r\n");
        }
        self.out.extend_from_slice(b"--");
        self.out.extend_from_slice(self.boundary.as_bytes());
        self.out.extend_from_slice(b"\r\n");
        for (name, values) in header.iter() {
            for value in values {
                self.out.extend_from_slice(name.as_bytes());
                self.out.extend_from_slice(b": ");
                self.out.extend_from_slice(value);
                self.out.extend_from_slice(b"\r\n");
            }
        }
        self.out.extend_from_slice(b"\r\n");
        self.out.extend_from_slice(data);
        self.wrote_part = true;
    }

    /// Writes a field (Go's `WriteField`).
    pub fn write_field(&mut self, name: &str, value: &[u8]) {
        let mut header = Header::new();
        header.set(
            "Content-Disposition",
            format!("form-data; name=\"{}\"", escape_quotes(name)),
        );
        self.write_part(&header, value);
    }

    /// Writes a file as `application/octet-stream` (Go's `CreateFormFile`
    /// and a write).
    pub fn write_file(&mut self, name: &str, filename: &str, data: &[u8]) {
        let mut header = Header::new();
        header.set(
            "Content-Disposition",
            file_content_disposition(name, filename),
        );
        header.set("Content-Type", "application/octet-stream");
        self.write_part(&header, data);
    }

    /// The form, with its closing boundary line (Go's `Close`), which is
    /// written even when there are no parts.
    pub fn finish(mut self) -> Vec<u8> {
        self.out.extend_from_slice(b"\r\n--");
        self.out.extend_from_slice(self.boundary.as_bytes());
        self.out.extend_from_slice(b"--\r\n");
        self.out
    }
}

/// Go's `FileContentDisposition`: the `Content-Disposition` of a file part.
pub fn file_content_disposition(name: &str, filename: &str) -> String {
    format!(
        "form-data; name=\"{}\"; filename=\"{}\"",
        escape_quotes(name),
        escape_quotes(filename)
    )
}

/// Go's `escapeQuotes`: `\` and `"` escaped with a `\`, and CR and LF
/// percent-encoded.
pub fn escape_quotes(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\r' => out.push_str("%0D"),
            '\n' => out.push_str("%0A"),
            c => out.push(c),
        }
    }
    out
}
