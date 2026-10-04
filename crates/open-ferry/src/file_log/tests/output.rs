//! Not upstream's: where the lines go, `main.log`'s rotation, and when a
//! config changes the output. Upstream's tests of lumberjack aren't
//! upstream's own.

use std::fs::{self, File, OpenOptions};
use std::path::Path;
use std::sync::Arc;

use open_ferry_core::config::Config;
use tracing_subscriber::layer::SubscriberExt;

use crate::file_log::format::FormatLayer;
use crate::file_log::writer::Rotating;
use crate::file_log::{FileLog, MAIN_LOG, reconfigure};

/// Runs `log` with `file_log`'s layer as the logger, and waits until its
/// lines are written.
fn logged(file_log: &FileLog, log: impl FnOnce()) {
    let layer = FormatLayer::new(Arc::clone(&file_log.output));
    let subscriber = tracing_subscriber::registry().with(layer);
    tracing::subscriber::with_default(subscriber, log);
    file_log.sync();
}

/// The names of the files in `dir`, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Not upstream's: with `logging-to-file` the lines go to `main.log` in
/// the directory, which is made, and with it off to standard output again.
#[test]
fn lines_go_to_main_log_and_back() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("logs");
    let file_log = FileLog::default();
    assert_eq!(file_log.file(), None);

    file_log.configure(&dir, true, 0);
    let path = dir.join(MAIN_LOG);
    assert_eq!(file_log.file(), Some(path.clone()));
    logged(&file_log, || {
        tracing::info!(provider = "codex", credential = "a b", "hello");
        let span = tracing::info_span!(
            "request",
            request_id = "018f3a5b-1234-7abc-def0-12345678abcd"
        );
        span.in_scope(|| tracing::warn!("inside"));
    });
    let text = fs::read_to_string(&path).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text:?}");
    assert!(
        lines[0].contains("] [--------] [info ] [output.rs:"),
        "{text:?}"
    );
    assert!(
        lines[0].ends_with("hello provider=codex credential=\"a b\""),
        "{text:?}"
    );
    assert!(
        lines[1].contains("] [5678abcd] [warn ] [output.rs:"),
        "{text:?}"
    );

    file_log.configure(&dir, false, 0);
    assert_eq!(file_log.file(), None);
}

/// Not upstream's: a reload that leaves `logging-to-file` and
/// `logs-max-total-size-mb` as they were leaves the output alone, as
/// upstream's reload does.
#[test]
fn an_unrelated_reload_changes_nothing() {
    let mut config = Config::default();
    config.logging_to_file = true;
    let file_log = FileLog::default();
    reconfigure(&file_log, Some(&config), &config);
    assert_eq!(file_log.file(), None);
}

/// Not upstream's: a line that would take `main.log` past the limit moves
/// it to `main-<local time>.log` first, which a reader holding it open
/// doesn't prevent.
#[test]
fn rotates_past_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(MAIN_LOG);
    let line = [b'x'; 59].iter().chain(b"\n").copied().collect::<Vec<_>>();
    let mut writer = Rotating::new(path.clone(), 100);
    writer.write(&line).unwrap();
    writer.flush().unwrap();
    let reader = File::open(&path).unwrap();
    writer.write(&line).unwrap();
    writer.flush().unwrap();
    drop(reader);

    let names = names(dir.path());
    assert_eq!(names.len(), 2, "{names:?}");
    assert_eq!(names[1], MAIN_LOG);
    let rotated = &names[0];
    let time = rotated
        .strip_prefix("main-")
        .and_then(|rest| rest.strip_suffix(".log"))
        .unwrap();
    assert!(
        chrono::NaiveDateTime::parse_from_str(time, "%Y-%m-%dT%H-%M-%S%.3f").is_ok(),
        "{rotated}"
    );
    assert_eq!(fs::read(&path).unwrap(), line);
    assert_eq!(fs::read(dir.path().join(rotated)).unwrap(), line);
}

/// Not upstream's: an existing `main.log` is appended to, and one already
/// too full is rotated before the first line.
#[test]
fn opens_existing_or_new() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(MAIN_LOG);
    fs::write(&path, "old\n").unwrap();
    let mut writer = Rotating::new(path.clone(), 100);
    writer.write(b"new\n").unwrap();
    writer.flush().unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "old\nnew\n");
    drop(writer);

    let mut writer = Rotating::new(path.clone(), 10);
    writer.write(b"again\n").unwrap();
    writer.flush().unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "again\n");
    assert_eq!(names(dir.path()).len(), 2);
}

/// Not upstream's: after the logs route truncates `main.log`, lines go on
/// at its new end and it isn't rotated early.
#[test]
fn a_truncated_file_isnt_rotated_early() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(MAIN_LOG);
    let mut writer = Rotating::new(path.clone(), 100);
    writer.write(&[b'a'; 60]).unwrap();
    writer.flush().unwrap();
    OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(0)
        .unwrap();
    writer.write(&[b'b'; 60]).unwrap();
    writer.flush().unwrap();
    assert_eq!(names(dir.path()), [MAIN_LOG]);
    assert_eq!(fs::read(&path).unwrap(), [b'b'; 60]);
}

/// Not upstream's: a line longer than the limit isn't written, as
/// lumberjack refuses it.
#[test]
fn refuses_a_line_over_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Rotating::new(dir.path().join(MAIN_LOG), 10);
    let error = writer.write(&[b'x'; 20]).unwrap_err();
    assert_eq!(
        error.to_string(),
        "write length 20 exceeds maximum file size 10"
    );
}
