// Ported from CLIProxyAPI internal/logging/request_logger_writer.go
// (generateFilename, generateErrorFilename, createUniqueLogFile,
// sanitizeForFilename) and internal/api/handlers/management/logs.go
// (parseLogMetadata, logFileIsNewer) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The names of the request log's files: how a log is named when it is
//! written, how a name that is taken gets a sequence number, and how the
//! management routes tell which of two logs of a request is the newer.
//!
//! A request log is `<path>-<2006-01-02T150405>-<short request ID>.log`, the
//! path sanitized and the time local; a forced error log has `error-` in
//! front. A name already taken gets `_<n>` before its last `-`.
//!
//! Deviations from upstream:
//! - [`sanitize_for_filename`] also replaces `\`, and keeps at most
//!   [`MAX_SANITIZED`] characters, so a long path can't make a name Windows
//!   can't open.
//! - A log file is made with mode 0600 on Unix, where upstream's is 0644.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use chrono::{DateTime, Local, NaiveDate, NaiveDateTime};

use crate::observe::short_request_id;

/// The most characters a sanitized path keeps in a file name.
pub const MAX_SANITIZED: usize = 120;

/// How many sequence numbers a taken name tries (upstream's 1000).
const MAX_SEQUENCE: u32 = 1000;

/// The layout of the time in a log's name (Go's `2006-01-02T150405`).
const NAME_TIME_LAYOUT: &str = "%Y-%m-%dT%H%M%S";

/// The length of the time in a log's name.
const NAME_TIME_LEN: usize = 17;

/// The number given to a log of a request without an ID (upstream's
/// `requestLogID`).
static REQUEST_LOG_ID: AtomicU64 = AtomicU64::new(0);

/// `path` made safe for a file name (upstream's `sanitizeForFilename`):
/// `/`, `\`, `:`, `<`, `>`, `"`, `|`, `?`, `*` and white space become `-`,
/// runs of `-` become one, `-` is trimmed from both ends, and the result is
/// cut to [`MAX_SANITIZED`] characters; nothing left gives `root`.
pub fn sanitize_for_filename(path: &str) -> String {
    let mut sanitized = String::with_capacity(path.len());
    for c in path.chars() {
        let c = match c {
            '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*' => '-',
            // Go's `\s`.
            '\t' | '\n' | '\x0C' | '\r' | ' ' => '-',
            c => c,
        };
        if c == '-' && sanitized.ends_with('-') {
            continue;
        }
        sanitized.push(c);
    }
    let trimmed = sanitized.trim_matches('-');
    let cut = match trimmed.char_indices().nth(MAX_SANITIZED) {
        Some((end, _)) => trimmed.get(..end).unwrap_or(trimmed).trim_end_matches('-'),
        None => trimmed,
    };
    if cut.is_empty() {
        "root".to_owned()
    } else {
        cut.to_owned()
    }
}

/// The name of the log of a request to `url` with `request_id`, made at
/// `now` (upstream's `generateFilename`). A request without an ID gets the
/// next number instead.
pub(crate) fn filename(url: &str, request_id: &str, now: DateTime<Local>) -> String {
    let path = url.split('?').next().unwrap_or_default();
    let path = path.strip_prefix('/').unwrap_or(path);
    let id = if request_id.is_empty() {
        (REQUEST_LOG_ID.fetch_add(1, Ordering::Relaxed) + 1).to_string()
    } else {
        short_request_id(request_id).to_owned()
    };
    format!(
        "{}-{}-{id}.log",
        sanitize_for_filename(path),
        now.format(NAME_TIME_LAYOUT)
    )
}

/// The name of a forced error log (upstream's `generateErrorFilename`).
pub(crate) fn error_filename(url: &str, request_id: &str, now: DateTime<Local>) -> String {
    format!("error-{}", filename(url, request_id, now))
}

/// Whether `name` is a forced error log's: `error-*.log`.
pub fn is_error_log_name(name: &str) -> bool {
    name.starts_with("error-") && name.ends_with(".log")
}

/// Go's `filepath.Ext`: the suffix from the last `.` of `name`, or nothing.
fn extension(name: &str) -> &str {
    match name.rfind(['.', '/', '\\']) {
        Some(dot) if name.get(dot..dot + 1) == Some(".") => name.get(dot..).unwrap_or_default(),
        _ => "",
    }
}

/// Makes the file `filename` in `dir`, or, when that name is taken, the
/// first free `<prefix>_<n>-<id>.log` (upstream's `createUniqueLogFile`).
/// The file is made only if it doesn't exist, so a log is never written
/// over; it has mode 0600 on Unix.
pub(crate) fn create_unique_log_file(dir: &Path, filename: &str) -> io::Result<(File, PathBuf)> {
    let ext = extension(filename);
    let base = filename.strip_suffix(ext).unwrap_or(filename);
    let (prefix, id) = match base.rfind('-') {
        Some(idx) if idx > 0 => (
            base.get(..idx).unwrap_or(base),
            base.get(idx + 1..).unwrap_or_default(),
        ),
        _ => (base, ""),
    };

    let target = dir.join(filename);
    match create_new(&target) {
        Ok(file) => return Ok((file, target)),
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
        Err(_) => {}
    }
    for seq in 1..=MAX_SEQUENCE {
        let candidate = dir.join(format!("{prefix}_{seq}-{id}{ext}"));
        match create_new(&candidate) {
            Ok(file) => return Ok((file, candidate)),
            Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
            Err(_) => {}
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("too many conflicting log files for {filename}"),
    ))
}

/// Opens `path` for writing if it doesn't exist yet, with mode 0600 on
/// Unix.
fn create_new(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// What a log's name says of it (upstream's `logFileMeta`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogFileMeta {
    /// The name before its time, or before its ID when it has no time.
    pub prefix: String,
    /// The time in the name, when it has one.
    pub time: Option<NaiveDateTime>,
    /// The collision sequence number, or 0.
    pub seq: i64,
}

/// What `filename` says of its log (upstream's `parseLogMetadata`).
pub fn parse_log_metadata(filename: &str) -> LogFileMeta {
    let ext = extension(filename);
    let base = filename.strip_suffix(ext).unwrap_or(filename);
    let before_id = match base.rfind('-') {
        Some(idx) if idx > 0 => base.get(..idx).unwrap_or(base),
        _ => {
            return LogFileMeta {
                prefix: base.to_owned(),
                time: None,
                seq: 0,
            };
        }
    };

    let (mut seq, mut before_seq) = (0, before_id);
    if let Some(underscore) = before_id.rfind('_')
        && let Ok(parsed) = before_id
            .get(underscore + 1..)
            .unwrap_or_default()
            .parse::<i64>()
    {
        seq = parsed;
        before_seq = before_id.get(..underscore).unwrap_or(before_id);
    }

    let time = before_seq
        .len()
        .checked_sub(NAME_TIME_LEN)
        .and_then(|start| before_seq.get(start..))
        .and_then(parse_name_time);
    let Some(time) = time else {
        return LogFileMeta {
            prefix: before_seq.to_owned(),
            time: None,
            seq,
        };
    };
    let rest = before_seq.len() - NAME_TIME_LEN;
    let prefix = match rest.checked_sub(1) {
        Some(dash) if before_seq.as_bytes().get(dash) == Some(&b'-') => {
            before_seq.get(..dash).unwrap_or(before_seq)
        }
        _ => before_seq,
    };
    LogFileMeta {
        prefix: prefix.to_owned(),
        time: Some(time),
        seq,
    }
}

/// Go's `time.Parse("2006-01-02T150405", text)`.
fn parse_name_time(text: &str) -> Option<NaiveDateTime> {
    let bytes = text.as_bytes();
    if bytes.len() != NAME_TIME_LEN
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || bytes.get(10) != Some(&b'T')
    {
        return None;
    }
    let number = |start: usize, len: usize| -> Option<u32> {
        let digits = text.get(start..start + len)?;
        if !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        digits.parse().ok()
    };
    let year = i32::try_from(number(0, 4)?).ok()?;
    NaiveDate::from_ymd_opt(year, number(5, 2)?, number(8, 2)?)?.and_hms_opt(
        number(11, 2)?,
        number(13, 2)?,
        number(15, 2)?,
    )
}

/// Whether the log `candidate`, last changed at `candidate_mod`, is newer
/// than `current`, last changed at `current_mod` (`None`: unknown, which
/// any time is after) (upstream's `logFileIsNewer`): the later change wins;
/// on a tie the later time in the name, then the higher sequence number of
/// two names with the same prefix, then the greater name.
pub fn log_file_is_newer(
    candidate: &str,
    candidate_mod: SystemTime,
    current: &str,
    current_mod: Option<SystemTime>,
) -> bool {
    let Some(current_mod) = current_mod else {
        return true;
    };
    if candidate_mod > current_mod {
        return true;
    }
    if candidate_mod < current_mod {
        return false;
    }
    let candidate_meta = parse_log_metadata(candidate);
    let current_meta = parse_log_metadata(current);
    if let (Some(candidate_time), Some(current_time)) = (candidate_meta.time, current_meta.time) {
        if candidate_time != current_time {
            return candidate_time > current_time;
        }
        if candidate_meta.prefix == current_meta.prefix && candidate_meta.seq != current_meta.seq {
            return candidate_meta.seq > current_meta.seq;
        }
    }
    candidate > current
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::TimeZone;

    use super::*;

    // Not upstream's: sanitizing follows upstream's rules, and also
    // replaces `\` and cuts long paths.
    #[test]
    fn sanitizes_paths() {
        for (path, want) in [
            ("v1/chat/completions", "v1-chat-completions"),
            (
                "v1beta/models/gemini:generateContent",
                "v1beta-models-gemini-generateContent",
            ),
            ("a//b::c", "a-b-c"),
            ("-a b\t<c>|\"d\"?*-", "a-b-c-d"),
            ("a\\b", "a-b"),
            ("", "root"),
            ("///", "root"),
        ] {
            assert_eq!(sanitize_for_filename(path), want, "{path:?}");
        }
        let long = "x".repeat(300);
        assert_eq!(sanitize_for_filename(&long).len(), MAX_SANITIZED);
        let dashed = format!("{}/y", "x".repeat(MAX_SANITIZED - 1));
        assert_eq!(
            sanitize_for_filename(&dashed),
            "x".repeat(MAX_SANITIZED - 1)
        );
    }

    // Not upstream's: a name has the path, the local time and the short
    // request ID; an error log has `error-` in front.
    #[test]
    fn names_logs() {
        let now = Local.with_ymd_and_hms(2026, 9, 23, 12, 0, 5).unwrap();
        assert_eq!(
            filename(
                "/v1/chat/completions?key=x",
                "018f3a5b-1234-7abc-def0-12345678abcd",
                now
            ),
            "v1-chat-completions-2026-09-23T120005-5678abcd.log"
        );
        assert_eq!(
            error_filename("/", "req-1", now),
            "error-root-2026-09-23T120005-req-1.log"
        );
        let numbered = filename("/v1/messages", "", now);
        assert!(
            numbered.starts_with("v1-messages-2026-09-23T120005-"),
            "{numbered}"
        );
        assert!(is_error_log_name("error-root-x.log"));
        assert!(!is_error_log_name("root-x.log"));
        assert!(!is_error_log_name("error-root-x.log.gz"));
    }

    // Not upstream's: the parts of a name, as parseLogMetadata reads them.
    #[test]
    fn parses_log_names() {
        let time = NaiveDate::from_ymd_opt(2026, 9, 23)
            .unwrap()
            .and_hms_opt(12, 0, 0);
        let meta = |prefix: &str, time, seq| LogFileMeta {
            prefix: prefix.to_owned(),
            time,
            seq,
        };
        for (name, want) in [
            (
                "v1-responses-2026-09-23T120000-abcd1234.log",
                meta("v1-responses", time, 0),
            ),
            (
                "error-v1-responses-2026-09-23T120000_3-abcd1234.log",
                meta("error-v1-responses", time, 3),
            ),
            (
                "2026-09-23T120000-abcd1234.log",
                meta("2026-09-23T120000", time, 0),
            ),
            ("plain.log", meta("plain", None, 0)),
            ("-x.log", meta("-x", None, 0)),
            ("a_b-c.log", meta("a_b", None, 0)),
            ("a_7-c.log", meta("a", None, 7)),
            (
                "v1-2026-13-23T120000-id.log",
                meta("v1-2026-13-23T120000", None, 0),
            ),
            (
                "v1-2026-02-30T120000-id.log",
                meta("v1-2026-02-30T120000", None, 0),
            ),
        ] {
            assert_eq!(parse_log_metadata(name), want, "{name}");
        }
    }

    // Not upstream's: the later change wins, then the later name time, then
    // the higher sequence, then the greater name.
    #[test]
    fn compares_logs() {
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let later = base + Duration::from_secs(1);
        let a = "v1-2026-09-23T120000-id.log";
        let b = "v1-2026-09-23T120001-id.log";
        let a1 = "v1-2026-09-23T120000_1-id.log";
        let a2 = "v1-2026-09-23T120000_2-id.log";
        assert!(log_file_is_newer(a, base, b, None));
        assert!(log_file_is_newer(a, later, b, Some(base)));
        assert!(!log_file_is_newer(b, base, a, Some(later)));
        assert!(log_file_is_newer(b, base, a, Some(base)));
        assert!(!log_file_is_newer(a, base, b, Some(base)));
        assert!(log_file_is_newer(a2, base, a1, Some(base)));
        assert!(!log_file_is_newer(a1, base, a2, Some(base)));
        assert!(log_file_is_newer("z.log", base, "y.log", Some(base)));
    }
}
