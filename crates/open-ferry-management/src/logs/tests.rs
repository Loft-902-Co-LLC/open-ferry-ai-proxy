//! Tests of the helpers of `crate::logs` that upstream's tests reach only
//! through `GetLogs`, if at all. None of them is upstream's.

use std::fs::{self, File};
use std::io::Write as _;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{Local, TimeZone};

use super::read::{MAX_LINE, scan_lines};
use super::timestamp::parse_timestamp;
use super::{
    collect_log_files, decode_log_cursor, is_allowed_log_cursor_file, parse_cutoff, parse_limit,
    rotation_order,
};

/// The Unix time of a local time on 2026-06-15.
fn june_15(hour: u32, minute: u32, second: u32) -> i64 {
    Local
        .with_ymd_and_hms(2026, 6, 15, hour, minute, second)
        .single()
        .unwrap()
        .timestamp()
}

/// Not upstream's: a line's time, after an optional `[`, is read as Go's
/// `time.ParseInLocation` reads `2006-01-02 15:04:05`.
#[test]
fn line_times_are_read_as_go_reads_them() {
    for (line, want) in [
        ("[2026-06-15 10:00:00] text", june_15(10, 0, 0)),
        ("2026-06-15 10:00:01 text", june_15(10, 0, 1)),
        ("2026-06-15 23:59:59", june_15(23, 59, 59)),
        // A space matches one or more, and the hour may be one digit.
        ("2026-06-15  9:00:00", june_15(9, 0, 0)),
        ("[2026-06-15  9:00:00] text", june_15(9, 0, 0)),
    ] {
        assert_eq!(parse_timestamp(line.as_bytes()), want, "{line:?}");
    }
    for line in [
        "",
        "[2026-06-15",
        "2026-06-15 10:00",
        "[[2026-06-15 10:00:00]",
        "2026-13-15 10:00:00",
        "2026-00-15 10:00:00",
        "2026-02-30 10:00:00",
        "2026-06-15 24:00:00",
        "2026-06-15 10:60:00",
        "2026-06-15 10:00:60",
        "2026-6-15 10:00:00 x",
        "2026-06-15T10:00:00",
        "+026-06-15 10:00:00",
        "2026-06-15 9:00:00x",
        // The 19 bytes read take in what follows a one-digit hour.
        "[2026-06-15 9:00:00] text",
        "no time on this line",
    ] {
        assert_eq!(parse_timestamp(line.as_bytes()), 0, "{line:?}");
    }
}

/// Not upstream's: the rotations' names and how they sort.
#[test]
fn rotations_are_named_as_upstream_names_them() {
    assert_eq!(rotation_order("main.log.1"), Some(1));
    assert_eq!(rotation_order("main.log.12"), Some(12));
    assert_eq!(rotation_order("main.log.+3"), Some(3));
    let plain = rotation_order("main-2026-06-15T10-00-00.log").unwrap();
    assert_eq!(plain, i64::MAX - june_15(10, 0, 0));
    for name in [
        "main-2026-06-15T10-00-00.log.gz",
        "main-2026-06-15T10-00-00.123.log",
        "main-2026-06-15T10-00-00.123.log.gz",
    ] {
        assert_eq!(rotation_order(name), Some(plain), "{name}");
    }
    assert!(rotation_order("main-2026-06-15T10-00-01.log").unwrap() < plain);
    for name in [
        "main.log",
        "main.log.",
        "main.log.x",
        "main.log.1.gz",
        "main-.log",
        "main-2026-06-15T10-00-00",
        "main-2026-06-15T10-00-00.gz",
        "main-2026-06-15 10-00-00.log",
        "main-2026-06-15T10:00:00.log",
        "error-x.log",
        "other.log",
    ] {
        assert_eq!(rotation_order(name), None, "{name}");
    }
}

/// Not upstream's: `main.log` and its rotations are listed oldest first,
/// `main.log` last; other files and directories aren't.
#[test]
fn log_files_are_listed_oldest_first() {
    let dir = tempfile::tempdir().unwrap();
    for name in [
        "main.log",
        "main.log.1",
        "main.log.2",
        "main-2026-06-15T10-00-00.log",
        "main-2026-06-14T10-00-00.123.log.gz",
        "error-x.log",
        "notes.txt",
    ] {
        fs::write(dir.path().join(name), "").unwrap();
    }
    fs::create_dir(dir.path().join("main.log.3")).unwrap();

    let names: Vec<_> = collect_log_files(dir.path())
        .unwrap()
        .iter()
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        [
            "main-2026-06-14T10-00-00.123.log.gz",
            "main-2026-06-15T10-00-00.log",
            "main.log.2",
            "main.log.1",
            "main.log",
        ]
    );
}

/// Not upstream's: `limit` and `after` as upstream parses them.
#[test]
fn query_values_are_parsed_as_upstream_parses_them() {
    assert_eq!(parse_limit(b""), Ok(0));
    assert_eq!(parse_limit(b"  "), Ok(0));
    assert_eq!(parse_limit(b" 5 "), Ok(5));
    assert_eq!(parse_limit(b"+7"), Ok(7));
    for raw in ["abc", "1.5", "5x", "99999999999999999999"] {
        assert_eq!(
            parse_limit(raw.as_bytes()),
            Err("must be a positive integer"),
            "{raw}"
        );
    }
    for raw in ["0", "-1", "-0"] {
        assert_eq!(
            parse_limit(raw.as_bytes()),
            Err("must be greater than zero"),
            "{raw}"
        );
    }

    assert_eq!(parse_cutoff(b" 1750000000 "), 1_750_000_000);
    for raw in ["", "abc", "0", "-5", "1.5"] {
        assert_eq!(parse_cutoff(raw.as_bytes()), 0, "{raw}");
    }
}

/// Not upstream's: a cursor may only name `main.log` or a rotation, by
/// bare name.
#[test]
fn cursor_files_are_bare_log_names() {
    for name in ["main.log", "main.log.1", "main-2026-06-15T10-00-00.log.gz"] {
        assert!(is_allowed_log_cursor_file(name), "{name}");
    }
    for name in [
        "",
        ".",
        "..",
        "main.log/",
        "logs/main.log",
        "logs\\main.log.1",
        "error-x.log",
        "main.log.x",
    ] {
        assert!(!is_allowed_log_cursor_file(name), "{name}");
    }
}

/// The cursor holding `json`.
fn cursor_of(json: &str) -> Result<super::LogCursor, &'static str> {
    decode_log_cursor(URL_SAFE_NO_PAD.encode(json).as_bytes())
}

/// Not upstream's: a cursor is read as Go's `json.Unmarshal` reads one into
/// upstream's struct: names matched ignoring case, `null` leaving a field
/// alone, unknown names ignored, padding and line breaks allowed, and
/// anything after the object, or a number that isn't an `int64`, refused.
#[test]
fn cursors_are_read_as_go_reads_them() {
    let cursor = cursor_of(
        r#"{"V":1,"FILE":"main.log","offset":5,"size":null,"modtime":7,"extra":[1],"fingerprint":"f"}"#,
    )
    .unwrap();
    assert_eq!(
        (cursor.version, cursor.file.as_str(), cursor.offset),
        (1, "main.log", 5)
    );
    assert_eq!((cursor.size, cursor.mod_time), (0, 7));

    let json = r#"{"v":1,"file":"main.log","fingerprint":"f"}"#;
    let padded = base64::engine::general_purpose::URL_SAFE.encode(json);
    assert!(padded.ends_with('='), "{padded}");
    let (head, tail) = padded.split_at(8);
    assert!(decode_log_cursor(format!(" {head}\r\n{tail} ").as_bytes()).is_ok());

    for (json, error) in [
        (
            r#"{"v":1,"file":"main.log","fingerprint":"f"} {}"#,
            "invalid cursor payload",
        ),
        (
            r#"{"v":1,"file":"main.log","offset":1.5,"fingerprint":"f"}"#,
            "invalid cursor payload",
        ),
        (
            r#"{"v":1,"file":"main.log","offset":"1","fingerprint":"f"}"#,
            "invalid cursor payload",
        ),
        (r#"[1]"#, "invalid cursor payload"),
        (
            r#"{"v":2,"file":"main.log","fingerprint":"f"}"#,
            "unsupported cursor version",
        ),
        (
            r#"{"v":1,"file":"main.log","offset":-1,"fingerprint":"f"}"#,
            "invalid cursor position",
        ),
        (
            r#"{"v":1,"file":"main.log","fingerprint":" "}"#,
            "invalid cursor fingerprint",
        ),
    ] {
        assert_eq!(cursor_of(json), Err(error), "{json}");
    }
    assert_eq!(decode_log_cursor(b" \n "), Err("empty cursor"));
    assert_eq!(
        decode_log_cursor(b"not base64!"),
        Err("invalid cursor encoding")
    );
}

/// Not upstream's: a cursor's numbers are read as Go reads an `int64`, from
/// the number as written: `-0` is 0, and `1e2` or one past the range is
/// refused.
#[test]
fn cursor_numbers_are_read_as_go_reads_them() {
    let cursor = |offset: &str| {
        cursor_of(&format!(
            r#"{{"v":1,"file":"main.log","offset":{offset},"fingerprint":"f"}}"#
        ))
    };
    assert_eq!(cursor("-0").unwrap().offset, 0);
    assert_eq!(cursor("9223372036854775807").unwrap().offset, i64::MAX);
    for offset in ["1e2", "-0.0", "9223372036854775808", "true"] {
        assert_eq!(cursor(offset), Err("invalid cursor payload"), "{offset}");
    }
}

/// Writes `content` to a new file in `dir` and scans it.
fn scan(dir: &Path, content: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let path = dir.join("scan.log");
    File::create(&path).unwrap().write_all(content).unwrap();
    let mut lines = Vec::new();
    scan_lines(File::open(&path).unwrap(), |line| lines.push(line.to_vec()))
        .map_err(|error| error.to_string())?;
    Ok(lines)
}

/// Not upstream's: lines are split as Go's `bufio.Scanner` splits them with
/// upstream's buffer.
#[test]
fn lines_are_scanned_as_go_scans_them() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(scan(dir.path(), b"").unwrap(), Vec::<Vec<u8>>::new());
    assert_eq!(
        scan(dir.path(), b"a\r\r\nb\n\nlast").unwrap(),
        [b"a".to_vec(), b"b".to_vec(), Vec::new(), b"last".to_vec()]
    );

    let longest = vec![b'x'; MAX_LINE - 1];
    let mut content = longest.clone();
    content.extend_from_slice(b"\nnext\n");
    assert_eq!(
        scan(dir.path(), &content).unwrap(),
        [longest, b"next".to_vec()]
    );

    let too_long = vec![b'x'; MAX_LINE];
    assert_eq!(
        scan(dir.path(), &too_long),
        Err("bufio.Scanner: token too long".to_owned())
    );
    let mut content = too_long;
    content.push(b'\n');
    assert_eq!(
        scan(dir.path(), &content),
        Err("bufio.Scanner: token too long".to_owned())
    );
}
