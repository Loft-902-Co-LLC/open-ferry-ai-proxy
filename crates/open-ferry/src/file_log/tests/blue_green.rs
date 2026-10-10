//! Not upstream's: worker-facing controls before configure and on resume.

use std::fs;
use std::time::{Duration, Instant};

use crate::file_log::{self, FileLog, MAIN_LOG};

// Not upstream's: isolate process-wide switches from parallel existing tests.
#[test]
fn worker_controls_apply_before_configure_and_reopen_the_queue() {
    const CHILD: &str = "OFP_BG_LOG_CONTROLS_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "file_log::tests::blue_green::worker_controls_apply_before_configure_and_reopen_the_queue"])
            .env(CHILD, "1").output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("test result: ok. 1 passed;"));
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(MAIN_LOG);
    fs::write(&path, vec![b'a'; 10 * 1024 * 1024]).unwrap();
    let old = dir.path().join("old.log");
    fs::write(&old, b"old").unwrap();
    file_log::set_rotation(false);
    file_log::set_cleaning(false);
    let errors = dir.path().join("errors");
    fs::create_dir(&errors).unwrap();
    for n in 0..3 {
        fs::write(errors.join(format!("error-{n}.log")), b"e").unwrap();
    }
    let request = open_ferry_core::observe::request_log::CleaningControls::process_default();
    request.cleanup(&errors, 1).unwrap();
    assert_eq!(fs::read_dir(&errors).unwrap().count(), 3);
    let log = FileLog::default();
    let snapshot = file_log::control::cleaning().snapshot();
    log.configure(dir.path(), true, 1);
    log.output.write(b"standby\n".to_vec());
    log.sync();
    file_log::control::cleaning().wait_observed(snapshot);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 3);
    assert!(fs::read(&path).unwrap().ends_with(b"standby\n"));

    let aside = dir.path().join("peer-rotated.log");
    fs::rename(&path, &aside).unwrap();
    fs::write(&path, b"peer\n").unwrap();
    log.reopen().unwrap();
    log.output.write(b"resumed\n".to_vec());
    log.sync();
    assert_eq!(fs::read(&path).unwrap(), b"peer\nresumed\n");
    assert!(fs::read(&aside).unwrap().ends_with(b"standby\n"));

    // The latest reconfiguration while disabled is the one enabled uses.
    let latest = dir.path().join("latest");
    fs::create_dir(&latest).unwrap();
    let retained = latest.join("retained.log");
    fs::write(&retained, vec![b'r'; 1536 * 1024]).unwrap();
    let snapshot = file_log::control::cleaning().snapshot();
    log.configure(&latest, true, 2);
    file_log::control::cleaning().wait_observed(snapshot);
    assert!(retained.exists());
    file_log::set_rotation(true);
    file_log::set_cleaning(true);
    request.cleanup(&errors, 1).unwrap();
    assert_eq!(fs::read_dir(&errors).unwrap().count(), 1);
    let deadline = Instant::now() + Duration::from_secs(5);
    // A 1 MB limit would delete retained.log; the latest 2 MB limit keeps it.
    file_log::set_cleaning(false);
    let pressure = latest.join("pressure.log");
    fs::write(&pressure, vec![b'p'; 3 * 1024 * 1024]).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&pressure)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
        .unwrap();
    // Wake a pass after adding distinguishable pressure.
    file_log::set_cleaning(true);
    while pressure.exists() {
        assert!(Instant::now() < deadline, "cleaning did not resume");
        std::thread::sleep(Duration::from_millis(10));
    }
    // Drain the observed pass before checking its retention decision.
    drop(file_log::control::cleaning().enabled.lock().unwrap());
    assert!(
        retained.exists(),
        "latest 2 MB limit must retain the 1.5 MB file"
    );
    assert!(aside.exists(), "old directory must no longer be cleaned");
    assert_eq!(fs::read(&path).unwrap(), b"peer\nresumed\n");
    log.configure(dir.path(), false, 0);
}

#[test]
fn supervised_start_does_not_rotate_until_activation() {
    let controls = file_log::LogControls::supervised();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(MAIN_LOG);
    fs::write(&path, vec![b'x'; 10 * 1024 * 1024]).unwrap();
    let log = FileLog::with_controls(controls.clone());
    log.configure(dir.path(), true, 0);
    log.output.write(b"waiting for lease\n".to_vec());
    log.sync();
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    assert!(fs::read(&path).unwrap().ends_with(b"waiting for lease\n"));

    controls.set_rotation(true);
    log.output.write(b"active\n".to_vec());
    log.sync();
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    assert_eq!(fs::read(&path).unwrap(), b"active\n");
}

#[test]
fn supervised_start_retains_logs_until_peers_gone() {
    let controls = file_log::LogControls::supervised();
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join(MAIN_LOG), b"active\n").unwrap();
    let old = dir.path().join("old.log");
    fs::write(&old, vec![b'o'; 2 * 1024 * 1024]).unwrap();
    for n in 0..3 {
        fs::write(dir.path().join(format!("error-{n}.log")), b"e").unwrap();
    }
    let log = FileLog::with_controls(controls.clone());
    let snapshot = controls.cleaning.snapshot();
    log.configure(dir.path(), true, 1);
    controls.cleaning.wait_observed(snapshot);
    controls.request_cleaning().cleanup(dir.path(), 1).unwrap();
    assert!(old.exists());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 5);
    assert!(!controls.set_cleaning(true));
    assert!(controls.set_cleaning(true));
    controls.request_cleaning().cleanup(dir.path(), 1).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while old.exists() {
        assert!(
            Instant::now() < deadline,
            "supervised cleaning did not start"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(controls.set_cleaning(false));
    assert!(!controls.set_cleaning(false));
}

// Not upstream's: private worker handles affect their writers and request
// cleaners, not another worker or the production defaults.
#[test]
fn private_handles_leave_other_workers_and_defaults_alone() {
    let a = file_log::LogControls::default();
    let b = file_log::LogControls::new();
    assert!(std::sync::Arc::ptr_eq(
        &FileLog::default().controls.cleaning,
        &file_log::control::cleaning(),
    ));
    a.set_rotation(false);
    a.set_cleaning(false);
    assert!(*b.rotation.lock().unwrap());
    assert!(*file_log::control::rotation().lock().unwrap());
    assert!(*file_log::control::cleaning().enabled.lock().unwrap());
    let da = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    for dir in [&da, &db] {
        fs::write(dir.path().join(MAIN_LOG), vec![b'x'; 10 * 1024 * 1024]).unwrap();
        for n in 0..3 {
            fs::write(dir.path().join(format!("error-{n}.log")), b"e").unwrap();
        }
    }
    let (_, la) = file_log::init_with_controls::<tracing_subscriber::Registry>(a.clone());
    let (_, lb) = file_log::init_with_controls::<tracing_subscriber::Registry>(b.clone());
    let old = da.path().join("old.log");
    fs::write(&old, vec![b'o'; 2 * 1024 * 1024]).unwrap();
    let snapshot = a.cleaning.snapshot();
    la.configure(da.path(), true, 1);
    a.cleaning.wait_observed(snapshot);
    assert!(
        old.exists(),
        "the main cleaner must use its private disabled handle"
    );
    lb.configure(db.path(), true, 0);
    la.output.write(b"a\n".to_vec());
    lb.output.write(b"b\n".to_vec());
    la.sync();
    lb.sync();
    assert!(
        fs::read(da.path().join(MAIN_LOG))
            .unwrap()
            .ends_with(b"a\n")
    );
    assert_eq!(fs::read(db.path().join(MAIN_LOG)).unwrap(), b"b\n");
    a.request_cleaning().cleanup(da.path(), 1).unwrap();
    b.request_cleaning().cleanup(db.path(), 1).unwrap();
    assert_eq!(fs::read_dir(da.path()).unwrap().count(), 5);
    assert_eq!(
        fs::read_dir(db.path())
            .unwrap()
            .filter(|e| e
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("error-"))
            .count(),
        1
    );
}

// Not upstream's: pause between the halves, force an opposing setter, then
// verify both halves agree, not merely that the pair mutex was held.
#[test]
fn opposing_cleaning_setters_serialize_the_pair() {
    let controls = file_log::LogControls::new();
    let (arrived, wait) = std::sync::mpsc::channel();
    let (release, resume) = std::sync::mpsc::channel();
    *controls.pair_pause.lock().unwrap() = Some(std::sync::Arc::new(file_log::control::Pass {
        arrived,
        release: std::sync::Mutex::new(resume),
    }));
    let disable = controls.clone();
    let thread = std::thread::spawn(move || disable.set_cleaning(false));
    wait.recv_timeout(Duration::from_secs(5)).unwrap();
    let enable = controls.clone();
    let (done, finished) = std::sync::mpsc::channel();
    let other = std::thread::spawn(move || {
        enable.set_cleaning(true);
        done.send(()).unwrap();
    });
    let overtook = finished.recv_timeout(Duration::from_secs(1)).is_ok();
    release.send(()).unwrap();
    thread.join().unwrap();
    other.join().unwrap();
    let main_enabled = *controls.cleaning.enabled.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    for n in 0..3 {
        fs::write(dir.path().join(format!("error-{n}.log")), b"e").unwrap();
    }
    controls.request_cleaning().cleanup(dir.path(), 1).unwrap();
    let request_enabled = fs::read_dir(dir.path()).unwrap().count() == 1;
    assert_eq!(main_enabled, request_enabled, "cleaner halves diverged");
    assert!(
        main_enabled && !overtook,
        "opposing setter bypassed the pair"
    );
}

// Not upstream's: disable must drain a deletion already inside remove_file's
// closure, not merely stop a pass that has not reached its stop check yet.
#[test]
fn disabling_cleaning_drains_an_inflight_deletion() {
    let controls = file_log::LogControls::new();
    let (arrived, release) = held_deletion(&controls);
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("old.log"), vec![b'o'; 2 * 1024 * 1024]).unwrap();
    let log = FileLog::with_controls(controls.clone());
    log.configure(dir.path(), true, 1);
    arrived.recv_timeout(Duration::from_secs(5)).unwrap();
    let disable = controls.clone();
    let (done, finished) = std::sync::mpsc::channel();
    let snapshot = controls.cleaning.snapshot();
    let thread = std::thread::spawn(move || {
        disable.set_cleaning(false);
        done.send(()).unwrap();
    });
    let completed = finished.recv_timeout(Duration::from_secs(1)).is_ok();
    let still_running = !thread.is_finished();
    release.send(()).unwrap();
    thread.join().unwrap();
    assert!(
        !completed && still_running,
        "disable did not drain deletion"
    );
    controls.cleaning.wait_observed_enabled(snapshot, false);
    let pressure = dir.path().join("pressure.log");
    fs::write(&pressure, vec![b'p'; 2 * 1024 * 1024]).unwrap();
    let snapshot = controls.cleaning.snapshot();
    controls.set_cleaning(false);
    controls.cleaning.wait_observed_enabled(snapshot, false);
    assert!(pressure.exists(), "deletion followed the completed disable");
}

// Not upstream's: reload lets at most the old deletion already under way
// finish; observation of the new generation cannot come from that old pass.
#[test]
fn reload_allows_only_the_inflight_old_generation_deletion() {
    let controls = file_log::LogControls::new();
    let (arrived, release) = held_deletion(&controls);
    let old = tempfile::tempdir().unwrap();
    let next = tempfile::tempdir().unwrap();
    for n in 0..3 {
        fs::write(
            old.path().join(format!("old-{n}.log")),
            vec![b'o'; 2 * 1024 * 1024],
        )
        .unwrap();
    }
    let log = FileLog::with_controls(controls.clone());
    log.configure(old.path(), true, 1);
    arrived.recv_timeout(Duration::from_secs(5)).unwrap();
    let snapshot = controls.cleaning.snapshot();
    log.configure(next.path(), true, 1);
    release.send(()).unwrap();
    controls.cleaning.wait_observed(snapshot);
    drop(controls.cleaning.enabled.lock().unwrap());
    assert_eq!(
        fs::read_dir(old.path()).unwrap().count(),
        2,
        "only one old deletion may finish"
    );
    let snapshot = controls.cleaning.snapshot();
    controls.set_cleaning(false);
    controls.cleaning.wait_observed(snapshot);
    let pressure = next.path().join("after-reload.log");
    fs::write(&pressure, vec![b'p'; 2 * 1024 * 1024]).unwrap();
    let snapshot = controls.cleaning.snapshot();
    controls.set_cleaning(false);
    controls.cleaning.wait_observed(snapshot);
    assert!(pressure.exists());
    assert_eq!(fs::read_dir(old.path()).unwrap().count(), 2);
}

fn held_deletion(
    controls: &file_log::LogControls,
) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (arrived, wait) = std::sync::mpsc::channel();
    let (release, resume) = std::sync::mpsc::channel();
    *controls.cleaning.deletion.lock().unwrap() =
        Some(std::sync::Arc::new(file_log::control::Pass {
            arrived,
            release: std::sync::Mutex::new(resume),
        }));
    (wait, release)
}

fn held_pass(
    controls: &file_log::LogControls,
) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (arrived, wait) = std::sync::mpsc::channel();
    let (release, resume) = std::sync::mpsc::channel();
    *controls.cleaning.pass.lock().unwrap() = Some(std::sync::Arc::new(file_log::control::Pass {
        arrived,
        release: std::sync::Mutex::new(resume),
    }));
    (wait, release)
}

// Not upstream's: disabling cleaning drains a real pass holding the IO lock.
#[test]
fn disabling_cleaning_waits_for_an_inflight_pass() {
    let controls = file_log::LogControls::new();
    let (arrived, release) = held_pass(&controls);
    let dir = tempfile::tempdir().unwrap();
    let log = FileLog::with_controls(controls.clone());
    log.configure(dir.path(), true, 1);
    arrived.recv_timeout(Duration::from_secs(5)).unwrap();
    let disable = controls.clone();
    let thread = std::thread::spawn(move || disable.set_cleaning(false));
    let deadline = Instant::now() + Duration::from_secs(5);
    while controls.cleaning_pair.try_lock().is_ok() {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert!(!thread.is_finished());
    assert!(controls.cleaning.enabled.try_lock().is_err());
    release.send(()).unwrap();
    thread.join().unwrap();
    assert!(!*controls.cleaning.enabled.lock().unwrap());
}

// Not upstream's: ordinary reload never joins a held pass; the retired
// cleaner sees its stop signal before deleting anything else.
#[test]
fn configure_signals_a_held_cleaner_without_waiting_for_it() {
    let controls = file_log::LogControls::new();
    let (arrived, release) = held_pass(&controls);
    let dir = tempfile::tempdir().unwrap();
    let protected = dir.path().join("old.log");
    fs::write(&protected, vec![b'x'; 2 * 1024 * 1024]).unwrap();
    let log = FileLog::with_controls(controls.clone());
    log.configure(dir.path(), true, 1);
    arrived.recv_timeout(Duration::from_secs(5)).unwrap();
    let next = log.clone();
    let path = dir.path().to_path_buf();
    let (done, wait) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        next.configure(&path, true, 0);
        done.send(()).unwrap();
    });
    // Release even on a failing assertion, so no test-owned thread is stranded.
    let result = wait.recv_timeout(Duration::from_secs(5));
    release.send(()).unwrap();
    thread.join().unwrap();
    assert!(result.is_ok(), "configure waited for a cleaning pass");
    // Taking the IO lock proves the retired pass has ended.
    drop(controls.cleaning.enabled.lock().unwrap());
    assert!(protected.exists(), "retired cleaner deleted after stop");
}
