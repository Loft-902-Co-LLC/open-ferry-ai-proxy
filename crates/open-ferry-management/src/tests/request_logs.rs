//! Ports CLIProxyAPI internal/api/handlers/management/logs_test.go
//! (v8.0.10, MIT): the `GetRequestLogByID` tests. The rest of that file,
//! the main log's, is P3 WP-B's.
//!
//! Upstream calls the handler with a hand-built gin context; here each
//! request goes through the router, with the management key, to a log
//! directory of the test's own. The cases run on both the v0 and the v8
//! path.
//!
//! Added: the list of error logs, the download of one, and the answers to
//! a bad name or ID, a missing file or directory, and a missing key.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use http::{Method, StatusCode};
use open_ferry_core::config::Config;
use open_ferry_core::observe::Observability;
use serde_json::{Value, json};

use super::{Api, LOCAL, keyed_config, request_from};

/// The by-ID routes, v0 and v8.
const BY_ID: [&str; 2] = [
    "/v0/management/request-log-by-id",
    "/v8/management/observability/logs/requests",
];

/// The error log routes, v0 and v8.
const ERRORS: [&str; 2] = [
    "/v0/management/request-error-logs",
    "/v8/management/observability/logs/errors",
];

/// 2026-09-23 10:00:00 UTC in Unix seconds.
const SEPT_23_10H: u64 = 1_790_157_600;

/// The API reading the logs in `dir`, with `config`.
fn api_in(dir: &Path, config: Config) -> Api {
    let log_dir = dir.to_path_buf();
    Api::build(config, None, None, move |state, _| {
        state.with_observability(Observability {
            log_dir: Some(log_dir),
            ..Observability::default()
        })
    })
}

/// The API reading the logs in `dir`, with `request-log` off.
fn api(dir: &Path) -> Api {
    api_in(dir, keyed_config())
}

/// Writes log `name` in `dir` with `content`, last changed at `modified`.
fn write_log(dir: &Path, name: &str, content: &str, modified: SystemTime) {
    let path = dir.join(name);
    fs::write(&path, content).unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
}

/// 2026-09-23 at `hour`:00:00 UTC, `hour` from 10.
fn sept_23(hour: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(SEPT_23_10H + (hour - 10) * 3600)
}

/// Checks that the log of request `id` is `want`, on both paths.
async fn assert_by_id(api: &Api, id: &str, want: &str) {
    for path in BY_ID {
        api.get(&format!("{path}/{id}"))
            .await
            .assert(StatusCode::OK, want);
    }
}

// Ports TestGetRequestLogByID_SelectsLatestOnReusedID.
#[tokio::test]
async fn get_request_log_by_id_selects_latest_on_reused_id() {
    let dir = tempfile::tempdir().unwrap();
    let now = SystemTime::now();
    write_log(
        dir.path(),
        "v1_chat_completions-2026-09-23T100000-00000000.log",
        "old log content",
        now - Duration::from_secs(3600),
    );
    write_log(
        dir.path(),
        "v1_chat_completions-2026-09-23T110000-00000000.log",
        "new log content",
        now,
    );
    assert_by_id(&api(dir.path()), "00000000", "new log content").await;
}

// Ports TestGetRequestLogByID_SelectsLatestOnSameModTimeTieBreak.
#[tokio::test]
async fn get_request_log_by_id_selects_latest_on_same_mod_time_tie_break() {
    let dir = tempfile::tempdir().unwrap();
    let same = sept_23(10);
    write_log(
        dir.path(),
        "v1_chat_completions-2026-09-23T100000-00000000.log",
        "file1 content",
        same,
    );
    write_log(
        dir.path(),
        "v1_chat_completions-2026-09-23T100000_1-00000000.log",
        "file2 content",
        same,
    );
    assert_by_id(&api(dir.path()), "00000000", "file2 content").await;
}

// Ports TestGetRequestLogByID_SelectsLatestOnSameModTimeSequence9Vs10.
#[tokio::test]
async fn get_request_log_by_id_selects_latest_on_same_mod_time_sequence_9_vs_10() {
    let dir = tempfile::tempdir().unwrap();
    let same = sept_23(10);
    write_log(
        dir.path(),
        "v1_chat_completions-2026-09-23T100000_9-00000000.log",
        "file9 content",
        same,
    );
    write_log(
        dir.path(),
        "v1_chat_completions-2026-09-23T100000_10-00000000.log",
        "file10 content",
        same,
    );
    assert_by_id(&api(dir.path()), "00000000", "file10 content").await;
}

// Ports TestGetRequestLogByID_SelectsLatestAcrossTimestampsWithSequenceOnModTimeTie.
#[tokio::test]
async fn get_request_log_by_id_selects_latest_across_timestamps_with_sequence_on_mod_time_tie() {
    let dir = tempfile::tempdir().unwrap();
    let same = sept_23(11);
    write_log(
        dir.path(),
        "v1_chat_completions-2026-09-23T100000_9-00000000.log",
        "old with seq content",
        same,
    );
    write_log(
        dir.path(),
        "v1_chat_completions-2026-09-23T110000-00000000.log",
        "new without seq content",
        same,
    );
    assert_by_id(&api(dir.path()), "00000000", "new without seq content").await;
}

// Ports TestGetRequestLogByID_SelectsLatestCrossYearBoundaryOnModTimeTie.
#[tokio::test]
async fn get_request_log_by_id_selects_latest_cross_year_boundary_on_mod_time_tie() {
    let dir = tempfile::tempdir().unwrap();
    // 2027-01-01 00:00:00 UTC.
    let same = UNIX_EPOCH + Duration::from_secs(1_798_761_600);
    write_log(
        dir.path(),
        "v1_chat_completions-2026-12-31T235959-00000000.log",
        "2026 content",
        same,
    );
    write_log(
        dir.path(),
        "v1_chat_completions-2027-01-01T000000-00000000.log",
        "2027 content",
        same,
    );
    assert_by_id(&api(dir.path()), "00000000", "2027 content").await;
}

// Ports TestGetRequestLogByID_MatchesFullUUIDAndShortID.
#[tokio::test]
async fn get_request_log_by_id_matches_full_uuid_and_short_id() {
    let dir = tempfile::tempdir().unwrap();
    write_log(
        dir.path(),
        "v1_chat_completions-2026-09-28T100000-1234abcd.log",
        "uuid v7 request log content",
        SystemTime::now(),
    );
    let api = api(dir.path());
    assert_by_id(&api, "1234abcd", "uuid v7 request log content").await;
    assert_by_id(
        &api,
        "018f3a5b-1234-7abc-def0-00001234abcd",
        "uuid v7 request log content",
    )
    .await;
}

// Not upstream's: a log is sent as an attachment named after its file,
// as plain text.
#[tokio::test]
async fn sends_a_log_as_an_attachment() {
    let dir = tempfile::tempdir().unwrap();
    let name = "v1-responses-2026-09-28T100000-1234abcd.log";
    write_log(dir.path(), name, "content", sept_23(10));
    let answer = api(dir.path())
        .get("/v0/management/request-log-by-id/1234abcd")
        .await;
    answer.assert(StatusCode::OK, "content");
    assert_eq!(
        answer.header("content-disposition"),
        Some(format!("attachment; filename=\"{name}\"").as_str())
    );
    assert_eq!(
        answer.header("content-type"),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(
        answer.header("last-modified"),
        Some("Wed, 23 Sep 2026 10:00:00 GMT")
    );
}

// Not upstream's: the ID may be given as `?id=` when the path's is blank,
// and a bad or unknown one is refused.
#[tokio::test]
async fn refuses_bad_ids() {
    let dir = tempfile::tempdir().unwrap();
    write_log(
        dir.path(),
        "v1-responses-2026-09-28T100000-1234abcd.log",
        "content",
        sept_23(10),
    );
    fs::create_dir(dir.path().join("v1-x-2026-09-28T100000-dir00000.log")).unwrap();
    let api = api(dir.path());
    for path in BY_ID {
        api.get(&format!("{path}/%20?id=1234abcd"))
            .await
            .assert(StatusCode::OK, "content");
        api.get(&format!("{path}/%20"))
            .await
            .assert(StatusCode::BAD_REQUEST, r#"{"error":"missing request ID"}"#);
        api.get(&format!("{path}/a%5Cb"))
            .await
            .assert(StatusCode::BAD_REQUEST, r#"{"error":"invalid request ID"}"#);
        api.get(&format!("{path}/ffffffff")).await.assert(
            StatusCode::NOT_FOUND,
            r#"{"error":"log file not found for the given request ID"}"#,
        );
        api.get(&format!("{path}/dir00000")).await.assert(
            StatusCode::NOT_FOUND,
            r#"{"error":"log file not found for the given request ID"}"#,
        );
    }

    let gone = api_in(&dir.path().join("missing"), keyed_config());
    gone.get("/v0/management/request-log-by-id/1234abcd")
        .await
        .assert(
            StatusCode::NOT_FOUND,
            r#"{"error":"log directory not found"}"#,
        );
}

// Not upstream's: the error logs are listed newest first, with their size
// and change time, and none while `request-log` is on or the directory is
// missing.
#[tokio::test]
async fn lists_error_logs() {
    let dir = tempfile::tempdir().unwrap();
    write_log(
        dir.path(),
        "error-v1-responses-2026-09-23T100000-aaaaaaaa.log",
        "older",
        sept_23(10),
    );
    write_log(
        dir.path(),
        "error-v1-responses-2026-09-23T110000-bbbbbbbb.log",
        "newer!",
        sept_23(11),
    );
    write_log(
        dir.path(),
        "v1-responses-2026-09-23T110000-cccccccc.log",
        "not an error log",
        sept_23(11),
    );
    write_log(dir.path(), "main.log", "main", sept_23(11));
    fs::create_dir(dir.path().join("error-dir.log")).unwrap();

    let api = api(dir.path());
    for path in ERRORS {
        let files = api.get(path).await.expect(StatusCode::OK);
        assert_eq!(
            files,
            json!({"files": [
                {
                    "name": "error-v1-responses-2026-09-23T110000-bbbbbbbb.log",
                    "size": 6,
                    "modified": SEPT_23_10H + 3600,
                },
                {
                    "name": "error-v1-responses-2026-09-23T100000-aaaaaaaa.log",
                    "size": 5,
                    "modified": SEPT_23_10H,
                },
            ]})
        );
    }

    let mut config = keyed_config();
    config.request_log = true;
    let on = api_in(dir.path(), config);
    let gone = api_in(&dir.path().join("missing"), keyed_config());
    for path in ERRORS {
        on.get(path).await.assert(StatusCode::OK, r#"{"files":[]}"#);
        gone.get(path)
            .await
            .assert(StatusCode::OK, r#"{"files":[]}"#);
    }
}

// Not upstream's: an error log is downloaded by name; another name, a bad
// one, a missing file or a directory is refused.
#[tokio::test]
async fn downloads_error_logs() {
    let dir = tempfile::tempdir().unwrap();
    let name = "error-v1-responses-2026-09-23T100000-aaaaaaaa.log";
    write_log(dir.path(), name, "error log", sept_23(10));
    write_log(
        dir.path(),
        "v1-responses-2026-09-23T100000-aaaaaaaa.log",
        "request log",
        sept_23(10),
    );
    fs::create_dir(dir.path().join("error-dir.log")).unwrap();
    let api = api(dir.path());
    for path in ERRORS {
        let answer = api.get(&format!("{path}/{name}")).await;
        answer.assert(StatusCode::OK, "error log");
        assert_eq!(
            answer.header("content-disposition"),
            Some(format!("attachment; filename=\"{name}\"").as_str())
        );
        api.get(&format!(
            "{path}/v1-responses-2026-09-23T100000-aaaaaaaa.log"
        ))
        .await
        .assert(StatusCode::NOT_FOUND, r#"{"error":"log file not found"}"#);
        api.get(&format!("{path}/error-a%5Cb.log")).await.assert(
            StatusCode::BAD_REQUEST,
            r#"{"error":"invalid log file name"}"#,
        );
        api.get(&format!("{path}/%20")).await.assert(
            StatusCode::BAD_REQUEST,
            r#"{"error":"invalid log file name"}"#,
        );
        api.get(&format!("{path}/error-missing.log"))
            .await
            .assert(StatusCode::NOT_FOUND, r#"{"error":"log file not found"}"#);
        api.get(&format!("{path}/error-dir.log"))
            .await
            .assert(StatusCode::BAD_REQUEST, r#"{"error":"invalid log file"}"#);
    }
}

/// Checks that an error log and a request's log that `link` makes lead to
/// a file outside the log directory are refused, not sent.
async fn assert_links_are_refused(link: impl Fn(&Path, &Path) -> bool) {
    let dir = tempfile::tempdir().unwrap();
    let logs = dir.path().join("logs");
    fs::create_dir(&logs).unwrap();
    let outside = dir.path().join("outside.txt");
    fs::write(&outside, "OUTSIDE-SECRET").unwrap();
    let error_log = "error-v1-responses-2026-09-23T100000-aaaaaaaa.log";
    if !link(&outside, &logs.join(error_log)) {
        return;
    }
    assert!(link(
        &outside,
        &logs.join("v1-responses-2026-09-23T100000-bbbbbbbb.log")
    ));
    let api = api(&logs);
    let refused = r#"{"error":"invalid log file"}"#;
    for path in ERRORS {
        api.get(&format!("{path}/{error_log}"))
            .await
            .assert(StatusCode::BAD_REQUEST, refused);
    }
    for path in BY_ID {
        for id in ["aaaaaaaa", "bbbbbbbb"] {
            api.get(&format!("{path}/{id}"))
                .await
                .assert(StatusCode::BAD_REQUEST, refused);
        }
    }
}

// Not upstream's: a log hard-linked to a file outside the log directory
// is refused.
#[tokio::test]
async fn hard_linked_logs_are_refused() {
    assert_links_are_refused(|target, link| {
        fs::hard_link(target, link).unwrap();
        true
    })
    .await;
}

// Not upstream's: a log that is a symbolic link to a file outside the log
// directory is refused, not followed. Skipped where the tests can't make
// one.
#[tokio::test]
async fn symbolic_linked_logs_are_refused() {
    assert_links_are_refused(crate::log_dir::symlink_file).await;
}

// Not upstream's: the routes need the management key.
#[tokio::test]
async fn needs_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let name = "error-v1-responses-2026-09-23T100000-aaaaaaaa.log";
    write_log(dir.path(), name, "error log", sept_23(10));
    for path in [
        ERRORS[0].to_owned(),
        ERRORS[1].to_owned(),
        format!("{}/{name}", ERRORS[0]),
        format!("{}/{name}", ERRORS[1]),
        format!("{}/aaaaaaaa", BY_ID[0]),
        format!("{}/aaaaaaaa", BY_ID[1]),
    ] {
        // A fresh API for each, as repeated failures ban the address.
        let answer = api(dir.path())
            .send(request_from(LOCAL, Method::GET, &path, ""))
            .await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{path}");
        let body: Value = serde_json::from_str(&answer.body).unwrap();
        assert_eq!(body, json!({"error": "missing management key"}), "{path}");
    }
}
