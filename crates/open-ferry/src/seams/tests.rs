//! Not upstream's: the installer uses update's real file-based barrier.

use std::fs;
use std::io::Read as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

// Not upstream's: isolate the once-only registry and keep the root in the
// parent until the child and all its blocking polls have finished.
#[test]
fn installed_barriers_use_update_files_without_blocking_the_runtime() {
    const CHILD: &str = "OFP_BG_INSTALL_BARRIERS_CHILD";
    const ROOT: &str = "OFP_BG_INSTALL_BARRIERS_ROOT";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "seams::tests::installed_barriers_use_update_files_without_blocking_the_runtime",
            ])
            .env(CHILD, "1")
            .env(ROOT, root.path())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let output = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes).unwrap();
            bytes
        });
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let bytes = output.join().unwrap();
                    let text = String::from_utf8_lossy(&bytes);
                    assert!(status.success(), "{text}");
                    assert!(
                        text.contains("test result: ok. 1 passed;"),
                        "child ran no test: {text}"
                    );
                    break;
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                result => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = output.join();
                    panic!("barrier test child did not finish: {result:?}");
                }
            }
        }
        return;
    }
    let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
    let dir = root.join(".blue-green-test");
    fs::create_dir_all(&dir).unwrap();
    super::install_barriers(&root).unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            fs::write(dir.join("persist.before.hold"), []).unwrap();
            let task = tokio::spawn(open_ferry_core::test_hooks::barrier("persist.before"));
            tokio::time::timeout(Duration::from_secs(5), async {
                while !dir.join("persist.before.arrived").exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert!(!task.is_finished());
            fs::write(dir.join("persist.before.release"), []).unwrap();
            task.await.unwrap().unwrap();
            fs::write(dir.join("refresh.before_save.fail"), []).unwrap();
            let error = open_ferry_core::test_hooks::barrier("refresh.before_save")
                .await
                .unwrap_err();
            assert!(error.to_string().contains("refresh.before_save"));
            assert_eq!(
                fs::read(dir.join("refresh.before_save.failed")).unwrap(),
                b"x"
            );
        });
}

use crate::service::worker::hooks::{Barrier, Event, Fault};

use super::{Seams, binary_marker, event_name, fault_name, mark, parse_fault};

fn events(dir: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(dir.join(".blue-green-test").join("events.log"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

// A fault file is `<value>[@<marker>]`: a bare value applies to every worker,
// one with a marker only to the worker so marked.
#[test]
fn a_fault_applies_to_the_worker_its_marker_names() {
    assert_eq!(parse_fault("ready", "a").as_deref(), Some("ready"));
    assert_eq!(parse_fault("ready\n", "a").as_deref(), Some("ready"));
    assert_eq!(parse_fault("ready@a", "a").as_deref(), Some("ready"));
    assert_eq!(parse_fault("ready@b", "a"), None);
    assert_eq!(parse_fault("", "a").as_deref(), Some(""));
    assert_eq!(parse_fault("@a", "a").as_deref(), Some(""));
    assert_eq!(parse_fault("@b", "a"), None);
    // The last `@` is the marker's.
    assert_eq!(parse_fault("x@y@a", "a").as_deref(), Some("x@y"));
}

#[test]
fn a_fault_file_is_read_each_time_and_by_marker() {
    let dir = tempfile::tempdir().unwrap();
    let a = Seams::with_marker(dir.path(), "a".to_owned()).unwrap();
    let b = Seams::with_marker(dir.path(), "b".to_owned()).unwrap();
    let file = dir.path().join(".blue-green-test").join("fault.slow-ready");
    assert_eq!(a.fault(Fault::SlowReady), None, "no file, no fault");
    std::fs::write(&file, "250@b").unwrap();
    assert_eq!(a.fault(Fault::SlowReady), None);
    assert_eq!(b.fault(Fault::SlowReady).as_deref(), Some("250"));
    std::fs::write(&file, "1").unwrap();
    assert_eq!(a.fault(Fault::SlowReady).as_deref(), Some("1"));
    std::fs::remove_file(&file).unwrap();
    assert_eq!(b.fault(Fault::SlowReady), None, "gone with its file");
    // Another kind's file is another fault.
    std::fs::write(
        dir.path().join(".blue-green-test").join("fault.fail-load"),
        "",
    )
    .unwrap();
    assert_eq!(a.fault(Fault::FailLoad).as_deref(), Some(""));
    assert_eq!(a.fault(Fault::SlowReady), None);
}

#[test]
fn every_fault_barrier_and_event_has_its_name() {
    let faults = [
        (Fault::FailLoad, "fail-load"),
        (Fault::FailBeforeReady, "fail-before-ready"),
        (Fault::ExitAfter, "exit-after"),
        (Fault::SlowReady, "slow-ready"),
        (Fault::IgnoreHandover, "ignore-handover"),
        (Fault::HoldRefresh, "hold-refresh"),
        (Fault::DropChannel, "drop-channel"),
    ];
    for (fault, name) in faults {
        assert_eq!(fault_name(fault), name);
    }
    assert_eq!(event_name(Event::HandoverWritten), "handover.written");
    assert_eq!(event_name(Event::HandoverFailed), "handover_failed");
    assert_eq!(event_name(Event::PeersGone), "peers_gone");
}

// L9.12: one JSON line per event, with the process, the marker and whether
// the lease was held.
#[test]
fn the_event_log_has_a_line_per_event() {
    let dir = tempfile::tempdir().unwrap();
    let seams = Seams::with_marker(dir.path(), "m1".to_owned()).unwrap();
    seams.event("loaded", false);
    seams.event("ready", true);
    let lines = events(dir.path());
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["event"], "loaded");
    assert_eq!(lines[0]["lease"], false);
    assert_eq!(lines[1]["event"], "ready");
    assert_eq!(lines[1]["lease"], true);
    for line in &lines {
        assert_eq!(line["marker"], "m1");
        assert_eq!(line["pid"], std::process::id());
    }
}

// A barrier logs its arrival before it waits, and an injected failure is an
// error the caller reports.
#[test]
fn a_barrier_logs_its_arrival_and_fails_on_request() {
    let dir = tempfile::tempdir().unwrap();
    let seams = Seams::with_marker(dir.path(), "m".to_owned()).unwrap();
    seams.barrier_blocking(Barrier::GateClosed, true).unwrap();
    std::fs::write(
        dir.path()
            .join(".blue-green-test")
            .join("lease.release.fail"),
        [],
    )
    .unwrap();
    let error = seams
        .barrier_blocking(Barrier::LeaseRelease, false)
        .unwrap_err();
    assert!(error.to_string().contains("lease.release"), "{error}");
    let lines = events(dir.path());
    let seen: Vec<_> = lines
        .iter()
        .map(|line| {
            (
                line["event"].as_str().unwrap().to_owned(),
                line["lease"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            ("handover.gate_closed".to_owned(), true),
            ("lease.release".to_owned(), false)
        ]
    );
}

#[test]
fn clearing_an_arrival_removes_only_its_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let seams = Seams::with_marker(dir.path(), "m".to_owned()).unwrap();
    let files = dir.path().join(".blue-green-test");
    std::fs::write(files.join("handover.gate_closing.arrived"), []).unwrap();
    std::fs::write(files.join("lease.release.arrived"), []).unwrap();
    seams.clear_arrivals(&[Barrier::GateClosing, Barrier::BeforeReload]);
    assert!(!files.join("handover.gate_closing.arrived").exists());
    assert!(files.join("lease.release.arrived").exists());
}

// Without a `test-marker` file beside the binary the marker is the version.
#[test]
fn the_binary_marker_defaults_to_the_version() {
    if std::env::current_exe()
        .unwrap()
        .with_file_name("test-marker")
        .exists()
    {
        return;
    }
    assert_eq!(binary_marker().unwrap(), env!("CARGO_PKG_VERSION"));
}

// Every answer carries the marker, a miss and a failure included.
#[tokio::test]
async fn every_response_carries_the_marker_header() {
    use axum::routing::get;
    use tower_service::Service as _;

    let mut router = mark(
        axum::Router::new()
            .route("/ok", get(|| async { "ok" }))
            .route(
                "/bad",
                get(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }),
            ),
        "marker-7",
    );
    for (path, status) in [("/ok", 200), ("/bad", 503), ("/missing", 404)] {
        let response = router
            .call(
                axum::http::Request::builder()
                    .uri(path)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status, "{path}");
        assert_eq!(
            response.headers().get("x-open-ferry-test-marker").unwrap(),
            "marker-7",
            "{path}"
        );
    }
}
