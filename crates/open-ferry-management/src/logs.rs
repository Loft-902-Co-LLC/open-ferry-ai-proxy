// Ported from CLIProxyAPI internal/api/handlers/management/logs.go
// (GetLogs, DeleteLogs, collectLogFiles, logAccumulator, writeLogsResponse,
// isAllowedLogCursorFile, parseCutoff, parseLimit, isRotatedLogFile,
// rotationOrder, numericRotationOrder, timestampRotationOrder) and
// internal/api/server_management.go and server_management_v8.go (their
// routes) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The main log's routes, for clients with the management key:
//!
//! - `GET /v0/management/logs` (`/v8/management/observability/logs`) reads
//!   `main.log` and its rotations (`main.log.N`, and
//!   `main-<local time>.log`, gzipped or not), oldest first. `limit` alone
//!   gives the last lines; `after` (Unix seconds) the lines logged after
//!   that time, scanning every file, and `line-count` counts every line
//!   scanned; neither gives every line. `cursor`, from an earlier answer's
//!   `next-cursor`, gives the complete lines written since, at most
//!   `limit`; a cursor that can't be followed answers the last lines, with
//!   `cursor-reset`. `latest-timestamp` is the latest time a line starts
//!   with.
//! - `DELETE /v0/management/logs` (`/v8/management/observability/logs`)
//!   empties `main.log` and removes its rotations, answering how many it
//!   removed.
//!
//! Both answer 400 while `logging-to-file` is off. The files are read and
//! removed on the blocking pool. `GET` reads the files twice: once to
//! count the lines and find where the answer's are, then again from the
//! same handles as the body is sent (see `body`). On Windows a file
//! another process holds without sharing it is retried a few times before
//! the removal fails.
//!
//! Deviations from upstream:
//! - `GET` sends its lines as it reads them, holding only a few chunks of
//!   the body at a time, where upstream gathers them all; the body's
//!   bytes are upstream's (see `body`).
//! - A file that is a symbolic link or other reparse point, isn't a plain
//!   file, or has more than one hard link is refused, not followed: `GET`
//!   and `DELETE` fail with `invalid log file` (see
//!   [`open_log_file`](crate::log_dir::open_log_file)). A rotation is
//!   checked so before it is removed; the removal takes only its name out
//!   of the directory, so a file swapped in between is never reached.
//! - An I/O error's text is Rust's, without Go's operation and path.
//! - Rotated files of the same order are listed in the order Go's
//!   insertion sort gives at any count, where Go's sort of twelve or more
//!   leaves them in no set order.

mod body;
mod cursor;
mod read;
#[cfg(test)]
mod tests;
mod timestamp;

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::extract::{RawQuery, State};
use axum::response::Response;
use axum::routing::get;
use http::StatusCode;
use open_ferry_translate::go::trim_space;

use self::cursor::{decode_log_cursor, read_log_files_from_cursor};
use self::read::{
    ReadResult, Segment, cursor_for_latest_log_file, len_i64, scan_lines, tail_log_files,
    trim_right_cr,
};
use self::timestamp::{ROTATION_LAYOUT, parse_local, parse_timestamp};
use crate::Route;
use crate::auth_files::run_blocking;
use crate::go::atoi;
use crate::json::{self, Json};
use crate::log_dir::{self, Access, log_directory};
use crate::query::Query;
use crate::state::ManagementState;

#[cfg(test)]
pub(crate) use self::cursor::{
    LogCursor, cursor_mod_time_unix_nano, decode_log_cursor as decode_cursor, encode_log_cursor,
    new_log_cursor,
};
#[cfg(test)]
pub(crate) use self::read::{complete_log_boundary, complete_log_lines};

/// The active log's name (upstream's `defaultLogFileName`).
const MAIN_LOG: &str = "main.log";

/// How long to wait before each retry of a file another process holds.
const SHARING_RETRIES: [Duration; 3] = [
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
];

/// The module's routes.
pub(crate) fn routes() -> Vec<Route> {
    ["/v0/management/logs", "/v8/management/observability/logs"]
        .into_iter()
        .map(|path| Route::key(path, get(get_logs).delete(delete_logs)))
        .collect()
}

/// A failed request: its status and error.
type Failure = (StatusCode, String);

/// An error answered with a 500.
fn internal(message: String) -> Failure {
    (StatusCode::INTERNAL_SERVER_ERROR, message)
}

/// `GET /v0/management/logs` (upstream's `GetLogs`).
async fn get_logs(State(state): State<ManagementState>, RawQuery(raw): RawQuery) -> Response {
    let dir = match log_files_directory(&state) {
        Ok(dir) => dir,
        Err((status, message)) => return json::error(status, &message),
    };
    let query = Query::parse(raw.as_deref());
    let cursor = trim_space(query.value("cursor")).to_vec();
    let after = query.value("after").to_vec();
    let limit = query.value("limit").to_vec();
    match run_blocking(move || read_logs(&dir, &cursor, &after, &limit)).await {
        Ok(page) => body::respond(page).await,
        Err((status, message)) => json::error(status, &message),
    }
}

/// `DELETE /v0/management/logs` (upstream's `DeleteLogs`).
async fn delete_logs(State(state): State<ManagementState>) -> Response {
    let dir = match log_files_directory(&state) {
        Ok(dir) => dir,
        Err((status, message)) => return json::error(status, &message),
    };
    match run_blocking(move || clear_logs(&dir)).await {
        Ok(removed) => json::response(
            StatusCode::OK,
            &Json::map([
                ("success", Json::Bool(true)),
                ("message", Json::Str("Logs cleared successfully".to_owned())),
                ("removed", Json::Int(len_i64(removed))),
            ]),
        ),
        Err((status, message)) => json::error(status, &message),
    }
}

/// The log directory, once logging to file is on and the directory is
/// known.
fn log_files_directory(state: &ManagementState) -> Result<PathBuf, Failure> {
    if !state.config().logging_to_file {
        return Err((
            StatusCode::BAD_REQUEST,
            "logging to file disabled".to_owned(),
        ));
    }
    let dir = log_directory(state);
    if trim_space(dir.as_os_str().as_encoded_bytes()).is_empty() {
        return Err(internal("log directory not configured".to_owned()));
    }
    Ok(dir)
}

/// What `GET /v0/management/logs` answers, before its lines are read
/// again to send them.
#[derive(Debug, Default)]
struct Page {
    /// Where the lines are, oldest first.
    segments: Vec<Segment>,
    /// Which of the segments' lines the answer has.
    filter: Filter,
    /// How many of those are left out first: all but the last `limit`.
    skip: usize,
    /// How many lines the answer has, at most, after those.
    wanted: usize,
    line_count: usize,
    latest: i64,
    next_cursor: Vec<u8>,
    cursor_reset: bool,
}

impl Page {
    /// The page of `result`, counting the lines it holds.
    fn of(result: ReadResult, cursor_reset: bool) -> Self {
        Self {
            segments: result.segments,
            wanted: result.count,
            line_count: result.count,
            latest: result.latest,
            next_cursor: result.next_cursor,
            cursor_reset,
            ..Self::default()
        }
    }
}

/// Reads the logs in `dir` as `GET /v0/management/logs` does, given its
/// trimmed `cursor`, `after` and `limit`.
fn read_logs(dir: &Path, cursor: &[u8], after: &[u8], limit: &[u8]) -> Result<Page, Failure> {
    let files = match collect_log_files(dir) {
        Ok(files) => files,
        Err(error) if is_not_found(&error) => {
            let mut latest = parse_cutoff(after);
            if !cursor.is_empty()
                && let Ok(cursor) = decode_log_cursor(cursor)
            {
                latest = latest.max(cursor.latest_timestamp);
            }
            return Ok(Page {
                latest,
                cursor_reset: !cursor.is_empty(),
                ..Page::default()
            });
        }
        Err(error) => return Err(internal(format!("failed to list log files: {error}"))),
    };

    let limit = parse_limit(limit)
        .map_err(|message| (StatusCode::BAD_REQUEST, format!("invalid limit: {message}")))?;
    let cutoff = parse_cutoff(after);
    let read_failed = |error: io::Error| internal(format!("failed to read log files: {error}"));
    if !cursor.is_empty() {
        let (result, reset) =
            read_log_files_from_cursor(dir, &files, cursor, limit).map_err(read_failed)?;
        if reset {
            let result = tail_log_files(&files, limit, result.latest).map_err(read_failed)?;
            return Ok(Page::of(result, true));
        }
        return Ok(Page::of(result, false));
    }

    if cutoff == 0 && limit > 0 {
        let result = tail_log_files(&files, limit, 0).map_err(read_failed)?;
        return Ok(Page::of(result, false));
    }

    let mut accumulator = Accumulator::new(cutoff, limit);
    for path in &files {
        accumulator
            .consume_file(path)
            .map_err(|error| internal(format!("failed to read log file: {error}")))?;
    }
    let mut latest = accumulator.latest;
    if latest == 0 || latest < cutoff {
        latest = cutoff;
    }
    let next_cursor = cursor_for_latest_log_file(&files, latest)
        .map_err(|error| internal(format!("failed to prepare log cursor: {error}")))?;
    Ok(accumulator.into_page(latest, next_cursor.into_bytes()))
}

/// Which lines a scan of every file keeps (upstream's `logAccumulator`'s
/// `cutoff` and `include`): those after `cutoff`, a line without a time
/// going with the line before it; every line when `cutoff` is 0.
#[derive(Clone, Copy, Debug, Default)]
struct Filter {
    cutoff: i64,
    /// Whether the last line with a time was after the cutoff.
    include: bool,
}

impl Filter {
    fn new(cutoff: i64) -> Self {
        Self {
            cutoff,
            include: false,
        }
    }

    /// Whether the next line, starting with the time `timestamp` (0 for
    /// none), is kept (upstream's `logAccumulator.addLine`).
    fn admits(&mut self, timestamp: i64) -> bool {
        if timestamp > 0 {
            self.include = self.cutoff == 0 || timestamp > self.cutoff;
        }
        self.cutoff == 0 || self.include
    }
}

/// A scan of every file (upstream's `logAccumulator`): the lines it
/// keeps are counted, and the files that may hold the answer's lines are
/// kept open to read them again; the answer has the last `limit` of them
/// when it isn't 0.
#[derive(Debug, Default)]
struct Accumulator {
    filter: Filter,
    limit: usize,
    /// The files that may hold the answer's lines, oldest first.
    files: VecDeque<Scanned>,
    /// The lines kept in those files.
    held: usize,
    /// Every line scanned.
    total: usize,
    latest: i64,
}

/// A file scanned and kept open: its lines, the filter as it was when it
/// began, and how many lines it kept.
#[derive(Debug)]
struct Scanned {
    segment: Segment,
    filter: Filter,
    kept: usize,
}

impl Accumulator {
    fn new(cutoff: i64, limit: usize) -> Self {
        Self {
            filter: Filter::new(cutoff),
            limit,
            ..Self::default()
        }
    }

    /// Adds the lines of the file at `path`, if there is one, then closes
    /// the oldest files none of whose lines the answer can have: those
    /// that kept none, and with a limit those before the last `limit` lines
    /// kept.
    fn consume_file(&mut self, path: &Path) -> io::Result<()> {
        let mut file = match open_log_file(path) {
            Ok((file, _)) => file,
            Err(error) if is_not_found(&error) => return Ok(()),
            Err(error) => return Err(error),
        };
        let filter = self.filter;
        let mut kept = 0;
        let end = scan_lines(&mut file, |line| {
            if self.add_line(line) {
                kept += 1;
            }
            Ok(())
        })?;
        let segment = Segment {
            file,
            start: 0,
            end,
        };
        self.files.push_back(Scanned {
            segment,
            filter,
            kept,
        });
        self.held += kept;
        while let Some(oldest) = self.files.front() {
            let rest = self.held - oldest.kept;
            if oldest.kept > 0 && (self.limit == 0 || rest < self.limit) {
                break;
            }
            self.held = rest;
            self.files.pop_front();
        }
        Ok(())
    }

    /// The answer, with `latest` and `next_cursor`: the last `limit` lines
    /// kept, every one when it is 0, read again from the files held, the
    /// first read from the filter as it began.
    fn into_page(self, latest: i64, next_cursor: Vec<u8>) -> Page {
        let skip = if self.limit > 0 {
            self.held.saturating_sub(self.limit)
        } else {
            0
        };
        let filter = self.files.front().map_or(self.filter, |file| file.filter);
        Page {
            segments: self.files.into_iter().map(|file| file.segment).collect(),
            filter,
            skip,
            wanted: self.held - skip,
            line_count: self.total,
            latest,
            next_cursor,
            cursor_reset: false,
        }
    }

    /// Counts a line, and whether it is kept.
    fn add_line(&mut self, raw: &[u8]) -> bool {
        let line = trim_right_cr(raw);
        self.total += 1;
        let timestamp = parse_timestamp(line);
        self.latest = self.latest.max(timestamp);
        self.filter.admits(timestamp)
    }
}

/// Empties `main.log` in `dir` and removes its rotations, answering how
/// many were removed (upstream's `DeleteLogs`).
fn clear_logs(dir: &Path) -> Result<usize, Failure> {
    let entries = match read_dir_sorted(dir) {
        Ok(entries) => entries,
        Err(error) if is_not_found(&error) => {
            return Err((StatusCode::NOT_FOUND, "log directory not found".to_owned()));
        }
        Err(error) => return Err(internal(format!("failed to list log directory: {error}"))),
    };
    let mut removed = 0;
    for entry in entries.into_iter().filter(|entry| !entry.is_dir) {
        if entry.name == MAIN_LOG {
            let truncate = || {
                log_dir::open_log_file(&entry.path, Access::Write)?
                    .0
                    .set_len(0)
            };
            match retry_shared(truncate) {
                Err(error) if !is_not_found(&error) => {
                    return Err(internal(format!("failed to truncate log file: {error}")));
                }
                _ => continue,
            }
        }
        if is_rotated_log_file(&entry.name) {
            let remove = || {
                drop(open_log_file(&entry.path)?);
                fs::remove_file(&entry.path)
            };
            match retry_shared(remove) {
                Err(error) if !is_not_found(&error) => {
                    return Err(internal(format!(
                        "failed to remove {}: {error}",
                        entry.name
                    )));
                }
                _ => removed += 1,
            }
        }
    }
    Ok(removed)
}

/// An entry of a directory.
struct Entry {
    /// Its name, each sequence that isn't valid Unicode replaced.
    name: String,
    path: PathBuf,
    /// Whether it is a directory itself, not a link to one.
    is_dir: bool,
}

/// The entries of `dir`, sorted by name, as Go's `os.ReadDir` lists them.
fn read_dir_sorted(dir: &Path) -> io::Result<Vec<Entry>> {
    let mut entries = fs::read_dir(dir)?
        .map(|entry| {
            let entry = entry?;
            Ok(Entry {
                name: entry.file_name().to_string_lossy().into_owned(),
                path: entry.path(),
                is_dir: entry.file_type()?.is_dir(),
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

/// The paths of `main.log` and its rotations in `dir`, oldest first
/// (upstream's `collectLogFiles`).
fn collect_log_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut candidates: Vec<(PathBuf, i64)> = read_dir_sorted(dir)?
        .into_iter()
        .filter(|entry| !entry.is_dir)
        .filter_map(|entry| {
            let order = if entry.name == MAIN_LOG {
                0
            } else {
                rotation_order(&entry.name)?
            };
            Some((entry.path, order))
        })
        .collect();
    candidates.sort_by_key(|&(_, order)| order);
    Ok(candidates.into_iter().rev().map(|(path, _)| path).collect())
}

/// Whether `name` may be a cursor's file: `main.log` or a rotation's name,
/// a bare name (upstream's `isAllowedLogCursorFile`). Neither can have a
/// volume name, so Go's `filepath.Base` check passes.
fn is_allowed_log_cursor_file(name: &str) -> bool {
    !(name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']))
        && (name == MAIN_LOG || is_rotated_log_file(name))
}

/// `after`: Unix seconds, or 0 when blank, not a number or not positive
/// (upstream's `parseCutoff`).
fn parse_cutoff(raw: &[u8]) -> i64 {
    std::str::from_utf8(trim_space(raw))
        .ok()
        .and_then(atoi)
        .filter(|&cutoff| cutoff > 0)
        .unwrap_or(0)
}

/// `limit`: 0 when blank (upstream's `parseLimit`).
fn parse_limit(raw: &[u8]) -> Result<usize, &'static str> {
    let value = trim_space(raw);
    if value.is_empty() {
        return Ok(0);
    }
    let limit = std::str::from_utf8(value)
        .ok()
        .and_then(atoi)
        .ok_or("must be a positive integer")?;
    usize::try_from(limit)
        .ok()
        .filter(|&limit| limit > 0)
        .ok_or("must be greater than zero")
}

/// Whether `name` is a rotation of `main.log` (upstream's
/// `isRotatedLogFile`).
fn is_rotated_log_file(name: &str) -> bool {
    rotation_order(name).is_some()
}

/// Where a rotation sorts, newest first after `main.log`'s 0: `main.log.N`
/// at N, then `main-<time>.log` and `main-<time>.log.gz` newest first
/// (upstream's `rotationOrder`).
fn rotation_order(name: &str) -> Option<i64> {
    numeric_rotation_order(name).or_else(|| timestamp_rotation_order(name))
}

/// `N` of `main.log.N` (upstream's `numericRotationOrder`).
fn numeric_rotation_order(name: &str) -> Option<i64> {
    name.strip_prefix("main.log.")
        .filter(|suffix| !suffix.is_empty())
        .and_then(atoi)
}

/// The order of `main-<time>[.<anything>].log[.gz]`, newest first, its
/// time read in the local zone (upstream's `timestampRotationOrder`).
fn timestamp_rotation_order(name: &str) -> Option<i64> {
    let clean = name.strip_prefix("main-")?;
    let clean = clean.strip_suffix(".gz").unwrap_or(clean);
    let clean = clean
        .strip_suffix(".log")
        .filter(|clean| !clean.is_empty())?;
    let clean = clean.split('.').next().unwrap_or(clean);
    let unix = parse_local(clean.as_bytes(), &ROTATION_LAYOUT)?;
    Some(i64::MAX.wrapping_sub(unix))
}

/// The file at `path`, opened to read, and its metadata; a link, anything
/// but a plain file and a file with another hard link are refused (see
/// [`log_dir::open_log_file`]).
fn open_log_file(path: &Path) -> io::Result<(File, fs::Metadata)> {
    log_dir::open_log_file(path, Access::Read)
}

/// A file's size as Go's `int64`.
fn file_size(info: &fs::Metadata) -> i64 {
    i64::try_from(info.len()).unwrap_or(i64::MAX)
}

/// The base name of `path`, each sequence that isn't valid Unicode
/// replaced.
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// An error with upstream's `message`.
fn invalid(message: &str) -> io::Error {
    io::Error::other(message.to_owned())
}

/// Whether `error` is Go's `os.ErrNotExist`.
fn is_not_found(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
}

/// Runs `op`, trying again a few times while another process holds the
/// file without sharing it, as Windows reports.
fn retry_shared<T>(mut op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    for delay in SHARING_RETRIES {
        match op() {
            Err(error) if is_sharing_error(&error) => std::thread::sleep(delay),
            result => return result,
        }
    }
    op()
}

/// Whether `error` is Windows refusing a file another process holds:
/// access denied, or a sharing or lock violation (32 and 33).
fn is_sharing_error(error: &io::Error) -> bool {
    cfg!(windows)
        && (error.kind() == io::ErrorKind::PermissionDenied
            || matches!(error.raw_os_error(), Some(32 | 33)))
}
