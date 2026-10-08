//! The ledger's tests: its schema, how records become rows, pruning, and
//! that the file holds no client key. None is upstream's: upstream has no
//! ledger.

use std::sync::mpsc;
use std::time::Duration;

use open_ferry_core::config::Config;
use open_ferry_core::observe::usage::{ClientKey, EventCredential, Usage};
use rusqlite::Connection;

use super::schema::{self, Settings, client_key_secret, read_settings, write_settings};
use super::writer::{client_key_id, insert, prune};
use super::{LEDGER_FILE, Ledger, LedgerError};
use crate::tests::{event, ms, tokens};

/// A day, in milliseconds.
const DAY: i64 = 24 * 60 * 60 * 1000;

/// Statistics with `usage-statistics-enabled` set to `enabled`.
fn usage(enabled: bool) -> Usage {
    let mut config = Config::default();
    config.usage_statistics_enabled = enabled;
    Usage::new(&config)
}

/// The values of `sql`'s first column.
fn column<T: rusqlite::types::FromSql>(connection: &Connection, sql: &str) -> Vec<T> {
    let mut statement = connection.prepare(sql).unwrap();
    statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<T>>>()
        .unwrap()
}

/// Not upstream's: a new ledger has the schema at version 1, the default
/// settings and a secret of its own, all kept when it is opened again.
#[test]
fn a_new_ledger_has_the_schema_and_its_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested").join(LEDGER_FILE);
    let connection = schema::open(&path).unwrap();

    assert_eq!(
        column::<i64>(&connection, "SELECT version FROM schema_version"),
        [1]
    );
    let tables = column::<String>(
        &connection,
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
         ORDER BY name",
    );
    assert_eq!(tables, ["prices", "requests", "schema_version", "settings"]);
    assert_eq!(
        read_settings(&connection).unwrap(),
        Settings {
            retention_days: 90,
            max_rows: 1_000_000,
            currency: "USD".to_owned(),
        }
    );
    let journal: String = connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(journal, "wal");
    let auto_vacuum: i64 = connection
        .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
        .unwrap();
    assert_eq!(auto_vacuum, 2, "incremental");
    let secret = client_key_secret(&connection).unwrap();
    assert_eq!(secret.len(), 64);
    assert!(secret.iter().all(u8::is_ascii_hexdigit));
    drop(connection);

    let other = schema::open(&dir.path().join("other").join(LEDGER_FILE)).unwrap();
    assert_ne!(client_key_secret(&other).unwrap(), secret);

    let again = schema::open(&path).unwrap();
    assert_eq!(client_key_secret(&again).unwrap(), secret);
    assert_eq!(
        column::<i64>(&again, "SELECT version FROM schema_version"),
        [1]
    );
}

/// Not upstream's: a ledger a newer open-ferry made is left alone, and the
/// ledger is unavailable, saying why.
#[test]
fn a_newer_schema_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(LEDGER_FILE);
    let newer = Connection::open(&path).unwrap();
    newer
        .execute_batch(
            "CREATE TABLE schema_version (version INTEGER NOT NULL);
             INSERT INTO schema_version (version) VALUES (2);
             CREATE TABLE future (x INTEGER);",
        )
        .unwrap();
    drop(newer);

    let error = schema::open(&path).unwrap_err();
    assert!(error.contains("version 2"), "{error}");

    let ledger = Ledger::start(dir.path(), &usage(true));
    let reason = ledger.unavailable_reason().unwrap();
    assert!(reason.contains("version 2"), "{reason}");
    assert!(!ledger.recording());
    assert_eq!(ledger.file(), None);
    ledger.shutdown();

    let check = Connection::open(&path).unwrap();
    assert_eq!(
        column::<i64>(&check, "SELECT version FROM schema_version"),
        [2]
    );
    assert_eq!(
        column::<String>(
            &check,
            "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name"
        ),
        ["future", "schema_version"]
    );
}

/// Not upstream's: a ledger that can't be made is unavailable, and the
/// server runs on.
#[test]
fn a_ledger_that_cant_be_made_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("not-a-directory");
    std::fs::write(&file, "x").unwrap();
    let ledger = Ledger::start(&file, &usage(true));
    assert!(ledger.unavailable_reason().is_some());
    assert!(!ledger.recording());
    assert_eq!(ledger.size_bytes(), None);
    ledger.shutdown();
}

/// Not upstream's: the ledger records while `usage-statistics-enabled` is
/// on, and says so.
#[test]
fn recording_follows_the_statistics_setting() {
    let on = tempfile::tempdir().unwrap();
    let ledger = Ledger::start(on.path(), &usage(true));
    assert!(ledger.recording());
    assert_eq!(ledger.file(), Some(on.path().join(LEDGER_FILE).as_path()));
    assert!(ledger.size_bytes().unwrap() > 0);
    ledger.shutdown();

    let off = tempfile::tempdir().unwrap();
    let ledger = Ledger::start(off.path(), &usage(false));
    assert!(!ledger.recording());
    assert!(ledger.unavailable_reason().is_none());
    ledger.shutdown();
}

/// Not upstream's: the file is opened on the writer's thread, so starting
/// the ledger doesn't wait on the disk. The directory is made at once; the
/// API's calls wait for the opening, and the records made meanwhile are
/// written once it is done.
#[tokio::test]
async fn the_file_is_opened_in_the_background() {
    let dir = tempfile::tempdir().unwrap();
    let logs = dir.path().join("logs");
    let ledger = Ledger::start(&logs, &usage(true));
    assert!(logs.is_dir());
    ledger.opened().await;
    assert!(logs.join(LEDGER_FILE).is_file());
    ledger.shutdown();

    // An opening that waits for the test to let it go.
    let (go, wait) = mpsc::channel::<()>();
    let (sender, receiver) = mpsc::sync_channel(16);
    let path = dir.path().join(LEDGER_FILE);
    let ledger =
        Ledger::open_in_background(path.clone(), &usage(true), receiver, None, move |path| {
            wait.recv().unwrap();
            Ok((schema::open(path)?, schema::connect(path)?))
        });
    sender
        .send(event("2026-10-05T10:00:00Z", "codex", "gpt-5"))
        .unwrap();
    let count = tokio::spawn({
        let ledger = ledger.clone();
        async move {
            ledger
                .run(|connection| {
                    connection.query_row("SELECT COUNT(*) FROM settings", [], |row| {
                        row.get::<_, i64>(0)
                    })
                })
                .await
        }
    });
    let opened = tokio::spawn({
        let ledger = ledger.clone();
        async move { ledger.opened().await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!path.exists());
    assert!(!count.is_finished());
    assert!(!opened.is_finished());

    go.send(()).unwrap();
    opened.await.unwrap();
    assert_eq!(count.await.unwrap().unwrap(), 4);
    assert_eq!(ledger.unavailable_reason(), None);
    assert!(ledger.recording());
    drop(sender);
    ledger.shutdown();
    let connection = schema::connect(&path).unwrap();
    assert_eq!(
        column::<String>(&connection, "SELECT model FROM requests"),
        ["gpt-5"]
    );
}

/// Not upstream's: a ledger whose opening fails on the writer's thread is
/// unavailable, saying why, and ends its observation; one whose opening
/// panics is unavailable too, rather than leaving its callers waiting.
#[tokio::test]
async fn a_failed_opening_leaves_it_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let usage = usage(true);
    let (receiver, observation) = usage.observe();
    let ledger = Ledger::open_in_background(
        dir.path().join(LEDGER_FILE),
        &usage,
        receiver,
        Some(observation),
        |_| Err("no disk".to_owned()),
    );
    let read = ledger.run(|_| Ok(())).await;
    assert!(
        matches!(&read, Err(LedgerError::Unavailable(reason)) if reason == "no disk"),
        "{read:?}"
    );
    assert_eq!(ledger.unavailable_reason(), Some("no disk"));
    assert!(!ledger.recording());
    assert!(ledger.inner.observation.lock().unwrap().is_none());
    ledger.shutdown();

    let (_sender, receiver) = mpsc::sync_channel(1);
    let ledger =
        Ledger::open_in_background(dir.path().join(LEDGER_FILE), &usage, receiver, None, |_| {
            panic!("the test's opening panics")
        });
    ledger.opened().await;
    assert_eq!(
        ledger.unavailable_reason(),
        Some("opening the ledger stopped")
    );
    ledger.shutdown();
}

/// Not upstream's: each record the writer gets becomes a row, with the
/// credential by ID, index and label and the client key masked and
/// hashed; a credential without an ID and a call without a client key are
/// kept as none.
#[test]
fn records_become_rows() {
    let dir = tempfile::tempdir().unwrap();
    let (sender, receiver) = mpsc::sync_channel(16);
    let ledger = Ledger::for_test(dir.path(), &usage(true), receiver);

    let mut full = event("2026-10-05T10:00:00.250Z", "codex", "gpt-5");
    full.request_id = "0b7c3f4e-1234abcd".to_owned();
    full.alias = "fast".to_owned();
    full.stream = true;
    full.latency = Duration::from_millis(4210);
    full.ttft = Some(Duration::from_millis(640));
    full.tokens = tokens(12_400, 9000, 100, 830, 512);
    full.total_tokens = 13_230;
    full.credential = Some(EventCredential {
        id: "codex-user@example.com.json".to_owned(),
        auth_index: "3".to_owned(),
        label: "user@example.com".to_owned(),
        auth_type: "oauth".to_owned(),
    });
    full.client_key = ClientKey::new("sk-0123456789abcdefghijklmnopqrstuv9f3k");

    let mut failed = event("2026-10-05T10:00:01Z", "claude", "claude-x");
    failed.failed = true;
    failed.status = 529;
    failed.credential = Some(EventCredential::default());

    sender.send(full).unwrap();
    sender.send(failed).unwrap();
    drop(sender);
    ledger.shutdown();

    let connection = schema::connect(&dir.path().join(LEDGER_FILE)).unwrap();
    let secret = client_key_secret(&connection).unwrap();
    type Row = (
        i64,
        String,
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<Row> = connection
        .prepare(
            "SELECT ts, request_id, endpoint, provider, model, alias, credential_id, auth_index, \
             credential_label, auth_type, client_key_id, client_key_masked FROM requests ORDER BY id",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
                row.get(10)?,
                row.get(11)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let some = |text: &str| Some(text.to_owned());
    assert_eq!(
        rows,
        [
            (
                ms("2026-10-05T10:00:00.250Z"),
                "0b7c3f4e-1234abcd".to_owned(),
                "POST /v1/chat/completions".to_owned(),
                "codex".to_owned(),
                "gpt-5".to_owned(),
                "fast".to_owned(),
                some("codex-user@example.com.json"),
                some("3"),
                some("user@example.com"),
                some("oauth"),
                Some(client_key_id(
                    &secret,
                    "sk-0123456789abcdefghijklmnopqrstuv9f3k"
                )),
                some("sk-...9f3k"),
            ),
            (
                ms("2026-10-05T10:00:01Z"),
                "req-2026-10-05T10:00:01Z".to_owned(),
                "POST /v1/chat/completions".to_owned(),
                "claude".to_owned(),
                "claude-x".to_owned(),
                "claude-x".to_owned(),
                None,
                None,
                None,
                None,
                None,
                None,
            ),
        ]
    );

    type Numbers = (bool, bool, i64, i64, Option<i64>, [i64; 8]);
    let numbers: Vec<Numbers> = connection
        .prepare(
            "SELECT stream, failed, status, latency_ms, ttft_ms, input_tokens, \
             uncached_input_tokens, cache_read_tokens, cache_write_tokens, output_tokens, \
             reasoning_tokens, unclassified_tokens, total_tokens FROM requests ORDER BY id",
        )
        .unwrap()
        .query_map([], |row| {
            let mut counts = [0; 8];
            for (index, count) in counts.iter_mut().enumerate() {
                *count = row.get(5 + index)?;
            }
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                counts,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        numbers,
        [
            (
                true,
                false,
                200,
                4210,
                Some(640),
                [12_400, 3300, 9000, 100, 830, 512, 0, 13_230]
            ),
            (false, true, 529, 100, None, [0; 8]),
        ]
    );
    assert_eq!(ledger.dropped_records(), 0);
}

/// Not upstream's: rows past the retention go, then the oldest beyond the
/// row cap, by time and then by row.
#[test]
fn old_rows_and_rows_over_the_cap_are_pruned() {
    let dir = tempfile::tempdir().unwrap();
    let mut connection = schema::open(&dir.path().join(LEDGER_FILE)).unwrap();
    let now = ms("2026-10-05T12:00:00Z");
    let at = |offset_ms: i64, model: &str| {
        let mut event = event("2026-10-05T12:00:00Z", "codex", model);
        event.requested_at = chrono::DateTime::from_timestamp_millis(now + offset_ms).unwrap();
        event
    };
    insert(
        &mut connection,
        &[
            at(-91 * DAY, "too-old"),
            at(-89 * DAY, "kept-89"),
            at(-DAY, "kept-1"),
            at(0, "kept-0"),
        ],
        b"secret",
    )
    .unwrap();
    assert_eq!(prune(&connection, now).unwrap(), 1);
    let models = |connection: &Connection| {
        column::<String>(connection, "SELECT model FROM requests ORDER BY ts, id")
    };
    assert_eq!(models(&connection), ["kept-89", "kept-1", "kept-0"]);
    assert_eq!(prune(&connection, now).unwrap(), 0);

    write_settings(
        &mut connection,
        &Settings {
            retention_days: 2,
            max_rows: 1_000_000,
            currency: "USD".to_owned(),
        },
    )
    .unwrap();
    assert_eq!(prune(&connection, now).unwrap(), 1);
    assert_eq!(models(&connection), ["kept-1", "kept-0"]);

    insert(
        &mut connection,
        &[at(-10, "a"), at(-10, "b"), at(5, "c"), at(-20, "d")],
        b"secret",
    )
    .unwrap();
    write_settings(
        &mut connection,
        &Settings {
            retention_days: 2,
            max_rows: 3,
            currency: "USD".to_owned(),
        },
    )
    .unwrap();
    assert_eq!(prune(&connection, now).unwrap(), 3);
    assert_eq!(models(&connection), ["b", "kept-0", "c"]);
}

/// Not upstream's: neither the ledger file nor its write-ahead log holds a
/// client key, or any part of one longer than its masked form shows.
#[test]
fn no_client_key_is_kept() {
    const KEY: &str = "sk-live-Zq8W3xR7pL2mN9vB4tY6uK1jH5gF0dS";
    let dir = tempfile::tempdir().unwrap();
    let (sender, receiver) = mpsc::sync_channel(1024);
    let ledger = Ledger::for_test(dir.path(), &usage(true), receiver);
    for second in 0..200 {
        let mut event = event("2026-10-05T10:00:00Z", "codex", "gpt-5");
        event.requested_at += chrono::Duration::seconds(second);
        event.client_key = ClientKey::new(KEY);
        sender.send(event).unwrap();
    }
    drop(sender);
    ledger.shutdown();

    let mut bytes = Vec::new();
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        bytes.extend(std::fs::read(entry.unwrap().path()).unwrap());
    }
    let contains = |needle: &[u8]| bytes.windows(needle.len()).any(|window| window == needle);
    assert!(contains(b"sk-...F0dS"), "the masked key should be there");
    assert!(!contains(KEY.as_bytes()));
    for window in KEY.as_bytes().windows(8) {
        assert!(!contains(window), "{}", String::from_utf8_lossy(window));
    }
}
