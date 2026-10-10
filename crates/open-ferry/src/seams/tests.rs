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
