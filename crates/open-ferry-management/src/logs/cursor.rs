// Ported from CLIProxyAPI internal/api/handlers/management/logs.go
// (logCursor, readLogFilesFromCursor, locateLogCursorFile,
// shouldDeferEmptyMainCursorToRotated, shouldResetAmbiguousEmptyMainCursor,
// logFileChangedAfterCursor, logFileMatchesCursor, encodeLogCursor,
// decodeLogCursor, validateLogCursor, newLogCursor,
// cursorFingerprintBoundary, cursorModTimeUnixNano, logFileFingerprint,
// writeFileRange) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The cursor `GET /v0/management/logs` answers with, and reading on from
//! one.
//!
//! A cursor is a JSON object, base64url-encoded without padding: the
//! version (1), the file's name (`main.log` or a rotation's, never a
//! path), the offset after the last line read, the file's size and
//! modification time then, the latest time read, and a fingerprint of the
//! file up to the offset (of the whole file when the offset is 0), from
//! its first and last 4 KiB. Reading on finds the file the cursor was in,
//! by name or, once `main.log` has been rotated, by fingerprint, and reads
//! the complete lines after the offset, then those of the newer files.
//!
//! Deviations from upstream: a file is looked at through a handle of it
//! opened as the routes open one, which refuses links (see
//! [`open_log_file`](crate::log_dir::open_log_file)), and a cursor's
//! fingerprint is taken from the handle the file's size came from;
//! upstream follows links, and opens the file again for the fingerprint.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use base64::Engine as _;
use base64::alphabet::URL_SAFE;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use open_ferry_translate::go::trim_space;
use serde::de::{Error as _, IgnoredAny, MapAccess};
use sha2::{Digest, Sha256};

use super::read::{ReadResult, read_complete_log_lines};
use super::{
    MAIN_LOG, file_name, file_size, invalid, is_allowed_log_cursor_file, is_not_found,
    open_log_file,
};
use crate::bind::{GoStruct, decode, set_string};
use crate::go::lossy;
use crate::json::Json;
use crate::log_dir::safe_log_file_path;

/// The cursor's version (upstream's `logCursorVersion`).
const VERSION: i64 = 1;

/// How much of each end of a file its fingerprint covers (upstream's
/// `logCursorFingerprintMax`).
const FINGERPRINT_MAX: i64 = 4 * 1024;

/// How much is hashed at a time.
const CHUNK: usize = 32 * 1024;

/// Go's `base64.RawURLEncoding`: unpadded, and lenient about the unused
/// bits of the last character, as Go is.
const URL_RAW: GeneralPurpose = GeneralPurpose::new(
    &URL_SAFE,
    GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_allow_trailing_bits(true)
        .with_decode_padding_mode(DecodePaddingMode::RequireNone),
);

/// Go's `base64.URLEncoding`: padded.
const URL_PADDED: GeneralPurpose = GeneralPurpose::new(
    &URL_SAFE,
    GeneralPurposeConfig::new()
        .with_decode_allow_trailing_bits(true)
        .with_decode_padding_mode(DecodePaddingMode::RequireCanonical),
);

/// Where a read stopped (upstream's `logCursor`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LogCursor {
    /// `v`.
    pub(crate) version: i64,
    /// The file's name.
    pub(crate) file: String,
    pub(crate) offset: i64,
    pub(crate) size: i64,
    /// `modTime`: Unix seconds.
    pub(crate) mod_time: i64,
    /// `modTimeUnixNano`, left out when 0.
    pub(crate) mod_time_unix_nano: i64,
    /// `latestTimestamp`.
    pub(crate) latest_timestamp: i64,
    pub(crate) fingerprint: String,
}

impl GoStruct for LogCursor {
    const FIELDS: &'static [&'static str] = &[
        "v",
        "file",
        "offset",
        "size",
        "modTime",
        "modTimeUnixNano",
        "latestTimestamp",
        "fingerprint",
    ];

    fn set<'de, A: MapAccess<'de>>(&mut self, index: usize, map: &mut A) -> Result<(), A::Error> {
        let field = match index {
            0 => &mut self.version,
            1 => return set_string(&mut self.file, map),
            2 => &mut self.offset,
            3 => &mut self.size,
            4 => &mut self.mod_time,
            5 => &mut self.mod_time_unix_nano,
            6 => &mut self.latest_timestamp,
            _ => return set_string(&mut self.fingerprint, map),
        };
        // As Go reads an `int64`: the number as written, so `-0` is 0 and
        // `1.0` is refused.
        if let Some(number) = map.next_value::<Option<serde_json::Number>>()? {
            *field = number
                .as_str()
                .parse()
                .map_err(|_| A::Error::custom("number is not an int64"))?;
        }
        Ok(())
    }
}

/// How a file compares with a cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Match {
    /// It is the file the cursor was in.
    Same,
    Different,
    /// It is shorter than the cursor's offset or fingerprint.
    Truncated,
}

/// Reads on from the cursor `raw`: the complete lines after it, at most
/// `limit` when it isn't 0, and the cursor after them (upstream's
/// `readLogFilesFromCursor`). The flag says the cursor can't be followed
/// and the files should be read from their end instead.
pub(super) fn read_log_files_from_cursor(
    dir: &Path,
    files: &[PathBuf],
    raw: &[u8],
    limit: usize,
) -> io::Result<(ReadResult, bool)> {
    let Ok(cursor) = decode_log_cursor(raw) else {
        return Ok((ReadResult::default(), true));
    };
    let mut result = ReadResult {
        latest: cursor.latest_timestamp,
        next_cursor: raw.to_vec(),
        ..ReadResult::default()
    };
    if safe_log_file_path(dir, &cursor.file, is_allowed_log_cursor_file).is_err() {
        return Ok((result, true));
    }
    let Some(start) = locate_log_cursor_file(files, &cursor)? else {
        return Ok((result, true));
    };

    let mut current_path = files.get(start);
    let mut current_offset = cursor.offset;
    let mut advanced = false;
    for (index, path) in files.iter().enumerate().skip(start) {
        let remaining = if limit > 0 {
            match limit.saturating_sub(result.count) {
                0 => break,
                remaining => remaining,
            }
        } else {
            0
        };
        let offset = if index == start { cursor.offset } else { 0 };
        let (read, segment) = match read_complete_log_lines(path, offset, None, remaining) {
            Ok(found) => found,
            Err(error) if is_not_found(&error) => return Ok((result, true)),
            Err(error) => return Err(error),
        };
        if read.count > 0 {
            result.segments.push(segment);
            result.count += read.count;
            result.latest = result.latest.max(read.latest);
            current_path = Some(path);
            current_offset = read.end_offset;
            advanced = true;
        }
        if read.hit_limit {
            break;
        }
    }
    let Some(current_path) = current_path.filter(|_| advanced) else {
        return Ok((result, false));
    };

    match new_log_cursor(current_path, current_offset, result.latest) {
        Ok(next) => {
            result.next_cursor = next.into_bytes();
            Ok((result, false))
        }
        Err(error) if is_not_found(&error) => Ok((result, true)),
        Err(error) => Err(error),
    }
}

/// The index in `files` of the file `cursor` was in: the file of its name
/// if it still matches, else, for `main.log`, the rotation that does
/// (upstream's `locateLogCursorFile`).
fn locate_log_cursor_file(files: &[PathBuf], cursor: &LogCursor) -> io::Result<Option<usize>> {
    let empty_main = is_empty_main(cursor);
    let mut defer_empty_main_match = false;
    let named = files
        .iter()
        .enumerate()
        .rfind(|(_, path)| file_name(path) == cursor.file);
    if let Some((index, path)) = named {
        match log_file_matches_cursor(path, cursor) {
            Ok(Match::Same) => {
                if should_defer_empty_main_cursor_to_rotated(files, cursor) {
                    defer_empty_main_match = true;
                } else if should_reset_ambiguous_empty_main_cursor(files, index, cursor) {
                    return Ok(None);
                } else {
                    return Ok(Some(index));
                }
            }
            Ok(Match::Different | Match::Truncated) => {}
            Err(error) if is_not_found(&error) => return Ok(None),
            Err(error) => return Err(error),
        }
    }

    if cursor.file != MAIN_LOG || (empty_main && !defer_empty_main_match) {
        return Ok(None);
    }
    let rotated = files
        .iter()
        .enumerate()
        .filter(|(_, path)| file_name(path) != MAIN_LOG);
    if empty_main {
        // The oldest rotation written since the cursor was made.
        for (index, path) in rotated {
            if !log_file_changed_after_cursor(path, cursor) {
                continue;
            }
            match log_file_matches_cursor(path, cursor) {
                Ok(Match::Same) => return Ok(Some(index)),
                Ok(Match::Different | Match::Truncated) => {}
                Err(error) if is_not_found(&error) => {}
                Err(error) => return Err(error),
            }
        }
        return Ok(None);
    }
    for (index, path) in rotated.rev() {
        match log_file_matches_cursor(path, cursor) {
            Ok(Match::Same) => return Ok(Some(index)),
            Ok(Match::Different | Match::Truncated) => {}
            Err(error) if is_not_found(&error) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

/// Whether `cursor` is at the start of an empty `main.log`, which every
/// file matches.
fn is_empty_main(cursor: &LogCursor) -> bool {
    cursor.file == MAIN_LOG && cursor.offset == 0 && cursor.size == 0
}

/// Whether a cursor at the start of an empty `main.log` should look for a
/// rotation first: one has been written since the cursor was made
/// (upstream's `shouldDeferEmptyMainCursorToRotated`).
fn should_defer_empty_main_cursor_to_rotated(files: &[PathBuf], cursor: &LogCursor) -> bool {
    is_empty_main(cursor)
        && files
            .iter()
            .any(|path| file_name(path) != MAIN_LOG && log_file_changed_after_cursor(path, cursor))
}

/// Whether a cursor at the start of an empty `main.log` that has changed
/// since can't tell which rotation followed it: a rotation that isn't
/// empty wasn't written after it (upstream's
/// `shouldResetAmbiguousEmptyMainCursor`).
fn should_reset_ambiguous_empty_main_cursor(
    files: &[PathBuf],
    main_index: usize,
    cursor: &LogCursor,
) -> bool {
    if !is_empty_main(cursor) {
        return false;
    }
    let Some(info) = files.get(main_index).and_then(|path| file_info(path).ok()) else {
        return false;
    };
    if file_size(&info) == cursor.size && mod_time(&info).1 == cursor_mod_time_unix_nano(cursor) {
        return false;
    }
    files.iter().enumerate().any(|(index, path)| {
        index != main_index
            && file_name(path) != MAIN_LOG
            && file_info(path).is_ok_and(|info| info.len() != 0)
            && !log_file_changed_after_cursor(path, cursor)
    })
}

/// Whether the file at `path` isn't empty and was modified after the
/// cursor's file (upstream's `logFileChangedAfterCursor`).
fn log_file_changed_after_cursor(path: &Path, cursor: &LogCursor) -> bool {
    file_info(path)
        .is_ok_and(|info| info.len() != 0 && mod_time(&info).1 > cursor_mod_time_unix_nano(cursor))
}

/// The metadata of the file at `path`, opened as the routes open one: a
/// directory or a link fails.
fn file_info(path: &Path) -> io::Result<fs::Metadata> {
    open_log_file(path).map(|(_, info)| info)
}

/// How the file at `path` compares with `cursor` (upstream's
/// `logFileMatchesCursor`).
fn log_file_matches_cursor(path: &Path, cursor: &LogCursor) -> io::Result<Match> {
    let (mut file, info) = open_log_file(path)?;
    let size = file_size(&info);
    let boundary = cursor_fingerprint_boundary(cursor.offset, cursor.size);
    if size < cursor.offset || size < boundary {
        return Ok(Match::Truncated);
    }
    let fingerprint = log_file_fingerprint(&mut file, size, boundary)?;
    Ok(if fingerprint == cursor.fingerprint {
        Match::Same
    } else {
        Match::Different
    })
}

/// `cursor` as JSON, base64url-encoded without padding (upstream's
/// `encodeLogCursor`).
pub(crate) fn encode_log_cursor(cursor: &LogCursor) -> String {
    let mut fields = vec![
        ("v", Json::Int(cursor.version)),
        ("file", Json::Str(cursor.file.clone())),
        ("offset", Json::Int(cursor.offset)),
        ("size", Json::Int(cursor.size)),
        ("modTime", Json::Int(cursor.mod_time)),
    ];
    if cursor.mod_time_unix_nano != 0 {
        fields.push(("modTimeUnixNano", Json::Int(cursor.mod_time_unix_nano)));
    }
    fields.push(("latestTimestamp", Json::Int(cursor.latest_timestamp)));
    fields.push(("fingerprint", Json::Str(cursor.fingerprint.clone())));
    URL_RAW.encode(Json::Struct(fields).encode())
}

/// The cursor `raw` holds, or why it is refused (upstream's
/// `decodeLogCursor`): base64url without padding, else with it, Go's line
/// breaks skipped, holding a JSON object as Go's `json.Unmarshal` reads it
/// into the struct, and valid.
pub(crate) fn decode_log_cursor(raw: &[u8]) -> Result<LogCursor, &'static str> {
    let value = trim_space(raw);
    if value.is_empty() {
        return Err("empty cursor");
    }
    let value: Vec<u8> = value
        .iter()
        .copied()
        .filter(|b| !matches!(b, b'\r' | b'\n'))
        .collect();
    let data = URL_RAW
        .decode(&value)
        .or_else(|_| URL_PADDED.decode(&value))
        .map_err(|_| "invalid cursor encoding")?;
    // `json.Unmarshal` refuses anything after the value, which `decode`
    // ignores.
    let text = lossy(&data);
    if serde_json::from_str::<IgnoredAny>(&text).is_err() {
        return Err("invalid cursor payload");
    }
    let cursor = decode::<LogCursor>(text.as_bytes()).ok_or("invalid cursor payload")?;
    validate_log_cursor(&cursor)?;
    Ok(cursor)
}

/// Why `cursor` is refused, if it is (upstream's `validateLogCursor`).
fn validate_log_cursor(cursor: &LogCursor) -> Result<(), &'static str> {
    if cursor.version != VERSION {
        return Err("unsupported cursor version");
    }
    if !is_allowed_log_cursor_file(&cursor.file) {
        return Err("invalid cursor file");
    }
    if cursor.offset < 0 || cursor.size < 0 || cursor.mod_time < 0 || cursor.latest_timestamp < 0 {
        return Err("invalid cursor position");
    }
    if trim_space(cursor.fingerprint.as_bytes()).is_empty() {
        return Err("invalid cursor fingerprint");
    }
    Ok(())
}

/// A cursor at `offset` in the file at `path`, carrying `latest`
/// (upstream's `newLogCursor`).
pub(crate) fn new_log_cursor(path: &Path, offset: i64, latest: i64) -> io::Result<String> {
    let (mut file, info) = open_log_file(path)?;
    let size = file_size(&info);
    if offset < 0 || offset > size {
        return Err(invalid("invalid cursor offset"));
    }
    let boundary = cursor_fingerprint_boundary(offset, size);
    let fingerprint = log_file_fingerprint(&mut file, size, boundary)?;
    let (seconds, nanos) = mod_time(&info);
    Ok(encode_log_cursor(&LogCursor {
        version: VERSION,
        file: file_name(path),
        offset,
        size,
        mod_time: seconds,
        mod_time_unix_nano: nanos,
        latest_timestamp: latest,
        fingerprint,
    }))
}

/// How much of the file a cursor's fingerprint covers: up to its offset,
/// or the whole file when the offset is 0 (upstream's
/// `cursorFingerprintBoundary`).
fn cursor_fingerprint_boundary(offset: i64, size: i64) -> i64 {
    if offset == 0 && size > 0 {
        size
    } else {
        offset
    }
}

/// The cursor's file's modification time in Unix nanoseconds (upstream's
/// `cursorModTimeUnixNano`).
pub(crate) fn cursor_mod_time_unix_nano(cursor: &LogCursor) -> i64 {
    if cursor.mod_time_unix_nano > 0 {
        cursor.mod_time_unix_nano
    } else {
        cursor.mod_time.wrapping_mul(1_000_000_000)
    }
}

/// A file's modification time as Go's `Time.Unix` and `Time.UnixNano` give
/// it.
fn mod_time(info: &fs::Metadata) -> (i64, i64) {
    let modified = info.modified().unwrap_or(UNIX_EPOCH);
    let nanos: i128 = match modified.duration_since(UNIX_EPOCH) {
        Ok(after) => i128::try_from(after.as_nanos()).unwrap_or(i128::MAX),
        Err(before) => i128::try_from(before.duration().as_nanos()).map_or(i128::MIN, |n| -n),
    };
    let seconds = i64::try_from(nanos.div_euclid(1_000_000_000)).unwrap_or(i64::MAX);
    // Go's `UnixNano` wraps outside int64's range, as `as` does.
    (seconds, nanos as i64)
}

/// The fingerprint of `file`, `size` bytes long, up to `boundary`: a hash
/// of the boundary and the first and last 4 KiB before it (upstream's
/// `logFileFingerprint`).
fn log_file_fingerprint(file: &mut File, size: i64, boundary: i64) -> io::Result<String> {
    if boundary < 0 {
        return Err(invalid("invalid fingerprint boundary"));
    }
    if boundary > size {
        return Err(invalid("invalid fingerprint boundary"));
    }
    let mut hash = Sha256::new();
    hash.update(format!("log-cursor-v1:{boundary}:"));
    let first_len = boundary.min(FINGERPRINT_MAX);
    write_file_range(&mut hash, file, 0, first_len)?;
    let tail_len = boundary.min(FINGERPRINT_MAX);
    let tail_start = boundary - tail_len;
    hash.update(format!(":{tail_start}:"));
    write_file_range(&mut hash, file, tail_start, tail_len)?;
    let sum = hash.finalize();
    Ok(URL_RAW.encode(sum.get(..12).unwrap_or_default()))
}

/// Hashes `length` bytes of `file` from `start`; the file ending first
/// fails with Go's `EOF` (upstream's `writeFileRange`).
fn write_file_range(hash: &mut Sha256, file: &mut File, start: i64, length: i64) -> io::Result<()> {
    let Ok(mut remaining) = usize::try_from(length) else {
        return Ok(());
    };
    file.seek(SeekFrom::Start(u64::try_from(start).unwrap_or_default()))?;
    let mut buf = vec![0; CHUNK];
    while remaining > 0 {
        let want = remaining.min(CHUNK);
        let Some(chunk) = buf.get_mut(..want) else {
            break;
        };
        match file.read(chunk) {
            Ok(0) => return Err(io::Error::other("EOF")),
            Ok(n) => {
                hash.update(chunk.get(..n).unwrap_or_default());
                remaining = remaining.saturating_sub(n);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}
