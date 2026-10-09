//! Child ownership, stable process identity and the supervisor lifetime lock.
use std::{
    fs::{File, OpenOptions, TryLockError},
    io,
    path::{Path, PathBuf},
};

#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;
#[cfg(unix)]
pub use unix::process_alive;
#[cfg(windows)]
pub use windows::process_alive;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub started: u64,
}

/// Held for the supervisor's entire lifetime, including switching-disabled mode.
/// Callers must validate the containing data directory before creating this file.
/// Never unlink or replace supervisor.lock: the OS lock protects its open inode.
#[derive(Debug)]
pub struct SupervisorLock {
    _file: File,
    path: PathBuf,
}
impl SupervisorLock {
    /// Nonblocking: a CLI must refuse fallback when this returns WouldBlock.
    pub fn try_acquire(data_dir: &Path) -> io::Result<Self> {
        let path = data_dir.join("supervisor.lock");
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
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
                    "supervisor lock is a reparse point",
                ));
            }
        }
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "supervisor lock is not a regular file",
            ));
        }
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file, path }),
            Err(TryLockError::WouldBlock) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "a supervisor exists or is starting",
            )),
            Err(TryLockError::Error(error)) => Err(error),
        }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Not upstream's: a live Windows lock cannot be deleted to bypass exclusion.
    #[cfg(windows)]
    #[test]
    fn a_live_lock_cannot_be_unlinked() {
        let dir = tempfile::tempdir().unwrap();
        let lock = SupervisorLock::try_acquire(dir.path()).unwrap();
        assert!(std::fs::remove_file(lock.path()).is_err());
        drop(lock);
        std::fs::remove_file(dir.path().join("supervisor.lock")).unwrap();
    }
    // Not upstream's: the lock protects startup and is released only with its owner.
    #[test]
    fn lifetime_lock() {
        let dir = tempfile::tempdir().unwrap();
        let lock = SupervisorLock::try_acquire(dir.path()).unwrap();
        assert_eq!(
            SupervisorLock::try_acquire(dir.path()).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(lock);
        assert!(SupervisorLock::try_acquire(dir.path()).is_ok());
    }
}
