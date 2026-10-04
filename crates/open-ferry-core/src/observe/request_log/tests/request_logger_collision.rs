//! Ports CLIProxyAPI internal/logging/request_logger_collision_test.go
//! (v8.0.10, MIT): a log never overwrites another.

use std::collections::HashSet;
use std::fs;
use std::io::Write as _;
use std::thread;

use super::super::names::create_unique_log_file;

const FILENAME: &str = "v1_chat_completions-2026-09-23T120000-00000000.log";

// Ports TestCreateUniqueLogFile_ConcurrentCollisions.
#[test]
fn create_unique_log_file_concurrent_collisions() {
    let dir = tempfile::tempdir().unwrap();
    const WORKERS: usize = 20;
    let paths: Vec<_> = thread::scope(|scope| {
        let handles: Vec<_> = (0..WORKERS)
            .map(|worker| {
                let dir = dir.path();
                scope.spawn(move || {
                    let (mut file, path) = create_unique_log_file(dir, FILENAME).unwrap();
                    file.write_all(format!("worker-{worker}-content\n").as_bytes())
                        .unwrap();
                    path
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });

    let mut seen_paths = HashSet::new();
    let mut seen_contents = HashSet::new();
    for path in &paths {
        assert!(seen_paths.insert(path.clone()), "duplicate path {path:?}");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.ends_with("-00000000.log"), "{name}");
        let content = fs::read_to_string(path).unwrap();
        assert!(
            seen_contents.insert(content.clone()),
            "overwritten: {content:?}"
        );
    }
    assert_eq!(seen_paths.len(), WORKERS);
}

// Ports TestFileRequestLogger_DeterministicPreExistingCollision.
#[test]
fn file_request_logger_deterministic_pre_existing_collision() {
    let dir = tempfile::tempdir().unwrap();
    let (mut first, first_path) = create_unique_log_file(dir.path(), FILENAME).unwrap();
    first.write_all(b"first-log-content").unwrap();
    drop(first);

    let (mut second, second_path) = create_unique_log_file(dir.path(), FILENAME).unwrap();
    second.write_all(b"second-log-content").unwrap();
    drop(second);

    assert_eq!(
        second_path.file_name().unwrap(),
        "v1_chat_completions-2026-09-23T120000_1-00000000.log"
    );
    assert_eq!(fs::read_to_string(first_path).unwrap(), "first-log-content");
    assert_eq!(
        fs::read_to_string(second_path).unwrap(),
        "second-log-content"
    );
}
