// Ported from CLIProxyAPI internal/auth/codex/filelock_unix.go,
// filelock_windows.go and token.go:19-62 (v8.0.36, MIT).
//! Per-credential process mutex and exclusive OS lock, with a bounded wait.
//!
//! Async acquisition uses `spawn_blocking`, or runs inline without a runtime.
//! Synchronous callers must offload the whole operation when on a runtime.
//! An owned guard releases both locks on every exit, including cancellation. Lock
//! files are regular files, opened without following links, never removed or
//! truncated. The standard Windows lock overlaps upstream's byte-zero lock.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, TryLockError, Weak};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

/// Maximum time waiting for a credential's process and OS locks.
pub const CREDENTIAL_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const BACKOFF: Duration = Duration::from_millis(20);
type LockRegistry = Mutex<HashMap<PathBuf, Weak<AsyncMutex<()>>>>;
static LOCKS: OnceLock<LockRegistry> = OnceLock::new();
/// Owns both locks. Dropping it releases the OS lock before the process lock.
pub struct CredentialLock {
    file: File,
    path: PathBuf,
    _process: OwnedMutexGuard<()>,
}

impl std::fmt::Debug for CredentialLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CredentialLock")
    }
}

impl Drop for CredentialLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Upstream's trimmed absolute path plus `.lock`, resolving an existing
/// regular target or its parent so permitted directory aliases share one lock.
/// Nonregular leaf targets keep their lock in the credential parent so the
/// store can refuse them without creating locks beside devices or directories.
pub fn credential_lock_path(credential: &Path) -> io::Result<PathBuf> {
    let text = credential
        .as_os_str()
        .to_str()
        .ok_or_else(|| io::Error::other("credential path is not valid UTF-8"))?;
    let absolute = std::path::absolute(text.trim())?;
    let cleaned = super::path::clean(&absolute);
    let canonical = cleaned
        .canonicalize()
        .and_then(|resolved| {
            if resolved.metadata()?.is_file() {
                Ok(resolved)
            } else {
                Err(io::Error::other("credential target is not a regular file"))
            }
        })
        .or_else(|_| {
            let parent = cleaned
                .parent()
                .ok_or_else(|| io::Error::other("credential has no parent"))?;
            let leaf = cleaned
                .file_name()
                .ok_or_else(|| io::Error::other("credential has no file name"))?;
            parent.canonicalize().map(|parent| parent.join(leaf))
        })
        .unwrap_or(cleaned);
    let mut name = canonical.into_os_string();
    name.push(".lock");
    Ok(PathBuf::from(name))
}

impl CredentialLock {
    pub(crate) fn guards(&self, credential: &Path) -> io::Result<bool> {
        Ok(self.path == credential_lock_path(credential)?)
    }
}

fn process_lock(path: &Path) -> Arc<AsyncMutex<()>> {
    let key = path.to_path_buf();
    let mut locks = LOCKS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return lock;
    }
    locks.retain(|_, lock| lock.strong_count() != 0);
    let lock = Arc::new(AsyncMutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

fn open_lock(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Open the reparse point itself so it can be refused below.
        options.custom_flags(0x0020_0000 | 0x0200_0000);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(io::Error::other("credential lock is a reparse point"));
        }
    }
    if !metadata.is_file() {
        return Err(io::Error::other("credential lock is not a regular file"));
    }
    Ok(file)
}

/// Tries both process and OS locks once, without waiting or creating parent
/// directories. The lock file itself is opened with the usual no-follow checks.
/// Call from a blocking thread: path resolution and opening still perform I/O.
pub fn try_acquire_credential_lock(credential: &Path) -> io::Result<Option<CredentialLock>> {
    let path = credential_lock_path(credential)?;
    let process = {
        let mut locks = match LOCKS.get_or_init(Mutex::default).try_lock() {
            Ok(locks) => locks,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => return Ok(None),
        };
        match locks.get(&path).and_then(Weak::upgrade) {
            Some(lock) => lock,
            None => {
                let lock = Arc::new(AsyncMutex::new(()));
                locks.insert(path.clone(), Arc::downgrade(&lock));
                lock
            }
        }
    };
    let Ok(process) = process.try_lock_owned() else {
        return Ok(None);
    };
    let file = open_lock(&path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(CredentialLock {
            file,
            path,
            _process: process,
        })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

fn timeout() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "timed out waiting for credential lock",
    )
}

fn acquire_blocking(path: PathBuf, wait: Duration) -> io::Result<CredentialLock> {
    let start = Instant::now();
    let path = credential_lock_path(&path)?;
    if let Some(parent) = path.parent() {
        super::file_store::create_dir_all(parent)?;
    }
    let process = process_lock(&path);
    let process = loop {
        match process.clone().try_lock_owned() {
            Ok(guard) => break guard,
            Err(_) if start.elapsed() >= wait => return Err(timeout()),
            Err(_) => {
                #[cfg(test)]
                test_event(&path, "waiting");
                std::thread::sleep(BACKOFF.min(wait.saturating_sub(start.elapsed())));
            }
        }
    };
    let file = open_lock(&path)?;
    loop {
        match file.try_lock() {
            Ok(()) => {
                #[cfg(test)]
                test_event(&path, "acquired");
                return Ok(CredentialLock {
                    file,
                    path,
                    _process: process,
                });
            }
            Err(std::fs::TryLockError::WouldBlock) if start.elapsed() >= wait => {
                return Err(timeout());
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                #[cfg(test)]
                test_event(&path, "waiting");
                std::thread::sleep(BACKOFF.min(wait.saturating_sub(start.elapsed())))
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error),
        }
    }
}

/// Acquires both locks off the runtime worker, returning an owned guard.
/// A cancelled waiter drops its eventual result and therefore releases it.
pub async fn acquire_credential_lock(path: impl AsRef<Path>) -> io::Result<CredentialLock> {
    acquire_with_timeout(path.as_ref().to_path_buf(), CREDENTIAL_LOCK_TIMEOUT).await
}

pub(crate) async fn acquire_with_timeout(
    path: PathBuf,
    wait: Duration,
) -> io::Result<CredentialLock> {
    if tokio::runtime::Handle::try_current().is_err() {
        return acquire_blocking(path, wait);
    }
    let work = move || acquire_blocking(path, wait);
    let task = if let Some(operation) = crate::gate::Operation::current() {
        operation.run_blocking(work)
    } else {
        // Standalone locking admits no credential mutation; stores check their gate.
        #[allow(clippy::disallowed_methods)]
        tokio::task::spawn_blocking(work)
    };
    task.await
        .map_err(|_| io::Error::other("credential lock task failed"))?
}

/// Synchronous bridge. May block for the bounded credential-lock wait.
/// Async callers must offload the whole store operation.
pub(crate) fn acquire_sync(path: &Path) -> io::Result<CredentialLock> {
    acquire_blocking(path.to_path_buf(), CREDENTIAL_LOCK_TIMEOUT)
}

#[cfg(test)]
type TestHook = Arc<dyn Fn(&str) + Send + Sync>;
#[cfg(test)]
static TEST_HOOKS: OnceLock<Mutex<HashMap<PathBuf, TestHook>>> = OnceLock::new();
#[cfg(test)]
pub(crate) fn test_event(path: &Path, event: &str) {
    let hook = TEST_HOOKS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap()
        .get(path)
        .cloned();
    if let Some(hook) = hook {
        hook(event);
    }
}
#[cfg(test)]
pub(crate) fn set_test_hook(path: &Path, hook: Option<TestHook>) {
    let key = credential_lock_path(path).unwrap();
    let mut hooks = TEST_HOOKS.get_or_init(Mutex::default).lock().unwrap();
    if let Some(hook) = hook {
        hooks.insert(key, hook);
    } else {
        hooks.remove(&key);
    }
}

#[cfg(test)]
#[path = "credential_lock_tests.rs"]
mod tests;
