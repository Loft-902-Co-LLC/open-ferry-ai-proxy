//! The request-log routes: searching the logs of the log directory,
//! reading one in pieces, and sending one whole, byte for byte.
//!
//! The logs are the request logger's, named as it names them
//! ([`parse_log_metadata`]), and opened as the management API's log
//! routes open them ([`open_log_file`]): a link, anything but a plain
//! file, or a file with another hard link is refused. Nothing is masked
//! beyond what the request logger masked when it wrote the file.
//!
//! A search is bounded: it lists at most [`MAX_NAMES`] names, opens at
//! most [`MAX_FILES`] files and reads at most [`MAX_BYTES`], and of a file
//! over [`WHOLE_FILE`] only its first and last [`PART`] bytes.
//!
//! A download streams the file from the handle its checks were made on,
//! [`CHUNK`] bytes at a time, up to the size it had when it was opened.

use std::fmt::Write as _;
use std::fs::{self, File, Metadata};
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::path::Path;
use std::time::SystemTime;

use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, State};
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Local, NaiveDateTime, TimeZone as _, Utc};
use futures_util::{Stream, stream};
use http::{HeaderValue, StatusCode, Uri, header};
use open_ferry_core::observe::request_log::{is_error_log_name, parse_log_metadata};
use open_ferry_management::{is_refused_log_file, open_log_file};
use serde::Serialize;
use serde_json::json;
use tokio::io::AsyncReadExt as _;

use super::{ApiError, Query, ok};
use crate::DashboardState;
use crate::ledger::format_time;

/// The most directory entries a search lists.
const MAX_NAMES: usize = 100_000;

/// The most files a search opens.
const MAX_FILES: usize = 2000;

/// The most bytes a search reads.
const MAX_BYTES: u64 = 64 * 1024 * 1024;

/// A file up to this size is read whole.
const WHOLE_FILE: u64 = 1024 * 1024;

/// Of a larger file, this much of its start and of its end is read.
const PART: u64 = 512 * 1024;

/// The most a read of one log returns.
const MAX_LENGTH: i64 = 4 * 1024 * 1024;

/// What a read of one log returns unless told.
const DEFAULT_LENGTH: i64 = 1024 * 1024;

/// What a download reads at a time.
const CHUNK: usize = 64 * 1024;

/// The longest text a filter takes, in bytes.
const MAX_FILTER: usize = 1024;

/// The longest URL or model an entry shows, in bytes.
const MAX_FIELD: usize = 2048;

/// A log as the API describes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct Entry {
    name: String,
    kind: &'static str,
    request_id: String,
    time: String,
    size: u64,
    modified: Option<String>,
    method: Option<String>,
    url: Option<String>,
    status: Option<u16>,
    model: Option<String>,
}

/// A log's name, and what the name says of it: the order logs are listed
/// in is newest first by these.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Named {
    time: NaiveDateTime,
    seq: i64,
    name: String,
}

impl Named {
    /// `name`, if it is a request or error log's.
    fn parse(name: &str) -> Option<Self> {
        if name.is_empty()
            || name.contains(['/', '\\'])
            || !name.ends_with(".log")
            || name.starts_with('.')
        {
            return None;
        }
        let meta = parse_log_metadata(name);
        Some(Self {
            time: meta.time?,
            seq: meta.seq,
            name: name.to_owned(),
        })
    }

    /// `request` or `error`.
    fn kind(&self) -> &'static str {
        if is_error_log_name(&self.name) {
            "error"
        } else {
            "request"
        }
    }

    /// The name's time, which is the server's local time, as UTC
    /// milliseconds.
    fn utc_ms(&self) -> i64 {
        let local = Local.from_local_datetime(&self.time);
        local.earliest().or_else(|| local.latest()).map_or_else(
            || self.time.and_utc().timestamp_millis(),
            |time| time.with_timezone(&Utc).timestamp_millis(),
        )
    }

    /// The short request ID at the end of the name.
    fn request_id(&self) -> String {
        let base = self.name.strip_suffix(".log").unwrap_or(&self.name);
        base.rsplit_once('-')
            .map(|(_, id)| id.to_owned())
            .unwrap_or_default()
    }
}

/// The parts of a log read to describe it: all of a small one, the start
/// and end of a large one.
struct Sample {
    head: String,
    tail: Option<String>,
    /// The bytes read.
    bytes: u64,
}

impl Sample {
    /// Reads `file`, of `size` bytes.
    fn read(file: &mut File, size: u64) -> io::Result<Self> {
        if size <= WHOLE_FILE {
            let head = read_at(file, 0, size)?;
            let bytes = head.len() as u64;
            return Ok(Self {
                head: String::from_utf8_lossy(&head).into_owned(),
                tail: None,
                bytes,
            });
        }
        let head = read_at(file, 0, PART)?;
        let tail = read_at(file, size.saturating_sub(PART), PART)?;
        let bytes = (head.len() + tail.len()) as u64;
        Ok(Self {
            head: String::from_utf8_lossy(&head).into_owned(),
            tail: Some(String::from_utf8_lossy(&tail).into_owned()),
            bytes,
        })
    }

    /// The parts read.
    fn parts(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.head.as_str()).chain(self.tail.as_deref())
    }

    /// The part the answer is in: the end of a large log.
    fn last(&self) -> &str {
        self.tail.as_deref().unwrap_or(&self.head)
    }
}

/// Up to `length` bytes of `file` from `offset`.
fn read_at(file: &mut File, offset: u64, length: u64) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(length).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// The value of the first line of `text` starting with `prefix`, cut to
/// [`MAX_FIELD`].
fn line_value(text: &str, prefix: &str) -> Option<String> {
    let start = if text.starts_with(prefix) {
        prefix.len()
    } else {
        text.find(&format!("\n{prefix}"))? + 1 + prefix.len()
    };
    let rest = text.get(start..)?;
    let line = rest
        .split('\n')
        .next()
        .unwrap_or_default()
        .trim_end_matches('\r');
    Some(cut(line))
}

/// `text` cut to at most [`MAX_FIELD`] bytes, at a character boundary.
fn cut(text: &str) -> String {
    let mut end = text.len().min(MAX_FIELD);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or_default().to_owned()
}

/// The status of the log's answer: the last `=== RESPONSE ===` section's.
fn response_status(text: &str) -> Option<u16> {
    const MARK: &str = "=== RESPONSE ===\nStatus: ";
    let start = text.rfind(MARK)? + MARK.len();
    let digits: String = text
        .get(start..)?
        .chars()
        .take_while(char::is_ascii_digit)
        .take(3)
        .collect();
    digits.parse().ok()
}

/// The values of the `"model"` string fields in `text`, in order.
fn model_fields(text: &str) -> impl Iterator<Item = String> + '_ {
    text.match_indices("\"model\"").filter_map(|(at, mark)| {
        let rest = text.get(at + mark.len()..)?.trim_start();
        let rest = rest.strip_prefix(':')?.trim_start();
        let rest = rest.strip_prefix('"')?;
        let mut value = String::new();
        let mut escaped = false;
        for c in rest.chars() {
            if value.len() >= MAX_FIELD {
                break;
            }
            match (escaped, c) {
                (false, '\\') => escaped = true,
                (false, '"') => return Some(value),
                (_, c) => {
                    escaped = false;
                    value.push(c);
                }
            }
        }
        (!value.is_empty()).then_some(value)
    })
}

/// The model in a Gemini URL: `/models/<model>:<action>`.
fn gemini_url_model(url: &str) -> Option<String> {
    let path = url.split('?').next().unwrap_or_default();
    let (_, rest) = path.split_once("/models/")?;
    let (model, _) = rest.split_once(':')?;
    (!model.is_empty() && !model.contains('/')).then(|| cut(model))
}

/// The request body section of `head`.
fn request_body(head: &str) -> Option<&str> {
    const MARK: &str = "=== REQUEST BODY ===\n";
    let start = head.find(MARK)? + MARK.len();
    let body = head.get(start..)?;
    Some(match body.find("\n=== ") {
        Some(end) => body.get(..end).unwrap_or(body),
        None => body,
    })
}

/// The answer section of `text`, from its last `=== RESPONSE ===`.
fn response_section(text: &str) -> Option<&str> {
    text.get(text.rfind("=== RESPONSE ===")?..)
}

/// What a log's sample says of it.
#[derive(Debug, Default, PartialEq, Eq)]
struct Described {
    method: Option<String>,
    url: Option<String>,
    status: Option<u16>,
    model: Option<String>,
}

/// Reads the method, URL, status and model out of `sample`.
fn describe(sample: &Sample) -> Described {
    let method = line_value(&sample.head, "Method: ");
    let url = line_value(&sample.head, "URL: ");
    let status = response_status(sample.last());
    let model = request_body(&sample.head)
        .and_then(|body| model_fields(body).next())
        .or_else(|| url.as_deref().and_then(gemini_url_model))
        .or_else(|| {
            sample
                .parts()
                .filter_map(response_section)
                .last()
                .and_then(|section| model_fields(section).next())
        });
    Described {
        method,
        url,
        status,
        model,
    }
}

/// What a search matches beyond names.
#[derive(Debug, Default)]
struct Filters {
    /// Lowercased.
    path: Option<String>,
    status: Option<StatusFilter>,
    /// Lowercased.
    model: Option<String>,
    /// ASCII-lowercased.
    q: Option<String>,
}

/// An exact status, or a class of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatusFilter {
    Exact(u16),
    Class(u16),
}

impl StatusFilter {
    fn parse(text: &str) -> Option<Self> {
        if let Some(class) = text.strip_suffix("xx") {
            return match class {
                "1" | "2" | "3" | "4" | "5" => class.parse().ok().map(Self::Class),
                _ => None,
            };
        }
        let status: u16 = text.parse().ok()?;
        (100..=599).contains(&status).then_some(Self::Exact(status))
    }

    fn matches(self, status: Option<u16>) -> bool {
        match (self, status) {
            (Self::Exact(want), Some(status)) => status == want,
            (Self::Class(class), Some(status)) => status / 100 == class,
            (_, None) => false,
        }
    }
}

impl Filters {
    /// Whether these filters read the files' content.
    fn any(&self) -> bool {
        self.path.is_some() || self.status.is_some() || self.model.is_some() || self.q.is_some()
    }

    /// Whether a log with `sample`, described as `described`, matches.
    fn matches(&self, sample: &Sample, described: &Described) -> bool {
        if let Some(path) = &self.path
            && !described
                .url
                .as_ref()
                .is_some_and(|url| url.to_lowercase().contains(path))
        {
            return false;
        }
        if let Some(status) = self.status
            && !status.matches(described.status)
        {
            return false;
        }
        if let Some(model) = &self.model {
            let mut models = sample
                .parts()
                .flat_map(model_fields)
                .chain(described.url.as_deref().and_then(gemini_url_model));
            if !models.any(|name| name.to_lowercase().contains(model)) {
                return false;
            }
        }
        if let Some(q) = &self.q
            && !sample
                .parts()
                .any(|part| part.to_ascii_lowercase().contains(q.as_str()))
        {
            return false;
        }
        true
    }
}

/// A text filter, at most [`MAX_FILTER`] bytes.
fn text_filter(query: &Query, name: &str) -> Result<Option<String>, ApiError> {
    match query.non_empty(name)? {
        Some(text) if text.len() > MAX_FILTER => Err(ApiError::invalid(format!(
            "{name} must be at most {MAX_FILTER} bytes"
        ))),
        other => Ok(other.map(str::to_owned)),
    }
}

/// Which kinds of log a search lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    All,
    Request,
    Error,
}

/// The search's parameters.
#[derive(Debug)]
struct Search {
    from_ms: Option<i64>,
    to_ms: Option<i64>,
    kind: Kind,
    filters: Filters,
    limit: usize,
    after: Option<Named>,
}

/// The answer to a search.
#[derive(Debug, Default)]
struct Found {
    logs: Vec<Entry>,
    next: Option<String>,
    files: usize,
    bytes: u64,
    limit_reached: bool,
}

/// `GET /request-logs`.
pub(super) async fn search(
    State(state): State<DashboardState>,
    uri: Uri,
) -> Result<Response, ApiError> {
    let query = Query::of(&uri)?;
    let from_ms = query.time("from")?;
    let to_ms = query.time("to")?;
    if let (Some(from), Some(to)) = (from_ms, to_ms)
        && from >= to
    {
        return Err(ApiError::invalid("from must be before to"));
    }
    let kind = match query.non_empty("kind")?.unwrap_or("all") {
        "all" => Kind::All,
        "request" => Kind::Request,
        "error" => Kind::Error,
        _ => return Err(ApiError::invalid("kind must be request, error or all")),
    };
    let status = match query.non_empty("status")? {
        None => None,
        Some(text) => Some(StatusFilter::parse(text).ok_or_else(|| {
            ApiError::invalid("status must be a status such as 502, or a class such as 5xx")
        })?),
    };
    let filters = Filters {
        path: text_filter(&query, "path")?.map(|text| text.to_lowercase()),
        status,
        model: text_filter(&query, "model")?.map(|text| text.to_lowercase()),
        q: text_filter(&query, "q")?.map(|text| text.to_ascii_lowercase()),
    };
    let limit = query.count("limit", 1, 200, 50)?;
    let after = match query.non_empty("cursor")? {
        None => None,
        Some(cursor) => Some(decode_cursor(cursor).ok_or_else(invalid_cursor)?),
    };
    let search = Search {
        from_ms,
        to_ms,
        kind,
        filters,
        limit,
        after,
    };
    let dir = state.management.log_directory();
    let found = tokio::task::spawn_blocking(move || run_search(&dir, &search))
        .await
        .map_err(|error| ApiError::internal(format!("the search stopped: {error}")))?
        .map_err(|error| ApiError::internal(format!("list the log directory: {error}")))?;
    Ok(ok(&json!({
        "logs": found.logs,
        "next_cursor": found.next.as_deref().map(encode_cursor),
        "scanned": {
            "files": found.files,
            "bytes": found.bytes,
            "limit_reached": found.limit_reached,
        },
        "request_log": state.management.config().request_log,
    })))
}

/// The cursor that continues after the log `name`.
fn encode_cursor(name: &str) -> String {
    URL_SAFE_NO_PAD.encode(name)
}

/// The log a cursor continues after, if it is one this server gave.
fn decode_cursor(cursor: &str) -> Option<Named> {
    let bytes = URL_SAFE_NO_PAD.decode(cursor).ok()?;
    Named::parse(std::str::from_utf8(&bytes).ok()?)
}

fn invalid_cursor() -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "invalid_cursor",
        "the cursor isn't one this server gave",
    )
}

/// Runs `search` over the logs in `dir`.
fn run_search(dir: &Path, search: &Search) -> io::Result<Found> {
    let mut candidates = match list(dir, search) {
        Ok(candidates) => candidates,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Found::default()),
        Err(error) => return Err(error),
    };
    candidates.sort_unstable_by(|a, b| b.cmp(a));

    let mut found = Found::default();
    let mut last_read: Option<&Named> = None;
    for (index, named) in candidates.iter().enumerate() {
        if found.files >= MAX_FILES || found.bytes >= MAX_BYTES {
            found.limit_reached = true;
            found.next = last_read.map(|named| named.name.clone());
            break;
        }
        let path = dir.join(&named.name);
        let Ok((mut file, meta)) = open_log_file(&path) else {
            continue;
        };
        let Ok(sample) = Sample::read(&mut file, meta.len()) else {
            continue;
        };
        found.files += 1;
        found.bytes = found.bytes.saturating_add(sample.bytes);
        last_read = Some(named);
        let described = describe(&sample);
        if search.filters.any() && !search.filters.matches(&sample, &described) {
            continue;
        }
        found.logs.push(entry(named, &meta, described));
        if found.logs.len() == search.limit && index + 1 < candidates.len() {
            found.next = Some(named.name.clone());
            break;
        }
    }
    Ok(found)
}

/// The logs in `dir` whose names `search` takes, unsorted.
fn list(dir: &Path, search: &Search) -> io::Result<Vec<Named>> {
    let mut candidates = Vec::new();
    for entry in fs::read_dir(dir)?.take(MAX_NAMES) {
        let Ok(entry) = entry else {
            continue;
        };
        let file_name = entry.file_name();
        let Some(named) = file_name.to_str().and_then(Named::parse) else {
            continue;
        };
        let kind = named.kind();
        match search.kind {
            Kind::All => {}
            Kind::Request if kind == "request" => {}
            Kind::Error if kind == "error" => {}
            Kind::Request | Kind::Error => continue,
        }
        if search.from_ms.is_some() || search.to_ms.is_some() {
            let ms = named.utc_ms();
            if search.from_ms.is_some_and(|from| ms < from)
                || search.to_ms.is_some_and(|to| ms >= to)
            {
                continue;
            }
        }
        if search.after.as_ref().is_some_and(|after| named >= *after) {
            continue;
        }
        candidates.push(named);
    }
    Ok(candidates)
}

/// The entry of the log `named`, with `meta`, described as `described`.
fn entry(named: &Named, meta: &Metadata, described: Described) -> Entry {
    Entry {
        name: named.name.clone(),
        kind: named.kind(),
        request_id: named.request_id(),
        time: format_time(named.utc_ms()),
        size: meta.len(),
        modified: meta
            .modified()
            .ok()
            .and_then(system_time_ms)
            .map(format_time),
        method: described.method,
        url: described.url,
        status: described.status,
        model: described.model,
    }
}

/// `time` in milliseconds since the epoch.
fn system_time_ms(time: SystemTime) -> Option<i64> {
    Some(DateTime::<Utc>::from(time).timestamp_millis())
}

/// `GET /request-logs/{name}`.
pub(super) async fn read(
    State(state): State<DashboardState>,
    UrlPath(name): UrlPath<String>,
    uri: Uri,
) -> Result<Response, ApiError> {
    let query = Query::of(&uri)?;
    let offset = query.int("offset", 0, i64::MAX, 0)?;
    let length = query.int("length", 1, MAX_LENGTH, DEFAULT_LENGTH)?;
    let named = Named::parse(&name).ok_or_else(no_such_log)?;
    let path = state.management.log_directory().join(&named.name);
    let offset = u64::try_from(offset).unwrap_or(0);
    let length = u64::try_from(length).unwrap_or(0);
    let answer = tokio::task::spawn_blocking(move || read_piece(&path, &named, offset, length))
        .await
        .map_err(|error| ApiError::internal(format!("the read stopped: {error}")))??;
    Ok(ok(&answer))
}

fn no_such_log() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such log")
}

/// A piece of a log, as `GET /request-logs/{name}` answers it.
#[derive(Debug, Serialize)]
struct Piece {
    log: Entry,
    offset: u64,
    next_offset: Option<u64>,
    content: String,
}

/// The log at `path`, opened as the management API's log routes open one
/// ([`open_log_file`]), and its metadata, which is the handle's.
fn open(path: &Path) -> Result<(File, Metadata), ApiError> {
    open_log_file(path).map_err(|error| {
        if is_refused_log_file(&error) {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_log_file",
                "the log is a link, isn't a plain file, or has another hard link",
            )
        } else if error.kind() == io::ErrorKind::NotFound {
            no_such_log()
        } else {
            ApiError::internal(format!("open the log: {error}"))
        }
    })
}

/// Reads `length` bytes from `offset` of the log `named` at `path`.
fn read_piece(path: &Path, named: &Named, offset: u64, length: u64) -> Result<Piece, ApiError> {
    let (mut file, meta) = open(path)?;
    let size = meta.len();
    if offset > size {
        return Err(ApiError::invalid(format!(
            "offset is past the end of the log, which is {size} bytes"
        )));
    }
    let read_error = |error: io::Error| ApiError::internal(format!("read the log: {error}"));
    let sample = Sample::read(&mut file, size).map_err(read_error)?;
    let mut bytes = read_at(&mut file, offset, length).map_err(read_error)?;
    let end = offset.saturating_add(bytes.len() as u64);
    if end < size {
        let keep = complete_utf8_len(&bytes);
        if keep > 0 {
            bytes.truncate(keep);
        }
    }
    let end = offset.saturating_add(bytes.len() as u64);
    Ok(Piece {
        log: entry(named, &meta, describe(&sample)),
        offset,
        next_offset: (end < size).then_some(end),
        content: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

/// `GET /request-logs/{name}/download`: the log whole, as on disk, as a
/// file to save.
pub(super) async fn download(
    State(state): State<DashboardState>,
    UrlPath(name): UrlPath<String>,
) -> Result<Response, ApiError> {
    let named = Named::parse(&name).ok_or_else(no_such_log)?;
    let path = state.management.log_directory().join(&named.name);
    let (file, meta) = tokio::task::spawn_blocking(move || open(&path))
        .await
        .map_err(|error| ApiError::internal(format!("the read stopped: {error}")))??;
    let size = meta.len();
    let body = Body::from_stream(send_file(tokio::fs::File::from_std(file), size));
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
    headers.insert(
        header::CONTENT_DISPOSITION,
        content_disposition(&named.name),
    );
    Ok(response)
}

/// The first `size` bytes of `file`, [`CHUNK`] bytes at a time. Should the
/// file end before them, the stream ends with an error, which cuts the
/// answer off short of its `Content-Length` rather than ending it as a
/// shorter file.
fn send_file(file: tokio::fs::File, size: u64) -> impl Stream<Item = io::Result<Bytes>> {
    stream::unfold(Some((file, size)), |sending| async move {
        let (mut file, left) = sending?;
        if left == 0 {
            return None;
        }
        let want = usize::try_from(left).map_or(CHUNK, |left| left.min(CHUNK));
        let mut buffer = vec![0; want];
        match file.read(&mut buffer).await {
            Ok(0) => {
                tracing::warn!("dashboard API: a log got shorter while it was downloaded");
                let error = io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the log got shorter while it was sent",
                );
                Some((Err(error), None))
            }
            Ok(read) => {
                buffer.truncate(read);
                let left = left.saturating_sub(u64::try_from(read).unwrap_or(left));
                Some((Ok(Bytes::from(buffer)), Some((file, left))))
            }
            Err(error) => Some((Err(error), None)),
        }
    })
}

/// The `Content-Disposition` of a download of the log `name`: an
/// attachment named `name`. A name with a character a quoted header can't
/// hold, which the request log never writes, has it as `_` in `filename`,
/// and is given exactly in `filename*` (RFC 6266).
fn content_disposition(name: &str) -> HeaderValue {
    let plain: String = name
        .chars()
        .map(|c| match c {
            '"' | '\\' => '_',
            ' '..='~' => c,
            _ => '_',
        })
        .collect();
    let mut value = format!("attachment; filename=\"{plain}\"");
    if plain != name {
        value.push_str("; filename*=UTF-8''");
        for &byte in name.as_bytes() {
            if byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&byte) {
                value.push(char::from(byte));
            } else {
                let _ = write!(value, "%{byte:02X}");
            }
        }
    }
    HeaderValue::from_str(&value).unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

/// How much of `bytes` to keep so that it doesn't end inside a UTF-8
/// character: all of it, or up to three bytes less.
fn complete_utf8_len(bytes: &[u8]) -> usize {
    let len = bytes.len();
    // The start of the last character: back over at most three
    // continuation bytes.
    let mut start = len;
    for back in 1..=4 {
        let Some(index) = len.checked_sub(back) else {
            return len;
        };
        let Some(&byte) = bytes.get(index) else {
            return len;
        };
        if byte & 0xc0 != 0x80 {
            start = index;
            break;
        }
    }
    let Some(&lead) = bytes.get(start) else {
        return len;
    };
    let needed = match lead {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return len,
    };
    if len - start < needed { start } else { len }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not upstream's: a piece is cut before a character it would split,
    /// and only then.
    #[test]
    fn pieces_end_on_characters() {
        let text = "aé€😀".as_bytes();
        // a(1) é(2) €(3) 😀(4)
        assert_eq!(complete_utf8_len(text), text.len());
        assert_eq!(complete_utf8_len(&text[..2]), 1);
        assert_eq!(complete_utf8_len(&text[..3]), 3);
        assert_eq!(complete_utf8_len(&text[..5]), 3);
        assert_eq!(complete_utf8_len(&text[..9]), 6);
        assert_eq!(complete_utf8_len(&text[..8]), 6);
        assert_eq!(complete_utf8_len(b"ab\xff"), 3);
        assert_eq!(complete_utf8_len(b"\x80\x80\x80\x80"), 4);
        assert_eq!(complete_utf8_len(b""), 0);
    }

    /// Not upstream's: the method, URL, status and model are read from
    /// where the request logger writes them.
    #[test]
    fn logs_are_described() {
        let sample = Sample {
            head: "=== REQUEST INFO ===\nVersion: 1\nURL: /v1/chat/completions?x=1\nMethod: POST\n\
                   Timestamp: t\n\n\n=== HEADERS ===\n\n\n=== REQUEST BODY ===\n\
                   {\"model\" : \"gpt-5\\\"x\",\"stream\":true}\n\n\n=== API REQUEST 1 ===\n\
                   {\"model\":\"upstream\"}\n\n\n=== RESPONSE ===\nStatus: 502\n\n{\"model\":\"m2\"}\n"
                .to_owned(),
            tail: None,
            bytes: 0,
        };
        assert_eq!(
            describe(&sample),
            Described {
                method: Some("POST".to_owned()),
                url: Some("/v1/chat/completions?x=1".to_owned()),
                status: Some(502),
                model: Some("gpt-5\"x".to_owned()),
            }
        );

        let gemini = Sample {
            head: "=== REQUEST INFO ===\nURL: /v1beta/models/gemini-2.5-pro:generateContent\n\
                   Method: POST\n\n\n=== REQUEST BODY ===\n{}\n\n\n=== RESPONSE ===\nStatus: 200\n"
                .to_owned(),
            tail: None,
            bytes: 0,
        };
        assert_eq!(describe(&gemini).model.as_deref(), Some("gemini-2.5-pro"));

        let answered = Sample {
            head: "=== REQUEST INFO ===\nURL: /v1/responses\nMethod: POST\n\n\n\
                   === REQUEST BODY ===\n{}\n\n\n"
                .to_owned(),
            tail: Some(
                "data: {}\n=== RESPONSE ===\nStatus: 200\n\n{\"model\":\"answered\"}".to_owned(),
            ),
            bytes: 0,
        };
        let described = describe(&answered);
        assert_eq!(described.status, Some(200));
        assert_eq!(described.model.as_deref(), Some("answered"));
    }

    /// Not upstream's: only request and error logs' names are taken, and
    /// they sort newest first by time, then sequence, then name.
    #[test]
    fn names_are_parsed_and_ordered() {
        assert!(Named::parse("main.log").is_none());
        assert!(Named::parse("main-2026-10-05T11-58-02.000.log").is_none());
        assert!(Named::parse("../v1-2026-10-05T115802-1234abcd.log").is_none());
        assert!(Named::parse("v1-2026-10-05T115802-1234abcd.txt").is_none());
        let a = Named::parse("v1-chat-2026-10-05T115802-1234abcd.log").unwrap();
        let b = Named::parse("v1-chat-2026-10-05T115802_1-1234abcd.log").unwrap();
        let c = Named::parse("error-v1-chat-2026-10-05T115803-99999999.log").unwrap();
        assert_eq!(a.kind(), "request");
        assert_eq!(c.kind(), "error");
        assert_eq!(a.request_id(), "1234abcd");
        assert_eq!(b.request_id(), "1234abcd");
        let mut names = vec![a.clone(), c.clone(), b.clone()];
        names.sort_unstable_by(|x, y| y.cmp(x));
        assert_eq!(names, vec![c, b, a]);
    }

    /// Not upstream's: a status filter takes a status or a class.
    #[test]
    fn status_filters() {
        assert_eq!(StatusFilter::parse("502"), Some(StatusFilter::Exact(502)));
        assert_eq!(StatusFilter::parse("4xx"), Some(StatusFilter::Class(4)));
        assert_eq!(StatusFilter::parse("6xx"), None);
        assert_eq!(StatusFilter::parse("99"), None);
        assert_eq!(StatusFilter::parse("abc"), None);
        assert!(StatusFilter::Class(5).matches(Some(503)));
        assert!(!StatusFilter::Class(5).matches(Some(404)));
        assert!(!StatusFilter::Exact(200).matches(None));
    }

    /// Not upstream's: a download is named in a quoted `filename`; a name
    /// a quoted header can't hold has `_` there and is exact in
    /// `filename*`.
    #[test]
    fn downloads_are_named() {
        let name = "v1-chat-completions-2026-10-05T115802-1234abcd.log";
        assert_eq!(
            content_disposition(name),
            format!("attachment; filename=\"{name}\"")
        );
        assert_eq!(
            content_disposition("v1-caf\u{e9} \"x\"%-2026-10-05T115802-1234abcd.log"),
            "attachment; filename=\"v1-caf_ _x_%-2026-10-05T115802-1234abcd.log\"; \
             filename*=UTF-8''v1-caf%C3%A9%20%22x%22%25-2026-10-05T115802-1234abcd.log"
        );
    }

    /// Not upstream's: a download sends the size the log had when it was
    /// opened, and fails, rather than ending early, if the log gets
    /// shorter.
    #[tokio::test]
    async fn downloads_send_the_size_opened() {
        use futures_util::StreamExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        let bytes: Vec<u8> = (0..=255u8).cycle().take(CHUNK * 2 + 7).collect();
        fs::write(&path, &bytes).unwrap();

        let (file, meta) = open(&path).unwrap();
        fs::write(&path, [&bytes[..], &b"appended"[..]].concat()).unwrap();
        let mut sent = Vec::new();
        let mut pieces = 0;
        let mut stream = std::pin::pin!(send_file(tokio::fs::File::from_std(file), meta.len()));
        while let Some(piece) = stream.next().await {
            sent.extend_from_slice(&piece.unwrap());
            pieces += 1;
        }
        assert_eq!(sent, bytes);
        assert!(pieces >= 3, "{pieces}");

        let (file, meta) = open(&path).unwrap();
        let shorter = fs::OpenOptions::new().write(true).open(&path).unwrap();
        shorter.set_len(10).unwrap();
        drop(shorter);
        let mut stream = std::pin::pin!(send_file(tokio::fs::File::from_std(file), meta.len()));
        assert_eq!(stream.next().await.unwrap().unwrap().len(), 10);
        let error = stream.next().await.unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert!(stream.next().await.is_none());
    }
}
