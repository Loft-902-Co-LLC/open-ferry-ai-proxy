//! Writing a config file safely: the writer's own steps around upstream's
//! `os.WriteFile`.
//!
//! [`commit`] checks that the new contents load as a config, keeps the
//! file's current contents as `<file name>.bak`, and replaces the file
//! atomically: it writes a temporary file in the same directory, flushes
//! it to disk and renames it over the file. On Unix the new file and the
//! backup get the file's permissions. A symbolic link at the path is
//! refused, before anything is read and again before anything is written.
//! A file that can't be renamed over, such as a file bind-mounted on its
//! own into a container, is written in place instead, as upstream does.
//! [`commit_expecting`] writes only while the file still holds the bytes
//! the caller read.
//!
//! Every write holds the file's [`WriteLock`], an exclusive OS lock on
//! `<file name>.lock` beside it, from the read it checks (or works its
//! change out from) to the record of what it wrote, so two writes, in this
//! process or in two, never interleave: one waits for the other, a few
//! seconds at most, then reads what the other wrote.
//!
//! After each write, the SHA-256 of what was written is kept beside the
//! file as `<file name>.sha256`, in `sha256sum`'s format, so an undo can
//! tell whether the file was changed by hand since (see [`super::undo`]).
//! Keeping it is best effort: when it can't be written, the old one is
//! removed, and an undo then asks before it goes ahead.
//!
//! Deviations from upstream:
//! - Upstream writes the file in place with no check and no backup, and
//!   follows a symbolic link. The writer writes in place only when the
//!   rename is refused.
//! - Upstream keeps no record of its writes, and takes no file lock.
//! - A new file is created readable by its owner only (0600 on Unix),
//!   where upstream's management `WriteConfig` creates it 0644.
//! - I/O errors are worded `open <path>: <error>` with the platform's
//!   error text, which differs from Go's.

use std::fs;
use std::io::{self, Read as _, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};

use super::super::load::load_bytes;
use super::{SaveError, SaveErrorKind};

/// The most of a record file read.
const RECORD_LIMIT: u64 = 4096;

/// How long a write waits for another to let go of the file's lock.
pub(crate) const LOCK_WAIT: Duration = Duration::from_secs(5);

/// How long a write waiting for the lock sleeps between tries.
const LOCK_RETRY: Duration = Duration::from_millis(10);

/// The file name of the backup of `path`: `<file name>.bak`.
pub(crate) fn backup_path(path: &Path) -> PathBuf {
    with_suffix(path, ".bak")
}

/// The file name of the record of the last write to `path`: `<file
/// name>.sha256`.
pub(crate) fn record_path(path: &Path) -> PathBuf {
    with_suffix(path, ".sha256")
}

/// The file name of the lock file of `path`: `<file name>.lock` (see
/// [`WriteLock`]).
pub(crate) fn lock_path(path: &Path) -> PathBuf {
    with_suffix(path, ".lock")
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// The SHA-256 of `data`, in lowercase hex.
pub(crate) fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The SHA-256 the record beside `path` keeps of the last write, when
/// there is a record that holds one.
pub(crate) fn recorded_sha256(path: &Path) -> Option<String> {
    let file = fs::File::open(record_path(path)).ok()?;
    let mut text = String::new();
    file.take(RECORD_LIMIT).read_to_string(&mut text).ok()?;
    let hash = text.split_whitespace().next()?.to_ascii_lowercase();
    (hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(hash)
}

/// The lock every write of a config file holds, from the read its check
/// (or its change) is made from to the record of what it wrote, so writes
/// in this process and in others never interleave: one waits, then reads
/// what the other wrote. It is an exclusive OS file lock (`flock` on Unix,
/// `LockFileEx` on Windows) on `<file name>.lock` beside the file, not on
/// the file, which a write replaces by a rename. It is let go when dropped,
/// and the OS lets it go when the process ends, so a write that died
/// holds nothing.
///
/// The lock file stays: removing it while another writer had it open would
/// let that writer lock a file the next one doesn't open. It is empty, and
/// on Unix created readable and writable by its owner only, so no other
/// user can hold it. Where the file system can't lock files at all
/// (`Unsupported`), a write goes ahead without the lock, as before there
/// was one.
#[derive(Debug)]
pub(crate) struct WriteLock {
    _file: fs::File,
}

impl WriteLock {
    /// Locks the config file at `path`, waiting up to [`LOCK_WAIT`] for
    /// another write to let go. A symbolic link or a directory at `path`
    /// is refused first, so no lock file is made beside one.
    pub(crate) fn acquire(path: &Path) -> Result<Self, SaveError> {
        Self::acquire_within(path, LOCK_WAIT)
    }

    /// [`acquire`](Self::acquire), waiting up to `wait`.
    fn acquire_within(path: &Path, wait: Duration) -> Result<Self, SaveError> {
        refuse_link(path)?;
        let target = lock_path(path);
        refuse_link(&target)?;
        let file = open_lock(&target).map_err(|error| io_error("open", &target, error))?;
        let deadline = Instant::now() + wait;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(fs::TryLockError::WouldBlock) => {}
                Err(fs::TryLockError::Error(error))
                    if error.kind() == io::ErrorKind::Unsupported =>
                {
                    return Ok(Self { _file: file });
                }
                Err(fs::TryLockError::Error(error)) => {
                    return Err(io_error("lock", &target, error));
                }
            }
            #[cfg(test)]
            note_waiting(path);
            if Instant::now() >= deadline {
                return Err(SaveError::new(
                    SaveErrorKind::Io,
                    format!(
                        "another write of {} still held its lock, {}, after {wait:?}; nothing was written",
                        path.display(),
                        target.display()
                    ),
                ));
            }
            std::thread::sleep(LOCK_RETRY);
        }
    }
}

/// Opens the lock file `target`, creating it empty when it isn't there.
fn open_lock(target: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(target)
}

/// The config files a write waited for the lock of, for the tests.
#[cfg(test)]
static WAITED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn note_waiting(path: &Path) {
    if let Ok(mut waited) = WAITED.lock() {
        waited.push(path.to_owned());
    }
}

/// Whether a write of the config file at `path` waited for its lock since
/// this was last asked.
#[cfg(test)]
pub(crate) fn waited(path: &Path) -> bool {
    let Ok(mut waited) = WAITED.lock() else {
        return false;
    };
    let before = waited.len();
    waited.retain(|seen| seen != path);
    waited.len() != before
}

/// The failure when the file no longer holds what the caller read.
fn stale(path: &Path) -> SaveError {
    SaveError::new(
        SaveErrorKind::Stale,
        format!(
            "{} changed since it was read; nothing was written",
            path.display()
        ),
    )
}

/// Fails when `path` is a symbolic link (or, on Windows, another reparse
/// point) or a directory. A missing file is fine.
pub(crate) fn refuse_link(path: &Path) -> Result<(), SaveError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error("open", path, error)),
    };
    if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
        return Err(SaveError::new(
            SaveErrorKind::Symlink,
            format!(
                "refusing to write {}: it is a symbolic link",
                path.display()
            ),
        ));
    }
    if metadata.is_dir() {
        return Err(SaveError::new(
            SaveErrorKind::Io,
            format!("open {}: is a directory", path.display()),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

pub(crate) fn io_error(op: &str, path: &Path, error: io::Error) -> SaveError {
    SaveError::io(format!("{op} {}: {error}", path.display()), error)
}

/// Reads the config file at `path`, refusing a symbolic link.
pub(crate) fn read(path: &Path) -> Result<Vec<u8>, SaveError> {
    refuse_link(path)?;
    fs::read(path).map_err(|error| io_error("open", path, error))
}

/// Writes `data` to the config file at `path`: checks that it loads as a
/// config, backs up the file's current contents to `<file name>.bak`, and
/// replaces the file atomically, then records the SHA-256 of `data`.
/// Nothing is written if a step fails before the replacement.
pub(crate) fn commit(path: &Path, data: &[u8]) -> Result<(), SaveError> {
    commit_expecting(path, data, None)
}

/// [`commit`], made only when the file holds `expected`, if given: else a
/// [`SaveErrorKind::Stale`] error, and nothing is written. The check is
/// made with the read the backup is taken from, just before the file is
/// replaced, under the file's [`WriteLock`].
pub(crate) fn commit_expecting(
    path: &Path,
    data: &[u8],
    expected: Option<&[u8]>,
) -> Result<(), SaveError> {
    check(data)?;
    let lock = WriteLock::acquire(path)?;
    write_locked(&lock, path, data, expected)
}

/// [`commit_expecting`] under `lock`, the caller's lock of `path`, taken
/// before it read what it worked `data` out from.
pub(crate) fn commit_locked(
    lock: &WriteLock,
    path: &Path,
    data: &[u8],
    expected: Option<&[u8]>,
) -> Result<(), SaveError> {
    check(data)?;
    write_locked(lock, path, data, expected)
}

/// Fails when `data` doesn't load as a config.
fn check(data: &[u8]) -> Result<(), SaveError> {
    load_bytes(data).map_err(|error| SaveError::new(SaveErrorKind::Check, error.to_string()))?;
    Ok(())
}

/// The read, the check of `expected`, the backup, the replacement and the
/// record of a write, which holds `_lock`.
fn write_locked(
    _lock: &WriteLock,
    path: &Path,
    data: &[u8],
    expected: Option<&[u8]>,
) -> Result<(), SaveError> {
    refuse_link(path)?;
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let permissions = match fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(io_error("open", path, error)),
    };
    if permissions.is_some() {
        let current = fs::read(path).map_err(|error| io_error("open", path, error))?;
        if expected.is_some_and(|expected| expected != current.as_slice()) {
            return Err(stale(path));
        }
        let backup = backup_path(path);
        replace(dir, &backup, &current, permissions.as_ref())?;
    } else if expected.is_some() {
        return Err(stale(path));
    }
    replace(dir, path, data, permissions.as_ref())?;
    record(dir, path, data, permissions.as_ref());
    Ok(())
}

/// Keeps the SHA-256 of `data`, just written to `path`, in the record
/// beside it; when that fails, removes the old record, which no longer
/// says what the file holds.
fn record(dir: &Path, path: &Path, data: &[u8], permissions: Option<&fs::Permissions>) {
    let target = record_path(path);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let line = format!("{}  {name}\n", sha256_hex(data));
    if replace(dir, &target, line.as_bytes(), permissions).is_err() {
        let _ = fs::remove_file(&target);
    }
}

/// Writes `data` to a temporary file in `dir`, with `permissions` on Unix,
/// flushes it and renames it to `target`.
fn replace(
    dir: &Path,
    target: &Path,
    data: &[u8],
    permissions: Option<&fs::Permissions>,
) -> Result<(), SaveError> {
    let mut file = tempfile::Builder::new()
        .prefix(".")
        .suffix(".tmp")
        .tempfile_in(dir)
        .map_err(|error| io_error("open", target, error))?;
    set_permissions(file.as_file(), permissions)
        .map_err(|error| io_error("chmod", target, error))?;
    file.write_all(data)
        .and_then(|()| file.as_file().sync_all())
        .map_err(|error| io_error("write", target, error))?;
    match file.persist(target) {
        Ok(_) => {
            sync_dir(dir);
            Ok(())
        }
        // The temporary file is deleted as the error drops.
        Err(error) if rename_refused(&error.error) => write_in_place(target, data),
        Err(error) => Err(io_error("rename", target, error.error)),
    }
}

/// Whether a rename over a file failed because the file can't be renamed
/// over, though it can be written: Linux refuses with EBUSY, or EXDEV,
/// when the file is a mount point of its own, as a container's config.yaml
/// is when it's bind-mounted by itself.
fn rename_refused(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ResourceBusy | io::ErrorKind::CrossesDevices
    )
}

/// Writes `data` over the file at `target`, as upstream's `os.WriteFile`
/// does: the file keeps its identity and permissions, but a failure part of
/// the way through can leave it partly written. The caller has kept a
/// backup.
fn write_in_place(target: &Path, data: &[u8]) -> Result<(), SaveError> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(target)
        .map_err(|error| io_error("open", target, error))?;
    file.write_all(data)
        .and_then(|()| file.sync_all())
        .map_err(|error| io_error("write", target, error))
}

#[cfg(unix)]
fn set_permissions(file: &fs::File, permissions: Option<&fs::Permissions>) -> io::Result<()> {
    match permissions {
        Some(permissions) => file.set_permissions(permissions.clone()),
        None => Ok(()),
    }
}

#[cfg(not(unix))]
fn set_permissions(_file: &fs::File, _permissions: Option<&fs::Permissions>) -> io::Result<()> {
    Ok(())
}

/// Flushes the directory entry of a rename to disk, where the platform
/// allows it; a failure only weakens durability, so it is ignored.
#[cfg(unix)]
fn sync_dir(dir: &Path) {
    if let Ok(dir) = fs::File::open(dir) {
        let _ = dir.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::testing::TempDir;

    // Not upstream's: a write replaces the file and keeps one backup of what
    // it held.
    #[test]
    fn commit_replaces_and_backs_up() {
        let dir = TempDir::new();
        let path = dir.path().join("config.yaml");
        fs::write(&path, "port: 1\n").expect("seed");
        commit(&path, b"port: 2\n").expect("first");
        commit(&path, b"port: 3\n").expect("second");
        assert_eq!(fs::read_to_string(&path).expect("read"), "port: 3\n");
        assert_eq!(
            fs::read_to_string(backup_path(&path)).expect("backup"),
            "port: 2\n"
        );
        let mut names: Vec<String> = fs::read_dir(dir.path())
            .expect("list")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "config.yaml",
                "config.yaml.bak",
                "config.yaml.lock",
                "config.yaml.sha256"
            ]
        );
        // The lock file is left empty; the backup and the record are the
        // config's.
        assert_eq!(fs::read(lock_path(&path)).expect("lock file"), b"");
        assert_eq!(recorded_sha256(&path), Some(sha256_hex(b"port: 3\n")));
    }

    // Not upstream's: a write waits while another holds the file's lock,
    // and then reads the file as that one left it: one that expected the
    // bytes from before is refused as stale, and writes nothing. (Two
    // writers that both passed the check before either wrote would have
    // let the later write over the earlier one.)
    #[test]
    fn a_write_waits_for_the_lock_then_sees_the_newer_file() {
        let dir = TempDir::new();
        let path = dir.path().join("config.yaml");
        commit(&path, b"port: 1\n").expect("seed");
        let held = WriteLock::acquire(&path).expect("lock");
        let writer = {
            let path = path.clone();
            std::thread::spawn(move || commit_expecting(&path, b"port: 3\n", Some(b"port: 1\n")))
        };
        let start = Instant::now();
        while !waited(&path) {
            assert!(!writer.is_finished(), "the writer didn't wait");
            assert!(start.elapsed() < LOCK_WAIT, "the writer never waited");
            std::thread::sleep(Duration::from_millis(1));
        }
        // Another write, made while the first waits.
        commit_locked(&held, &path, b"port: 2\n", Some(b"port: 1\n")).expect("the holder");
        assert_eq!(fs::read_to_string(&path).expect("read"), "port: 2\n");
        drop(held);
        let error = writer
            .join()
            .expect("join")
            .expect_err("the waiting write is stale");
        assert_eq!(error.kind(), SaveErrorKind::Stale);
        assert_eq!(fs::read_to_string(&path).expect("read"), "port: 2\n");
        assert_eq!(
            fs::read_to_string(backup_path(&path)).expect("backup"),
            "port: 1\n"
        );
        assert_eq!(recorded_sha256(&path), Some(sha256_hex(b"port: 2\n")));
    }

    // Not upstream's: the wait for the lock is bounded, and says why it
    // failed; the lock is free again once its holder lets go.
    #[test]
    fn the_wait_for_the_lock_is_bounded() {
        let dir = TempDir::new();
        let path = dir.path().join("config.yaml");
        fs::write(&path, "port: 1\n").expect("seed");
        let held = WriteLock::acquire(&path).expect("lock");
        let start = Instant::now();
        let wait = Duration::from_millis(100);
        let error = WriteLock::acquire_within(&path, wait).expect_err("held");
        assert!(start.elapsed() >= wait);
        assert_eq!(error.kind(), SaveErrorKind::Io);
        let message = error.to_string();
        assert!(message.contains("another write of"), "{message}");
        assert!(message.contains("config.yaml.lock"), "{message}");
        assert!(message.ends_with("nothing was written"), "{message}");
        assert_eq!(fs::read_to_string(&path).expect("read"), "port: 1\n");
        drop(held);
        WriteLock::acquire_within(&path, wait).expect("free");
    }

    // Not upstream's: no lock file is made beside a directory, and a
    // symbolic link as the lock file is refused.
    #[test]
    fn the_lock_refuses_what_a_write_refuses() {
        let dir = TempDir::new();
        let inner = dir.path().join("inner");
        fs::create_dir(&inner).expect("mkdir");
        let error = WriteLock::acquire(&inner).expect_err("a directory");
        assert_eq!(error.kind(), SaveErrorKind::Io);
        assert!(!lock_path(&inner).exists());
        #[cfg(unix)]
        {
            let path = dir.path().join("config.yaml");
            fs::write(&path, "port: 1\n").expect("seed");
            let other = dir.path().join("other");
            std::os::unix::fs::symlink(&other, lock_path(&path)).expect("link");
            let error = commit(&path, b"port: 2\n").expect_err("a link");
            assert_eq!(error.kind(), SaveErrorKind::Symlink);
            assert!(!other.exists());
            assert_eq!(fs::read_to_string(&path).expect("read"), "port: 1\n");
        }
    }

    // Not upstream's: each write records the SHA-256 of what it wrote, in
    // sha256sum's format; a record that holds no hash reads as none.
    #[test]
    fn commit_records_what_it_wrote() {
        let dir = TempDir::new();
        let path = dir.path().join("config.yaml");
        assert_eq!(recorded_sha256(&path), None);
        commit(&path, b"port: 2\n").expect("write");
        let hash = sha256_hex(b"port: 2\n");
        assert_eq!(recorded_sha256(&path).as_deref(), Some(hash.as_str()));
        assert_eq!(
            fs::read_to_string(record_path(&path)).expect("record"),
            format!("{hash}  config.yaml\n")
        );
        // A hand edit leaves the record as it was.
        fs::write(&path, "port: 3\n").expect("edit");
        assert_eq!(recorded_sha256(&path).as_deref(), Some(hash.as_str()));
        fs::write(record_path(&path), "not a hash\n").expect("garble");
        assert_eq!(recorded_sha256(&path), None);
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    // Not upstream's: a write that expects the bytes it read is refused,
    // and writes nothing, when the file holds others or is gone.
    #[test]
    fn commit_expecting_refuses_a_changed_file() {
        let dir = TempDir::new();
        let path = dir.path().join("config.yaml");
        fs::write(&path, "port: 1\n").expect("seed");
        commit_expecting(&path, b"port: 2\n", Some(b"port: 1\n")).expect("matches");
        assert_eq!(fs::read_to_string(&path).expect("read"), "port: 2\n");
        let error = commit_expecting(&path, b"port: 3\n", Some(b"port: 1\n")).expect_err("stale");
        assert_eq!(error.kind(), SaveErrorKind::Stale);
        assert_eq!(fs::read_to_string(&path).expect("read"), "port: 2\n");
        assert_eq!(
            fs::read_to_string(backup_path(&path)).expect("backup"),
            "port: 1\n"
        );
        let missing = dir.path().join("missing.yaml");
        let error = commit_expecting(&missing, b"port: 3\n", Some(b"")).expect_err("gone");
        assert_eq!(error.kind(), SaveErrorKind::Stale);
        assert!(!missing.exists());
    }

    // Not upstream's: contents that don't load are refused and nothing is
    // written.
    #[test]
    fn commit_refuses_what_does_not_load() {
        let dir = TempDir::new();
        let path = dir.path().join("config.yaml");
        fs::write(&path, "port: 1\n").expect("seed");
        let error = commit(&path, b"port: [\n").expect_err("refused");
        assert_eq!(error.kind(), SaveErrorKind::Check);
        assert!(
            error
                .to_string()
                .starts_with("failed to parse config file: yaml: "),
            "{error}"
        );
        assert_eq!(fs::read_to_string(&path).expect("read"), "port: 1\n");
        assert!(!backup_path(&path).exists());
    }

    // Not upstream's: a new file is created without a backup.
    #[test]
    fn commit_creates_a_new_file() {
        let dir = TempDir::new();
        let path = dir.path().join("config.yaml");
        commit(&path, b"port: 2\n").expect("write");
        assert_eq!(fs::read_to_string(&path).expect("read"), "port: 2\n");
        assert!(!backup_path(&path).exists());
    }

    // Not upstream's: the backup and the new file keep the file's mode.
    #[cfg(unix)]
    #[test]
    fn commit_keeps_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let path = dir.path().join("config.yaml");
        fs::write(&path, "port: 1\n").expect("seed");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("chmod");
        commit(&path, b"port: 2\n").expect("write");
        let mode = |p: &Path| fs::metadata(p).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o640);
        assert_eq!(mode(&backup_path(&path)), 0o640);
    }

    // Not upstream's: a symbolic link at the path is refused.
    #[cfg(unix)]
    #[test]
    fn commit_refuses_a_symlink() {
        let dir = TempDir::new();
        let target = dir.path().join("real.yaml");
        let path = dir.path().join("config.yaml");
        fs::write(&target, "port: 1\n").expect("seed");
        std::os::unix::fs::symlink(&target, &path).expect("link");
        let error = commit(&path, b"port: 2\n").expect_err("refused");
        assert_eq!(error.kind(), SaveErrorKind::Symlink);
        assert_eq!(fs::read_to_string(&target).expect("read"), "port: 1\n");
    }

    // Not upstream's: a directory at the path is refused.
    #[test]
    fn commit_refuses_a_directory() {
        let dir = TempDir::new();
        let error = commit(dir.path(), b"port: 2\n").expect_err("refused");
        assert_eq!(error.kind(), SaveErrorKind::Io);
    }

    // Not upstream's: only a rename the file refuses falls back to a write
    // in place; other failures are reported.
    #[test]
    fn only_a_refused_rename_writes_in_place() {
        for kind in [io::ErrorKind::ResourceBusy, io::ErrorKind::CrossesDevices] {
            assert!(rename_refused(&io::Error::from(kind)), "{kind:?}");
        }
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::NotFound,
            io::ErrorKind::ReadOnlyFilesystem,
            io::ErrorKind::Other,
        ] {
            assert!(!rename_refused(&io::Error::from(kind)), "{kind:?}");
        }
    }

    // Upstream's os.WriteFile: the write replaces the contents, shorter
    // ones included, and leaves no other file.
    #[test]
    fn write_in_place_replaces_the_contents() {
        let dir = TempDir::new();
        let path = dir.path().join("config.yaml");
        fs::write(&path, "port: 1\nhost: example\n").expect("seed");
        write_in_place(&path, b"port: 2\n").expect("write");
        assert_eq!(fs::read_to_string(&path).expect("read"), "port: 2\n");
        assert_eq!(fs::read_dir(dir.path()).expect("list").count(), 1);
    }

    // Not upstream's: a write in place keeps the file, and so its mode, as a
    // bind mount needs; it creates no file.
    #[cfg(unix)]
    #[test]
    fn write_in_place_keeps_the_file() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = TempDir::new();
        let path = dir.path().join("config.yaml");
        fs::write(&path, "port: 1\n").expect("seed");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("chmod");
        let before = fs::metadata(&path).expect("stat");
        write_in_place(&path, b"port: 2\n").expect("write");
        let after = fs::metadata(&path).expect("stat");
        assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
        assert_eq!(after.permissions().mode() & 0o777, 0o640);
        let missing = dir.path().join("missing.yaml");
        let error = write_in_place(&missing, b"port: 2\n").expect_err("no file");
        assert_eq!(error.kind(), SaveErrorKind::Io);
        assert!(!missing.exists());
    }
}
