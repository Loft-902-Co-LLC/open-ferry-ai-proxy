// Ported from CLIProxyAPI internal/auth/codex/filelock_windows.go and
// filelock_unix.go (v8.0.27, MIT); additional lock safety regression tests.
use super::*;
use std::fs;

fn scratch() -> tempfile::TempDir {
    let root = std::env::var_os("OFP_TEST_SCRATCH")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/ofp-scratch")
        });
    fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}

#[tokio::test]
async fn os_handles_contend_and_wait_times_out() {
    let dir = scratch();
    let path = dir.path().join("codex.json");
    let lock_path = credential_lock_path(&path).unwrap();
    fs::write(&lock_path, b"keep-content").unwrap();
    let first = open_lock(&lock_path).unwrap();
    first.try_lock().unwrap();
    let second = open_lock(&lock_path).unwrap();
    assert!(matches!(
        second.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    let start = Instant::now();
    let error = acquire_with_timeout(path.clone(), Duration::from_millis(100))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(start.elapsed() < Duration::from_secs(2));
    first.unlock().unwrap();
    drop(acquire_credential_lock(&path).await.unwrap());
    assert_eq!(fs::read(lock_path).unwrap(), b"keep-content");
}

#[tokio::test]
async fn process_mutex_contends_and_aliases_share_identity() {
    let dir = scratch();
    let path = dir.path().join("Codex.json");
    fs::write(&path, b"{}").unwrap();
    let lock = acquire_credential_lock(&path).await.unwrap();
    let alias = dir.path().join(".").join("Codex.json");
    let error = acquire_with_timeout(alias.clone(), Duration::from_millis(100))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(Arc::ptr_eq(
        &process_lock(&credential_lock_path(&path).unwrap()),
        &process_lock(&credential_lock_path(&alias).unwrap())
    ));
    drop(lock);
    drop(acquire_credential_lock(alias).await.unwrap());
}

#[test]
fn lock_identity_is_trimmed_absolute_and_lexical() {
    let dir = scratch();
    let path = dir.path().join("sub").join("..").join("codex.json");
    let expected = crate::auth::path::clean(dir.path()).join("codex.json.lock");
    assert_eq!(credential_lock_path(&path).unwrap(), expected);
    let padded = format!("  {}  ", path.display());
    assert_eq!(credential_lock_path(Path::new(&padded)).unwrap(), expected);
    let marker = dir.path().join("siwc-1-short-alias-result.txt");
    fs::write(marker, b"lexical identity deliberately preserves aliases").unwrap();
}

#[test]
fn credential_symlink_lock_stays_next_to_the_link() {
    let dir = scratch();
    let target = dir.path().join("target.json");
    let link = dir.path().join("link.json");
    fs::write(&target, b"{}").unwrap();
    #[cfg(windows)]
    let result = std::os::windows::fs::symlink_file(&target, &link);
    #[cfg(unix)]
    let result = std::os::unix::fs::symlink(&target, &link);
    if let Err(error) = result {
        #[cfg(windows)]
        if error.raw_os_error() == Some(1314) {
            eprintln!("skipped credential symlink: Windows denies symlink creation (1314)");
            return;
        }
        panic!("symlink creation failed: {error}");
    }
    assert_eq!(
        credential_lock_path(&link).unwrap(),
        crate::auth::path::clean(dir.path()).join("link.json.lock")
    );
    assert_ne!(
        credential_lock_path(&link).unwrap(),
        credential_lock_path(&target).unwrap()
    );
}

#[tokio::test]
async fn cancelled_waiter_and_error_release_the_lock() {
    let dir = scratch();
    let path = dir.path().join("codex.json");
    let first = acquire_credential_lock(&path).await.unwrap();
    let (started, started_rx) = std::sync::mpsc::channel();
    let (acquired, acquired_rx) = std::sync::mpsc::channel();
    set_test_hook(
        &path,
        Some(Arc::new(move |event| {
            if event == "waiting" {
                let _ = started.send(());
            }
            if event == "acquired" {
                let _ = acquired.send(());
            }
        })),
    );
    let waiter = tokio::spawn(acquire_with_timeout(
        path.clone(),
        Duration::from_millis(200),
    ));
    tokio::task::yield_now().await;
    // The hook rendezvous proves the blocking acquisition reached contention.
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    drop(first);
    acquired_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    set_test_hook(&path, None);
    let fail = async {
        let _guard = acquire_credential_lock(&path).await?;
        Err::<(), _>(io::Error::other("test operation failed"))
    }
    .await;
    assert!(fail.is_err());
    drop(
        acquire_with_timeout(path, Duration::from_millis(200))
            .await
            .unwrap(),
    );
}

#[tokio::test]
async fn lock_symlink_and_non_regular_file_are_refused() {
    let dir = scratch();
    let path = dir.path().join("codex.json");
    let lock_path = credential_lock_path(&path).unwrap();
    let target = dir.path().join("target");
    fs::write(&target, b"untouched").unwrap();
    #[cfg(windows)]
    let link = std::os::windows::fs::symlink_file(&target, &lock_path);
    #[cfg(unix)]
    let link = std::os::unix::fs::symlink(&target, &lock_path);
    #[cfg(windows)]
    let junction = if let Err(error) = link {
        assert_eq!(error.raw_os_error(), Some(1314));
        // This account cannot make file symlinks. A directory junction needs
        // no elevated privilege and exercises refusing a Windows reparse point.
        use std::os::windows::process::CommandExt;
        let destination = dir.path().join("junction-target");
        fs::create_dir(&destination).unwrap();
        let output = std::process::Command::new("cmd.exe")
            .current_dir(dir.path())
            .args(["/c", "mklink", "/J", "codex.json.lock", "junction-target"])
            .creation_flags(0x0800_0000)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "junction creation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        true
    } else {
        false
    };
    #[cfg(unix)]
    link.unwrap();
    assert!(acquire_credential_lock(&path).await.is_err());
    assert_eq!(fs::read(&target).unwrap(), b"untouched");
    #[cfg(windows)]
    if junction {
        fs::remove_dir(&lock_path).unwrap();
    } else {
        fs::remove_file(&lock_path).unwrap();
    }
    #[cfg(unix)]
    fs::remove_file(&lock_path).unwrap();
    fs::create_dir(&lock_path).unwrap();
    assert!(acquire_credential_lock(&path).await.is_err());
}

#[cfg(windows)]
#[tokio::test]
async fn std_lock_overlaps_upstream_lockfileex_first_byte() {
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        event: *mut std::ffi::c_void,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LockFileEx(
            file: *mut std::ffi::c_void,
            flags: u32,
            reserved: u32,
            low: u32,
            high: u32,
            ov: *mut Overlapped,
        ) -> i32;
        fn UnlockFileEx(
            file: *mut std::ffi::c_void,
            reserved: u32,
            low: u32,
            high: u32,
            ov: *mut Overlapped,
        ) -> i32;
    }
    let dir = scratch();
    let path = dir.path().join("codex.json");
    let lock_path = credential_lock_path(&path).unwrap();
    let upstream = open_lock(&lock_path).unwrap();
    let mut ov = Overlapped {
        internal: 0,
        internal_high: 0,
        offset: 0,
        offset_high: 0,
        event: std::ptr::null_mut(),
    };
    // Same byte-zero, one-byte range as upstream; fail immediately on contention.
    assert_ne!(
        unsafe { LockFileEx(upstream.as_raw_handle(), 3, 0, 1, 0, &mut ov) },
        0
    );
    let error = acquire_with_timeout(path.clone(), Duration::from_millis(100))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert_ne!(
        unsafe { UnlockFileEx(upstream.as_raw_handle(), 0, 1, 0, &mut ov) },
        0
    );
    let own = acquire_credential_lock(&path).await.unwrap();
    assert_eq!(
        unsafe { LockFileEx(upstream.as_raw_handle(), 3, 0, 1, 0, &mut ov) },
        0
    );
    drop(own);
    assert_ne!(
        unsafe { LockFileEx(upstream.as_raw_handle(), 3, 0, 1, 0, &mut ov) },
        0
    );
    assert_ne!(
        unsafe { UnlockFileEx(upstream.as_raw_handle(), 0, 1, 0, &mut ov) },
        0
    );
}

#[test]
#[ignore = "child helper; invoked by lock_child_process_contends"]
fn credential_lock_child() {
    let path = PathBuf::from(std::env::var_os("OFP_SIWC_LOCK_CHILD").expect("child path"));
    let ready = path.with_extension("ready");
    let done = path.with_extension("done");
    let _guard = acquire_sync(&path).unwrap();
    fs::write(ready, b"ready").unwrap();
    let start = Instant::now();
    while !done.exists() {
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[tokio::test]
async fn lock_child_process_contends() {
    let dir = scratch();
    let path = dir.path().join("codex.json");
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "auth::credential_lock::tests::credential_lock_child",
            "--exact",
            "--ignored",
        ])
        .env("OFP_SIWC_LOCK_CHILD", &path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().unwrap();
    let start = Instant::now();
    while !path.with_extension("ready").exists() {
        assert!(start.elapsed() < Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let error = acquire_with_timeout(path.clone(), Duration::from_millis(100))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    fs::write(path.with_extension("done"), b"done").unwrap();
    assert!(child.wait().unwrap().success());
    drop(acquire_credential_lock(path).await.unwrap());
}

#[cfg(unix)]
#[tokio::test]
async fn direct_upstream_flock_blocks_our_lock() {
    use std::os::fd::AsRawFd;
    let dir = scratch();
    let path = dir.path().join("codex.json");
    let upstream = open_lock(&credential_lock_path(&path).unwrap()).unwrap();
    assert_eq!(
        unsafe { libc::flock(upstream.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    assert_eq!(
        acquire_with_timeout(path, Duration::from_millis(100))
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::TimedOut
    );
    assert_eq!(
        unsafe { libc::flock(upstream.as_raw_fd(), libc::LOCK_UN) },
        0
    );
}
