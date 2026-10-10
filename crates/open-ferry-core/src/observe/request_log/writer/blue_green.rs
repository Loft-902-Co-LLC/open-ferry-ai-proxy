//! Not upstream's: the request error-file cleaner shares the worker's switch.

use super::*;

// Not upstream's: isolate the process-wide switch from parallel logger tests.
#[test]
fn error_cleanup_can_be_disabled_and_resumed() {
    const CHILD: &str = "OFP_BG_ERROR_CLEANUP_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "observe::request_log::writer::blue_green::error_cleanup_can_be_disabled_and_resumed"])
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
    for n in 0..3 {
        fs::write(dir.path().join(format!("error-{n}.log")), b"kept").unwrap();
    }
    set_cleaning(false);
    cleanup_old_error_logs(dir.path(), 1).unwrap();
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 3);
    set_cleaning(true);
    cleanup_old_error_logs(dir.path(), 1).unwrap();
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}
