//! Not upstream's: two in-process writers stand for separate workers.

use super::*;
use crate::file_log::cleaner::Cleaner;
use crate::file_log::control::Cleaning;

fn writer(path: &Path, rotation: bool) -> Rotating {
    let mut writer = Rotating::new(path.to_path_buf(), 64);
    writer.rotation = Arc::new(Mutex::new(rotation));
    writer
}

fn backups(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.file_name().unwrap() != "main.log")
        .collect()
}

fn wait_until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(Instant::now() < deadline, "cleaner did not run");
        thread::sleep(Duration::from_millis(10));
    }
}

// Not upstream's: standby's very first write must not rotate an oversized log;
// nor may an oversized line be rejected while rotation is disabled.
#[test]
fn rotation_off_from_first_open_and_back_on() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.log");
    fs::write(&path, [b'x'; 70]).unwrap();
    let mut a = writer(&path, false);
    a.write(&[b'a'; 80]).unwrap();
    a.flush().unwrap();
    assert!(backups(dir.path()).is_empty());
    assert_eq!(fs::metadata(&path).unwrap().len(), 150);
    *a.rotation.lock().unwrap() = true;
    a.write(b"rotation-on\n").unwrap();
    a.flush().unwrap();
    assert_eq!(backups(dir.path()).len(), 1);
    assert_eq!(fs::read(&path).unwrap(), b"rotation-on\n");
}

// Not upstream's: under retention pressure A's drain stays in the renamed
// file until A exits; peers_gone then wakes B's cleaner with its saved config.
#[test]
fn cleaning_waits_for_the_draining_writer_to_exit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.log");
    // Pre-existing pressure also reaches Windows directory-entry metadata,
    // which may lag writes through a handle that has never been closed.
    fs::write(&path, vec![b'a'; 1024 * 1024 + 64]).unwrap();
    let mut a = writer(&path, false);
    a.write(b"A-before\n").unwrap();
    a.flush().unwrap();
    let cleaning = Arc::new(Cleaning::default());
    cleaning.set(false);
    let mut b = writer(&path, true);
    b.write(b"B-current\n").unwrap();
    b.flush().unwrap();
    let rotated = backups(dir.path()).pop().unwrap();
    a.write(b"A-draining\n").unwrap();
    a.flush().unwrap();
    let snapshot = cleaning.snapshot();
    let cleaner =
        Cleaner::start_with(dir.path(), 1, Some(path.clone()), Arc::clone(&cleaning)).unwrap();
    // Wait for an actual control observation, not a scheduling guess.
    cleaning.wait_observed(snapshot);
    let old = fs::read(&rotated).unwrap();
    assert_eq!(
        old.len(),
        1024 * 1024 + 64 + b"A-before\n".len() + b"A-draining\n".len()
    );
    assert!(old.ends_with(b"A-draining\n"));
    assert_eq!(fs::read(&path).unwrap(), b"B-current\n");
    drop(a);
    cleaning.set(true);
    wait_until(|| !rotated.exists());
    assert_eq!(fs::read(&path).unwrap(), b"B-current\n");
    assert!(fs::metadata(&path).unwrap().len() <= 1024 * 1024);
    drop(cleaner);
}

// Not upstream's: resuming A reopens the current name, not its renamed inode.
#[test]
fn resume_reopens_by_name_after_a_peer_rotated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.log");
    let mut a = writer(&path, false);
    a.write(&[b'a'; 64]).unwrap();
    a.flush().unwrap();
    let mut b = writer(&path, true);
    b.write(b"B\n").unwrap();
    b.flush().unwrap();
    let rotated = backups(dir.path()).pop().unwrap();
    a.reopen().unwrap();
    a.write(b"A-resumed\n").unwrap();
    a.flush().unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"B\nA-resumed\n");
    assert_eq!(fs::read(&rotated).unwrap(), [b'a'; 64]);
}

// Not upstream's: deliberately enabling B's cleaner too soon demonstrates
// the loss this switch prevents, including delete sharing on Windows.
#[test]
fn cleaning_during_drain_deletes_the_file_a_still_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.log");
    // Pre-existing pressure also reaches Windows directory-entry metadata,
    // which may lag writes through a handle that has never been closed.
    fs::write(&path, vec![b'a'; 1024 * 1024 + 64]).unwrap();
    let mut a = writer(&path, false);
    a.write(b"A-before\n").unwrap();
    a.flush().unwrap();
    let mut b = writer(&path, true);
    b.write(b"B\n").unwrap();
    b.flush().unwrap();
    let rotated = backups(dir.path()).pop().unwrap();
    let cleaner = Cleaner::start_with(
        dir.path(),
        1,
        Some(path.clone()),
        Arc::new(Cleaning::default()),
    )
    .unwrap();
    // Windows leaves a delete-pending name until A closes its handle,
    // but new readers can no longer open it; Unix unlinks it immediately.
    wait_until(|| fs::read(&rotated).is_err());
    a.write(b"A-lost\n").unwrap();
    a.flush().unwrap();
    assert!(fs::read(&rotated).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"B\n");
    drop(a);
    wait_until(|| !rotated.exists());
    assert!(backups(dir.path()).is_empty());
    drop(cleaner);
}

// Not upstream's: B takes over with a stale standby handle, reopens before
// ready, and go enables size checks against the current name (L9.18).
#[test]
fn takeover_reopens_and_go_resumes_at_the_current_size() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.log");
    fs::write(&path, vec![b'x'; 2 * 1024 * 1024]).unwrap();
    let controls = crate::file_log::LogControls::new();
    controls.set_rotation(false);
    let mut b = Rotating::with_controls(path.clone(), 3 * 1024 * 1024, controls.clone());
    b.write(b"B-standby\n").unwrap();
    b.flush().unwrap();
    let mut a = writer(&path, true);
    a.write(b"A\n").unwrap();
    a.flush().unwrap();
    drop(a);
    b.reopen().unwrap();
    assert_eq!(b.size, 2, "reopen must refresh the cached size");
    controls.set_rotation(true);
    b.write(&vec![b'b'; 2 * 1024 * 1024]).unwrap();
    b.flush().unwrap();
    assert!(fs::read(&path).unwrap().starts_with(b"A\n"));
    assert_eq!(backups(dir.path()).len(), 1);
    // Backup names stop at the millisecond, so a rotation in A's millisecond
    // would replace A's backup, as lumberjack's rename would.
    while backup_path(&path).exists() {
        thread::sleep(Duration::from_millis(1));
    }
    b.write(&vec![b'c'; 2 * 1024 * 1024]).unwrap();
    b.flush().unwrap();
    assert_eq!(backups(dir.path()).len(), 2);
    assert_eq!(fs::metadata(&path).unwrap().len(), 2 * 1024 * 1024);
}

// Not upstream's: external writes while rotation is off cannot leave its
// cached size stale after an off/on cycle with no intervening local write.
#[test]
fn enabling_rotation_rechecks_the_handle_size() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.log");
    let controls = crate::file_log::LogControls::new();
    let mut b = Rotating::with_controls(path.clone(), 64, controls.clone());
    b.write(b"B\n").unwrap();
    b.flush().unwrap();
    controls.set_rotation(false);
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&[b'a'; 64])
        .unwrap();
    controls.set_rotation(true);
    b.write(b"go\n").unwrap();
    b.flush().unwrap();
    assert_eq!(backups(dir.path()).len(), 1);
    assert_eq!(fs::read(path).unwrap(), b"go\n");
}
