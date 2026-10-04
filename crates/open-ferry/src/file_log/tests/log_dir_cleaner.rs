// Ported from CLIProxyAPI internal/logging/log_dir_cleaner_test.go
// (TestEnforceLogDirSizeLimitDeletesOldest,
// TestEnforceLogDirSizeLimitSkipsProtected) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The log directory's cleaner.

use std::fs::{self, File};
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::file_log::cleaner::{enforce_log_dir_size_limit, is_log_file_name};

/// Writes `size` bytes to `path`, last modified `modified_secs` after the
/// epoch.
fn write_log_file(path: &Path, size: usize, modified_secs: u64) {
    fs::write(path, vec![0u8; size]).unwrap();
    let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(modified_secs);
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
}

// Ports TestEnforceLogDirSizeLimitDeletesOldest.
#[test]
fn deletes_oldest() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    write_log_file(&dir.join("old.log"), 60, 1);
    write_log_file(&dir.join("mid.log"), 60, 2);
    let protected = dir.join("main.log");
    write_log_file(&protected, 60, 3);

    let deleted = enforce_log_dir_size_limit(dir, 120, Some(&protected)).unwrap();
    assert_eq!(deleted, 1);
    assert!(!dir.join("old.log").exists());
    assert!(dir.join("mid.log").exists());
    assert!(protected.exists());
}

// Ports TestEnforceLogDirSizeLimitSkipsProtected.
#[test]
fn skips_protected() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let protected = dir.join("main.log");
    write_log_file(&protected, 200, 1);
    write_log_file(&dir.join("other.log"), 50, 2);

    let deleted = enforce_log_dir_size_limit(dir, 100, Some(&protected)).unwrap();
    assert_eq!(deleted, 1);
    assert!(protected.exists());
    assert!(!dir.join("other.log").exists());
}

/// Not upstream's: only `*.log` and `*.log.gz` files count, in any case,
/// and a missing directory has nothing to delete.
#[test]
fn counts_log_files_only() {
    for name in ["a.log", "A.LOG", " b.log.gz ", "error-x.log"] {
        assert!(is_log_file_name(name), "{name:?}");
    }
    for name in ["", " ", "config.yaml", "a.log.bak", "a.gz", "auth.json"] {
        assert!(!is_log_file_name(name), "{name:?}");
    }

    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    write_log_file(&dir.join("old.json"), 500, 1);
    fs::create_dir(dir.join("nested.log")).unwrap();
    write_log_file(&dir.join("new.log"), 50, 2);
    assert_eq!(enforce_log_dir_size_limit(dir, 100, None).unwrap(), 0);
    assert!(dir.join("old.json").exists());

    let missing = dir.join("missing");
    assert_eq!(enforce_log_dir_size_limit(&missing, 1, None).unwrap(), 0);
}

/// Not upstream's: a log file another handle has open is deleted all the
/// same, as Rust opens files sharing delete access on Windows.
#[test]
fn deletes_files_open_elsewhere() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    write_log_file(&dir.join("open.log"), 80, 1);
    write_log_file(&dir.join("newer.log"), 80, 2);
    let held = File::open(dir.join("open.log")).unwrap();

    let deleted = enforce_log_dir_size_limit(dir, 100, None).unwrap();
    assert_eq!(deleted, 1);
    drop(held);
    assert!(!dir.join("open.log").exists());
    assert!(dir.join("newer.log").exists());
}
