// Ported from CLIProxyAPI internal/api/handlers/management/logs_test.go
// (TestDecodeLogCursorRejectsUnsafeFiles,
// TestLogCursorRoundTripOmitsAbsolutePath,
// TestReadCompleteLogLinesSkipsTrailingPartial,
// TestGetLogsTailLimitReturnsRecentLinesWithCursor,
// TestGetLogsTailLimitDoesNotScanOlderFilesForLineCount,
// TestGetLogsNoLimitKeepsFullScanBehavior,
// TestGetLogsAfterKeepsTimestampScanAndReturnsCursor,
// TestGetLogsCursorReturnsOnlyNewCompleteLines,
// TestGetLogsCursorRejectsOversizedLine,
// TestGetLogsCursorNoNewLinesKeepsCursorStable,
// TestGetLogsCursorDoesNotAdvancePastTrailingPartial,
// TestGetLogsCursorResetAfterTruncateTailsLimit,
// TestGetLogsCursorReadsAcrossRotation,
// TestGetLogsCursorReadsRotatedFileWhenNewMainIsSmaller,
// TestGetLogsZeroOffsetCursorWithPartialLineReadsAcrossRotation,
// TestGetLogsZeroOffsetCursorWithEmptyFileReadsAcrossRotation,
// TestGetLogsZeroOffsetCursorWithEmptyFileReadsAcrossTwoRotations,
// TestGetLogsZeroOffsetCursorWithEmptyFileResetsWhenRotationModTimeAmbiguous,
// TestGetLogsInvalidCursorResetsToTail,
// TestGetLogsMissingRotatedCursorFileResetsToTail,
// TestGetLogsMissingLogDirKeepsOKEmptyResponse,
// TestGetLogsLoggingDisabledKeepsBadRequest) (v8.0.20, MIT)
// https://github.com/router-for-me/CLIProxyAPI

//! Tests of the routes of `crate::logs`, and of the cursor helpers
//! upstream's tests call.
//!
//! Upstream's tests call the handler directly; these go through the router
//! with the key. The six `TestGetRequestLogByID_*` tests of logs_test.go
//! are `crate::request_logs`' (P3 WP-A) and are not here. Upstream has no
//! tests of `DeleteLogs` or of the routes' access; those here are not
//! upstream's.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{Local, TimeZone};
use http::{Method, StatusCode, header};
use http_body_util::BodyExt as _;
use open_ferry_core::observe::Observability;
use tower::ServiceExt as _;

use super::{Api, KEY, LOCAL, keyed, keyed_config, request_from};
use crate::json::Json;
use crate::logs::{
    LogCursor, complete_log_boundary, complete_log_lines, cursor_mod_time_unix_nano, decode_cursor,
    encode_log_cursor, new_log_cursor,
};

const MAIN_LOG: &str = "main.log";

/// The longest line read (upstream's `logScannerMaxBuffer`).
const MAX_LINE: usize = 8 * 1024 * 1024;

/// What a `GET` of the logs answered (upstream's `logsAPIResponse`).
#[derive(Debug)]
struct Logs {
    lines: Vec<String>,
    line_count: i64,
    latest: i64,
    next_cursor: String,
    cursor_reset: bool,
}

/// The API over the log directory `dir`, with `logging-to-file` on or off
/// (upstream's `newLogsTestHandler`).
fn api(dir: &Path, logging_to_file: bool) -> Api {
    let mut config = keyed_config();
    config.logging_to_file = logging_to_file;
    let dir = dir.to_path_buf();
    Api::build(config, None, None, move |state, _| {
        state.with_observability(Observability {
            log_dir: Some(dir),
            ..Observability::default()
        })
    })
}

/// The answer to `GET target`, which must be a 200 (upstream's
/// `performGetLogs`).
async fn get_logs(api: &Api, target: &str) -> Logs {
    let body = api.get(target).await.expect(StatusCode::OK);
    Logs {
        lines: body["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|line| line.as_str().unwrap().to_owned())
            .collect(),
        line_count: body["line-count"].as_i64().unwrap(),
        latest: body["latest-timestamp"].as_i64().unwrap(),
        next_cursor: body["next-cursor"].as_str().unwrap().to_owned(),
        cursor_reset: body
            .get("cursor-reset")
            .is_some_and(|reset| reset.as_bool().unwrap()),
    }
}

/// The logs from `cursor`, at most `limit` lines.
fn from_cursor(cursor: &str, limit: usize) -> String {
    let cursor: String = url::form_urlencoded::byte_serialize(cursor.as_bytes()).collect();
    format!("/v0/management/logs?cursor={cursor}&limit={limit}")
}

/// The cursor `raw` holds.
fn decode(raw: &str) -> LogCursor {
    decode_cursor(raw.as_bytes()).unwrap()
}

/// A cursor of `file` as upstream's tests write one (upstream's
/// `mustEncodeRawCursor`).
fn raw_cursor(file: &str) -> String {
    encode_log_cursor(&LogCursor {
        version: 1,
        file: file.to_owned(),
        fingerprint: "fingerprint".to_owned(),
        ..LogCursor::default()
    })
}

/// Writes `main.log` (upstream's `writeMainLog`).
fn write_main_log(dir: &Path, content: &str) {
    fs::write(dir.join(MAIN_LOG), content).unwrap();
}

/// Appends to `main.log` (upstream's `appendMainLog`).
fn append_main_log(dir: &Path, content: &str) {
    OpenOptions::new()
        .append(true)
        .create(true)
        .open(dir.join(MAIN_LOG))
        .unwrap()
        .write_all(content.as_bytes())
        .unwrap();
}

/// Renames `main.log` to `to`.
fn rotate_main_log(dir: &Path, to: &str) {
    fs::rename(dir.join(MAIN_LOG), dir.join(to)).unwrap();
}

/// Sets the modification time of the file at `path` (Go's `os.Chtimes`).
fn set_modified(path: &Path, time: SystemTime) {
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(time)
        .unwrap();
}

/// The time `nanos` after the epoch (Go's `time.Unix(0, nanos)`).
fn unix_nanos(nanos: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(u64::try_from(nanos).unwrap())
}

/// The Unix time of a local time on 2026-06-15 (Go's `time.Date` in
/// `time.Local`).
fn june_15(hour: u32, minute: u32, second: u32) -> i64 {
    Local
        .with_ymd_and_hms(2026, 6, 15, hour, minute, second)
        .single()
        .unwrap()
        .timestamp()
}

// Ports TestDecodeLogCursorRejectsUnsafeFiles.
#[test]
fn decode_log_cursor_rejects_unsafe_files() {
    for name in [
        "",
        ".",
        "..",
        "../secret",
        "nested/main.log",
        "nested\\main.log",
        "error.log",
    ] {
        assert!(
            decode_cursor(raw_cursor(name).as_bytes()).is_err(),
            "{name:?}"
        );
    }
    for name in ["main.log", "main.log.1", "main-2026-06-15T10-00-00.log"] {
        assert_eq!(decode(&raw_cursor(name)).file, name);
    }
}

// Ports TestLogCursorRoundTripOmitsAbsolutePath.
#[test]
fn log_cursor_round_trip_omits_absolute_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(MAIN_LOG);
    fs::write(&path, "line one\nline two\n").unwrap();

    let boundary = complete_log_boundary(&path).unwrap();
    let raw = new_log_cursor(&path, boundary, 123).unwrap();
    let decoded = decode(&raw);
    assert_eq!(decoded.file, MAIN_LOG);
    assert_eq!(decoded.offset, boundary);
    assert_eq!(decoded.latest_timestamp, 123);
    assert!(!raw.contains(&*dir.path().to_string_lossy()), "{raw}");
}

// Ports TestReadCompleteLogLinesSkipsTrailingPartial.
#[test]
fn read_complete_log_lines_skips_trailing_partial() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(MAIN_LOG);
    let initial = "first\nsecond\r\npartial";
    fs::write(&path, initial).unwrap();

    let (lines, read) = complete_log_lines(&path, 0, None, 0).unwrap();
    assert_eq!(lines, [b"first".to_vec(), b"second".to_vec()]);
    assert_eq!(read.count, 2);
    assert_eq!(read.end_offset, "first\nsecond\r\n".len() as i64);

    append_main_log(dir.path(), "\n");
    let (lines, next) = complete_log_lines(&path, read.end_offset, None, 0).unwrap();
    assert_eq!(lines, [b"partial".to_vec()]);
    assert_eq!(next.end_offset, initial.len() as i64 + 1);
}

/// Not upstream's: an answer longer than a chunk is sent as its lines are
/// read, in chunks and without a length, where upstream gathers every line
/// first; its bytes are those upstream's `c.JSON` writes.
#[tokio::test]
async fn long_answers_are_sent_as_they_are_read() {
    let dir = tempfile::tempdir().unwrap();
    let api = api(dir.path(), true);
    let (mut rotated, mut current, mut lines) = (Vec::new(), Vec::new(), Vec::new());
    for i in 0..20_000 {
        let (file, hour) = if i < 10_000 {
            (&mut rotated, 9)
        } else {
            (&mut current, 10)
        };
        let mut line = format!("[2026-06-15 {hour:02}:00:00] line <{i}> & more").into_bytes();
        if i % 1000 == 7 {
            line.push(0xff);
        }
        file.extend_from_slice(&line);
        if i % 3 == 0 {
            file.push(b'\r');
        }
        file.push(b'\n');
        lines.push(line);
    }
    fs::write(dir.path().join("main.log.1"), &rotated).unwrap();
    fs::write(dir.path().join(MAIN_LOG), &current).unwrap();

    let after = june_15(9, 0, 0);
    for (query, from, line_count) in [
        (String::new(), 0, 20_000),
        (format!("?after={after}"), 10_000, 20_000),
        ("?limit=15000".to_owned(), 5_000, 15_000),
    ] {
        let target = format!("/v0/management/logs{query}");
        let request = keyed(Method::GET, &target, "");
        let response = api.router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{target}");
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/json; charset=utf-8"
        );
        let length = response.headers().get(header::CONTENT_LENGTH);
        assert!(length.is_none(), "{target}: {length:?}");
        let mut body = response.into_body();
        let mut frames = Vec::new();
        while let Some(frame) = body.frame().await {
            if let Ok(data) = frame.unwrap().into_data() {
                frames.push(data);
            }
        }
        assert!(frames.len() > 1, "{target}: {} frames", frames.len());
        assert!(
            frames.iter().all(|frame| frame.len() < 65 * 1024),
            "{target}"
        );

        let body = frames.concat();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let next_cursor = parsed["next-cursor"].as_str().unwrap().to_owned();
        assert_eq!(decode(&next_cursor).file, MAIN_LOG);
        let expected = Json::map([
            (
                "lines",
                Json::Array(lines[from..].iter().cloned().map(Json::Bytes).collect()),
            ),
            ("line-count", Json::Int(line_count)),
            ("latest-timestamp", Json::Int(june_15(10, 0, 0))),
            ("next-cursor", Json::Str(next_cursor)),
        ]);
        // Not assert_eq!, which would print both bodies.
        assert!(body == expected.encode().as_bytes(), "{target}");
    }
}

// Ports TestGetLogsTailLimitReturnsRecentLinesWithCursor.
#[tokio::test]
async fn tail_limit_returns_recent_lines_with_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let lines = [
        "[2026-06-15 10:00:00] first",
        "[2026-06-15 10:00:01] second",
        "[2026-06-15 10:00:02] third",
        "[2026-06-15 10:00:03] fourth",
    ];
    write_main_log(dir.path(), &(lines.join("\n") + "\n"));

    let resp = get_logs(&api(dir.path(), true), "/v0/management/logs?limit=2").await;
    assert_eq!(resp.lines, lines[2..]);
    assert_eq!(resp.line_count, 2);
    assert!(!resp.next_cursor.is_empty());
    assert_eq!(resp.latest, june_15(10, 0, 3));
}

// Ports TestGetLogsTailLimitDoesNotScanOlderFilesForLineCount.
#[tokio::test]
async fn tail_limit_does_not_scan_older_files_for_line_count() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("main.log.1"),
        "x".repeat(MAX_LINE + 1) + "\n",
    )
    .unwrap();
    write_main_log(dir.path(), "[2026-06-15 10:00:00] current\n");

    let resp = get_logs(&api(dir.path(), true), "/v0/management/logs?limit=1").await;
    assert_eq!(resp.lines, ["[2026-06-15 10:00:00] current"]);
    assert_eq!(resp.line_count, 1);
}

// Ports TestGetLogsNoLimitKeepsFullScanBehavior.
#[tokio::test]
async fn no_limit_keeps_full_scan_behavior() {
    let dir = tempfile::tempdir().unwrap();
    write_main_log(dir.path(), "complete\npartial");

    let resp = get_logs(&api(dir.path(), true), "/v0/management/logs").await;
    assert_eq!(resp.lines, ["complete", "partial"]);
    assert_eq!(resp.line_count, 2);
    assert!(!resp.next_cursor.is_empty());
    assert_eq!(decode(&resp.next_cursor).offset, "complete\n".len() as i64);
}

// Ports TestGetLogsAfterKeepsTimestampScanAndReturnsCursor.
#[tokio::test]
async fn after_keeps_timestamp_scan_and_returns_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let lines = [
        "[2026-06-15 10:00:00] first",
        "[2026-06-15 10:00:01] second",
        "[2026-06-15 10:00:02] third",
    ];
    write_main_log(dir.path(), &(lines.join("\n") + "\n"));

    let target = format!("/v0/management/logs?after={}", june_15(10, 0, 0));
    let resp = get_logs(&api(dir.path(), true), &target).await;
    assert_eq!(resp.lines, lines[1..]);
    assert_eq!(resp.line_count, 3);
    assert!(!resp.next_cursor.is_empty());
}

// Ports TestGetLogsCursorReturnsOnlyNewCompleteLines.
#[tokio::test]
async fn cursor_returns_only_new_complete_lines() {
    let dir = tempfile::tempdir().unwrap();
    let api = api(dir.path(), true);
    let lines = [
        "[2026-06-15 10:00:00] first",
        "[2026-06-15 10:00:01] second",
        "[2026-06-15 10:00:02] third",
    ];
    write_main_log(dir.path(), &(lines.join("\n") + "\n"));
    let initial = get_logs(&api, "/v0/management/logs?limit=2").await;
    assert!(!initial.next_cursor.is_empty());

    append_main_log(dir.path(), "[2026-06-15 10:00:03] fourth\n");
    let resp = get_logs(&api, &from_cursor(&initial.next_cursor, 10)).await;
    assert_eq!(resp.lines, ["[2026-06-15 10:00:03] fourth"]);
    assert_eq!(resp.line_count, 1);
    assert!(!resp.cursor_reset);
    assert_eq!(resp.latest, june_15(10, 0, 3));
}

// Ports TestGetLogsCursorRejectsOversizedLine. The whole body is checked.
#[tokio::test]
async fn cursor_rejects_oversized_line() {
    let dir = tempfile::tempdir().unwrap();
    let api = api(dir.path(), true);
    write_main_log(dir.path(), "[2026-06-15 10:00:00] first\n");
    let initial = get_logs(&api, "/v0/management/logs?limit=1").await;
    assert!(!initial.next_cursor.is_empty());

    append_main_log(dir.path(), &("x".repeat(MAX_LINE + 1) + "\n"));
    let answer = api.get(&from_cursor(&initial.next_cursor, 1)).await;
    answer.assert(
        StatusCode::INTERNAL_SERVER_ERROR,
        r#"{"error":"failed to read log files: log line exceeds 8388608 bytes"}"#,
    );
}

// Ports TestGetLogsCursorNoNewLinesKeepsCursorStable.
#[tokio::test]
async fn cursor_no_new_lines_keeps_cursor_stable() {
    let dir = tempfile::tempdir().unwrap();
    let api = api(dir.path(), true);
    write_main_log(dir.path(), "[2026-06-15 10:00:00] first\n");
    let initial = get_logs(&api, "/v0/management/logs?limit=1").await;

    let resp = get_logs(&api, &from_cursor(&initial.next_cursor, 10)).await;
    assert!(resp.lines.is_empty(), "{resp:?}");
    assert_eq!(resp.line_count, 0);
    assert_eq!(resp.next_cursor, initial.next_cursor);
    assert_eq!(resp.latest, initial.latest);
}

// Ports TestGetLogsCursorDoesNotAdvancePastTrailingPartial.
#[tokio::test]
async fn cursor_does_not_advance_past_trailing_partial() {
    let dir = tempfile::tempdir().unwrap();
    let api = api(dir.path(), true);
    write_main_log(dir.path(), "[2026-06-15 10:00:00] first\n");
    let initial = get_logs(&api, "/v0/management/logs?limit=1").await;

    append_main_log(dir.path(), "partial");
    let partial = get_logs(&api, &from_cursor(&initial.next_cursor, 10)).await;
    assert!(partial.lines.is_empty(), "{partial:?}");
    assert_eq!(partial.next_cursor, initial.next_cursor);

    append_main_log(dir.path(), "\n");
    let complete = get_logs(&api, &from_cursor(&initial.next_cursor, 10)).await;
    assert_eq!(complete.lines, ["partial"]);
    assert_eq!(complete.latest, initial.latest);
}

// Ports TestGetLogsCursorResetAfterTruncateTailsLimit.
#[tokio::test]
async fn cursor_reset_after_truncate_tails_limit() {
    let dir = tempfile::tempdir().unwrap();
    let api = api(dir.path(), true);
    let lines = [
        "[2026-06-15 10:00:00] first",
        "[2026-06-15 10:00:01] second",
        "[2026-06-15 10:00:02] third",
    ];
    write_main_log(dir.path(), &(lines.join("\n") + "\n"));
    let initial = get_logs(&api, "/v0/management/logs?limit=3").await;

    let reset_line = "[2026-06-15 10:00:03] reset";
    write_main_log(dir.path(), &format!("{reset_line}\n"));
    let resp = get_logs(&api, &from_cursor(&initial.next_cursor, 1)).await;
    assert!(resp.cursor_reset);
    assert_eq!(resp.lines, [reset_line]);
    assert_eq!(resp.line_count, 1);
}

// Ports TestGetLogsCursorReadsAcrossRotation.
#[tokio::test]
async fn cursor_reads_across_rotation() {
    let dir = tempfile::tempdir().unwrap();
    let api = api(dir.path(), true);
    let line1 = "[2026-06-15 10:00:00] first";
    let line2 = "[2026-06-15 10:00:01] second";
    let line3 = "[2026-06-15 10:00:02] third";
    write_main_log(dir.path(), &format!("{line1}\n"));
    let initial = get_logs(&api, "/v0/management/logs?limit=1").await;

    append_main_log(dir.path(), &format!("{line2}\n"));
    rotate_main_log(dir.path(), "main.log.1");
    write_main_log(dir.path(), &format!("{line3}\n"));

    let resp = get_logs(&api, &from_cursor(&initial.next_cursor, 10)).await;
    assert_eq!(resp.lines, [line2, line3]);
    assert!(!resp.cursor_reset);
}

// Ports TestGetLogsCursorReadsRotatedFileWhenNewMainIsSmaller.
#[tokio::test]
async fn cursor_reads_rotated_file_when_new_main_is_smaller() {
    let dir = tempfile::tempdir().unwrap();
    let api = api(dir.path(), true);
    let line1 = "[2026-06-15 10:00:00] first line with enough bytes";
    let line2 = "[2026-06-15 10:00:01] second";
    let line3 = "new";
    write_main_log(dir.path(), &format!("{line1}\n"));
    let initial = get_logs(&api, "/v0/management/logs?limit=1").await;

    append_main_log(dir.path(), &format!("{line2}\n"));
    rotate_main_log(dir.path(), "main.log.1");
    write_main_log(dir.path(), &format!("{line3}\n"));

    let resp = get_logs(&api, &from_cursor(&initial.next_cursor, 1)).await;
    assert_eq!(resp.lines, [line2]);
    assert!(!resp.cursor_reset);

    let next = get_logs(&api, &from_cursor(&resp.next_cursor, 1)).await;
    assert_eq!(next.lines, [line3]);
    assert!(!next.cursor_reset);
}

// Ports TestGetLogsZeroOffsetCursorWithPartialLineReadsAcrossRotation.
#[tokio::test]
async fn zero_offset_cursor_with_partial_line_reads_across_rotation() {
    let dir = tempfile::tempdir().unwrap();
    let api = api(dir.path(), true);
    write_main_log(dir.path(), "partial");
    let initial = get_logs(&api, "/v0/management/logs?limit=1").await;
    assert!(!initial.next_cursor.is_empty());
    let cursor = decode(&initial.next_cursor);
    assert_eq!(cursor.offset, 0);
    assert_ne!(cursor.size, 0);

    append_main_log(dir.path(), " complete\n");
    rotate_main_log(dir.path(), "main.log.1");
    write_main_log(dir.path(), "new\n");

    let resp = get_logs(&api, &from_cursor(&initial.next_cursor, 10)).await;
    assert_eq!(resp.lines, ["partial complete", "new"]);
    assert!(!resp.cursor_reset);
}

// Ports TestGetLogsZeroOffsetCursorWithEmptyFileReadsAcrossRotation.
#[tokio::test]
async fn zero_offset_cursor_with_empty_file_reads_across_rotation() {
    let dir = tempfile::tempdir().unwrap();
    let api = api(dir.path(), true);
    write_main_log(dir.path(), "");
    let initial = get_logs(&api, "/v0/management/logs?limit=1").await;
    assert!(!initial.next_cursor.is_empty());
    let cursor = decode(&initial.next_cursor);
    assert_eq!((cursor.offset, cursor.size), (0, 0));

    append_main_log(dir.path(), "first\n");
    let main = dir.path().join(MAIN_LOG);
    let modified = unix_nanos(cursor_mod_time_unix_nano(&cursor) + 1_000_000_000);
    set_modified(&main, modified);
    rotate_main_log(dir.path(), "main.log.1");
    write_main_log(dir.path(), "second\n");

    let resp = get_logs(&api, &from_cursor(&initial.next_cursor, 1)).await;
    assert_eq!(resp.lines, ["first"]);
    assert!(!resp.cursor_reset);

    let next = get_logs(&api, &from_cursor(&resp.next_cursor, 1)).await;
    assert_eq!(next.lines, ["second"]);
    assert!(!next.cursor_reset);
}

// Ports TestGetLogsZeroOffsetCursorWithEmptyFileReadsAcrossTwoRotations.
#[tokio::test]
async fn zero_offset_cursor_with_empty_file_reads_across_two_rotations() {
    let dir = tempfile::tempdir().unwrap();
    let api = api(dir.path(), true);
    write_main_log(dir.path(), "");
    let initial = get_logs(&api, "/v0/management/logs?limit=1").await;
    assert!(!initial.next_cursor.is_empty());
    let cursor = decode(&initial.next_cursor);
    assert_eq!((cursor.offset, cursor.size), (0, 0));

    let main = dir.path().join(MAIN_LOG);
    let made = cursor_mod_time_unix_nano(&cursor);
    append_main_log(dir.path(), "first\n");
    set_modified(&main, unix_nanos(made + 1_000_000_000));
    rotate_main_log(dir.path(), "main.log.1");
    write_main_log(dir.path(), "second\n");
    set_modified(&main, unix_nanos(made + 2_000_000_000));
    fs::rename(dir.path().join("main.log.1"), dir.path().join("main.log.2")).unwrap();
    rotate_main_log(dir.path(), "main.log.1");
    write_main_log(dir.path(), "third\n");

    let resp = get_logs(&api, &from_cursor(&initial.next_cursor, 1)).await;
    assert_eq!(resp.lines, ["first"]);
    assert!(!resp.cursor_reset);

    let next = get_logs(&api, &from_cursor(&resp.next_cursor, 1)).await;
    assert_eq!(next.lines, ["second"]);
    assert!(!next.cursor_reset);

    let latest = get_logs(&api, &from_cursor(&next.next_cursor, 1)).await;
    assert_eq!(latest.lines, ["third"]);
    assert!(!latest.cursor_reset);
}

// Ports TestGetLogsZeroOffsetCursorWithEmptyFileResetsWhenRotationModTimeAmbiguous.
#[tokio::test]
async fn zero_offset_cursor_with_empty_file_resets_when_rotation_mod_time_ambiguous() {
    let dir = tempfile::tempdir().unwrap();
    let api = api(dir.path(), true);
    let main = dir.path().join(MAIN_LOG);
    let fixed = UNIX_EPOCH + Duration::from_secs(u64::try_from(june_15(10, 0, 0)).unwrap());
    write_main_log(dir.path(), "");
    set_modified(&main, fixed);
    let initial = get_logs(&api, "/v0/management/logs?limit=1").await;
    assert!(!initial.next_cursor.is_empty());
    let cursor = decode(&initial.next_cursor);
    assert_eq!((cursor.offset, cursor.size), (0, 0));

    let first = "[2026-06-15 10:00:01] first";
    let second = "[2026-06-15 10:00:02] second";
    append_main_log(dir.path(), &format!("{first}\n"));
    set_modified(&main, fixed);
    rotate_main_log(dir.path(), "main.log.1");
    write_main_log(dir.path(), &format!("{second}\n"));
    set_modified(&main, fixed);

    let resp = get_logs(&api, &from_cursor(&initial.next_cursor, 2)).await;
    assert_eq!(resp.lines, [first, second]);
    assert!(resp.cursor_reset);
    assert_eq!(resp.line_count, 2);
}

// Ports TestGetLogsInvalidCursorResetsToTail.
#[tokio::test]
async fn invalid_cursor_resets_to_tail() {
    let dir = tempfile::tempdir().unwrap();
    let api = api(dir.path(), true);
    let lines = [
        "[2026-06-15 10:00:00] first",
        "[2026-06-15 10:00:01] second",
    ];
    write_main_log(dir.path(), &(lines.join("\n") + "\n"));

    for raw in ["not-base64".to_owned(), raw_cursor("../secret")] {
        let resp = get_logs(&api, &from_cursor(&raw, 1)).await;
        assert!(resp.cursor_reset, "{raw}");
        assert_eq!(resp.lines, lines[1..]);
        assert_eq!(resp.line_count, 1);
    }
}

// Ports TestGetLogsMissingRotatedCursorFileResetsToTail.
#[tokio::test]
async fn missing_rotated_cursor_file_resets_to_tail() {
    let dir = tempfile::tempdir().unwrap();
    let current = "[2026-06-15 10:00:01] current";
    write_main_log(dir.path(), &format!("{current}\n"));
    let rotated = dir.path().join("main.log.1");
    let old = "[2026-06-15 10:00:00] old\n";
    fs::write(&rotated, old).unwrap();
    let cursor = new_log_cursor(&rotated, old.len() as i64, 0).unwrap();
    fs::remove_file(&rotated).unwrap();

    let resp = get_logs(&api(dir.path(), true), &from_cursor(&cursor, 1)).await;
    assert!(resp.cursor_reset);
    assert_eq!(resp.lines, [current]);
}

// Ports TestGetLogsMissingLogDirKeepsOKEmptyResponse.
#[tokio::test]
async fn missing_log_dir_keeps_ok_empty_response() {
    let dir = tempfile::tempdir().unwrap();
    let api = api(&dir.path().join("missing"), true);
    let resp = get_logs(&api, &from_cursor("not-base64", 1)).await;
    assert!(resp.lines.is_empty(), "{resp:?}");
    assert_eq!(resp.line_count, 0);
    assert!(resp.cursor_reset);
}

// Ports TestGetLogsLoggingDisabledKeepsBadRequest. The whole body is
// checked.
#[tokio::test]
async fn logging_disabled_keeps_bad_request() {
    let dir = tempfile::tempdir().unwrap();
    let answer = api(dir.path(), false)
        .get("/v0/management/logs?cursor=not-base64&limit=1")
        .await;
    answer.assert(
        StatusCode::BAD_REQUEST,
        r#"{"error":"logging to file disabled"}"#,
    );
}

/// Not upstream's: whole answers as gin writes them; a bad `limit` once the
/// directory is there, though not when it is missing; and `after` as the
/// latest time of a missing directory's answer.
#[tokio::test]
async fn answers_are_written_as_upstream_writes_them() {
    let dir = tempfile::tempdir().unwrap();
    let api = api(dir.path(), true);
    api.get("/v0/management/logs").await.assert(
        StatusCode::OK,
        r#"{"latest-timestamp":0,"line-count":0,"lines":[],"next-cursor":""}"#,
    );
    for (limit, error) in [
        ("abc", "must be a positive integer"),
        ("0", "must be greater than zero"),
        ("-3", "must be greater than zero"),
    ] {
        let answer = api.get(&format!("/v0/management/logs?limit={limit}")).await;
        answer.assert(
            StatusCode::BAD_REQUEST,
            &format!(r#"{{"error":"invalid limit: {error}"}}"#),
        );
    }

    let missing = self::api(&dir.path().join("missing"), true);
    missing
        .get("/v0/management/logs?after=1750000000&limit=abc")
        .await
        .assert(
            StatusCode::OK,
            r#"{"latest-timestamp":1750000000,"line-count":0,"lines":[],"next-cursor":""}"#,
        );
}

/// Not upstream's: the files are read oldest first and `main.log` last, a
/// line without a time is kept after `after` with the line before it,
/// `limit` keeps the last lines, and other logs aren't read.
#[tokio::test]
async fn rotations_are_read_oldest_first() {
    let dir = tempfile::tempdir().unwrap();
    let api = api(dir.path(), true);
    for (name, content) in [
        (
            "main-2026-06-15T09-00-00.000.log",
            "[2026-06-15 09:00:00] oldest\n",
        ),
        (
            "main-2026-06-15T09-30-00.log",
            "[2026-06-15 09:30:00] older\ncontinued\n",
        ),
        ("main.log.1", "[2026-06-15 09:45:00] newer\n"),
        ("error-x.log", "[2026-06-15 09:50:00] not a main log\n"),
    ] {
        fs::write(dir.path().join(name), content).unwrap();
    }
    write_main_log(dir.path(), "[2026-06-15 10:00:00] newest\r\n");

    let all = get_logs(&api, "/v0/management/logs").await;
    assert_eq!(
        all.lines,
        [
            "[2026-06-15 09:00:00] oldest",
            "[2026-06-15 09:30:00] older",
            "continued",
            "[2026-06-15 09:45:00] newer",
            "[2026-06-15 10:00:00] newest",
        ]
    );
    assert_eq!(all.latest, june_15(10, 0, 0));
    assert_eq!(decode(&all.next_cursor).file, MAIN_LOG);

    let target = format!("/v0/management/logs?after={}&limit=3", june_15(9, 0, 0));
    let after = get_logs(&api, &target).await;
    assert_eq!(
        after.lines,
        [
            "continued",
            "[2026-06-15 09:45:00] newer",
            "[2026-06-15 10:00:00] newest",
        ]
    );
    assert_eq!(after.line_count, 5);

    let tail = get_logs(&api, "/v0/management/logs?limit=4").await;
    assert_eq!(
        tail.lines,
        [
            "[2026-06-15 09:30:00] older",
            "continued",
            "[2026-06-15 09:45:00] newer",
            "[2026-06-15 10:00:00] newest",
        ]
    );
}

/// The names in `dir`, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Not upstream's: `DELETE` empties `main.log`, which its writer goes on
/// appending to, and removes the rotations, even one another handle has
/// open; other files, and a directory named like a rotation, stay.
#[tokio::test]
async fn delete_empties_main_log_and_removes_rotations() {
    let dir = tempfile::tempdir().unwrap();
    let api = api(dir.path(), true);
    write_main_log(dir.path(), "[2026-06-15 10:00:00] old\n");
    let mut writer = OpenOptions::new()
        .append(true)
        .open(dir.path().join(MAIN_LOG))
        .unwrap();
    for name in [
        "main.log.1",
        "main-2026-06-15T09-00-00.000.log",
        "main-2026-06-14T09-00-00.log.gz",
        "error-x.log",
        "other.txt",
    ] {
        fs::write(dir.path().join(name), "data\n").unwrap();
    }
    fs::create_dir(dir.path().join("main.log.2")).unwrap();
    let reader = File::open(dir.path().join("main.log.1")).unwrap();

    let answer = api
        .send(keyed(Method::DELETE, "/v0/management/logs", ""))
        .await;
    answer.assert(
        StatusCode::OK,
        r#"{"message":"Logs cleared successfully","removed":3,"success":true}"#,
    );
    drop(reader);
    assert_eq!(
        names(dir.path()),
        ["error-x.log", "main.log", "main.log.2", "other.txt"]
    );
    assert_eq!(fs::read(dir.path().join(MAIN_LOG)).unwrap(), b"");
    writer.write_all(b"new\n").unwrap();
    drop(writer);
    assert_eq!(
        fs::read_to_string(dir.path().join(MAIN_LOG)).unwrap(),
        "new\n"
    );
}

/// A log directory, `logs`, and a file outside it, `outside.txt`, holding
/// a secret; and the API over the log directory.
fn outside_and_logs() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    Api,
) {
    let dir = tempfile::tempdir().unwrap();
    let logs = dir.path().join("logs");
    fs::create_dir(&logs).unwrap();
    let outside = dir.path().join("outside.txt");
    fs::write(&outside, "[2026-06-15 10:00:00] OUTSIDE-SECRET\n").unwrap();
    let api = api(&logs, true);
    (dir, logs, outside, api)
}

/// Checks that neither `GET` nor `DELETE` reads or empties the file
/// outside the log directory that `main.log` and then `main.log.1` lead
/// to, `link` making each.
async fn assert_links_are_refused(link: impl Fn(&Path, &Path) -> bool) {
    let (_dir, logs, outside, api) = outside_and_logs();
    if !link(&outside, &logs.join(MAIN_LOG)) {
        return;
    }
    let first = api.get("/v0/management/logs?limit=1").await;
    first.assert(
        StatusCode::INTERNAL_SERVER_ERROR,
        r#"{"error":"failed to read log files: invalid log file"}"#,
    );
    for target in ["/v0/management/logs", "/v0/management/logs?after=1"] {
        api.get(target).await.assert(
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":"failed to read log file: invalid log file"}"#,
        );
    }
    api.send(keyed(Method::DELETE, "/v0/management/logs", ""))
        .await
        .assert(
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":"failed to truncate log file: invalid log file"}"#,
        );
    let secret = "[2026-06-15 10:00:00] OUTSIDE-SECRET\n";
    assert_eq!(fs::read_to_string(&outside).unwrap(), secret);

    // A cursor from a plain `main.log` isn't followed into a link either.
    fs::remove_file(logs.join(MAIN_LOG)).unwrap();
    write_main_log(&logs, "[2026-06-15 09:00:00] mine\n");
    let plain = get_logs(&api, "/v0/management/logs?limit=1").await;
    fs::remove_file(logs.join(MAIN_LOG)).unwrap();
    assert!(link(&outside, &logs.join(MAIN_LOG)));
    api.get(&from_cursor(&plain.next_cursor, 1)).await.assert(
        StatusCode::INTERNAL_SERVER_ERROR,
        r#"{"error":"failed to read log files: invalid log file"}"#,
    );

    // A rotation linked so is refused, and stays.
    fs::remove_file(logs.join(MAIN_LOG)).unwrap();
    write_main_log(&logs, "[2026-06-15 09:00:00] mine\n");
    assert!(link(&outside, &logs.join("main.log.1")));
    api.send(keyed(Method::DELETE, "/v0/management/logs", ""))
        .await
        .assert(
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":"failed to remove main.log.1: invalid log file"}"#,
        );
    assert_eq!(names(&logs), [MAIN_LOG, "main.log.1"]);
    assert_eq!(fs::read_to_string(&outside).unwrap(), secret);
}

/// Not upstream's: a `main.log` or rotation hard-linked to a file outside
/// the log directory is refused, not read, emptied or removed.
#[tokio::test]
async fn hard_linked_logs_are_refused() {
    assert_links_are_refused(|target, link| {
        fs::hard_link(target, link).unwrap();
        true
    })
    .await;
}

/// Not upstream's: a `main.log` or rotation that is a symbolic link to a
/// file outside the log directory is refused, not followed. Skipped where
/// the tests can't make one.
#[tokio::test]
async fn symbolic_linked_logs_are_refused() {
    assert_links_are_refused(crate::log_dir::symlink_file).await;
}

/// Not upstream's: `DELETE` of a missing directory answers 404, and with
/// `logging-to-file` off 400.
#[tokio::test]
async fn delete_needs_the_directory_and_logging_to_file() {
    let dir = tempfile::tempdir().unwrap();
    api(&dir.path().join("missing"), true)
        .send(keyed(Method::DELETE, "/v0/management/logs", ""))
        .await
        .assert(
            StatusCode::NOT_FOUND,
            r#"{"error":"log directory not found"}"#,
        );
    api(dir.path(), false)
        .send(keyed(Method::DELETE, "/v0/management/logs", ""))
        .await
        .assert(
            StatusCode::BAD_REQUEST,
            r#"{"error":"logging to file disabled"}"#,
        );
}

/// Not upstream's: both paths need the management key, v8's serves the
/// same as v0's, and another method answers the empty 404.
#[tokio::test]
async fn routes_need_the_key_and_v8_serves_them() {
    let dir = tempfile::tempdir().unwrap();
    let api = api(dir.path(), true);
    write_main_log(dir.path(), "[2026-06-15 10:00:00] line\n");
    fs::write(dir.path().join("main.log.1"), "old\n").unwrap();

    for path in ["/v0/management/logs", "/v8/management/observability/logs"] {
        for method in [Method::GET, Method::DELETE] {
            let answer = api
                .send(request_from(LOCAL, method.clone(), path, ""))
                .await;
            assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{method} {path}");
            assert!(!answer.body.contains(KEY), "{method} {path}");
        }
        api.send(keyed(Method::POST, path, ""))
            .await
            .assert(StatusCode::NOT_FOUND, "");
    }

    let v8 = get_logs(&api, "/v8/management/observability/logs?limit=1").await;
    assert_eq!(v8.lines, ["[2026-06-15 10:00:00] line"]);
    api.send(keyed(
        Method::DELETE,
        "/v8/management/observability/logs",
        "",
    ))
    .await
    .assert(
        StatusCode::OK,
        r#"{"message":"Logs cleared successfully","removed":1,"success":true}"#,
    );
    assert_eq!(names(dir.path()), [MAIN_LOG]);
}
