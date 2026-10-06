//! Not upstream's: where the lines go, `main.log`'s rotation, and when a
//! config changes the output. Upstream's tests of lumberjack aren't
//! upstream's own.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use open_ferry_core::config::Config;
use tracing_subscriber::layer::SubscriberExt;

use crate::file_log::format::FormatLayer;
use crate::file_log::writer::{Output, QUEUE, Rotating};
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

/// Holds the thread the lines go to now, and lets it go on after a moment
/// from another thread, as a slow disk would.
fn hold_briefly(file_log: &FileLog) -> JoinHandle<()> {
    let (go, held) = mpsc::channel();
    file_log.output.hold(held);
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        let _ = go.send(());
    })
}

/// Not upstream's: turning `logging-to-file` off returns once the lines
/// queued for `main.log` are written and the file is closed, so the file
/// doesn't go on growing after.
#[test]
fn turning_off_waits_for_the_queued_lines() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("logs");
    let file_log = FileLog::default();
    file_log.configure(&dir, true, 0);
    let release = hold_briefly(&file_log);
    for n in 0..100 {
        file_log.output.write(format!("line {n}\n").into_bytes());
    }

    file_log.configure(&dir, false, 0);
    let text = fs::read_to_string(dir.join(MAIN_LOG)).unwrap();
    assert_eq!(text.lines().count(), 100, "{text:?}");
    assert_eq!(text.lines().last(), Some("line 99"));
    release.join().unwrap();
}

/// Not upstream's: turning `logging-to-file` off and on again at once
/// leaves one writer on `main.log`, which counts its size alone: the lines
/// queued before come first, and the new writer's only after them.
#[test]
fn off_and_on_again_keeps_one_writer() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("logs");
    let file_log = FileLog::default();
    file_log.configure(&dir, true, 0);
    let release = hold_briefly(&file_log);
    for n in 0..100 {
        file_log.output.write(format!("before {n}\n").into_bytes());
    }

    file_log.configure(&dir, false, 0);
    file_log.configure(&dir, true, 0);
    file_log.output.write(b"after\n".to_vec());
    file_log.sync();
    let text = fs::read_to_string(dir.join(MAIN_LOG)).unwrap();
    let want: Vec<String> = (0..100)
        .map(|n| format!("before {n}"))
        .chain(["after".to_owned()])
        .collect();
    assert_eq!(text.lines().collect::<Vec<_>>(), want);
    release.join().unwrap();
}

/// A standard output nothing reads until `go` says, which then keeps what
/// is written to it.
struct Stalled {
    go: Option<Receiver<()>>,
    written: Arc<Mutex<Vec<u8>>>,
}

impl Write for Stalled {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(go) = self.go.take() {
            let _ = go.recv();
        }
        let mut written = self.written.lock().unwrap_or_else(PoisonError::into_inner);
        written.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Not upstream's: lines for standard output go through a bounded queue,
/// so logging doesn't wait while nothing reads standard output. Past the
/// queue's size lines are dropped, and the writer says how many once it
/// can write again.
#[test]
fn standard_output_never_blocks() {
    let (go, stalled): (Sender<()>, _) = mpsc::channel();
    let written = Arc::new(Mutex::new(Vec::new()));
    let file_log = FileLog {
        output: Arc::new(Output::with_console(Box::new(Stalled {
            go: Some(stalled),
            written: Arc::clone(&written),
        }))),
        cleaner: Arc::default(),
    };
    let total = QUEUE + 100;
    let (done, logged) = mpsc::channel();
    let output = Arc::clone(&file_log.output);
    thread::spawn(move || {
        for n in 0..total {
            output.write(format!("line {n}\n").into_bytes());
        }
        let _ = done.send(());
    });
    logged
        .recv_timeout(Duration::from_secs(30))
        .expect("logging waited on standard output");

    go.send(()).unwrap();
    file_log.sync();
    let text = String::from_utf8(written.lock().unwrap().clone()).unwrap();
    let (notes, lines): (Vec<&str>, Vec<&str>) = text
        .lines()
        .partition(|line| line.contains("logging: dropped "));
    assert_eq!(notes.len(), 1, "{notes:?}");
    let dropped: usize = notes[0]
        .split("logging: dropped ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .unwrap()
        .parse()
        .unwrap();
    assert!(notes[0].ends_with("log line(s): the main log writer fell behind"));
    assert!(dropped >= 99, "{dropped}");
    assert_eq!(lines.len() + dropped, total);
    assert_eq!(lines.first(), Some(&"line 0"));
}

/// Not upstream's: while standard output is muted, as while the TUI runs,
/// its lines are dropped and `main.log`'s still written, and the tap sees
/// every line, wherever it goes.
#[test]
fn muting_drops_standard_output_and_the_tap_sees_every_line() {
    let written = Arc::new(Mutex::new(Vec::new()));
    let file_log = FileLog {
        output: Arc::new(Output::with_console(Box::new(Stalled {
            go: None,
            written: Arc::clone(&written),
        }))),
        cleaner: Arc::default(),
    };
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tap_seen = Arc::clone(&seen);
    file_log.set_tap(Some(Arc::new(move |line: &[u8]| {
        let line = String::from_utf8_lossy(line).trim_end().to_owned();
        tap_seen.lock().unwrap().push(line);
    })));
    file_log.mute_console(true);
    logged(&file_log, || tracing::info!("muted"));
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("logs");
    file_log.configure(&dir, true, 0);
    logged(&file_log, || tracing::warn!("to the file"));
    file_log.configure(&dir, false, 0);
    file_log.mute_console(false);
    file_log.set_tap(None);
    logged(&file_log, || tracing::info!("heard"));

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert!(
        seen[0].starts_with('[') && seen[0].ends_with("] muted"),
        "{seen:?}"
    );
    assert!(
        seen[1].contains("[warn") && seen[1].ends_with("] to the file"),
        "{seen:?}"
    );
    let file = fs::read_to_string(dir.join(MAIN_LOG)).unwrap();
    assert_eq!(file.trim_end(), seen[1]);
    let text = String::from_utf8(written.lock().unwrap().clone()).unwrap();
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.trim_end().ends_with("] heard"), "{text}");
}

/// Not upstream's: an email in a line, its message's or a field's, in a
/// file name, a path or an access line's query, reaches `main.log` and the
/// tap masked; upstream writes it as it is.
#[test]
fn emails_reach_main_log_masked() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("logs");
    let file_log = FileLog::default();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tap_seen = Arc::clone(&seen);
    file_log.set_tap(Some(Arc::new(move |line: &[u8]| {
        tap_seen
            .lock()
            .unwrap()
            .push(String::from_utf8_lossy(line).trim_end().to_owned());
    })));
    file_log.configure(&dir, true, 0);
    logged(&file_log, || {
        tracing::warn!(
            "skipping auth file /home/me/.cli-proxy-api/claude-john.doe@example.com.json: denied"
        );
        tracing::error!(
            auth_id = "codex-1a2b3c4d-jane@example.org-plus.json",
            "failed to refresh for jane@example.org"
        );
        tracing::info!(
            "200 | 1ms | 127.0.0.1 | GET     \"/v0/management/auth-files/download?name=codex-jane%40example.org-plus.json\""
        );
    });
    file_log.set_tap(None);

    let text = fs::read_to_string(dir.join(MAIN_LOG)).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text}");
    assert!(
        lines[0].ends_with(
            "skipping auth file /home/me/.cli-proxy-api/claude-j***@e***.com.json: denied"
        ),
        "{text}"
    );
    assert!(
        lines[1].ends_with(
            "failed to refresh for j***@e***.org auth_id=\"codex-1a2b3c4d-j***@e***.org-plus.json\""
        ),
        "{text}"
    );
    assert!(
        lines[2].ends_with("?name=codex-j***%40e***.org-plus.json\""),
        "{text}"
    );
    for leaked in ["john.doe", "jane@", "jane%40", "example.com", "example.org"] {
        assert!(!text.contains(leaked), "{leaked} in {text}");
    }
    assert_eq!(*seen.lock().unwrap(), lines);
}
