//! Putting a file or directory in place without replacing what is there,
//! and making the directories this crate creates durable.
//!
//! One module for every no-replace publication (the kept copy, a no-replace
//! `atomic_write`, a staged version directory) so the platform rules and
//! fallbacks live in one place.
//!
//! The order a no-replace publication tries, per platform:
//!
//! - **Windows:** `MoveFileExW` with `MOVEFILE_WRITE_THROUGH` and without
//!   `MOVEFILE_REPLACE_EXISTING`: atomic, never replaces, and works on NTFS,
//!   FAT, exFAT and SMB. An access-denied, sharing or lock error (an
//!   antivirus scan of the file) is retried after 50, 100, 200, 400 and
//!   800 ms.
//! - **Linux:** `renameat2(RENAME_NOREPLACE)` through the raw syscall (the
//!   musl builds can't rely on the libc wrapper). On EINVAL, ENOSYS or
//!   EOPNOTSUPP, a file falls back to a hard link and then removes the
//!   source; if the link is unsupported too (EPERM, EOPNOTSUPP, ENOSYS: vfat
//!   has no hard links), to an absence check and a plain rename, which is
//!   safe only under the update lock.
//! - **macOS:** `renamex_np(RENAME_EXCL)`, then as Linux.
//! - **Directories** that the kernel can't rename without replacing: claim
//!   the destination with `create_dir`, sync its parent, `rename` onto the
//!   claim (which replaces only an empty directory), sync the parent again.
//!   A crash can leave an empty claim directory; see [`publish_directory`].
//!
//! The fallback logic is written against a small [`Fs`] trait so tests run
//! every branch on every platform.

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How long to wait before each retry of a transient Windows error.
const RETRY_WAITS_MS: [u64; 5] = [50, 100, 200, 400, 800];

/// The file system operations the publication logic uses, so tests can make
/// the first (native) attempt report "unsupported" and watch the order of
/// the rest.
trait Fs {
    /// Moves `from` to `to`, never replacing `to`; `AlreadyExists` if it
    /// exists. The platform's native primitive.
    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Moves `from` to `to`, replacing a file at `to`, written through on
    /// Windows.
    fn rename_replace(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// A plain rename (replaces a file, or an empty directory, at `to`).
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn create_dir(&self, path: &Path) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    fn remove_dir(&self, path: &Path) -> io::Result<()>;
    /// Whether `path` exists (a dangling link does). Only `NotFound` means
    /// absent; any other failure to look is an error, never "absent", so a
    /// caller can't go on to replace or delete on a failed read.
    fn exists(&self, path: &Path) -> io::Result<bool>;
    fn is_dir(&self, path: &Path) -> bool;
    fn sync_directory(&self, path: &Path) -> io::Result<()>;
    /// Called when a file is published by the last resort, the check and
    /// rename, because the file system has no hard links.
    fn note_no_hard_links(&self, to: &Path);
    fn sleep(&self, wait: Duration);
}

struct RealFs;

/// Set once the process has warned that a file system has no hard links.
static NOTED_NO_HARD_LINKS: AtomicBool = AtomicBool::new(false);

/// Whether this is the first call with `flag` (and marks it).
fn first_time(flag: &AtomicBool) -> bool {
    !flag.swap(true, Ordering::Relaxed)
}

impl Fs for RealFs {
    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
        native_rename_noreplace(from, to)
    }

    fn rename_replace(&self, from: &Path, to: &Path) -> io::Result<()> {
        native_rename_replace(from, to)
    }

    fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::hard_link(from, to)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o755);
        }
        builder.create(path)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_dir(path)
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        match std::fs::symlink_metadata(path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn is_dir(&self, path: &Path) -> bool {
        std::fs::metadata(path).is_ok_and(|metadata| metadata.is_dir())
    }

    fn sync_directory(&self, path: &Path) -> io::Result<()> {
        sync_directory(path)
    }

    fn note_no_hard_links(&self, to: &Path) {
        if first_time(&NOTED_NO_HARD_LINKS) {
            tracing::warn!(
                path = %to.display(),
                "this file system has no hard links; publishing files by checking the \
                 name is free and renaming, which is safe only while the update lock is held"
            );
        }
    }

    fn sleep(&self, wait: Duration) {
        std::thread::sleep(wait);
    }
}

/// Whether `error` carries one of the OS error numbers in `codes`.
fn raw_in(error: &io::Error, codes: &[i32]) -> bool {
    error
        .raw_os_error()
        .is_some_and(|code| codes.contains(&code))
}

/// The native no-replace rename isn't available here: EINVAL (the file
/// system doesn't take the flag), ENOSYS or EOPNOTSUPP/ENOTSUP. On Linux also
/// EPERM, which older Docker and seccomp profiles answer for a syscall they
/// don't know. That is safe: every later step is still no-replace (the hard
/// link, then the check and rename under the lock), and a real permission
/// problem fails at those steps as well.
fn native_unsupported(error: &io::Error) -> bool {
    #[cfg(target_os = "macos")]
    const CODES: &[i32] = &[libc::EINVAL, libc::ENOSYS, libc::EOPNOTSUPP, libc::ENOTSUP];
    #[cfg(target_os = "linux")]
    const CODES: &[i32] = &[libc::EINVAL, libc::ENOSYS, libc::EOPNOTSUPP, libc::EPERM];
    #[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
    const CODES: &[i32] = &[libc::EINVAL, libc::ENOSYS, libc::EOPNOTSUPP];
    #[cfg(not(unix))]
    const CODES: &[i32] = &[];
    error.kind() == io::ErrorKind::Unsupported || raw_in(error, CODES)
}

/// The file system can't make hard links: EPERM, EOPNOTSUPP or ENOSYS (vfat
/// reports EPERM); on macOS also ENOTSUP, a different value from EOPNOTSUPP
/// there that Rust's std doesn't map to `Unsupported`, as in
/// [`native_unsupported`]; on Windows ERROR_INVALID_FUNCTION or
/// ERROR_NOT_SUPPORTED.
pub(crate) fn link_unsupported(error: &io::Error) -> bool {
    // The macOS arm isn't compiled or tested here; it mirrors the macOS arm
    // of `native_unsupported`.
    #[cfg(target_os = "macos")]
    const CODES: &[i32] = &[libc::EPERM, libc::EOPNOTSUPP, libc::ENOTSUP, libc::ENOSYS];
    #[cfg(all(unix, not(target_os = "macos")))]
    const CODES: &[i32] = &[libc::EPERM, libc::EOPNOTSUPP, libc::ENOSYS];
    #[cfg(windows)]
    const CODES: &[i32] = &[1, 50];
    #[cfg(not(any(unix, windows)))]
    const CODES: &[i32] = &[];
    error.kind() == io::ErrorKind::Unsupported || raw_in(error, CODES)
}

/// A Windows error worth retrying: `ERROR_ACCESS_DENIED`,
/// `ERROR_SHARING_VIOLATION` or `ERROR_LOCK_VIOLATION`.
fn windows_transient(error: &io::Error) -> bool {
    raw_in(error, &[5, 32, 33])
}

/// What this platform retries: the Windows errors above, nothing on Unix,
/// where EACCES is a real refusal.
fn platform_transient(error: &io::Error) -> bool {
    cfg!(windows) && windows_transient(error)
}

/// Runs `op`, again after each of [`RETRY_WAITS_MS`] while `transient` says
/// its error is worth another try.
fn retry_with<T>(
    fs: &dyn Fs,
    transient: fn(&io::Error) -> bool,
    mut op: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    let mut retries = 0;
    loop {
        match op() {
            Err(error) if transient(&error) => match RETRY_WAITS_MS.get(retries) {
                Some(wait) => {
                    fs.sleep(Duration::from_millis(*wait));
                    retries += 1;
                }
                None => return Err(error),
            },
            result => return result,
        }
    }
}

#[cfg(unix)]
fn path_cstring(path: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    Ok(std::ffi::CString::new(path.as_os_str().as_bytes())?)
}

#[cfg(target_os = "linux")]
fn native_rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let from = path_cstring(from)?;
    let to = path_cstring(to)?;
    // SAFETY: both C strings stay valid for the call and the syscall keeps no
    // pointer. The raw syscall is used, not the libc wrapper, whose presence
    // in the musl builds is unverified.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn native_rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let from = path_cstring(from)?;
    let to = path_cstring(to)?;
    // SAFETY: both C strings stay valid for the call; RENAME_EXCL forbids
    // replacing the destination.
    let result = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

#[cfg(windows)]
fn move_file(from: &Path, to: &Path, flags: u32) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
    let (from, to) = (wide(from), wide(to));
    // SAFETY: both are NUL-terminated UTF-16 strings that outlive the call.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), flags) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn native_rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH;
    move_file(from, to, MOVEFILE_WRITE_THROUGH)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn native_rename_noreplace(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no-replace rename is unavailable",
    ))
}

#[cfg(windows)]
fn native_rename_replace(from: &Path, to: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    move_file(from, to, MOVEFILE_WRITE_THROUGH | MOVEFILE_REPLACE_EXISTING)
}

#[cfg(not(windows))]
fn native_rename_replace(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(from, to)
}

fn publish_file_with(
    fs: &dyn Fs,
    from: &Path,
    to: &Path,
    transient: fn(&io::Error) -> bool,
) -> io::Result<()> {
    match retry_with(fs, transient, || fs.rename_noreplace(from, to)) {
        Ok(()) => return Ok(()),
        Err(error) if native_unsupported(&error) => {}
        Err(error) => return Err(error),
    }
    match fs.hard_link(from, to) {
        Ok(()) => {
            // The link is the published file; only the source name goes.
            let _ = fs.remove_file(from);
            return Ok(());
        }
        Err(error) if link_unsupported(&error) => {}
        Err(error) => return Err(error),
    }
    fs.note_no_hard_links(to);
    // A failed look is not "absent": the rename below replaces.
    if fs.exists(to)? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists", to.display()),
        ));
    }
    fs.rename(from, to)
}

fn publish_directory_with(
    fs: &dyn Fs,
    from: &Path,
    to: &Path,
    transient: fn(&io::Error) -> bool,
) -> io::Result<()> {
    match retry_with(fs, transient, || fs.rename_noreplace(from, to)) {
        Ok(()) => return Ok(()),
        Err(error) if native_unsupported(&error) => {}
        Err(error) => return Err(error),
    }
    // Claim the name (fails if it exists), make the claim durable, and
    // rename onto it, which replaces only an empty directory: the claim.
    fs.create_dir(to)?;
    sync_parent_with(fs, to)?;
    if let Err(error) = fs.rename(from, to) {
        // A claim that someone filled (or a name that appeared) is theirs:
        // fail closed and leave it. Any other failure leaves our empty claim,
        // which is ours to remove (and `remove_dir` takes only an empty one).
        if !matches!(
            error.kind(),
            io::ErrorKind::AlreadyExists | io::ErrorKind::DirectoryNotEmpty
        ) {
            let _ = fs.remove_dir(to);
        }
        return Err(error);
    }
    sync_parent_with(fs, to)
}

fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}

fn sync_parent_with(fs: &dyn Fs, path: &Path) -> io::Result<()> {
    fs.sync_directory(parent_dir(path))
}

fn create_dir_all_durable_with(fs: &dyn Fs, path: &Path, root: &Path) -> io::Result<()> {
    if !path.starts_with(root) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not inside {}", path.display(), root.display()),
        ));
    }
    // `path` up to and including `root`, deepest first: every one of these is
    // synced whether or not this call made it, because an earlier call may
    // have made it and failed before its sync.
    let mut within = Vec::new();
    let mut level = path;
    loop {
        within.push(level);
        if level == root {
            break;
        }
        match level.parent() {
            Some(parent) => level = parent,
            None => break,
        }
    }
    // The levels that don't exist, deepest first.
    let mut missing = Vec::new();
    let mut current = path;
    while !current.as_os_str().is_empty() && !fs.is_dir(current) {
        missing.push(current);
        match current.parent() {
            Some(parent) => current = parent,
            None => break,
        }
    }
    // Create from the top down, recording the levels this call made. A level
    // another process made first is theirs, not ours to sync.
    let mut created = Vec::new();
    for level in missing.iter().rev() {
        match fs.create_dir(level) {
            Ok(()) => created.push(*level),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && fs.is_dir(level) => {}
            Err(error) => return Err(error),
        }
    }
    // Deepest first: the levels from `path` up to `root` (new or already
    // there). A level another process made between two of ours still has our
    // child in it.
    let mut to_sync: Vec<&Path> = within.clone();
    // Only when this call made `root` is its entry in its parent not yet
    // synced; that one directory is the boundary. Ancestors above `root` that
    // this call had to make are not synced (a limit for callers whose root's
    // parent chain doesn't exist).
    if created.contains(&root) {
        let parent = parent_dir(root);
        if !to_sync.contains(&parent) {
            to_sync.push(parent);
        }
    }
    for dir in to_sync {
        // The lexical parent of a bare relative path is empty. It denotes
        // the accepted current-directory root, not an empty OS pathname.
        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        fs.sync_directory(dir)?;
    }
    Ok(())
}

/// Publishes the file `from` as `to` without replacing: `to` is whole or
/// absent, and `ErrorKind::AlreadyExists` says it was there (nothing was
/// changed; `from` is left). On success `from` no longer exists. The caller
/// syncs the directory after ([`sync_parent`]); on Windows the move is
/// written through already.
///
/// Platform rules are in the [module docs](self). The caller must hold the
/// update lock: the last-resort path on a file system with neither a
/// no-replace rename nor hard links checks that `to` is absent and then
/// renames, which another writer could slip between. A warning names that
/// limit, once per process.
pub(crate) fn publish_file(from: &Path, to: &Path) -> io::Result<()> {
    publish_file_with(&RealFs, from, to, platform_transient)
}

/// Not upstream's: `publish_file` for the supervisor, which places a missing
/// installed file with it (re-exported by `switch`). Same contract: `to` is
/// whole or absent, `AlreadyExists` says it was there, the move is written
/// through on Windows, the caller syncs the directory after on Unix
/// (`sync_parent`), and the update lock is held.
pub fn publish_file_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    publish_file(from, to)
}

/// Publishes the directory `from` as `to` without replacing, even an empty
/// `to` (`AlreadyExists`; `from` is left). The caller syncs the directory
/// holding `to` after on the native path ([`sync_parent`]); the fallback
/// syncs it itself.
///
/// Where the kernel has no no-replace rename the fallback claims `to` as an
/// empty directory, and a crash can leave that empty claim. It is not a
/// version: the next staging finds the directory, its hash check fails, and
/// it is removed if the version isn't in the keep set. The caller must hold
/// the update lock.
pub(crate) fn publish_directory(from: &Path, to: &Path) -> io::Result<()> {
    publish_directory_with(&RealFs, from, to, platform_transient)
}

/// Moves the file `from` onto `to`, replacing any file there: written
/// through on Windows (`MoveFileExW` with `MOVEFILE_WRITE_THROUGH |
/// MOVEFILE_REPLACE_EXISTING`, retrying access-denied, sharing and lock
/// errors), a `rename` on Unix. The caller syncs the directory after
/// ([`sync_parent`]).
pub(crate) fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    replace_file_with(&RealFs, from, to, platform_transient)
}

fn replace_file_with(
    fs: &dyn Fs,
    from: &Path,
    to: &Path,
    transient: fn(&io::Error) -> bool,
) -> io::Result<()> {
    retry_with(fs, transient, || fs.rename_replace(from, to))
}

/// Creates `path` (which must be `root` or inside it) and any missing parents
/// one level at a time, and before returning syncs every directory from
/// `path` up to `root`, deepest first, including ones that already existed:
/// an earlier call may have made them and failed before its sync, and a retry
/// must not skip that. Nothing above `root` is synced unless this call newly
/// made `root`, and then only `root`'s parent, once (ancestors above that this
/// call had to make are not synced). Directory syncs do nothing on
/// Windows (NTFS journals them and the write-through moves flush the log).
///
/// A first call that made `root` and failed before syncing its parent leaves
/// nothing to tell the retry; that parent entry is not re-synced.
pub(crate) fn create_dir_all_durable(path: &Path, root: &Path) -> io::Result<()> {
    create_dir_all_durable_with(&RealFs, path, root)
}

/// The result of an fsync of a directory (never of a file). A filesystem that
/// can't sync directories answers EINVAL (Rust's `InvalidInput`): there is
/// nothing more to do for it, as PostgreSQL's `fsync_fname` takes it for
/// directories, so it is done. Every other error, EACCES and EIO included, is
/// a failure. This is the one place the rule lives; install-self uses it too.
#[cfg(unix)]
pub(crate) fn dir_sync_outcome(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => Ok(()),
        other => other,
    }
}

/// Syncs the directory `path` so a rename or link in it survives a crash.
/// Does nothing on Windows. Failing to open it is an error; an fsync that a
/// filesystem refuses for directories (EINVAL) is done, and any other fsync
/// error is a failure.
#[cfg(unix)]
pub fn sync_directory(path: &Path) -> io::Result<()> {
    dir_sync_outcome(std::fs::File::open(path)?.sync_all())
}

/// Syncs the directory `path` so a rename or link in it survives a crash.
/// Does nothing on Windows.
#[cfg(not(unix))]
pub fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Syncs the directory holding `path` (`.` for a bare name).
pub(crate) fn sync_parent(path: &Path) -> io::Result<()> {
    sync_directory(parent_dir(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn publication_directories_are_not_writable_by_others_with_umask_zero() {
        use crate::journal::creation_tests::{assert_mode, with_umask_zero};
        with_umask_zero(
            "publication::tests::publication_directories_are_not_writable_by_others_with_umask_zero",
            |root| {
                let direct = root.join("direct");
                RealFs.create_dir(&direct).unwrap();
                assert_mode(&direct, 0o755);
                let chain = root.join("data/a/b");
                create_dir_all_durable(&chain, &root.join("data")).unwrap();
                for path in [root.join("data"), root.join("data/a"), chain] {
                    assert_mode(&path, 0o755);
                }
            },
        );
    }

    #[cfg(unix)]
    #[test]
    fn publication_directories_are_not_writable_by_others_with_default_acl() {
        use crate::journal::creation_tests::{assert_acl_mode, with_default_acl};
        with_default_acl(
            "publication::tests::publication_directories_are_not_writable_by_others_with_default_acl",
            |root| {
                let direct = root.join("direct");
                RealFs.create_dir(&direct).unwrap();
                assert_acl_mode(&direct, 0o755);
                let chain = root.join("data/a/b");
                create_dir_all_durable(&chain, &root.join("data")).unwrap();
                for path in [root.join("data"), root.join("data/a"), chain] {
                    assert_acl_mode(&path, 0o755);
                }
            },
        );
    }
    use std::cell::{Cell, RefCell};
    use std::fs;
    use std::path::PathBuf;

    /// What a [`Probe`]'s native no-replace attempt does.
    #[derive(Clone, Copy)]
    enum Native {
        /// The real primitive.
        Real,
        /// Reports `Unsupported`, the way EINVAL or ENOSYS would.
        Unsupported,
        /// Fails with this raw OS error, the way a seccomp profile answers
        /// a syscall it doesn't know.
        #[cfg(target_os = "linux")]
        Raw(i32),
    }

    /// A file system that records its calls by file name, can report the
    /// native attempt, or the hard link, as unsupported, and does the rest
    /// for real.
    struct Probe {
        log: RefCell<Vec<String>>,
        native: Native,
        links_unsupported: bool,
        rename_fails: Option<io::ErrorKind>,
        /// The existence check fails with this kind.
        exists_fails: Option<io::ErrorKind>,
        notes: Cell<u32>,
        /// When the first path is created, the second appears too, as if
        /// another process made it in between.
        race: Option<(PathBuf, PathBuf)>,
        /// The next sync of this directory fails (once).
        sync_fails_once: RefCell<Option<PathBuf>>,
        /// Relative-path tests record directory operations without touching cwd.
        record_only_directories: bool,
    }

    impl Probe {
        fn new(native: Native) -> Self {
            Probe {
                log: RefCell::new(Vec::new()),
                native,
                links_unsupported: false,
                rename_fails: None,
                exists_fails: None,
                notes: Cell::new(0),
                race: None,
                sync_fails_once: RefCell::new(None),
                record_only_directories: false,
            }
        }

        fn record(&self, what: &str, path: &Path) {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.log.borrow_mut().push(format!("{what}:{name}"));
        }

        fn log(&self) -> Vec<String> {
            self.log.borrow().clone()
        }
    }

    impl Fs for Probe {
        fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.record("noreplace", to);
            match self.native {
                Native::Real => RealFs.rename_noreplace(from, to),
                Native::Unsupported => Err(io::Error::from(io::ErrorKind::Unsupported)),
                #[cfg(target_os = "linux")]
                Native::Raw(code) => Err(io::Error::from_raw_os_error(code)),
            }
        }

        fn rename_replace(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.record("replace", to);
            RealFs.rename_replace(from, to)
        }

        fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.record("link", to);
            if self.links_unsupported {
                Err(io::Error::from(io::ErrorKind::Unsupported))
            } else {
                RealFs.hard_link(from, to)
            }
        }

        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.record("rename", to);
            if let Some(kind) = self.rename_fails {
                return Err(io::Error::from(kind));
            }
            // POSIX rename replaces an empty directory; Windows's std rename
            // doesn't, so the probe does that step itself to stand in for it.
            if RealFs.is_dir(from) && RealFs.is_dir(to) {
                fs::remove_dir(to)?;
            }
            RealFs.rename(from, to)
        }

        fn create_dir(&self, path: &Path) -> io::Result<()> {
            self.record("create_dir", path);
            RealFs.create_dir(path)?;
            if let Some((first, second)) = &self.race
                && first == path
            {
                RealFs.create_dir(second)?;
            }
            Ok(())
        }

        fn remove_file(&self, path: &Path) -> io::Result<()> {
            self.record("remove_file", path);
            RealFs.remove_file(path)
        }

        fn remove_dir(&self, path: &Path) -> io::Result<()> {
            self.record("remove_dir", path);
            RealFs.remove_dir(path)
        }

        fn exists(&self, path: &Path) -> io::Result<bool> {
            if let Some(kind) = self.exists_fails {
                return Err(io::Error::from(kind));
            }
            RealFs.exists(path)
        }

        fn is_dir(&self, path: &Path) -> bool {
            self.record_only_directories || RealFs.is_dir(path)
        }

        fn sync_directory(&self, path: &Path) -> io::Result<()> {
            self.record("sync", path);
            if self.record_only_directories {
                assert!(!path.as_os_str().is_empty(), "empty directory pathname");
                return Ok(());
            }
            if self.sync_fails_once.borrow().as_deref() == Some(path) {
                self.sync_fails_once.borrow_mut().take();
                return Err(io::Error::other("injected sync failure"));
            }
            RealFs.sync_directory(path)
        }

        fn note_no_hard_links(&self, _to: &Path) {
            self.notes.set(self.notes.get() + 1);
        }

        fn sleep(&self, wait: Duration) {
            self.log
                .borrow_mut()
                .push(format!("sleep:{}", wait.as_millis()));
        }
    }

    fn no_retry(_: &io::Error) -> bool {
        false
    }

    /// A scratch directory holding a source file `src` ("new") and, if
    /// `existing`, a destination `dst` ("old").
    fn file_pair(existing: bool) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("src");
        let to = dir.path().join("dst");
        fs::write(&from, "new").unwrap();
        if existing {
            fs::write(&to, "old").unwrap();
        }
        (dir, from, to)
    }

    /// A scratch directory holding a source directory `src` (with a file)
    /// and, if `existing`, an empty destination directory `dst`.
    fn dir_pair(existing: bool) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("src");
        let to = dir.path().join("dst");
        fs::create_dir(&from).unwrap();
        fs::write(from.join("bin"), "new").unwrap();
        if existing {
            fs::create_dir(&to).unwrap();
        }
        (dir, from, to)
    }

    // Not upstream's. The real primitive on this platform (renameat2 on
    // Linux, renamex_np on macOS, write-through MoveFileExW on Windows)
    // publishes a file and then refuses to replace it.
    #[test]
    fn publish_file_native_publishes_and_never_replaces() {
        let (_dir, from, to) = file_pair(false);
        publish_file(&from, &to).unwrap();
        assert_eq!(fs::read_to_string(&to).unwrap(), "new");
        assert!(!from.exists());

        fs::write(&from, "newer").unwrap();
        let error = publish_file(&from, &to).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&to).unwrap(), "new");
        assert_eq!(fs::read_to_string(&from).unwrap(), "newer");
    }

    // Not upstream's. With the native attempt reported unsupported (an
    // injected report, not a failure this platform produces), a file falls
    // back to a hard link and the source name is removed.
    #[test]
    fn publish_file_falls_back_to_link_when_native_unsupported() {
        let (_dir, from, to) = file_pair(false);
        let probe = Probe::new(Native::Unsupported);
        publish_file_with(&probe, &from, &to, no_retry).unwrap();
        assert_eq!(fs::read_to_string(&to).unwrap(), "new");
        assert!(!from.exists());
        assert_eq!(
            probe.log(),
            ["noreplace:dst", "link:dst", "remove_file:src"]
        );
        assert_eq!(probe.notes.get(), 0);
    }

    // Not upstream's. A destination that exists is never replaced on the
    // native path, the link path, or the last-resort path, and the source is
    // left in place each time.
    #[test]
    fn publish_file_existing_destination_survives_every_path() {
        for (native, links_unsupported) in [
            (Native::Real, false),
            (Native::Unsupported, false),
            (Native::Unsupported, true),
        ] {
            let (_dir, from, to) = file_pair(true);
            let mut probe = Probe::new(native);
            probe.links_unsupported = links_unsupported;
            let error = publish_file_with(&probe, &from, &to, no_retry).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
            assert_eq!(fs::read_to_string(&to).unwrap(), "old");
            assert_eq!(fs::read_to_string(&from).unwrap(), "new");
            assert!(!probe.log().iter().any(|call| call.starts_with("rename:")));
        }
    }

    // Not upstream's. With neither the native rename nor hard links (an
    // injected report), a free name is taken by the absence check and a plain
    // rename, and the no-hard-links note is made.
    #[test]
    fn publish_file_last_resort_checks_absence_then_renames() {
        let (_dir, from, to) = file_pair(false);
        let mut probe = Probe::new(Native::Unsupported);
        probe.links_unsupported = true;
        publish_file_with(&probe, &from, &to, no_retry).unwrap();
        assert_eq!(fs::read_to_string(&to).unwrap(), "new");
        assert!(!from.exists());
        assert_eq!(probe.log(), ["noreplace:dst", "link:dst", "rename:dst"]);
        assert_eq!(probe.notes.get(), 1);
    }

    // Not upstream's. With no native rename and no hard links, a failed
    // existence check (not NotFound) is an error, not "absent": nothing is
    // renamed, the destination keeps its bytes and the source stays.
    #[test]
    fn publish_file_last_resort_fails_closed_when_the_check_fails() {
        let (_dir, from, to) = file_pair(true);
        let mut probe = Probe::new(Native::Unsupported);
        probe.links_unsupported = true;
        probe.exists_fails = Some(io::ErrorKind::PermissionDenied);
        let error = publish_file_with(&probe, &from, &to, no_retry).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read_to_string(&to).unwrap(), "old");
        assert_eq!(fs::read_to_string(&from).unwrap(), "new");
        assert!(!probe.log().iter().any(|call| call.starts_with("rename:")));
    }

    // Not upstream's. The real existence check says absent only for NotFound.
    #[test]
    fn real_exists_reports_absent_only_for_not_found() {
        let (dir, from, _to) = file_pair(false);
        assert!(RealFs.exists(&from).unwrap());
        assert!(!RealFs.exists(&dir.path().join("missing")).unwrap());
    }

    // Not upstream's. Under an old seccomp profile the native step answers
    // EPERM; the file still goes through the hard link and publishes, and an
    // existing destination is never replaced on that path or the last resort.
    #[cfg(target_os = "linux")]
    #[test]
    fn publish_file_native_eperm_falls_back_and_never_replaces() {
        let (_dir, from, to) = file_pair(false);
        let probe = Probe::new(Native::Raw(libc::EPERM));
        publish_file_with(&probe, &from, &to, no_retry).unwrap();
        assert_eq!(fs::read_to_string(&to).unwrap(), "new");
        assert!(!from.exists());
        assert_eq!(
            probe.log(),
            ["noreplace:dst", "link:dst", "remove_file:src"]
        );

        for links_unsupported in [false, true] {
            let (_dir, from, to) = file_pair(true);
            let mut probe = Probe::new(Native::Raw(libc::EPERM));
            probe.links_unsupported = links_unsupported;
            let error = publish_file_with(&probe, &from, &to, no_retry).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
            assert_eq!(fs::read_to_string(&to).unwrap(), "old");
            assert_eq!(fs::read_to_string(&from).unwrap(), "new");
            assert!(!probe.log().iter().any(|call| call.starts_with("rename:")));
        }
    }

    // Not upstream's. An error that isn't "unsupported" (here a missing
    // source) comes back as it is, with no fallback tried.
    #[test]
    fn publish_file_other_errors_do_not_fall_back() {
        let dir = tempfile::tempdir().unwrap();
        let probe = Probe::new(Native::Real);
        let error = publish_file_with(
            &probe,
            &dir.path().join("missing"),
            &dir.path().join("dst"),
            no_retry,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(probe.log(), ["noreplace:dst"]);
    }

    // Not upstream's. The warning about a file system with no hard links is
    // given once per flag, the way once per process works.
    #[test]
    fn first_time_is_true_once() {
        let flag = AtomicBool::new(false);
        assert!(first_time(&flag));
        assert!(!first_time(&flag));
        assert!(!first_time(&flag));
    }

    // Not upstream's. Windows's retry set is access denied, sharing and lock
    // violations; nothing else, and nothing at all on other platforms.
    #[test]
    fn transient_errors_are_the_windows_set() {
        for code in [5, 32, 33] {
            assert!(windows_transient(&io::Error::from_raw_os_error(code)));
            assert_eq!(
                platform_transient(&io::Error::from_raw_os_error(code)),
                cfg!(windows)
            );
        }
        assert!(!windows_transient(&io::Error::from_raw_os_error(2)));
        assert!(!platform_transient(&io::Error::from_raw_os_error(2)));
    }

    // Not upstream's. A transient error is retried after 50, 100, 200, 400
    // and 800 ms and then succeeds (the classifier is injected, so this runs
    // on every platform).
    #[test]
    fn retry_waits_are_50_to_800_ms_then_succeeds() {
        let probe = Probe::new(Native::Real);
        let attempts = Cell::new(0);
        let result = retry_with(
            &probe,
            |_| true,
            || {
                attempts.set(attempts.get() + 1);
                if attempts.get() <= 5 {
                    Err(io::Error::from(io::ErrorKind::PermissionDenied))
                } else {
                    Ok(attempts.get())
                }
            },
        );
        assert_eq!(result.unwrap(), 6);
        assert_eq!(
            probe.log(),
            [
                "sleep:50",
                "sleep:100",
                "sleep:200",
                "sleep:400",
                "sleep:800"
            ]
        );
    }

    // Not upstream's. A transient error that never clears is returned after
    // the last wait, and an error the classifier rejects isn't retried.
    #[test]
    fn retry_gives_up_after_the_last_wait_and_skips_other_errors() {
        let probe = Probe::new(Native::Real);
        let attempts = Cell::new(0);
        let result: io::Result<()> = retry_with(
            &probe,
            |_| true,
            || {
                attempts.set(attempts.get() + 1);
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            },
        );
        assert!(result.is_err());
        assert_eq!(attempts.get(), 6);
        assert_eq!(probe.log().len(), 5);

        let probe = Probe::new(Native::Real);
        let result: io::Result<()> = retry_with(&probe, no_retry, || {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        });
        assert!(result.is_err());
        assert!(probe.log().is_empty());
    }

    // Not upstream's. The native attempt is retried on a transient error
    // before anything falls back, for a file and for a directory.
    #[test]
    fn publish_retries_the_native_attempt_on_transient_errors() {
        struct Flaky {
            inner: Probe,
            failures: Cell<u32>,
        }
        impl Fs for Flaky {
            fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
                if self.failures.get() > 0 {
                    self.failures.set(self.failures.get() - 1);
                    return Err(io::Error::from_raw_os_error(32));
                }
                self.inner.rename_noreplace(from, to)
            }
            fn rename_replace(&self, from: &Path, to: &Path) -> io::Result<()> {
                self.inner.rename_replace(from, to)
            }
            fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
                self.inner.hard_link(from, to)
            }
            fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
                self.inner.rename(from, to)
            }
            fn create_dir(&self, path: &Path) -> io::Result<()> {
                self.inner.create_dir(path)
            }
            fn remove_file(&self, path: &Path) -> io::Result<()> {
                self.inner.remove_file(path)
            }
            fn remove_dir(&self, path: &Path) -> io::Result<()> {
                self.inner.remove_dir(path)
            }
            fn exists(&self, path: &Path) -> io::Result<bool> {
                self.inner.exists(path)
            }
            fn is_dir(&self, path: &Path) -> bool {
                self.inner.is_dir(path)
            }
            fn sync_directory(&self, path: &Path) -> io::Result<()> {
                self.inner.sync_directory(path)
            }
            fn note_no_hard_links(&self, to: &Path) {
                self.inner.note_no_hard_links(to);
            }
            fn sleep(&self, wait: Duration) {
                self.inner.sleep(wait);
            }
        }

        let (_dir, from, to) = file_pair(false);
        let flaky = Flaky {
            inner: Probe::new(Native::Real),
            failures: Cell::new(2),
        };
        publish_file_with(&flaky, &from, &to, windows_transient).unwrap();
        assert_eq!(fs::read_to_string(&to).unwrap(), "new");
        assert_eq!(
            flaky.inner.log(),
            ["sleep:50", "sleep:100", "noreplace:dst"]
        );

        let (_dir, from, to) = dir_pair(false);
        let flaky = Flaky {
            inner: Probe::new(Native::Real),
            failures: Cell::new(1),
        };
        publish_directory_with(&flaky, &from, &to, windows_transient).unwrap();
        assert_eq!(fs::read_to_string(to.join("bin")).unwrap(), "new");
        assert_eq!(flaky.inner.log(), ["sleep:50", "noreplace:dst"]);
    }

    // Not upstream's. The real primitive publishes a directory and then
    // refuses to replace it, even an empty one.
    #[test]
    fn publish_directory_native_publishes_and_never_replaces() {
        let (_dir, from, to) = dir_pair(false);
        publish_directory(&from, &to).unwrap();
        assert_eq!(fs::read_to_string(to.join("bin")).unwrap(), "new");
        assert!(!from.exists());

        let (_dir, from, to) = dir_pair(true);
        let error = publish_directory(&from, &to).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(fs::read_dir(&to).unwrap().next().is_none());
        assert_eq!(fs::read_to_string(from.join("bin")).unwrap(), "new");
    }

    // Not upstream's. With the native attempt reported unsupported (an
    // injected report), a directory is claimed, the parent synced, renamed
    // onto the claim, and the parent synced again, in that order.
    #[test]
    fn publish_directory_fallback_claims_syncs_renames_syncs() {
        let (dir, from, to) = dir_pair(false);
        let probe = Probe::new(Native::Unsupported);
        publish_directory_with(&probe, &from, &to, no_retry).unwrap();
        assert_eq!(fs::read_to_string(to.join("bin")).unwrap(), "new");
        assert!(!from.exists());
        let parent = dir.path().file_name().unwrap().to_string_lossy();
        assert_eq!(
            probe.log(),
            [
                "noreplace:dst".to_string(),
                "create_dir:dst".to_string(),
                format!("sync:{parent}"),
                "rename:dst".to_string(),
                format!("sync:{parent}"),
            ]
        );
    }

    // Not upstream's. The fallback never replaces a destination that exists,
    // not even an empty one: the claim fails first and nothing is renamed.
    #[test]
    fn publish_directory_fallback_never_replaces_an_existing_destination() {
        let (_dir, from, to) = dir_pair(true);
        let probe = Probe::new(Native::Unsupported);
        let error = publish_directory_with(&probe, &from, &to, no_retry).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(probe.log(), ["noreplace:dst", "create_dir:dst"]);
        assert!(to.is_dir());
        assert_eq!(fs::read_to_string(from.join("bin")).unwrap(), "new");

        let (_dir, from, to) = dir_pair(true);
        fs::write(to.join("kept"), "kept").unwrap();
        let probe = Probe::new(Native::Unsupported);
        publish_directory_with(&probe, &from, &to, no_retry).unwrap_err();
        assert_eq!(fs::read_to_string(to.join("kept")).unwrap(), "kept");
    }

    // Not upstream's. A rename that finds the claim filled or the name taken
    // (ENOTEMPTY, EEXIST) fails closed and leaves what is there; any other
    // failure removes only the empty claim this call made.
    #[test]
    fn publish_directory_fallback_failed_rename_fails_closed() {
        for kind in [
            io::ErrorKind::DirectoryNotEmpty,
            io::ErrorKind::AlreadyExists,
        ] {
            let (_dir, from, to) = dir_pair(false);
            let mut probe = Probe::new(Native::Unsupported);
            probe.rename_fails = Some(kind);
            let error = publish_directory_with(&probe, &from, &to, no_retry).unwrap_err();
            assert_eq!(error.kind(), kind);
            assert!(to.is_dir(), "the claim is left for the next staging");
            assert!(
                !probe
                    .log()
                    .iter()
                    .any(|call| call.starts_with("remove_dir"))
            );
            assert!(from.join("bin").exists());
        }

        let (_dir, from, to) = dir_pair(false);
        let mut probe = Probe::new(Native::Unsupported);
        probe.rename_fails = Some(io::ErrorKind::Other);
        publish_directory_with(&probe, &from, &to, no_retry).unwrap_err();
        assert!(!to.exists(), "our own empty claim is removed");
        assert!(from.join("bin").exists());
    }

    // Not upstream's. The state a crash between the claim and the rename
    // leaves is an empty `to`: publishing refuses it (so the staging code
    // sees an existing version directory, finds no binary, and removes it if
    // it isn't kept); once that empty directory is removed the publication
    // goes through.
    #[test]
    fn publish_directory_fallback_after_a_crash_left_an_empty_claim() {
        let (_dir, from, to) = dir_pair(true);
        let probe = Probe::new(Native::Unsupported);
        let error = publish_directory_with(&probe, &from, &to, no_retry).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(fs::read_dir(&to).unwrap().next().is_none());

        fs::remove_dir(&to).unwrap();
        let probe = Probe::new(Native::Unsupported);
        publish_directory_with(&probe, &from, &to, no_retry).unwrap();
        assert_eq!(fs::read_to_string(to.join("bin")).unwrap(), "new");
    }

    // Not upstream's. The native replacing move (write-through MoveFileExW
    // with REPLACE_EXISTING on Windows, rename on Unix) replaces a file and
    // creates one that is absent.
    #[test]
    fn replace_file_replaces_and_creates() {
        let (_dir, from, to) = file_pair(true);
        replace_file(&from, &to).unwrap();
        assert_eq!(fs::read_to_string(&to).unwrap(), "new");
        assert!(!from.exists());

        let (_dir, from, to) = file_pair(false);
        replace_file(&from, &to).unwrap();
        assert_eq!(fs::read_to_string(&to).unwrap(), "new");
        sync_parent(&to).unwrap();
    }

    // Not upstream's. The replacing move retries a transient error before
    // giving up (injected classifier and failure, so every platform runs it).
    #[test]
    fn replace_file_retries_transient_errors() {
        let (_dir, from, to) = file_pair(true);
        let probe = Probe::new(Native::Real);
        let calls = Cell::new(0);
        let result = retry_with(&probe, windows_transient, || {
            calls.set(calls.get() + 1);
            if calls.get() < 3 {
                Err(io::Error::from_raw_os_error(5))
            } else {
                probe.rename_replace(&from, &to)
            }
        });
        result.unwrap();
        assert_eq!(fs::read_to_string(&to).unwrap(), "new");
        assert_eq!(probe.log(), ["sleep:50", "sleep:100", "replace:dst"]);
        // And the wrapper over the same operation, with nothing to retry.
        fs::write(&from, "newest").unwrap();
        replace_file_with(&probe, &from, &to, no_retry).unwrap();
        assert_eq!(fs::read_to_string(&to).unwrap(), "newest");
    }

    // Not upstream's. The real move of a file that does not exist reports
    // NotFound and is not retried, with this platform's retry rule.
    #[test]
    fn replace_file_missing_source_is_not_retried() {
        let dir = tempfile::tempdir().unwrap();
        let probe = Probe::new(Native::Real);
        let error = replace_file_with(
            &probe,
            &dir.path().join("missing"),
            &dir.path().join("dst"),
            platform_transient,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(probe.log(), ["replace:dst"]);
    }

    // Not upstream's. Creating a path makes each new level, and syncs the
    // new directories deepest first and then the existing directory above
    // them, so every new entry is in a synced directory.
    #[test]
    fn create_dir_all_durable_syncs_new_levels_deepest_first() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let path = dir.path().join("a").join("b").join("c");
        let probe = Probe::new(Native::Real);
        create_dir_all_durable_with(&probe, &path, dir.path()).unwrap();
        assert!(path.is_dir());
        assert_eq!(
            probe.log(),
            [
                "create_dir:a".to_string(),
                "create_dir:b".to_string(),
                "create_dir:c".to_string(),
                "sync:c".to_string(),
                "sync:b".to_string(),
                "sync:a".to_string(),
                format!("sync:{root}"),
            ]
        );
    }

    // Not upstream's. An existing root is recorded with nothing above it:
    // the new level, then each existing level up to the root, and never the
    // root's parent (the temp directory holding `data`).
    #[test]
    fn create_dir_all_durable_syncs_nothing_above_an_existing_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("data");
        fs::create_dir_all(root.join("a")).unwrap();
        let tmp_name = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let probe = Probe::new(Native::Real);
        create_dir_all_durable_with(&probe, &root.join("a").join("b"), &root).unwrap();
        assert_eq!(
            probe.log(),
            ["create_dir:b", "sync:b", "sync:a", "sync:data"]
        );
        assert!(!probe.log().contains(&format!("sync:{tmp_name}")));
    }

    // Not upstream's: inferred roots for relative pins and a bare kept file
    // are empty lexically, but sync the current-directory root as '.', without
    // walking above it. The seam never opens or syncs the real current directory.
    #[test]
    fn relative_directory_roots_sync_dot_without_walking_above_it() {
        for (path, expected) in [
            (
                "versions/.pins",
                vec!["sync:.pins", "sync:versions", "sync:"],
            ),
            ("", vec!["sync:"]),
        ] {
            let mut probe = Probe::new(Native::Real);
            probe.record_only_directories = true;
            create_dir_all_durable_with(&probe, Path::new(path), Path::new("")).unwrap();
            assert_eq!(probe.log(), expected);
        }
    }

    // Not upstream's. A path that already exists makes nothing but is
    // re-synced up to the root, so a retry after a failed sync records it.
    #[test]
    fn create_dir_all_durable_resyncs_existing_levels_up_to_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("data");
        fs::create_dir_all(root.join("a").join("b")).unwrap();
        let probe = Probe::new(Native::Real);
        create_dir_all_durable_with(&probe, &root.join("a").join("b"), &root).unwrap();
        assert_eq!(probe.log(), ["sync:b", "sync:a", "sync:data"]);
    }

    // Not upstream's. The first call makes the levels and fails syncing the
    // middle one; the retry finds everything there and still syncs the
    // failed ancestor (and the rest of the chain to the root, not above).
    #[test]
    fn create_dir_all_durable_retry_resyncs_a_failed_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("data");
        fs::create_dir_all(&root).unwrap();
        let b = root.join("a").join("b");
        let c = b.join("c");
        let probe = Probe::new(Native::Real);
        *probe.sync_fails_once.borrow_mut() = Some(b.clone());
        create_dir_all_durable_with(&probe, &c, &root).unwrap_err();
        assert!(c.is_dir());
        assert_eq!(
            probe.log(),
            [
                "create_dir:a",
                "create_dir:b",
                "create_dir:c",
                "sync:c",
                "sync:b"
            ]
        );

        let probe = Probe::new(Native::Real);
        create_dir_all_durable_with(&probe, &c, &root).unwrap();
        assert_eq!(probe.log(), ["sync:c", "sync:b", "sync:a", "sync:data"]);
    }

    // Not upstream's. A root this call makes has its parent's entry synced
    // once, after the root, and a path outside the root is refused.
    #[test]
    fn create_dir_all_durable_syncs_the_parent_of_a_new_root_once() {
        let dir = tempfile::tempdir().unwrap();
        let tmp_name = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let root = dir.path().join("data");
        let probe = Probe::new(Native::Real);
        create_dir_all_durable_with(&probe, &root.join("a"), &root).unwrap();
        assert_eq!(
            probe.log(),
            [
                "create_dir:data".to_string(),
                "create_dir:a".to_string(),
                "sync:a".to_string(),
                "sync:data".to_string(),
                format!("sync:{tmp_name}"),
            ]
        );

        let error =
            create_dir_all_durable_with(&probe, &dir.path().join("other"), &root).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    // Not upstream's. Another process makes `a/b` right after this call makes
    // `a` (injected, deterministic): `b` is not ours, but `c` in it is, so
    // `b` is synced as `c`'s parent, and `a` and the root after it.
    #[test]
    fn create_dir_all_durable_syncs_the_parent_of_a_level_that_raced() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let (a, b) = (dir.path().join("a"), dir.path().join("a").join("b"));
        let mut probe = Probe::new(Native::Real);
        probe.race = Some((a, b.clone()));
        create_dir_all_durable_with(&probe, &b.join("c"), dir.path()).unwrap();
        assert!(b.join("c").is_dir());
        assert_eq!(
            probe.log(),
            [
                "create_dir:a".to_string(),
                "create_dir:b".to_string(),
                "create_dir:c".to_string(),
                "sync:c".to_string(),
                "sync:b".to_string(),
                "sync:a".to_string(),
                format!("sync:{root}"),
            ]
        );
    }

    // Not upstream's. A file where a directory is wanted is an error, and
    // nothing is synced for it.
    #[test]
    fn create_dir_all_durable_refuses_a_file_in_the_way() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a"), "file").unwrap();
        let probe = Probe::new(Native::Real);
        let error =
            create_dir_all_durable_with(&probe, &dir.path().join("a").join("b"), dir.path())
                .unwrap_err();
        assert!(error.kind() != io::ErrorKind::NotFound);
        assert!(!probe.log().iter().any(|call| call.starts_with("sync")));
    }

    // Not upstream's. The real helper makes a nested path, is repeatable,
    // and the directory syncs work on this platform (a no-op on Windows).
    #[test]
    fn create_dir_all_durable_real_and_sync_helpers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("versions").join("1.2.3");
        create_dir_all_durable(&path, dir.path()).unwrap();
        create_dir_all_durable(&path, dir.path()).unwrap();
        assert!(path.is_dir());
        sync_directory(&path).unwrap();
        sync_parent(&path).unwrap();
        sync_parent(&dir.path().join("bare")).unwrap();
    }

    // The one directory-sync rule: EINVAL on the fsync is done, everything
    // else is a failure, and a directory that can't be opened fails.
    #[cfg(unix)]
    #[test]
    fn a_directory_that_cannot_be_synced_is_done_but_other_errors_are_not() {
        assert!(dir_sync_outcome(Ok(())).is_ok());
        assert!(dir_sync_outcome(Err(io::Error::from_raw_os_error(libc_einval()))).is_ok());
        for errno in [13, 5, 28, 9] {
            let error = dir_sync_outcome(Err(io::Error::from_raw_os_error(errno))).unwrap_err();
            assert_eq!(error.raw_os_error(), Some(errno));
        }
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        assert_eq!(
            sync_directory(&missing).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[cfg(unix)]
    fn libc_einval() -> i32 {
        22
    }
}
