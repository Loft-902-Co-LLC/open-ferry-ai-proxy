// Ported from Go's mime/multipart/multipart.go (NewReader, NextPart,
// nextPart, newPart, populateHeaders, partReader.Read, scanUntilBoundary,
// matchAfterPrefix, isFinalBoundary, isBoundaryDelimiterLine,
// skipLWSPChar, FormName, FileName, parseContentDisposition),
// formdata.go (ReadForm, readForm, mimeHeaderSize) and
// net/textproto/reader.go (readMIMEHeader, readContinuedLineSlice,
// readLineSlice, skipSpace, trim, mustHaveFieldNameColon) (go1.26,
// BSD-3-Clause, see licenses/Go-LICENSE).
// https://github.com/golang/go

//! Reading a form: its parts one at a time, or all of them into a
//! [`Form`].
//!
//! A part's lines are read as Go's 4096-byte buffer reads them: a line
//! between parts, or before the first, may be at most 4096 bytes long with
//! its newline, else the read fails with `multipart: NextPart: bufio:
//! buffer full`.
//!
//! Deviations from Go: see the module above (a form stays in memory, its
//! names are text, its fields come in name order, and file names split at
//! `\` too).

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;

use aho_corasick::AhoCorasick;
use bytes::Bytes;
use open_ferry_translate::go::{equal_fold, quote_bytes};

use super::header::{Header, is_value_byte, read_key};
use super::media_type::{base_name, parse_media_type};
use super::{ByteCount, Error, lossy, quoted_printable};

/// The size of Go's `bufio.Reader` under a multipart reader
/// (`peekBufferSize`), the longest line read at once.
const PEEK_BUFFER: usize = 4096;
/// The most bytes a part's header may take (`maxMIMEHeaderSize`).
const MAX_HEADER_SIZE: i64 = 10 << 20;
/// The most header lines a part may have (`multipartmaxheaders`).
const MAX_HEADERS: i64 = 10000;
/// The most parts a form may have (`multipartmaxparts`).
const MAX_PARTS: i64 = 1000;
/// What Go counts for a map entry (`mapEntryOverhead`).
const MAP_ENTRY: i64 = 200;
/// What Go counts for a file's header (`fileHeaderSize`).
const FILE_HEADER: i64 = 100;

/// Why a header wasn't read: the body ended, which ends the form as its
/// final boundary does, or an error.
enum Fail {
    Eof,
    Error(Error),
}

impl From<Error> for Fail {
    fn from(error: Error) -> Self {
        Fail::Error(error)
    }
}

/// How a line read with Go's `ReadSlice('\n')` ended, when not at a
/// newline.
enum LineEnd {
    /// The body ended.
    Eof,
    /// 4096 bytes held no newline.
    Full,
}

/// A form's reader (Go's `multipart.Reader`) over a whole body.
pub struct Reader {
    data: Bytes,
    pos: usize,
    /// `--` and the boundary.
    dash_boundary: Vec<u8>,
    /// Whether lines end in `\n` alone, as the first boundary line's does.
    bare_newlines: bool,
    parts_read: usize,
}

impl fmt::Debug for Reader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reader")
            .field("data", &ByteCount(self.data.len()))
            .field("pos", &self.pos)
            .field("parts_read", &self.parts_read)
            .finish()
    }
}

impl Reader {
    /// A reader of the form `data`, whose parts are separated by
    /// `boundary` (Go's `NewReader`).
    pub fn new(data: Bytes, boundary: &[u8]) -> Self {
        let mut dash_boundary = b"--".to_vec();
        dash_boundary.extend_from_slice(boundary);
        Self {
            data,
            pos: 0,
            dash_boundary,
            bare_newlines: false,
            parts_read: 0,
        }
    }

    /// The next part, or `None` after the last (Go's `NextPart`).
    pub fn next_part(&mut self) -> Result<Option<Part>, Error> {
        self.next(MAX_HEADER_SIZE, MAX_HEADERS)
    }

    fn nl(&self) -> &'static [u8] {
        if self.bare_newlines { b"\n" } else { b"\r\n" }
    }

    /// Go's `nextPart`, with the header limits given.
    fn next(&mut self, max_header_size: i64, max_headers: i64) -> Result<Option<Part>, Error> {
        if self.dash_boundary == b"--" {
            return Err(Error::Other("multipart: boundary is empty".to_owned()));
        }
        let data = self.data.clone();
        let mut expect_new_part = false;
        loop {
            let (line, end) = self.read_slice(&data);
            match end {
                Some(LineEnd::Eof) if self.is_final_boundary(line) => return Ok(None),
                Some(LineEnd::Eof) => {
                    return Err(Error::Other("multipart: NextPart: EOF".to_owned()));
                }
                Some(LineEnd::Full) => {
                    return Err(Error::Other(
                        "multipart: NextPart: bufio: buffer full".to_owned(),
                    ));
                }
                None => {}
            }
            if self.is_boundary_delimiter_line(line) {
                self.parts_read += 1;
                return self.new_part(max_header_size, max_headers);
            }
            if self.is_final_boundary(line) {
                return Ok(None);
            }
            if expect_new_part {
                return Err(Error::Other(format!(
                    "multipart: expecting a new Part; got line {}",
                    quote_bytes(line)
                )));
            }
            if self.parts_read == 0 {
                continue;
            }
            if line == self.nl() {
                expect_new_part = true;
                continue;
            }
            return Err(Error::Other(format!(
                "multipart: unexpected line in Next(): {}",
                quote_bytes(line)
            )));
        }
    }

    /// Go's `ReadSlice('\n')` on a 4096-byte buffer: the line at the
    /// reader, with its newline, and how it ended if not at one.
    fn read_slice<'d>(&mut self, data: &'d [u8]) -> (&'d [u8], Option<LineEnd>) {
        let rest = data.get(self.pos..).unwrap_or_default();
        let window = rest.get(..PEEK_BUFFER).unwrap_or(rest);
        if let Some(i) = window.iter().position(|&b| b == b'\n') {
            let line = rest.get(..=i).unwrap_or(rest);
            self.pos += line.len();
            return (line, None);
        }
        if rest.len() >= PEEK_BUFFER {
            self.pos += window.len();
            return (window, Some(LineEnd::Full));
        }
        self.pos += rest.len();
        (rest, Some(LineEnd::Eof))
    }

    /// Go's `isFinalBoundary`: `--boundary--`, then spaces and tabs, then
    /// a newline or nothing.
    fn is_final_boundary(&self, line: &[u8]) -> bool {
        let Some(rest) = line.strip_prefix(self.dash_boundary.as_slice()) else {
            return false;
        };
        let Some(rest) = rest.strip_prefix(b"--") else {
            return false;
        };
        let rest = skip_lwsp(rest);
        rest.is_empty() || rest == self.nl()
    }

    /// Go's `isBoundaryDelimiterLine`: `--boundary`, then spaces and tabs,
    /// then a newline. The first such line ending in `\n` alone makes `\n`
    /// the newline from then on.
    fn is_boundary_delimiter_line(&mut self, line: &[u8]) -> bool {
        let Some(rest) = line.strip_prefix(self.dash_boundary.as_slice()) else {
            return false;
        };
        let rest = skip_lwsp(rest);
        if self.parts_read == 0 && rest == b"\n" {
            self.bare_newlines = true;
        }
        rest == self.nl()
    }

    /// Go's `newPart`: the header at the reader and the data after it,
    /// quoted-printable decoded if the header says so; `None` when the body
    /// ends in the header.
    fn new_part(&mut self, max_header_size: i64, max_headers: i64) -> Result<Option<Part>, Error> {
        let mut header = match self.read_header(max_header_size, max_headers) {
            Ok(header) => header,
            Err(Fail::Eof) => return Ok(None),
            Err(Fail::Error(error)) => return Err(error),
        };
        let (range, mut error) = self.scan_part();
        let mut data = self.data.slice(range);
        const CTE: &str = "Content-Transfer-Encoding";
        if equal_fold(&header.get_str(CTE), "quoted-printable") {
            header.remove(CTE);
            let (decoded, decode_error) = quoted_printable::decode(&data, error);
            data = Bytes::from(decoded);
            error = decode_error;
        }
        Ok(Some(Part {
            header,
            data,
            error,
        }))
    }

    /// The part's data from the reader to the boundary after it, and the
    /// error it ended with: none at a boundary, `unexpected EOF` at the end
    /// of the body (Go's `partReader`, read to its end).
    fn scan_part(&mut self) -> (Range<usize>, Option<Error>) {
        let mut nl_dash_boundary = self.nl().to_vec();
        nl_dash_boundary.extend_from_slice(&self.dash_boundary);
        let finder = AhoCorasick::new([&nl_dash_boundary]).ok();
        let start = self.pos;
        let mut total = 0;
        loop {
            let buf = self.data.get(self.pos..).unwrap_or_default();
            let (n, scan) = scan_until_boundary(
                buf,
                &self.dash_boundary,
                &nl_dash_boundary,
                finder.as_ref(),
                total,
            );
            self.pos += n;
            total += n;
            match scan {
                Scan::Boundary => return (start..self.pos, None),
                Scan::More if n > 0 => {}
                Scan::More | Scan::Truncated => {
                    return (start..self.pos, Some(Error::UnexpectedEof));
                }
            }
        }
    }

    /// Go's `readMIMEHeader` at the reader, within `max_memory` bytes and
    /// `max_headers` lines.
    fn read_header(&mut self, max_memory: i64, mut max_headers: i64) -> Result<Header, Fail> {
        let mut max_memory = max_memory.saturating_sub(400);
        let mut header = Header::new();
        if matches!(self.data.get(self.pos), Some(b' ' | b'\t')) {
            let line = self.read_line(80)?;
            return Err(Error::Other(format!(
                "malformed MIME header initial line: {}",
                quote_bytes(&line)
            ))
            .into());
        }
        loop {
            let line = self.read_continued_line(max_memory)?;
            if line.is_empty() {
                return Ok(header);
            }
            let malformed = || {
                Fail::Error(Error::Other(format!(
                    "malformed MIME header line: {}",
                    quote_bytes(&line)
                )))
            };
            let Some(colon) = line.iter().position(|&b| b == b':') else {
                return Err(malformed());
            };
            let (name, value) = line.split_at(colon);
            let value = value.get(1..).unwrap_or_default();
            let Some(key) = read_key(name) else {
                return Err(malformed());
            };
            if !value.iter().all(|&b| is_value_byte(b)) {
                return Err(malformed());
            }
            max_headers -= 1;
            if max_headers < 0 {
                return Err(Error::TooLarge.into());
            }
            let value = skip_lwsp(value);
            if !header.has_raw(&key) {
                max_memory = max_memory
                    .saturating_sub(length(key.len()))
                    .saturating_sub(MAP_ENTRY);
            }
            max_memory = max_memory.saturating_sub(length(value.len()));
            if max_memory < 0 {
                return Err(Error::TooLarge.into());
            }
            header.append_raw(key, value.to_vec());
        }
    }

    /// Go's `readContinuedLineSlice` with `mustHaveFieldNameColon`: a
    /// header line and the lines folded under it, joined by a space, each
    /// trimmed of spaces and tabs; empty for the blank line ending the
    /// header.
    fn read_continued_line(&mut self, lim: i64) -> Result<Vec<u8>, Fail> {
        let line = self.read_line(lim)?;
        if line.is_empty() {
            return Ok(line);
        }
        if !line.contains(&b':') {
            return Err(Error::Other(format!(
                "malformed MIME header: missing colon: {}",
                quote_bytes(&line)
            ))
            .into());
        }
        let mut buf = trim(&line).to_vec();
        let lim = if lim < 0 { i64::MAX } else { lim }.saturating_sub(length(buf.len()));
        while self.skip_space() > 0 {
            buf.push(b' ');
            if length(buf.len()) >= lim {
                return Err(Error::TooLarge.into());
            }
            match self.read_line(lim.saturating_sub(length(buf.len()))) {
                Ok(line) => buf.extend_from_slice(trim(&line)),
                Err(Fail::Eof) => break,
                Err(error) => return Err(error),
            }
        }
        Ok(buf)
    }

    /// Go's `readLineSlice`: the line at the reader without its newline
    /// (`\n`, or `\r\n`), of at most `lim` bytes when `lim` isn't negative.
    fn read_line(&mut self, lim: i64) -> Result<Vec<u8>, Fail> {
        let rest = self.data.get(self.pos..).unwrap_or_default();
        if rest.is_empty() {
            return Err(Fail::Eof);
        }
        let (line, consumed) = match rest.iter().position(|&b| b == b'\n') {
            Some(i) => {
                let line = rest.get(..i).unwrap_or_default();
                (line.strip_suffix(b"\r").unwrap_or(line), i + 1)
            }
            None => (rest, rest.len()),
        };
        if lim >= 0 && length(line.len()) > lim {
            return Err(Error::TooLarge.into());
        }
        let line = line.to_vec();
        self.pos += consumed;
        Ok(line)
    }

    /// Go's `skipSpace`: skips the spaces and tabs at the reader and counts
    /// them.
    fn skip_space(&mut self) -> usize {
        let rest = self.data.get(self.pos..).unwrap_or_default();
        let n = rest
            .iter()
            .position(|&b| b != b' ' && b != b'\t')
            .unwrap_or(rest.len());
        self.pos += n;
        n
    }

    /// Reads every part into a form, keeping at most `max_memory` bytes of
    /// files and 10 MiB more of everything else as Go counts them (Go's
    /// `ReadForm`). Parts without a field name are skipped.
    pub fn read_form(mut self, max_memory: i64) -> Result<Form, Error> {
        let mut form = Form::default();
        let mut max_parts = MAX_PARTS;
        let mut max_headers = MAX_HEADERS;
        let mut max_file_memory = if max_memory == i64::MAX {
            max_memory - 1
        } else {
            max_memory
        };
        let mut max_memory_bytes = match max_memory.checked_add(10 << 20) {
            Some(bytes) if bytes > 0 => bytes,
            _ if max_memory < 0 => 0,
            _ => i64::MAX,
        };
        loop {
            let Some(part) = self.next(max_memory_bytes, max_headers)? else {
                break;
            };
            if max_parts <= 0 {
                return Err(Error::TooLarge);
            }
            max_parts -= 1;
            let name = part.form_name();
            if name.is_empty() {
                continue;
            }
            let file_name = part.file_name();
            max_memory_bytes = max_memory_bytes
                .saturating_sub(length(name.len()))
                .saturating_sub(MAP_ENTRY);
            if max_memory_bytes < 0 {
                return Err(Error::TooLarge);
            }
            let Part {
                header,
                data,
                error,
            } = part;
            if file_name.is_empty() {
                // `io.CopyN` of one byte more than is left.
                let limit = max_memory_bytes.saturating_add(1);
                let read = length(data.len());
                if read <= limit
                    && let Some(error) = error
                {
                    return Err(error);
                }
                max_memory_bytes = max_memory_bytes.saturating_sub(read.min(limit));
                if max_memory_bytes < 0 {
                    return Err(Error::TooLarge);
                }
                form.values.entry(name).or_default().push(data);
                continue;
            }
            max_memory_bytes = max_memory_bytes
                .saturating_sub(header_size(&header))
                .saturating_sub(MAP_ENTRY)
                .saturating_sub(FILE_HEADER);
            if max_memory_bytes < 0 {
                return Err(Error::TooLarge);
            }
            for (_, values) in header.iter() {
                max_headers = max_headers.saturating_sub(length(values.len()));
            }
            if let Some(error) = error {
                return Err(error);
            }
            let read = length(data.len());
            // Go keeps a file over the limit on disk, uncounted; here it
            // stays in memory, as uncounted.
            if read <= max_file_memory {
                max_file_memory -= read;
                max_memory_bytes = max_memory_bytes.saturating_sub(read);
            }
            form.files.entry(name).or_default().push(FileHeader {
                filename: file_name,
                header,
                data,
            });
        }
        Ok(form)
    }
}

/// How [`scan_until_boundary`] left the part.
enum Scan {
    /// Its data goes on.
    More,
    /// A boundary ends it.
    Boundary,
    /// The body ends without a boundary.
    Truncated,
}

/// Go's `scanUntilBoundary` over the rest of the body: how many bytes of
/// part data `buf` starts with, and whether a boundary follows them. The
/// body is all there, so Go's read error is always set.
fn scan_until_boundary(
    buf: &[u8],
    dash_boundary: &[u8],
    nl_dash_boundary: &[u8],
    finder: Option<&AhoCorasick>,
    total: usize,
) -> (usize, Scan) {
    if total == 0 {
        if buf.starts_with(dash_boundary) {
            return if match_after_prefix(buf, dash_boundary) {
                (0, Scan::Boundary)
            } else {
                (dash_boundary.len(), Scan::More)
            };
        }
        if dash_boundary.starts_with(buf) {
            return (0, Scan::Truncated);
        }
    }
    let found = match finder {
        Some(finder) => finder.find(buf).map(|m| m.start()),
        None => buf
            .windows(nl_dash_boundary.len())
            .position(|window| window == nl_dash_boundary),
    };
    if let Some(i) = found {
        let at = buf.get(i..).unwrap_or_default();
        return if match_after_prefix(at, nl_dash_boundary) {
            (i, Scan::Boundary)
        } else {
            (i + nl_dash_boundary.len(), Scan::More)
        };
    }
    if nl_dash_boundary.starts_with(buf) {
        return (0, Scan::Truncated);
    }
    let first = nl_dash_boundary.first().copied().unwrap_or(b'\n');
    if let Some(i) = buf.iter().rposition(|&b| b == first)
        && nl_dash_boundary.starts_with(buf.get(i..).unwrap_or_default())
    {
        return (i, Scan::More);
    }
    (buf.len(), Scan::Truncated)
}

/// Go's `matchAfterPrefix` with the read error set: whether the boundary
/// `buf` starts with is one, followed by the end of the body, a space, a
/// tab, a newline or `--`.
fn match_after_prefix(buf: &[u8], prefix: &[u8]) -> bool {
    let rest = buf.get(prefix.len()..).unwrap_or_default();
    matches!(
        rest,
        [] | [b' ' | b'\t' | b'\r' | b'\n', ..] | [b'-', b'-', ..]
    )
}

/// Go's `skipLWSPChar`: `bytes` past the spaces and tabs at its start.
fn skip_lwsp(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|&b| b != b' ' && b != b'\t')
        .unwrap_or(bytes.len());
    bytes.get(start..).unwrap_or_default()
}

/// Go's textproto `trim`: `bytes` without spaces and tabs at either end.
fn trim(bytes: &[u8]) -> &[u8] {
    let bytes = skip_lwsp(bytes);
    let end = bytes
        .iter()
        .rposition(|&b| b != b' ' && b != b'\t')
        .map_or(0, |last| last + 1);
    bytes.get(..end).unwrap_or_default()
}

/// A length as Go's `int64` counts it.
fn length(len: usize) -> i64 {
    i64::try_from(len).unwrap_or(i64::MAX)
}

/// Go's `mimeHeaderSize`: what a file's header counts against a form's
/// memory.
fn header_size(header: &Header) -> i64 {
    let mut size: i64 = 400;
    for (name, values) in header.iter() {
        size = size
            .saturating_add(length(name.len()))
            .saturating_add(MAP_ENTRY);
        for value in values {
            size = size.saturating_add(length(value.len()));
        }
    }
    size
}

/// A part of a form (Go's `multipart.Part`), read whole: its header, its
/// data and the error that ended the data, if one did. Its `Debug` shows
/// header names and sizes only.
pub struct Part {
    /// Its header, without `Content-Transfer-Encoding` when that was
    /// `quoted-printable` (the data is decoded).
    pub header: Header,
    data: Bytes,
    error: Option<Error>,
}

impl fmt::Debug for Part {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Part")
            .field("header", &self.header)
            .field("data", &ByteCount(self.data.len()))
            .field("error", &self.error)
            .finish()
    }
}

impl Part {
    /// Its field name (Go's `FormName`): the `name` parameter of its
    /// `Content-Disposition`, if that is `form-data`; empty otherwise.
    pub fn form_name(&self) -> String {
        let (kind, params) = self.disposition();
        match params.get("name") {
            Some(name) if kind == "form-data" => lossy(name),
            _ => String::new(),
        }
    }

    /// Its file name (Go's `FileName`): the [`base_name`] of the
    /// `filename` parameter of its `Content-Disposition`; empty when it
    /// has none.
    pub fn file_name(&self) -> String {
        let (_, params) = self.disposition();
        match params.get("filename") {
            Some(name) if !name.is_empty() => lossy(base_name(name)),
            _ => String::new(),
        }
    }

    /// Its disposition and parameters, none when they don't parse.
    fn disposition(&self) -> (String, std::collections::HashMap<String, Vec<u8>>) {
        let value = self.header.get("Content-Disposition").unwrap_or_default();
        match parse_media_type(value) {
            Ok(parsed) => parsed,
            Err(error) => (error.media_type().to_owned(), Default::default()),
        }
    }

    /// Its data as far as it was read, all of it when [`Part::error`] is
    /// `None`.
    pub fn data(&self) -> &Bytes {
        &self.data
    }

    /// The error that ended its data before the boundary, if one did.
    pub fn error(&self) -> Option<&Error> {
        self.error.as_ref()
    }

    /// Its data (Go's `io.ReadAll`), or the error that ended it.
    pub fn read_all(self) -> Result<Bytes, Error> {
        match self.error {
            Some(error) => Err(error),
            None => Ok(self.data),
        }
    }
}

/// A form read whole (Go's `multipart.Form`): its values and files by field
/// name, in name order. Its `Debug` shows names and sizes only.
#[derive(Default)]
pub struct Form {
    values: BTreeMap<String, Vec<Bytes>>,
    files: BTreeMap<String, Vec<FileHeader>>,
}

impl fmt::Debug for Form {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let values: BTreeMap<&String, Vec<ByteCount>> = self
            .values
            .iter()
            .map(|(name, values)| {
                (
                    name,
                    values.iter().map(|value| ByteCount(value.len())).collect(),
                )
            })
            .collect();
        f.debug_struct("Form")
            .field("values", &values)
            .field("files", &self.files)
            .finish()
    }
}

impl Form {
    /// The first value of field `name` (Go's `form.Value[name][0]`).
    pub fn value(&self, name: &str) -> Option<&Bytes> {
        self.values.get(name).and_then(|values| values.first())
    }

    /// Each field with its values, in name order.
    pub fn values(&self) -> impl Iterator<Item = (&str, &[Bytes])> {
        self.values
            .iter()
            .map(|(name, values)| (name.as_str(), values.as_slice()))
    }

    /// The files of field `name` (Go's `form.File[name]`).
    pub fn files_of(&self, name: &str) -> &[FileHeader] {
        self.files.get(name).map(Vec::as_slice).unwrap_or_default()
    }

    /// Each field with its files, in name order.
    pub fn files(&self) -> impl Iterator<Item = (&str, &[FileHeader])> {
        self.files
            .iter()
            .map(|(name, files)| (name.as_str(), files.as_slice()))
    }
}

/// A file of a form (Go's `multipart.FileHeader`). Its `Debug` shows its
/// name and header names and sizes only.
#[derive(Clone)]
pub struct FileHeader {
    /// The [`base_name`] of the name it was sent with.
    pub filename: String,
    /// Its part's header.
    pub header: Header,
    /// Its contents.
    pub data: Bytes,
}

impl fmt::Debug for FileHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileHeader")
            .field("filename", &self.filename)
            .field("header", &self.header)
            .field("data", &ByteCount(self.data.len()))
            .finish()
    }
}
