// Ported from CLIProxyAPI internal/api/handlers/management/logs.go
// (logAccumulator's file scan, completeLogRead, logReadResult,
// tailLogFiles, readTailLogLines, tailStartOffset, cursorForLatestLogFile,
// readCompleteLogLines, completeLogBoundary) (v8.0.10, MIT), with Go's
// bufio/scan.go (Scanner, ScanLines) (go1.27, BSD-3-Clause).
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/golang/go

//! Reading `main.log` and its rotations: every line of a file, the
//! complete lines after an offset, where the last complete line ends, and
//! the last lines of the files.
//!
//! Deviations from upstream: a file is opened as the routes open one,
//! which refuses links (see
//! [`open_log_file`](crate::log_dir::open_log_file)), and the last lines of
//! a file are found and read through one handle of it, where upstream
//! opens it for each step.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::cursor::new_log_cursor;
use super::timestamp::parse_timestamp;
use super::{file_size, invalid, is_not_found, open_log_file};

/// The longest line read (upstream's `logScannerMaxBuffer`).
pub(super) const MAX_LINE: usize = 8 * 1024 * 1024;

/// How much is read at a time.
const CHUNK: usize = 32 * 1024;

/// The buffer a whole file's lines are first read with (upstream's
/// `logScannerInitialBuffer`).
const SCAN_BUFFER: usize = 64 * 1024;

/// What a read of complete lines found (upstream's `completeLogRead`).
#[derive(Debug, Default)]
pub(crate) struct CompleteRead {
    /// The lines, without their `\n` and trailing `\r`s.
    pub(crate) lines: Vec<Vec<u8>>,
    /// The offset after the last line read.
    pub(crate) end_offset: i64,
    /// The latest time a line starts with, or 0.
    pub(crate) latest: i64,
    /// Whether the read stopped at its limit.
    pub(crate) hit_limit: bool,
}

/// Lines read from the files, the latest time in them, and the cursor
/// after them (upstream's `logReadResult`).
#[derive(Debug, Default)]
pub(super) struct ReadResult {
    pub(super) lines: Vec<Vec<u8>>,
    pub(super) latest: i64,
    /// A Go string: a cursor given back as it came may not be UTF-8.
    pub(super) next_cursor: Vec<u8>,
}

/// Calls `each` with each line of `file`, as Go's `bufio.Scanner` splits
/// lines with a buffer of at most [`MAX_LINE`] bytes: the last line needn't
/// end with `\n`, trailing `\r`s are trimmed, and a line of [`MAX_LINE`]
/// bytes or more fails the read.
pub(super) fn scan_lines(file: File, mut each: impl FnMut(&[u8])) -> io::Result<()> {
    let mut reader = BufReader::with_capacity(SCAN_BUFFER, file);
    let mut line = Vec::new();
    loop {
        let data = match reader.fill_buf() {
            Ok(data) => data,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if data.is_empty() {
            if !line.is_empty() {
                each(trim_right_cr(&line));
            }
            return Ok(());
        }
        let (segment, used, complete) = match data.iter().position(|&b| b == b'\n') {
            Some(index) => (data.get(..index).unwrap_or_default(), index + 1, true),
            None => (data, data.len(), false),
        };
        if line.len() + segment.len() >= MAX_LINE {
            return Err(io::Error::other("bufio.Scanner: token too long"));
        }
        line.extend_from_slice(segment);
        reader.consume(used);
        if complete {
            each(trim_right_cr(&line));
            line.clear();
        }
    }
}

/// The complete lines of the file at `path` from `offset` up to
/// `max_offset` (the end of the file when `None` or past it), at most
/// `limit` of them when it isn't 0 (upstream's `readCompleteLogLines`). A
/// line over [`MAX_LINE`] bytes fails the read.
pub(crate) fn read_complete_log_lines(
    path: &Path,
    offset: i64,
    max_offset: Option<i64>,
    limit: usize,
) -> io::Result<CompleteRead> {
    let (mut file, info) = open_log_file(path)?;
    read_complete_lines(&mut file, file_size(&info), offset, max_offset, limit)
}

/// [`read_complete_log_lines`] of `file`, `size` bytes long.
fn read_complete_lines(
    file: &mut File,
    size: i64,
    offset: i64,
    max_offset: Option<i64>,
    limit: usize,
) -> io::Result<CompleteRead> {
    let start = u64::try_from(offset).map_err(|_| invalid("invalid log offset"))?;
    let max_offset = max_offset.filter(|&max| max <= size).unwrap_or(size);
    if offset > max_offset {
        return Err(invalid("invalid log offset"));
    }
    file.seek(SeekFrom::Start(start))?;
    let mut section = file.take(u64::try_from(max_offset - offset).unwrap_or_default());

    let mut result = CompleteRead {
        end_offset: offset,
        ..CompleteRead::default()
    };
    let mut current = offset;
    let mut buf = vec![0; CHUNK];
    let mut line = Vec::new();
    loop {
        let n = match section.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let mut data = buf.get(..n).unwrap_or_default();
        while !data.is_empty() {
            let Some(index) = data.iter().position(|&b| b == b'\n') else {
                if line.len() + data.len() > MAX_LINE {
                    return Err(line_too_long());
                }
                line.extend_from_slice(data);
                current += len_i64(data.len());
                break;
            };
            let (segment, rest) = data.split_at_checked(index).unwrap_or((data, &[]));
            if line.len() + segment.len() > MAX_LINE {
                return Err(line_too_long());
            }
            line.extend_from_slice(segment);
            current += len_i64(index) + 1;
            let text = trim_right_cr(&line).to_vec();
            result.latest = result.latest.max(parse_timestamp(&text));
            result.lines.push(text);
            result.end_offset = current;
            line.clear();
            if limit > 0 && result.lines.len() >= limit {
                result.hit_limit = true;
                return Ok(result);
            }
            data = rest.get(1..).unwrap_or_default();
        }
    }
    Ok(result)
}

/// The error of a line over [`MAX_LINE`] bytes.
fn line_too_long() -> io::Error {
    io::Error::other(format!("log line exceeds {MAX_LINE} bytes"))
}

/// The offset just after the last `\n` in the file at `path`, or 0
/// (upstream's `completeLogBoundary`).
pub(crate) fn complete_log_boundary(path: &Path) -> io::Result<i64> {
    let (mut file, info) = open_log_file(path)?;
    log_boundary(&mut file, file_size(&info))
}

/// [`complete_log_boundary`] of `file`, `size` bytes long.
fn log_boundary(file: &mut File, size: i64) -> io::Result<i64> {
    let mut buf = vec![0; CHUNK];
    let mut pos = size;
    while pos > 0 {
        let chunk = pos.min(len_i64(CHUNK));
        pos -= chunk;
        let data = read_at(file, &mut buf, chunk, pos)?;
        if let Some(index) = data.iter().rposition(|&b| b == b'\n') {
            return Ok(pos + len_i64(index) + 1);
        }
    }
    Ok(0)
}

/// Where the last `limit` complete lines before `boundary` start in
/// `file`; 0 when `limit` is 0 or there are no more lines than that
/// (upstream's `tailStartOffset`).
fn tail_start_offset(file: &mut File, boundary: i64, limit: usize) -> io::Result<i64> {
    if limit == 0 {
        return Ok(0);
    }
    let mut buf = vec![0; CHUNK];
    let mut pos = boundary;
    let mut line_breaks = 0;
    while pos > 0 {
        let chunk = pos.min(len_i64(CHUNK));
        pos -= chunk;
        let mut data = read_at(file, &mut buf, chunk, pos)?;
        while let Some(index) = data.iter().rposition(|&b| b == b'\n') {
            line_breaks += 1;
            if line_breaks > limit {
                return Ok(pos + len_i64(index) + 1);
            }
            data = data.get(..index).unwrap_or_default();
        }
    }
    Ok(0)
}

/// Reads up to `len` bytes at `pos` into `buf`, as Go's `File.ReadAt`
/// does: as many as there are, fewer only at the end of the file.
fn read_at<'a>(file: &mut File, buf: &'a mut [u8], len: i64, pos: i64) -> io::Result<&'a [u8]> {
    file.seek(SeekFrom::Start(u64::try_from(pos).unwrap_or_default()))?;
    let len = usize::try_from(len).unwrap_or_default().min(buf.len());
    let mut filled = 0;
    while filled < len {
        let Some(rest) = buf.get_mut(filled..len) else {
            break;
        };
        match file.read(rest) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(buf.get(..filled).unwrap_or_default())
}

/// The last `limit` complete lines of the file at `path`, all of them when
/// `limit` is 0 (upstream's `readTailLogLines`), read from one handle of
/// it, where upstream opens it for each step.
fn read_tail_log_lines(path: &Path, limit: usize) -> io::Result<CompleteRead> {
    let (mut file, info) = open_log_file(path)?;
    let size = file_size(&info);
    let boundary = log_boundary(&mut file, size)?;
    if boundary == 0 {
        return Ok(CompleteRead::default());
    }
    let start = tail_start_offset(&mut file, boundary, limit)?;
    read_complete_lines(&mut file, size, start, Some(boundary), limit)
}

/// The last `limit` complete lines of `files`, oldest first, all of them
/// when `limit` is 0; the latest time in them, or `fallback_latest`; and a
/// cursor at the end of the newest file (upstream's `tailLogFiles`).
/// Missing files are skipped.
pub(super) fn tail_log_files(
    files: &[PathBuf],
    limit: usize,
    fallback_latest: i64,
) -> io::Result<ReadResult> {
    let mut result = ReadResult {
        latest: fallback_latest,
        ..ReadResult::default()
    };
    for path in files.iter().rev() {
        let remaining = if limit > 0 {
            match limit.saturating_sub(result.lines.len()) {
                0 => break,
                remaining => remaining,
            }
        } else {
            0
        };
        let read = match read_tail_log_lines(path, remaining) {
            Ok(read) => read,
            Err(error) if is_not_found(&error) => continue,
            Err(error) => return Err(error),
        };
        if read.lines.is_empty() {
            continue;
        }
        let mut lines = read.lines;
        lines.append(&mut result.lines);
        result.lines = lines;
        result.latest = result.latest.max(read.latest);
    }
    result.next_cursor = cursor_for_latest_log_file(files, result.latest)?.into_bytes();
    Ok(result)
}

/// A cursor at the end of the last complete line of the newest file there
/// is, carrying `latest`; empty when there is none (upstream's
/// `cursorForLatestLogFile`).
pub(super) fn cursor_for_latest_log_file(files: &[PathBuf], latest: i64) -> io::Result<String> {
    for path in files.iter().rev() {
        let boundary = match complete_log_boundary(path) {
            Ok(boundary) => boundary,
            Err(error) if is_not_found(&error) => continue,
            Err(error) => return Err(error),
        };
        match new_log_cursor(path, boundary, latest) {
            Ok(cursor) => return Ok(cursor),
            Err(error) if is_not_found(&error) => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(String::new())
}

/// `line` without its trailing `\r`s.
pub(super) fn trim_right_cr(mut line: &[u8]) -> &[u8] {
    while let Some(rest) = line.strip_suffix(b"\r") {
        line = rest;
    }
    line
}

/// A length as Go's `int64`.
pub(super) fn len_i64(len: usize) -> i64 {
    i64::try_from(len).unwrap_or(i64::MAX)
}
