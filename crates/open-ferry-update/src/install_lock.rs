//! The install lock: `.<binary>.lock` beside the installed binary. Not
//! upstream's.
//!
//! Whoever replaces the installed binary holds it through the replacement
//! and its receipt, and a supervisor holds it for its whole life, so nothing
//! replaces the binary a supervisor runs from, whichever data directory
//! either of them uses (design 10.3). It is keyed on the binary, not on a
//! data directory: two processes of one install can disagree on the data
//! directory, when their environments differ, but not on the file.
//!
//! Never unlink or replace the lock file: the OS lock protects its open file.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

/// The install lock of a binary, held until dropped.
#[derive(Debug)]
pub struct InstallLock {
    _file: File,
    path: PathBuf,
}

impl InstallLock {
    /// The lock file of the installed binary `installed`: `.<file name>.lock`
    /// in its directory, resolved: from the file itself when it exists, so a
    /// link's target is what is locked, else from its directory and its name.
    pub fn path_for(installed: &Path) -> io::Result<PathBuf> {
        let resolved = match fs::canonicalize(installed) {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let name = installed.file_name().ok_or_else(no_file_name)?;
                let parent = installed
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                fs::canonicalize(parent)?.join(name)
            }
            Err(error) => return Err(error),
        };
        let name = resolved.file_name().ok_or_else(no_file_name)?;
        let mut lock = OsString::from(".");
        lock.push(name);
        lock.push(".lock");
        Ok(resolved.with_file_name(lock))
    }

    /// Takes the lock of the installed binary `installed` without waiting:
    /// [`io::ErrorKind::WouldBlock`] while another process holds it. Refuses
    /// links and non-regular lock files.
    pub fn try_acquire(installed: &Path) -> io::Result<Self> {
        let path = Self::path_for(installed)?;
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // Share READ and WRITE so other contenders can open it and try
            // the same OS lock, but not DELETE: a held lock's directory entry
            // must not be renamed or removed.
            options
                .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT)
                .share_mode(
                    windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ
                        | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE,
                );
        }
        let file = options.open(&path)?;
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if file.metadata()?.file_attributes()
                & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                != 0
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "install lock is a reparse point",
                ));
            }
        }
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "install lock is not a regular file",
            ));
        }
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file, path }),
            Err(TryLockError::WouldBlock) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "another open-ferry supervisor or install holds the install lock",
            )),
            Err(TryLockError::Error(error)) => Err(error),
        }
    }

    /// The lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn no_file_name() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "the installed binary's path has no file name",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_sits_beside_the_binary_whether_or_not_it_exists() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let installed = dir.path().join("open-ferry");
        let expected = root.join(".open-ferry.lock");
        assert_eq!(InstallLock::path_for(&installed).unwrap(), expected);
        fs::write(&installed, b"binary").unwrap();
        assert_eq!(InstallLock::path_for(&installed).unwrap(), expected);
        assert!(InstallLock::path_for(&dir.path().join("missing/open-ferry")).is_err());
    }

    #[test]
    fn one_holder_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("open-ferry");
        let held = InstallLock::try_acquire(&installed).unwrap();
        assert!(held.path().ends_with(".open-ferry.lock"));
        let error = InstallLock::try_acquire(&installed).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        drop(held);
        // A parallel child's fork-to-exec window can briefly retain our flock fd.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match InstallLock::try_acquire(&installed) {
                Ok(_) => break,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(std::time::Instant::now() < deadline, "lock stayed held");
                    std::thread::yield_now();
                }
                Err(error) => panic!("lock after release: {error}"),
            }
        }
    }

    #[test]
    fn moving_the_binary_aside_keeps_the_same_lock() {
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("open-ferry");
        fs::write(&installed, b"old").unwrap();
        let held = InstallLock::try_acquire(&installed).unwrap();
        fs::rename(&installed, dir.path().join("open-ferry.old-1")).unwrap();
        assert_eq!(InstallLock::path_for(&installed).unwrap(), held.path());
        fs::write(&installed, b"new").unwrap();
        assert_eq!(InstallLock::path_for(&installed).unwrap(), held.path());
        let error = InstallLock::try_acquire(&installed).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    }

    #[cfg(windows)]
    #[test]
    fn a_held_lock_cannot_be_deleted_or_renamed() {
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("open-ferry.exe");
        let held = InstallLock::try_acquire(&installed).unwrap();
        assert!(fs::remove_file(held.path()).is_err());
        assert!(fs::rename(held.path(), dir.path().join("moved.lock")).is_err());
        drop(held);
        fs::remove_file(InstallLock::path_for(&installed).unwrap()).unwrap();
    }

    #[test]
    fn a_lock_that_is_a_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("open-ferry");
        fs::create_dir(InstallLock::path_for(&installed).unwrap()).unwrap();
        assert!(InstallLock::try_acquire(&installed).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_locks_its_target_and_a_linked_lock_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir(dir.path().join("real")).unwrap();
        fs::write(dir.path().join("real/open-ferry-1"), b"binary").unwrap();
        std::os::unix::fs::symlink("real/open-ferry-1", dir.path().join("open-ferry")).unwrap();
        assert_eq!(
            InstallLock::path_for(&dir.path().join("open-ferry")).unwrap(),
            root.join("real/.open-ferry-1.lock")
        );
        let other = dir.path().join("other");
        fs::write(&other, b"").unwrap();
        std::os::unix::fs::symlink(&other, dir.path().join(".plain.lock")).unwrap();
        assert!(InstallLock::try_acquire(&dir.path().join("plain")).is_err());
    }
}
