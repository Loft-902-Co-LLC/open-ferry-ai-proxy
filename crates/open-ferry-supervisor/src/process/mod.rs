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

#[cfg(test)]
thread_local! {
    pub(crate) static TERMINATION_MUST_NOT_BLOCK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// What a termination call found, about the worker's own process.
///
/// `AlreadyExited`: it had exited before the call's kill could end it, so the
/// kill is not what ended it.
/// - Unix: when the first check finds the exit, nothing is sent (a reaped pid
///   could be reused). A worker that exits between that check and the kill is
///   not reaped yet, so the kill reaches only its zombie, never a reused pid;
///   its own exit status, not a SIGKILL, shows the kill did not end it.
/// - Windows: the job it ran in is still terminated, ending any descendants
///   that outlived it; the job handle is ours, so no pid is involved.
///
/// `Issued`: the call ended a process that was still running, to the extent
/// it can be told.
/// - Unix: the exit check came first and the status is SIGKILL. That is
///   consistent with our kill, not proof of it: a SIGKILL from elsewhere
///   (the worker itself, the OOM killer, a process-group kill) in the
///   microseconds before our kill is indistinguishable. Accepted residual.
/// - Windows: the exit check came first and the job was then terminated. A
///   worker that exits between the two is reported `Issued`. Accepted
///   residual.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Termination {
    AlreadyExited,
    Issued,
}

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
        // A parallel child's fork-to-exec window can briefly retain our flock fd.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match SupervisorLock::try_acquire(dir.path()) {
                Ok(_) => break,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(std::time::Instant::now() < deadline, "lock stayed held");
                    std::thread::yield_now();
                }
                Err(error) => panic!("lock after release: {error}"),
            }
        }
    }
}
