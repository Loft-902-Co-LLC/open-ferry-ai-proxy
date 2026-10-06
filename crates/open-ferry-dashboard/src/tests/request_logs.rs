//! The request-log routes, over logs written into the log directory as
//! the request logger names and writes them.

use std::fs;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{Local, NaiveDateTime, SecondsFormat, TimeZone as _, Utc};
use http::{Method, StatusCode};
use serde_json::{Value, json};

use super::{Answer, Dash, LOCAL, REMOTE, keyed, keyed_config, request};
use crate::CONTENT_SECURITY_POLICY;

const SEARCH: &str = "/open-ferry/api/v1/request-logs";

/// A log's name's time, the server's local time as `%Y-%m-%dT%H%M%S`, as
/// the API writes it in UTC.
fn utc(local: &str) -> String {
    let naive = NaiveDateTime::parse_from_str(local, "%Y-%m-%dT%H%M%S").unwrap();
    let time = Local.from_local_datetime(&naive).earliest().unwrap();
    time.with_timezone(&Utc)
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// A log as the request logger writes it.
fn log_text(method: &str, url: &str, body: &str, status: u16, answer: &str) -> String {
    format!(
        "=== REQUEST INFO ===\nVersion: 0.1.0\nURL: {url}\nMethod: {method}\n\
         Timestamp: 2026-10-05T11:58:02Z\n\n=== HEADERS ===\nContent-Type: application/json\n\n\
         === REQUEST BODY ===\n{body}\n\n=== RESPONSE ===\nStatus: {status}\n\n{answer}\n"
    )
}

/// The logs [`with_logs`] writes, oldest first: a name, its local time,
/// its method and URL, its request body, its status and its answer.
const LOGS: [(&str, &str, &str, &str, &str, u16, &str); 5] = [
    (
        "v1-chat-completions-2026-10-05T115802-1234abcd.log",
        "2026-10-05T115802",
        "POST",
        "/v1/chat/completions",
        r#"{"model":"gpt-5","messages":[]}"#,
        200,
        r#"{"id":"chatcmpl-1","model":"gpt-5-2026"}"#,
    ),
    (
        "v1-chat-completions-2026-10-05T115802_1-1234abcd.log",
        "2026-10-05T115802",
        "POST",
        "/v1/chat/completions",
        r#"{"model":"gpt-4o"}"#,
        400,
        r#"{"error":{"message":"bad request"}}"#,
    ),
    (
        "v1-messages-2026-10-05T120000-5678abcd.log",
        "2026-10-05T120000",
        "POST",
        "/v1/messages",
        r#"{"model":"claude-sonnet-4-5","max_tokens":10}"#,
        200,
        r#"{"content":[{"text":"Hello World"}]}"#,
    ),
    (
        "error-v1-chat-completions-2026-10-05T120100-9999aaaa.log",
        "2026-10-05T120100",
        "POST",
        "/v1/chat/completions",
        r#"{"model":"gpt-5"}"#,
        502,
        "upstream unavailable",
    ),
    (
        "v1beta-models-gemini-2.5-pro-generateContent-2026-10-05T120200-0000ffff.log",
        "2026-10-05T120200",
        "POST",
        "/v1beta/models/gemini-2.5-pro:generateContent",
        r#"{"contents":[]}"#,
        429,
        "quota",
    ),
];

/// The names of [`LOGS`] by their place in it.
fn names(places: &[usize]) -> Vec<&'static str> {
    places.iter().map(|&place| LOGS[place].0).collect()
}

/// A dashboard whose log directory has [`LOGS`], and files that aren't
/// request logs.
fn with_logs(dash: &Dash) {
    for (name, _, method, url, body, status, answer) in LOGS {
        fs::write(
            dash.logs().join(name),
            log_text(method, url, body, status, answer),
        )
        .unwrap();
    }
    for (name, text) in [
        ("main.log", "server log"),
        ("notes.txt", "notes"),
        ("v1-chat-2026-10-05T130000-abcdabcd.txt", "not a log"),
        (".v1-chat-2026-10-05T130000-abcdabcd.log", "hidden"),
    ] {
        fs::write(dash.logs().join(name), text).unwrap();
    }
}

/// The names of the logs a search found.
fn found(answer: &Value) -> Vec<String> {
    answer["logs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|log| log["name"].as_str().unwrap().to_owned())
        .collect()
}

/// Not upstream's: a search lists the request and error logs newest
/// first, each described from its name and content.
#[tokio::test]
async fn logs_are_listed_newest_first() {
    let dash = Dash::new();
    with_logs(&dash);
    let answer = dash.get(SEARCH).await.json(StatusCode::OK);
    assert_eq!(found(&answer), names(&[4, 3, 2, 1, 0]));
    assert_eq!(answer["next_cursor"], Value::Null);
    let bytes: usize = LOGS
        .iter()
        .map(|(_, _, method, url, body, status, text)| {
            log_text(method, url, body, *status, text).len()
        })
        .sum();
    assert_eq!(
        answer["scanned"],
        json!({"files": 5, "bytes": bytes, "limit_reached": false})
    );
    assert_eq!(answer["request_log"], false);

    let (name, local, method, url, body, status, text) = LOGS[0];
    let first = &answer["logs"][4];
    let modified = first["modified"].as_str().unwrap();
    assert!(modified.ends_with('Z'), "{modified}");
    assert_eq!(
        *first,
        json!({
            "name": name,
            "kind": "request",
            "request_id": "1234abcd",
            "time": utc(local),
            "size": log_text(method, url, body, status, text).len(),
            "modified": modified,
            "method": "POST",
            "url": "/v1/chat/completions",
            "status": 200,
            "model": "gpt-5",
        })
    );
    let error = &answer["logs"][1];
    assert_eq!(error["kind"], "error");
    assert_eq!(error["request_id"], "9999aaaa");
    assert_eq!(error["status"], 502);
    let gemini = &answer["logs"][0];
    assert_eq!(gemini["model"], "gemini-2.5-pro");
    assert_eq!(gemini["status"], 429);
    assert_eq!(answer["logs"][3]["request_id"], "1234abcd");

    let mut config = keyed_config();
    config.request_log = true;
    let dash = Dash::with_config(config);
    let answer = dash.get(SEARCH).await.json(StatusCode::OK);
    assert_eq!(answer["logs"], json!([]));
    assert_eq!(answer["request_log"], true);
}

/// Not upstream's: a search is paged by its cursor, and filtered by kind
/// and by the time in the names.
#[tokio::test]
async fn searches_are_paged_and_filtered_by_name() {
    let dash = Dash::new();
    with_logs(&dash);
    let mut pages = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let path = match &cursor {
            Some(cursor) => format!("{SEARCH}?limit=2&cursor={cursor}"),
            None => format!("{SEARCH}?limit=2"),
        };
        let answer = dash.get(&path).await.json(StatusCode::OK);
        assert_eq!(answer["scanned"]["limit_reached"], false);
        pages.push(found(&answer));
        match answer["next_cursor"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => break,
        }
    }
    assert_eq!(pages, [names(&[4, 3]), names(&[2, 1]), names(&[0])]);

    for (query, expected) in [
        ("kind=error".to_owned(), names(&[3])),
        ("kind=request".to_owned(), names(&[4, 2, 1, 0])),
        ("kind=all".to_owned(), names(&[4, 3, 2, 1, 0])),
        (
            format!("from={}", utc("2026-10-05T120000")),
            names(&[4, 3, 2]),
        ),
        (
            format!(
                "from={}&to={}",
                utc("2026-10-05T120000"),
                utc("2026-10-05T120200")
            ),
            names(&[3, 2]),
        ),
        (
            format!("to={}&kind=request", utc("2026-10-05T120000")),
            names(&[1, 0]),
        ),
    ] {
        let answer = dash
            .get(&format!("{SEARCH}?{query}"))
            .await
            .json(StatusCode::OK);
        assert_eq!(found(&answer), expected, "{query}");
    }

    for (query, code) in [
        ("kind=debug", "invalid_request"),
        ("limit=0", "invalid_request"),
        ("limit=201", "invalid_request"),
        (
            "from=2026-10-05T12:00:00Z&to=2026-10-05T11:00:00Z",
            "invalid_request",
        ),
        ("cursor=!!!", "invalid_cursor"),
    ] {
        dash.get(&format!("{SEARCH}?{query}"))
            .await
            .error(StatusCode::BAD_REQUEST, code);
    }
    let not_a_log = URL_SAFE_NO_PAD.encode("main.log");
    dash.get(&format!("{SEARCH}?cursor={not_a_log}"))
        .await
        .error(StatusCode::BAD_REQUEST, "invalid_cursor");
}

/// Not upstream's: a search filters by the URL, the status, the model and
/// any text of the logs, ignoring case.
#[tokio::test]
async fn searches_are_filtered_by_content() {
    let dash = Dash::new();
    with_logs(&dash);
    for (query, expected) in [
        ("path=messages", names(&[2])),
        ("path=/V1/CHAT", names(&[3, 1, 0])),
        ("status=502", names(&[3])),
        ("status=4xx", names(&[4, 1])),
        ("status=2xx", names(&[2, 0])),
        ("model=GPT", names(&[3, 1, 0])),
        ("model=gpt-4", names(&[1])),
        ("model=gemini-2.5", names(&[4])),
        ("q=hello%20WORLD", names(&[2])),
        ("q=quota", names(&[4])),
        ("q=nothing+like+this", vec![]),
        ("path=chat&status=5xx", names(&[3])),
        ("path=chat&model=gpt-5&kind=request", names(&[0])),
    ] {
        let answer = dash
            .get(&format!("{SEARCH}?{query}"))
            .await
            .json(StatusCode::OK);
        assert_eq!(found(&answer), expected, "{query}");
        // Every log of the kind is read.
        let files = if query.contains("kind=request") { 4 } else { 5 };
        assert_eq!(answer["scanned"]["files"], files, "{query}");
    }

    for query in ["status=600", "status=6xx", "status=abc"] {
        let message = dash
            .get(&format!("{SEARCH}?{query}"))
            .await
            .error(StatusCode::BAD_REQUEST, "invalid_request");
        assert!(message.contains("status"), "{message}");
    }
    let message = dash
        .get(&format!("{SEARCH}?q={}", "a".repeat(1025)))
        .await
        .error(StatusCode::BAD_REQUEST, "invalid_request");
    assert!(
        message.contains("q must be at most 1024 bytes"),
        "{message}"
    );
}

/// Not upstream's: a search opens at most 2,000 files; stopped there, it
/// says so and its cursor continues after the last file read.
#[tokio::test]
async fn a_search_stops_at_its_file_limit() {
    let dash = Dash::new();
    let start = NaiveDateTime::parse_from_str("2026-10-05T000000", "%Y-%m-%dT%H%M%S").unwrap();
    let name = |index: i64| {
        let time = start + chrono::Duration::seconds(index);
        format!(
            "v1-chat-completions-{}-{index:08x}.log",
            time.format("%Y-%m-%dT%H%M%S")
        )
    };
    for index in 0..2003 {
        let answer = if index == 1 { "the needle" } else { "hay" };
        fs::write(
            dash.logs().join(name(index)),
            log_text("POST", "/v1/chat/completions", "{}", 200, answer),
        )
        .unwrap();
    }

    let answer = dash
        .get(&format!("{SEARCH}?q=needle"))
        .await
        .json(StatusCode::OK);
    assert_eq!(answer["logs"], json!([]));
    assert_eq!(answer["scanned"]["files"], 2000);
    assert_eq!(answer["scanned"]["limit_reached"], true);
    let cursor = answer["next_cursor"].as_str().unwrap();
    assert_eq!(cursor, URL_SAFE_NO_PAD.encode(name(3)));

    let answer = dash
        .get(&format!("{SEARCH}?q=needle&cursor={cursor}"))
        .await
        .json(StatusCode::OK);
    assert_eq!(found(&answer), [name(1)]);
    assert_eq!(answer["scanned"]["files"], 3);
    assert_eq!(answer["scanned"]["limit_reached"], false);
    assert_eq!(answer["next_cursor"], Value::Null);

    // Without content filters, the first page fills before the limit.
    let answer = dash.get(SEARCH).await.json(StatusCode::OK);
    assert_eq!(answer["logs"].as_array().unwrap().len(), 50);
    assert_eq!(answer["scanned"]["files"], 50);
    assert_eq!(answer["scanned"]["limit_reached"], false);
}

/// Not upstream's: of a log over 1 MiB a search reads only its first and
/// last 512 KiB; a read of it gets any part.
#[tokio::test]
async fn large_logs_are_read_at_both_ends() {
    let dash = Dash::new();
    let name = "v1-chat-completions-2026-10-05T115802-1234abcd.log";
    let head = "=== REQUEST INFO ===\nURL: /v1/chat/completions\nMethod: POST\n\n\
                === REQUEST BODY ===\n{\"model\":\"big-model\",\"input\":\"";
    let tail = "\"}\n\n=== RESPONSE ===\nStatus: 200\n\n{\"model\":\"answered\"}\ntail-marker\n";
    let size = 3 * 1024 * 1024;
    let middle = size / 2;
    let mut text = String::with_capacity(size);
    text.push_str(head);
    text.push_str(&"a".repeat(middle - text.len()));
    text.push_str("needle-in-the-middle");
    text.push_str(&"b".repeat(size - text.len() - tail.len()));
    text.push_str(tail);
    assert_eq!(text.len(), size);
    fs::write(dash.logs().join(name), &text).unwrap();

    let answer = dash
        .get(&format!("{SEARCH}?q=needle-in-the-middle"))
        .await
        .json(StatusCode::OK);
    assert_eq!(answer["logs"], json!([]));
    assert_eq!(
        answer["scanned"],
        json!({"files": 1, "bytes": 1024 * 1024, "limit_reached": false})
    );
    let answer = dash
        .get(&format!("{SEARCH}?q=TAIL-MARKER&status=200&model=big"))
        .await
        .json(StatusCode::OK);
    assert_eq!(found(&answer), [name]);
    assert_eq!(answer["logs"][0]["model"], "big-model");
    assert_eq!(answer["logs"][0]["size"], size);

    let piece = dash
        .get(&format!("{SEARCH}/{name}?offset={middle}&length=20"))
        .await
        .json(StatusCode::OK);
    assert_eq!(piece["content"], "needle-in-the-middle");
    assert_eq!(piece["next_offset"], middle + 20);
    let piece = dash
        .get(&format!("{SEARCH}/{name}"))
        .await
        .json(StatusCode::OK);
    assert_eq!(piece["content"].as_str().unwrap().len(), 1024 * 1024);
    assert_eq!(piece["next_offset"], 1024 * 1024);
    assert_eq!(piece["log"]["status"], 200);
}

/// Not upstream's: a log is read in pieces, each ending on a character,
/// and described as a search describes it.
#[tokio::test]
async fn a_log_is_read_in_pieces() {
    let dash = Dash::new();
    with_logs(&dash);
    let (name, ..) = LOGS[2];
    let text = fs::read_to_string(dash.logs().join(name)).unwrap();
    let whole = dash
        .get(&format!("{SEARCH}/{name}"))
        .await
        .json(StatusCode::OK);
    assert_eq!(whole["offset"], 0);
    assert_eq!(whole["next_offset"], Value::Null);
    assert_eq!(whole["content"], text.as_str());
    let listed = dash
        .get(&format!("{SEARCH}?path=messages"))
        .await
        .json(StatusCode::OK);
    assert_eq!(whole["log"], listed["logs"][0]);

    let first = dash
        .get(&format!("{SEARCH}/{name}?length=20"))
        .await
        .json(StatusCode::OK);
    assert_eq!(first["content"], &text[..20]);
    assert_eq!(first["next_offset"], 20);
    let rest = dash
        .get(&format!("{SEARCH}/{name}?offset=20&length=4194304"))
        .await
        .json(StatusCode::OK);
    assert_eq!(rest["offset"], 20);
    assert_eq!(rest["content"], &text[20..]);
    assert_eq!(rest["next_offset"], Value::Null);
    let end = dash
        .get(&format!("{SEARCH}/{name}?offset={}", text.len()))
        .await
        .json(StatusCode::OK);
    assert_eq!(end["content"], "");
    assert_eq!(end["next_offset"], Value::Null);

    let utf8 = "error-v1-chat-completions-2026-10-05T120500-77777777.log";
    let mut bytes = "12345678é€\u{1F600}x".as_bytes().to_vec();
    bytes.push(0xff);
    fs::write(dash.logs().join(utf8), &bytes).unwrap();
    for (offset, length, content, next) in [
        (0, 9, "12345678", Some(8)),
        (8, 2, "é", Some(10)),
        (10, 5, "€", Some(13)),
        // A piece shorter than the character it starts gives what it has.
        (13, 3, "\u{FFFD}", Some(16)),
        (13, 4, "\u{1F600}", Some(17)),
        (17, 10, "x\u{FFFD}", None),
    ] {
        let piece = dash
            .get(&format!("{SEARCH}/{utf8}?offset={offset}&length={length}"))
            .await
            .json(StatusCode::OK);
        assert_eq!(piece["content"], content, "{offset}+{length}");
        assert_eq!(piece["next_offset"], json!(next), "{offset}+{length}");
    }

    for query in ["offset=100000", "offset=-1", "length=0", "length=4194305"] {
        dash.get(&format!("{SEARCH}/{name}?{query}"))
            .await
            .error(StatusCode::BAD_REQUEST, "invalid_request");
    }
}

/// Not upstream's: only request and error logs are served, and a log that
/// has another hard link is refused, as the management API refuses it.
#[tokio::test]
async fn only_plain_logs_are_served() {
    let dash = Dash::new();
    with_logs(&dash);
    for name in [
        "main.log",
        "notes.txt",
        "v1-chat-2026-10-05T130000-abcdabcd.txt",
        ".v1-chat-2026-10-05T130000-abcdabcd.log",
        "open-ferry-usage.sqlite3",
        "..%2Fmain.log",
        "..%5Cv1-chat-completions-2026-10-05T115802-1234abcd.log",
        "v1-chat-completions-2026-10-05T235959-00000000.log",
    ] {
        let message = dash
            .get(&format!("{SEARCH}/{name}"))
            .await
            .error(StatusCode::NOT_FOUND, "not_found");
        assert_eq!(message, "no such log", "{name}");
    }

    let (name, ..) = LOGS[0];
    let link = "v1-chat-completions-2026-10-05T130000-12121212.log";
    fs::hard_link(dash.logs().join(name), dash.logs().join(link)).unwrap();
    for name in [name, link] {
        dash.get(&format!("{SEARCH}/{name}"))
            .await
            .error(StatusCode::BAD_REQUEST, "invalid_log_file");
    }
    let answer = dash.get(SEARCH).await.json(StatusCode::OK);
    assert_eq!(found(&answer), names(&[4, 3, 2, 1]));
}

/// The download of the log `name`, with the key.
async fn download(dash: &Dash, name: &str) -> Answer {
    dash.get(&format!("{SEARCH}/{name}/download")).await
}

/// Makes `link` a symbolic link to the file `target`; false, after saying
/// so, when the system won't let the tests make one (Windows without the
/// privilege or developer mode).
fn symlink_file(target: &Path, link: &Path) -> bool {
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_file(target, link);
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(target, link);
    match made {
        Ok(()) => true,
        Err(error) => {
            eprintln!("skipped: can't make a symbolic link: {error}");
            false
        }
    }
}

/// Not upstream's: a download is the log's bytes exactly, those that
/// aren't UTF-8 too, as a file to save that isn't to be stored.
#[tokio::test]
async fn a_download_is_the_logs_bytes() {
    let dash = Dash::new();
    let name = "v1-images-edits-2026-10-05T115802-1234abcd.log";
    let mut bytes = b"=== REQUEST INFO ===\nURL: /v1/images/edits\nMethod: POST\n\n\
                      === REQUEST BODY ===\n--boundary\r\n\r\n"
        .to_vec();
    // An image's bytes, every byte value, many not UTF-8, more than one
    // piece of the stream.
    bytes.extend((0..=255u8).cycle().take(200_000));
    bytes.extend_from_slice(b"\xff\xfe\xc3\r\n--boundary--\r\n\n=== RESPONSE ===\nStatus: 200\n");
    fs::write(dash.logs().join(name), &bytes).unwrap();

    let answer = download(&dash, name).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert!(answer.bytes == bytes, "the download isn't the log's bytes");
    let length = bytes.len().to_string();
    let disposition = format!("attachment; filename=\"{name}\"");
    for (header, value) in [
        ("content-type", "application/octet-stream"),
        ("content-length", length.as_str()),
        ("content-disposition", disposition.as_str()),
        ("cache-control", "no-store"),
        ("content-security-policy", CONTENT_SECURITY_POLICY),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("x-frame-options", "DENY"),
    ] {
        assert_eq!(answer.header(header), Some(value), "{header}");
    }

    let empty = "error-v1-chat-completions-2026-10-05T120000-00000000.log";
    fs::write(dash.logs().join(empty), b"").unwrap();
    let answer = download(&dash, empty).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert!(answer.bytes.is_empty());
    assert_eq!(answer.header("content-length"), Some("0"));

    let answer = dash
        .send(keyed(
            Method::HEAD,
            &format!("{SEARCH}/{name}/download"),
            "",
        ))
        .await;
    assert_eq!(answer.status, StatusCode::OK);
    assert!(answer.bytes.is_empty());
    assert_eq!(answer.header("content-length"), Some(length.as_str()));
}

/// Not upstream's: a download checks the key and the name as every route
/// does: only a listed log's name, no path, no other file.
#[tokio::test]
async fn a_download_takes_only_a_logs_name() {
    let dash = Dash::new();
    with_logs(&dash);
    let (name, ..) = LOGS[0];
    let path = format!("{SEARCH}/{name}/download");
    dash.send(request(LOCAL, Method::GET, &path, ""))
        .await
        .error(StatusCode::UNAUTHORIZED, "missing_management_key");
    let mut remote = keyed(Method::GET, &path, "");
    remote
        .extensions_mut()
        .insert(axum::extract::ConnectInfo::<std::net::SocketAddr>(
            REMOTE.parse().unwrap(),
        ));
    dash.send(remote)
        .await
        .error(StatusCode::FORBIDDEN, "remote_management_disabled");
    dash.call(Method::POST, &path, "")
        .await
        .error(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed");

    for name in [
        "main.log",
        "notes.txt",
        "open-ferry-usage.sqlite3",
        ".v1-chat-2026-10-05T130000-abcdabcd.log",
        "..",
        "..%2Fmain.log",
        "..%2F..%2Fv1-chat-completions-2026-10-05T115802-1234abcd.log",
        "..%5Cv1-chat-completions-2026-10-05T115802-1234abcd.log",
        "%2E%2E%5Cmain.log",
        "v1-chat-completions-2026-10-05T235959-00000000.log",
    ] {
        let message = download(&dash, name)
            .await
            .error(StatusCode::NOT_FOUND, "not_found");
        assert_eq!(message, "no such log", "{name}");
    }
    for path in [
        format!("{SEARCH}/../main.log/download"),
        format!("{SEARCH}/x/../{name}/download"),
        format!("{SEARCH}/{name}/download/more"),
    ] {
        dash.get(&path)
            .await
            .error(StatusCode::NOT_FOUND, "not_found");
    }
}

/// Not upstream's: a log that has another hard link, that is a symbolic
/// link, or that isn't a file, isn't downloaded.
#[tokio::test]
async fn a_download_refuses_links() {
    let dash = Dash::new();
    with_logs(&dash);
    let (name, ..) = LOGS[0];
    let hard = "v1-chat-completions-2026-10-05T130000-12121212.log";
    fs::hard_link(dash.logs().join(name), dash.logs().join(hard)).unwrap();
    for name in [name, hard] {
        let message = download(&dash, name)
            .await
            .error(StatusCode::BAD_REQUEST, "invalid_log_file");
        assert!(message.contains("hard link"), "{message}");
    }

    let directory = "v1-chat-completions-2026-10-05T130100-34343434.log";
    fs::create_dir(dash.logs().join(directory)).unwrap();
    download(&dash, directory)
        .await
        .error(StatusCode::BAD_REQUEST, "invalid_log_file");

    let (target, ..) = LOGS[2];
    let soft = "v1-chat-completions-2026-10-05T130200-56565656.log";
    if symlink_file(&dash.logs().join(target), &dash.logs().join(soft)) {
        download(&dash, soft)
            .await
            .error(StatusCode::BAD_REQUEST, "invalid_log_file");
        dash.get(&format!("{SEARCH}/{soft}"))
            .await
            .error(StatusCode::BAD_REQUEST, "invalid_log_file");
        // The file it leads to is still served.
        assert_eq!(download(&dash, target).await.status, StatusCode::OK);
    }
}
